use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2::Message;
use objc2_core_foundation::{CFRange, CFRetained, CFString, CGPoint, CGSize};
use objc2_core_graphics::*;
use objc2_core_text::*;
use objc2_metal::{
    MTLDevice, MTLPixelFormat, MTLRegion, MTLSize, MTLTexture, MTLTextureDescriptor,
    MTLTextureUsage, MTLOrigin,
};
use unicode_width::UnicodeWidthChar;
use std::collections::HashMap;
use std::ffi::c_void;
use std::ptr::{self, NonNull};

#[derive(Copy, Clone, Debug)]
pub struct GlyphInfo {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
    pub is_color: bool,
}

pub struct GlyphAtlas {
    pub texture: Retained<ProtocolObject<dyn MTLTexture>>,
    pub glyphs: HashMap<char, GlyphInfo>,
    /// Multi-codepoint grapheme cluster glyphs (flags, ZWJ sequences, skin tones)
    pub cluster_glyphs: HashMap<Box<str>, GlyphInfo>,
    pub cell_width: f32,
    pub cell_height: f32,
    pub atlas_width: u32,
    pub atlas_height: u32,
    // Dynamic atlas state
    atlas_buf: Vec<u8>,
    next_x: u32,
    next_y: u32,
    glyph_cell_h: u32,
    font: Retained<CTFont>,
    descent: f64,
    pub device: Retained<ProtocolObject<dyn MTLDevice>>,
    color_space: Retained<CGColorSpace>,
    fallback_fonts: HashMap<char, CFRetained<CTFont>>,
    // Overlay font (larger, for overlays like Memory Report)
    overlay_font: Retained<CTFont>,
    overlay_descent: f64,
    pub overlay_glyphs: HashMap<char, GlyphInfo>,
    pub overlay_cell_width: f32,
    pub overlay_cell_height: f32,
}

impl GlyphAtlas {
    pub fn new(device: &ProtocolObject<dyn MTLDevice>, font_size: f64, font_name: &str) -> Self {
        let cf_font_name = CFString::from_str(font_name);
        let mut font = unsafe { CTFont::with_name(&cf_font_name, font_size, ptr::null()) };

        // Verify CoreText returned the requested font (it silently substitutes if not found)
        let actual_name = unsafe { font.display_name() }.to_string();
        if !actual_name.to_lowercase().contains(&font_name.to_lowercase()) {
            log::warn!(
                "Font '{}' not found (CoreText returned '{}'), falling back to Menlo",
                font_name, actual_name
            );
            let fallback_name = CFString::from_str("Menlo");
            font = unsafe { CTFont::with_name(&fallback_name, font_size, ptr::null()) };
        }

        let ascent = unsafe { font.ascent() };
        let descent = unsafe { font.descent() };
        let leading = unsafe { font.leading() };
        let cell_height = (ascent + descent + leading).ceil() as f32;

        // Get cell width from 'M'
        let mut uni: u16 = 'M' as u16;
        let mut glyph_id: CGGlyph = 0;
        unsafe {
            font.glyphs_for_characters(
                NonNull::new(&mut uni).unwrap(),
                NonNull::new(&mut glyph_id).unwrap(),
                1,
            );
        }
        let mut advance = CGSize { width: 0.0, height: 0.0 };
        unsafe {
            font.advances_for_glyphs(
                CTFontOrientation::Horizontal,
                NonNull::new(&mut glyph_id).unwrap(),
                &mut advance,
                1,
            );
        }
        let cell_width = advance.width.ceil() as f32;

        let chars_per_row = 16u32;
        let glyph_cell_w = cell_width.round() as u32;
        let glyph_cell_h = cell_height.round() as u32;
        let num_chars = 95u32; // ' ' to '~'
        let rows = (num_chars + chars_per_row - 1) / chars_per_row;
        let atlas_width = chars_per_row * glyph_cell_w;
        let atlas_height = rows * glyph_cell_h;

        let color_space = CGColorSpace::new_device_rgb()
            .expect("failed to create color space");

        let atlas_bpr = atlas_width as usize * 4;
        let mut atlas_buf = vec![0u8; atlas_bpr * atlas_height as usize];
        let mut glyphs = HashMap::new();

        let bmp_w = cell_width as usize;
        let bmp_h = cell_height as usize;
        let bmp_bpr = bmp_w * 4;

        for (i, c) in (' '..='~').enumerate() {
            let col = (i as u32) % chars_per_row;
            let row = (i as u32) / chars_per_row;
            let atlas_x = col * glyph_cell_w;
            let atlas_y = row * glyph_cell_h;

            let mut uni_char: u16 = c as u16;
            let mut glyph: CGGlyph = 0;
            let ok = unsafe {
                font.glyphs_for_characters(
                    NonNull::new(&mut uni_char).unwrap(),
                    NonNull::new(&mut glyph).unwrap(),
                    1,
                )
            };

            if !ok || glyph == 0 {
                glyphs.insert(c, GlyphInfo {
                    x: atlas_x, y: atlas_y,
                    width: glyph_cell_w, height: glyph_cell_h,
                    is_color: false,
                });
                continue;
            }

            // Render glyph into cell-sized bitmap with fixed baseline
            let mut bmp_buf = vec![0u8; bmp_bpr * bmp_h];

            let bmp_ctx = unsafe {
                CGBitmapContextCreate(
                    bmp_buf.as_mut_ptr() as *mut c_void,
                    bmp_w,
                    bmp_h,
                    8,
                    bmp_bpr,
                    Some(&color_space),
                    1u32,
                )
            };
            let bmp_ctx = match bmp_ctx {
                Some(ctx) => ctx,
                None => {
                    log::warn!("failed to create bitmap for '{}'", c);
                    glyphs.insert(c, GlyphInfo {
                        x: atlas_x, y: atlas_y,
                        width: glyph_cell_w, height: glyph_cell_h,
                        is_color: false,
                    });
                    continue;
                }
            };

            CGContext::set_rgb_fill_color(Some(&bmp_ctx), 1.0, 1.0, 1.0, 1.0);

            // Draw at baseline: CG y=0 is bottom, baseline sits at y=descent
            let mut pos = CGPoint { x: 0.0, y: descent };
            unsafe {
                font.draw_glyphs(
                    NonNull::new(&mut glyph).unwrap(),
                    NonNull::new(&mut pos).unwrap(),
                    1,
                    &bmp_ctx,
                );
            }

            // Copy cell bitmap to atlas
            for py in 0..bmp_h {
                let dst_y = atlas_y as usize + py;
                if dst_y >= atlas_height as usize { break; }
                let src_off = py * bmp_bpr;
                let dst_off = dst_y * atlas_bpr + atlas_x as usize * 4;
                let copy_bytes = (bmp_w * 4).min(atlas_bpr - atlas_x as usize * 4);
                atlas_buf[dst_off..dst_off + copy_bytes]
                    .copy_from_slice(&bmp_buf[src_off..src_off + copy_bytes]);
            }

            glyphs.insert(c, GlyphInfo {
                x: atlas_x,
                y: atlas_y,
                width: glyph_cell_w,
                height: glyph_cell_h,
                is_color: false,
            });
        }

        // Create MTLTexture
        let desc = unsafe {
            let d = MTLTextureDescriptor::texture2DDescriptorWithPixelFormat_width_height_mipmapped(
                MTLPixelFormat::RGBA8Unorm,
                atlas_width as usize,
                atlas_height as usize,
                false,
            );
            d.setUsage(MTLTextureUsage::ShaderRead);
            d
        };

        let texture = device
            .newTextureWithDescriptor(&desc)
            .expect("failed to create atlas texture");

        let region = MTLRegion {
            origin: MTLOrigin { x: 0, y: 0, z: 0 },
            size: MTLSize {
                width: atlas_width as usize,
                height: atlas_height as usize,
                depth: 1,
            },
        };

        unsafe {
            texture.replaceRegion_mipmapLevel_withBytes_bytesPerRow(
                region,
                0,
                NonNull::new(atlas_buf.as_mut_ptr() as *mut c_void).unwrap(),
                atlas_bpr,
            );
        }

        log::info!(
            "Glyph atlas: {}x{}, cell: {:.1}x{:.1}, ascent: {:.1}, descent: {:.1}",
            atlas_width, atlas_height, cell_width, cell_height, ascent, descent
        );

        // Track cursor position after initial ASCII glyphs
        let last_idx = num_chars - 1;
        let last_col = last_idx % chars_per_row;
        let last_row = last_idx / chars_per_row;
        let next_x = (last_col + 1) % chars_per_row;
        let next_y = if next_x == 0 { last_row + 1 } else { last_row };

        // Create overlay font at larger size (for overlay screens like Memory Report)
        const OVERLAY_FONT_SCALE: f64 = 1.3;
        let overlay_font_size = font_size * OVERLAY_FONT_SCALE;
        let overlay_font = unsafe { CTFont::with_name(&cf_font_name, overlay_font_size, ptr::null()) };
        let overlay_ascent = unsafe { overlay_font.ascent() };
        let overlay_descent = unsafe { overlay_font.descent() };
        let overlay_leading = unsafe { overlay_font.leading() };
        let overlay_cell_height = (overlay_ascent + overlay_descent + overlay_leading).ceil() as f32;

        // Get overlay cell width from 'M'
        let mut overlay_uni: u16 = 'M' as u16;
        let mut overlay_glyph_id: CGGlyph = 0;
        unsafe {
            overlay_font.glyphs_for_characters(
                NonNull::new(&mut overlay_uni).unwrap(),
                NonNull::new(&mut overlay_glyph_id).unwrap(),
                1,
            );
        }
        let mut overlay_advance = CGSize { width: 0.0, height: 0.0 };
        unsafe {
            overlay_font.advances_for_glyphs(
                CTFontOrientation::Horizontal,
                NonNull::new(&mut overlay_glyph_id).unwrap(),
                &mut overlay_advance,
                1,
            );
        }
        let overlay_cell_width = overlay_advance.width.ceil() as f32;

        let mut atlas = GlyphAtlas {
            texture,
            glyphs,
            cluster_glyphs: HashMap::new(),
            cell_width,
            cell_height,
            atlas_width,
            atlas_height,
            atlas_buf,
            next_x: next_x * glyph_cell_w,
            next_y: next_y * glyph_cell_h,
            glyph_cell_h,
            font: font.clone().into(),
            descent,
            device: device.retain(),
            color_space: color_space.into(),
            fallback_fonts: HashMap::new(),
            overlay_font: overlay_font.into(),
            overlay_descent,
            overlay_glyphs: HashMap::new(),
            overlay_cell_width,
            overlay_cell_height,
        };

        // Pre-rasterize ASCII glyphs at overlay size
        for c in ' '..='~' {
            atlas.rasterize_overlay_char(c);
        }

        log::info!(
            "Overlay font: cell {:.1}x{:.1}",
            overlay_cell_width, overlay_cell_height
        );

        atlas
    }

    pub fn glyph(&self, c: char) -> Option<&GlyphInfo> {
        self.glyphs.get(&c)
    }

    pub fn overlay_glyph(&self, c: char) -> Option<&GlyphInfo> {
        self.overlay_glyphs.get(&c)
    }

    /// Rasterize a character at overlay font size and insert into the atlas.
    pub fn rasterize_overlay_char(&mut self, c: char) -> Option<GlyphInfo> {
        if let Some(g) = self.overlay_glyphs.get(&c) {
            return Some(*g);
        }

        let bmp_w = self.overlay_cell_width as usize;
        let bmp_h = self.overlay_cell_height as usize;
        let bmp_bpr = bmp_w * 4;

        let mut uni_buf = [0u16; 2];
        let encoded = c.encode_utf16(&mut uni_buf);
        let count = encoded.len();
        let mut glyph_buf = [0u16; 2];
        let ok = unsafe {
            self.overlay_font.glyphs_for_characters(
                NonNull::new(uni_buf.as_mut_ptr()).unwrap(),
                NonNull::new(glyph_buf.as_mut_ptr()).unwrap(),
                count as isize,
            )
        };
        let mut glyph_id = glyph_buf[0];
        if !ok || glyph_id == 0 {
            return None;
        }

        let mut bmp_buf = vec![0u8; bmp_bpr * bmp_h];
        let bmp_ctx = unsafe {
            CGBitmapContextCreate(
                bmp_buf.as_mut_ptr() as *mut c_void,
                bmp_w,
                bmp_h,
                8,
                bmp_bpr,
                Some(&self.color_space),
                1u32,
            )
        };
        let bmp_ctx = match bmp_ctx {
            Some(ctx) => ctx,
            None => return None,
        };

        CGContext::set_rgb_fill_color(Some(&bmp_ctx), 1.0, 1.0, 1.0, 1.0);

        let mut pos = CGPoint { x: 0.0, y: self.overlay_descent };
        unsafe {
            self.overlay_font.draw_glyphs(
                NonNull::new(&mut glyph_id).unwrap(),
                NonNull::new(&mut pos).unwrap(),
                1,
                &bmp_ctx,
            );
        }

        let info = self.insert_bitmap_raw(&bmp_buf, bmp_w, bmp_h, false)?;
        self.overlay_glyphs.insert(c, info);
        Some(info)
    }

    /// Resolve glyph ID and font for a character, using fallback if needed.
    /// Returns (glyph_id, font_to_use) where font_to_use is either the primary
    /// font or a cached fallback font.
    fn resolve_glyph(&mut self, c: char) -> Option<(CGGlyph, *const CTFont)> {
        let mut uni_buf = [0u16; 2];
        let encoded = c.encode_utf16(&mut uni_buf);
        let count = encoded.len();
        log::trace!("resolve_glyph: '{}' U+{:04X} utf16_len={}", c, c as u32, count);

        let mut glyph_buf = [0u16; 2];
        let ok = unsafe {
            self.font.glyphs_for_characters(
                NonNull::new(uni_buf.as_mut_ptr()).unwrap(),
                NonNull::new(glyph_buf.as_mut_ptr()).unwrap(),
                count as isize,
            )
        };

        // For surrogate pairs, CoreText puts the glyph in the first slot
        let glyph_id = glyph_buf[0];

        log::trace!("  primary: ok={} glyph_buf={:?}", ok, &glyph_buf[..count]);

        if ok && glyph_id != 0 {
            return Some((glyph_id, &*self.font as *const CTFont));
        }

        // Try font fallback via CoreText
        if !self.fallback_fonts.contains_key(&c) {
            let s = c.to_string();
            let cf_str = CFString::from_str(&s);
            let fallback = unsafe {
                self.font.for_string(
                    &cf_str,
                    CFRange { location: 0, length: cf_str.length() },
                )
            };
            let fallback_name = unsafe { fallback.display_name() };
            log::trace!("  fallback font: {:?}", fallback_name);
            self.fallback_fonts.insert(c, fallback);
        }

        let fallback = self.fallback_fonts.get(&c)?;
        let mut glyph_buf2 = [0u16; 2];
        let ok2 = unsafe {
            fallback.glyphs_for_characters(
                NonNull::new(uni_buf.as_mut_ptr()).unwrap(),
                NonNull::new(glyph_buf2.as_mut_ptr()).unwrap(),
                count as isize,
            )
        };

        let glyph_id2 = glyph_buf2[0];
        log::trace!("  fallback: ok2={} glyph_buf2={:?}", ok2, &glyph_buf2[..count]);
        if ok2 && glyph_id2 != 0 {
            Some((glyph_id2, &**fallback as *const CTFont))
        } else {
            log::warn!("  no glyph found for '{}' U+{:04X} in any font", c, c as u32);
            None
        }
    }

    /// Draw a block element or box-drawing character directly into a bitmap buffer.
    /// Returns true if the character was handled, false otherwise.
    fn draw_builtin_glyph(c: char, buf: &mut [u8], w: usize, h: usize) -> bool {
        let bpr = w * 4;

        // Helper: fill a rectangular region with white (RGBA=255)
        let fill_rect = |buf: &mut [u8], x0: usize, y0: usize, x1: usize, y1: usize| {
            for y in y0..y1.min(h) {
                for x in x0..x1.min(w) {
                    let off = y * bpr + x * 4;
                    buf[off] = 255;
                    buf[off + 1] = 255;
                    buf[off + 2] = 255;
                    buf[off + 3] = 255;
                }
            }
        };

        let hw = w / 2; // half width
        let hh = h / 2; // half height

        // Scale-aware line thicknesses
        let light = 1.max(h / 16);  // thin line (~1px at 14pt, 2px at 28pt)
        let heavy = 2.max(h / 8);   // thick line (~3px at 14pt, 4px at 28pt)

        // Precompute centered offsets for light lines
        let lx0 = hw.saturating_sub(light / 2);
        let lx1 = lx0 + light;
        let ly0 = hh.saturating_sub(light / 2);
        let ly1 = ly0 + light;

        // Precompute centered offsets for heavy lines
        let hx0 = hw.saturating_sub(heavy / 2);
        let hx1 = hx0 + heavy;
        let hy0 = hh.saturating_sub(heavy / 2);
        let hy1 = hy0 + heavy;

        // Double-line: two thin lines with a gap between them
        let dbl_gap = 2.max(h / 10);
        let dbl_span = light * 2 + dbl_gap;
        // Horizontal double (two lines stacked vertically)
        let dy0 = hh.saturating_sub(dbl_span / 2);
        let dy0e = dy0 + light;
        let dy1 = dy0 + light + dbl_gap;
        let dy1e = dy1 + light;
        // Vertical double (two lines side by side)
        let dx0 = hw.saturating_sub(dbl_span / 2);
        let dx0e = dx0 + light;
        let dx1 = dx0 + light + dbl_gap;
        let dx1e = dx1 + light;

        match c {
            // === Block Elements (U+2580-U+259F) ===
            '\u{2580}' => { fill_rect(buf, 0, 0, w, hh); true }         // ▀ upper half
            '\u{2581}' => { let t = h - h/8; fill_rect(buf, 0, t, w, h); true } // ▁ lower 1/8
            '\u{2582}' => { let t = h - h/4; fill_rect(buf, 0, t, w, h); true } // ▂ lower 1/4
            '\u{2583}' => { let t = h - 3*h/8; fill_rect(buf, 0, t, w, h); true } // ▃ lower 3/8
            '\u{2584}' => { fill_rect(buf, 0, hh, w, h); true }         // ▄ lower half
            '\u{2585}' => { let t = h - 5*h/8; fill_rect(buf, 0, t, w, h); true } // ▅ lower 5/8
            '\u{2586}' => { let t = h - 3*h/4; fill_rect(buf, 0, t, w, h); true } // ▆ lower 3/4
            '\u{2587}' => { let t = h - 7*h/8; fill_rect(buf, 0, t, w, h); true } // ▇ lower 7/8
            '\u{2588}' => { fill_rect(buf, 0, 0, w, h); true }          // █ full block
            '\u{2589}' => { let r = 7*w/8; fill_rect(buf, 0, 0, r, h); true } // ▉ left 7/8
            '\u{258A}' => { let r = 3*w/4; fill_rect(buf, 0, 0, r, h); true } // ▊ left 3/4
            '\u{258B}' => { let r = 5*w/8; fill_rect(buf, 0, 0, r, h); true } // ▋ left 5/8
            '\u{258C}' => { fill_rect(buf, 0, 0, hw, h); true }         // ▌ left half
            '\u{258D}' => { let r = 3*w/8; fill_rect(buf, 0, 0, r, h); true } // ▍ left 3/8
            '\u{258E}' => { let r = w/4; fill_rect(buf, 0, 0, r, h); true }   // ▎ left 1/4
            '\u{258F}' => { let r = w/8; fill_rect(buf, 0, 0, r, h); true }   // ▏ left 1/8
            '\u{2590}' => { fill_rect(buf, hw, 0, w, h); true }         // ▐ right half
            '\u{2591}' => { // ░ light shade (25%)
                for y in 0..h { for x in 0..w { if (x + y) % 4 == 0 { let o = y*bpr+x*4; buf[o]=255; buf[o+1]=255; buf[o+2]=255; buf[o+3]=255; } } }
                true
            }
            '\u{2592}' => { // ▒ medium shade (50%)
                for y in 0..h { for x in 0..w { if (x + y) % 2 == 0 { let o = y*bpr+x*4; buf[o]=255; buf[o+1]=255; buf[o+2]=255; buf[o+3]=255; } } }
                true
            }
            '\u{2593}' => { // ▓ dark shade (75%)
                for y in 0..h { for x in 0..w { if (x + y) % 4 != 0 { let o = y*bpr+x*4; buf[o]=255; buf[o+1]=255; buf[o+2]=255; buf[o+3]=255; } } }
                true
            }
            '\u{2594}' => { fill_rect(buf, 0, 0, w, h/8); true }        // ▔ upper 1/8
            '\u{2595}' => { let l = w - w/8; fill_rect(buf, l, 0, w, h); true } // ▕ right 1/8
            // Quadrants
            '\u{2596}' => { fill_rect(buf, 0, hh, hw, h); true }        // ▖ lower left
            '\u{2597}' => { fill_rect(buf, hw, hh, w, h); true }        // ▗ lower right
            '\u{2598}' => { fill_rect(buf, 0, 0, hw, hh); true }        // ▘ upper left
            '\u{2599}' => { // ▙ upper left + lower left + lower right
                fill_rect(buf, 0, 0, hw, hh);
                fill_rect(buf, 0, hh, w, h);
                true
            }
            '\u{259A}' => { // ▚ upper left + lower right
                fill_rect(buf, 0, 0, hw, hh);
                fill_rect(buf, hw, hh, w, h);
                true
            }
            '\u{259B}' => { // ▛ upper left + upper right + lower left
                fill_rect(buf, 0, 0, w, hh);
                fill_rect(buf, 0, hh, hw, h);
                true
            }
            '\u{259C}' => { // ▜ upper left + upper right + lower right
                fill_rect(buf, 0, 0, w, hh);
                fill_rect(buf, hw, hh, w, h);
                true
            }
            '\u{259D}' => { fill_rect(buf, hw, 0, w, hh); true }        // ▝ upper right
            '\u{259E}' => { // ▞ upper right + lower left
                fill_rect(buf, hw, 0, w, hh);
                fill_rect(buf, 0, hh, hw, h);
                true
            }
            '\u{259F}' => { // ▟ upper right + lower left + lower right
                fill_rect(buf, hw, 0, w, hh);
                fill_rect(buf, 0, hh, w, h);
                true
            }

            // === Box-Drawing Light (U+2500-U+253C) ===
            '\u{2500}' => { // ─ light horizontal
                fill_rect(buf, 0, ly0, w, ly1);
                true
            }
            '\u{2502}' => { // │ light vertical
                fill_rect(buf, lx0, 0, lx1, h);
                true
            }
            '\u{250C}' => { // ┌ light down and right
                fill_rect(buf, lx0, ly0, lx1, h);
                fill_rect(buf, lx0, ly0, w, ly1);
                true
            }
            '\u{2510}' => { // ┐ light down and left
                fill_rect(buf, lx0, ly0, lx1, h);
                fill_rect(buf, 0, ly0, lx1, ly1);
                true
            }
            '\u{2514}' => { // └ light up and right
                fill_rect(buf, lx0, 0, lx1, ly1);
                fill_rect(buf, lx0, ly0, w, ly1);
                true
            }
            '\u{2518}' => { // ┘ light up and left
                fill_rect(buf, lx0, 0, lx1, ly1);
                fill_rect(buf, 0, ly0, lx1, ly1);
                true
            }
            '\u{251C}' => { // ├ light vertical and right
                fill_rect(buf, lx0, 0, lx1, h);
                fill_rect(buf, lx0, ly0, w, ly1);
                true
            }
            '\u{2524}' => { // ┤ light vertical and left
                fill_rect(buf, lx0, 0, lx1, h);
                fill_rect(buf, 0, ly0, lx1, ly1);
                true
            }
            '\u{252C}' => { // ┬ light down and horizontal
                fill_rect(buf, 0, ly0, w, ly1);
                fill_rect(buf, lx0, ly0, lx1, h);
                true
            }
            '\u{2534}' => { // ┴ light up and horizontal
                fill_rect(buf, 0, ly0, w, ly1);
                fill_rect(buf, lx0, 0, lx1, ly1);
                true
            }
            '\u{253C}' => { // ┼ light vertical and horizontal
                fill_rect(buf, 0, ly0, w, ly1);
                fill_rect(buf, lx0, 0, lx1, h);
                true
            }

            // === Box-Drawing Heavy ===
            '\u{2501}' => { // ━ heavy horizontal
                fill_rect(buf, 0, hy0, w, hy1);
                true
            }
            '\u{2503}' => { // ┃ heavy vertical
                fill_rect(buf, hx0, 0, hx1, h);
                true
            }
            '\u{250F}' => { // ┏ heavy down and right
                fill_rect(buf, hx0, hy0, hx1, h);
                fill_rect(buf, hx0, hy0, w, hy1);
                true
            }
            '\u{2513}' => { // ┓ heavy down and left
                fill_rect(buf, hx0, hy0, hx1, h);
                fill_rect(buf, 0, hy0, hx1, hy1);
                true
            }
            '\u{2517}' => { // ┗ heavy up and right
                fill_rect(buf, hx0, 0, hx1, hy1);
                fill_rect(buf, hx0, hy0, w, hy1);
                true
            }
            '\u{251B}' => { // ┛ heavy up and left
                fill_rect(buf, hx0, 0, hx1, hy1);
                fill_rect(buf, 0, hy0, hx1, hy1);
                true
            }
            '\u{2523}' => { // ┣ heavy vertical and right
                fill_rect(buf, hx0, 0, hx1, h);
                fill_rect(buf, hx0, hy0, w, hy1);
                true
            }
            '\u{252B}' => { // ┫ heavy vertical and left
                fill_rect(buf, hx0, 0, hx1, h);
                fill_rect(buf, 0, hy0, hx1, hy1);
                true
            }
            '\u{2533}' => { // ┳ heavy down and horizontal
                fill_rect(buf, 0, hy0, w, hy1);
                fill_rect(buf, hx0, hy0, hx1, h);
                true
            }
            '\u{253B}' => { // ┻ heavy up and horizontal
                fill_rect(buf, 0, hy0, w, hy1);
                fill_rect(buf, hx0, 0, hx1, hy1);
                true
            }
            '\u{254B}' => { // ╋ heavy vertical and horizontal
                fill_rect(buf, 0, hy0, w, hy1);
                fill_rect(buf, hx0, 0, hx1, h);
                true
            }

            // === Rounded corners (light, approximate with straight lines) ===
            '\u{256D}' => { // ╭ arc down and right
                fill_rect(buf, lx0, ly0, lx1, h);
                fill_rect(buf, lx0, ly0, w, ly1);
                true
            }
            '\u{256E}' => { // ╮ arc down and left
                fill_rect(buf, lx0, ly0, lx1, h);
                fill_rect(buf, 0, ly0, lx1, ly1);
                true
            }
            '\u{256F}' => { // ╯ arc up and left
                fill_rect(buf, lx0, 0, lx1, ly1);
                fill_rect(buf, 0, ly0, lx1, ly1);
                true
            }
            '\u{2570}' => { // ╰ arc up and right
                fill_rect(buf, lx0, 0, lx1, ly1);
                fill_rect(buf, lx0, ly0, w, ly1);
                true
            }

            // === Double lines ===
            '\u{2550}' => { // ═ double horizontal
                fill_rect(buf, 0, dy0, w, dy0e);
                fill_rect(buf, 0, dy1, w, dy1e);
                true
            }
            '\u{2551}' => { // ║ double vertical
                fill_rect(buf, dx0, 0, dx0e, h);
                fill_rect(buf, dx1, 0, dx1e, h);
                true
            }
            '\u{2554}' => { // ╔ double down and right
                fill_rect(buf, dx0, dy0, dx0e, h);    // outer vertical down
                fill_rect(buf, dx0, dy0, w, dy0e);    // outer horizontal right
                fill_rect(buf, dx1, dy1, dx1e, h);    // inner vertical down
                fill_rect(buf, dx1, dy1, w, dy1e);    // inner horizontal right
                fill_rect(buf, dx1, dy0, w, dy0e);    // extend outer horiz past inner vert
                fill_rect(buf, dx0, dy1, dx0e, dy1e); // extend inner horiz to outer vert
                true
            }
            '\u{2557}' => { // ╗ double down and left
                fill_rect(buf, dx1, dy0, dx1e, h);    // outer vertical down
                fill_rect(buf, 0, dy0, dx1e, dy0e);   // outer horizontal left
                fill_rect(buf, dx0, dy1, dx0e, h);    // inner vertical down
                fill_rect(buf, 0, dy1, dx0e, dy1e);   // inner horizontal left
                fill_rect(buf, 0, dy0, dx0e, dy0e);   // extend outer horiz past inner vert
                fill_rect(buf, dx1, dy1, dx1e, dy1e); // extend inner horiz to outer vert
                true
            }
            '\u{255A}' => { // ╚ double up and right
                fill_rect(buf, dx0, 0, dx0e, dy1e);   // outer vertical up
                fill_rect(buf, dx0, dy1, w, dy1e);     // outer horizontal right
                fill_rect(buf, dx1, 0, dx1e, dy0e);   // inner vertical up
                fill_rect(buf, dx1, dy0, w, dy0e);     // inner horizontal right
                fill_rect(buf, dx1, dy1, w, dy1e);     // extend outer horiz past inner vert
                fill_rect(buf, dx0, dy0, dx0e, dy0e); // extend inner horiz to outer vert
                true
            }
            '\u{255D}' => { // ╝ double up and left
                fill_rect(buf, dx1, 0, dx1e, dy1e);   // outer vertical up
                fill_rect(buf, 0, dy1, dx1e, dy1e);    // outer horizontal left
                fill_rect(buf, dx0, 0, dx0e, dy0e);   // inner vertical up
                fill_rect(buf, 0, dy0, dx0e, dy0e);    // inner horizontal left
                fill_rect(buf, 0, dy1, dx0e, dy1e);    // extend outer horiz past inner vert
                fill_rect(buf, dx1, dy0, dx1e, dy0e); // extend inner horiz to outer vert
                true
            }

            // Dashes — fall through to font for now
            '\u{2504}' | '\u{2505}' | '\u{2508}' | '\u{2509}' | // ┄ ┅ ┈ ┉
            '\u{254C}' | '\u{254D}' | '\u{254E}' | '\u{254F}' => { // ╌ ╍ ╎ ╏
                false
            }

            _ => false,
        }
    }

    /// Rasterize a single character on-demand and add it to the atlas.
    pub fn rasterize_char(&mut self, c: char) -> Option<GlyphInfo> {
        if let Some(g) = self.glyphs.get(&c) {
            return Some(*g);
        }

        let width_cells = UnicodeWidthChar::width(c).unwrap_or(1).max(1);

        // Try builtin drawing for block elements and box-drawing first
        let bmp_w = self.cell_width as usize * width_cells;
        let bmp_h = self.cell_height as usize;
        let bmp_bpr = bmp_w * 4;
        let mut builtin_buf = vec![0u8; bmp_bpr * bmp_h];

        if Self::draw_builtin_glyph(c, &mut builtin_buf, bmp_w, bmp_h) {
            log::trace!("builtin glyph for '{}' U+{:04X}", c, c as u32);
            return self.insert_bitmap(c, &builtin_buf, bmp_w, bmp_h, false);
        }

        let (mut glyph_id, draw_font) = self.resolve_glyph(c)?;

        // Detect if the resolved font is a color (emoji) font via symbolic traits
        let is_color = unsafe {
            let font_ref = &*draw_font;
            let traits = font_ref.symbolic_traits();
            // kCTFontTraitColorGlyphs = 1 << 13 = 0x2000
            (traits.0 & (1 << 13)) != 0
        };

        // Render glyph into bitmap (wide chars get width_cells * cell_width)
        let mut bmp_buf = vec![0u8; bmp_bpr * bmp_h];

        let bmp_ctx = unsafe {
            CGBitmapContextCreate(
                bmp_buf.as_mut_ptr() as *mut c_void,
                bmp_w,
                bmp_h,
                8,
                bmp_bpr,
                Some(&self.color_space),
                1u32,
            )
        };
        let bmp_ctx = match bmp_ctx {
            Some(ctx) => ctx,
            None => return None,
        };

        if !is_color {
            CGContext::set_rgb_fill_color(Some(&bmp_ctx), 1.0, 1.0, 1.0, 1.0);
        }

        // Draw with the resolved font (primary or fallback) but keep primary baseline
        let mut pos = CGPoint { x: 0.0, y: self.descent };
        unsafe {
            let font_ref = &*draw_font;
            font_ref.draw_glyphs(
                NonNull::new(&mut glyph_id).unwrap(),
                NonNull::new(&mut pos).unwrap(),
                1,
                &bmp_ctx,
            );
        }

        // For color emoji, un-premultiply alpha so the straight-alpha blend mode works
        if is_color {
            for pixel in bmp_buf.chunks_exact_mut(4) {
                let a = pixel[3] as f32;
                if a > 0.0 && a < 255.0 {
                    let inv = 255.0 / a;
                    pixel[0] = (pixel[0] as f32 * inv).min(255.0) as u8;
                    pixel[1] = (pixel[1] as f32 * inv).min(255.0) as u8;
                    pixel[2] = (pixel[2] as f32 * inv).min(255.0) as u8;
                }
            }
        }

        // Debug: count non-zero pixels
        let nonzero = bmp_buf.iter().filter(|&&b| b != 0).count();
        log::trace!("rasterize '{}' U+{:04X}: bmp {}x{}, nonzero_bytes={}, width_cells={}, is_color={}", c, c as u32, bmp_w, bmp_h, nonzero, width_cells, is_color);

        self.insert_bitmap(c, &bmp_buf, bmp_w, bmp_h, is_color)
    }

    pub fn cluster_glyph(&self, cluster: &str) -> Option<&GlyphInfo> {
        self.cluster_glyphs.get(cluster)
    }

    /// Rasterize a multi-codepoint grapheme cluster (flags, ZWJ, skin tones)
    /// using CoreText CTLine for proper shaping.
    pub fn rasterize_cluster(&mut self, cluster: &str) -> Option<GlyphInfo> {
        if let Some(g) = self.cluster_glyphs.get(cluster) {
            return Some(*g);
        }

        use unicode_width::UnicodeWidthStr;
        let width_cells = UnicodeWidthStr::width(cluster).max(1);
        let bmp_w = self.cell_width as usize * width_cells;
        let bmp_h = self.cell_height as usize;
        let bmp_bpr = bmp_w * 4;

        // Create attributed string with the cluster text
        let cf_str = CFString::from_str(cluster);

        // Use CTLine for proper cluster shaping (handles flag sequences, ZWJ, etc.)
        let attrs = unsafe {
            use objc2_core_foundation::{CFDictionary, CFType};
            let key = objc2_core_text::kCTFontAttributeName;
            let font_val: &CFType = self.font.as_ref();
            CFDictionary::from_slices(&[&*key], &[font_val])
        };

        let attr_str = unsafe {
            use objc2_core_foundation::{CFAttributedString, CFDictionary};
            // Cast typed dictionary to untyped for CFAttributedString API
            let untyped: &CFDictionary = attrs.as_ref();
            CFAttributedString::new(None, Some(&cf_str), Some(untyped))
        }.expect("failed to create CFAttributedString");

        let line = unsafe { CTLine::with_attributed_string(&attr_str) };

        // Render into bitmap
        let mut bmp_buf = vec![0u8; bmp_bpr * bmp_h];

        // Use premultiplied alpha for color emoji rendering
        let bmp_ctx = unsafe {
            CGBitmapContextCreate(
                bmp_buf.as_mut_ptr() as *mut c_void,
                bmp_w,
                bmp_h,
                8,
                bmp_bpr,
                Some(&self.color_space),
                // kCGImageAlphaPremultipliedLast = 1
                1u32,
            )
        };
        let bmp_ctx = match bmp_ctx {
            Some(ctx) => ctx,
            None => {
                log::warn!("failed to create bitmap for cluster '{}'", cluster);
                return None;
            }
        };

        // Draw the CTLine at baseline
        unsafe {
            CGContext::set_text_position(Some(&bmp_ctx), 0.0, self.descent);
            line.draw(&bmp_ctx);
        }

        // Cluster emoji are always color
        let is_color = true;

        // Un-premultiply alpha
        for pixel in bmp_buf.chunks_exact_mut(4) {
            let a = pixel[3] as f32;
            if a > 0.0 && a < 255.0 {
                let inv = 255.0 / a;
                pixel[0] = (pixel[0] as f32 * inv).min(255.0) as u8;
                pixel[1] = (pixel[1] as f32 * inv).min(255.0) as u8;
                pixel[2] = (pixel[2] as f32 * inv).min(255.0) as u8;
            }
        }

        let nonzero = bmp_buf.iter().filter(|&&b| b != 0).count();
        log::trace!("rasterize_cluster '{}': bmp {}x{}, nonzero_bytes={}, width_cells={}", cluster, bmp_w, bmp_h, nonzero, width_cells);

        let info = self.insert_bitmap_raw(&bmp_buf, bmp_w, bmp_h, is_color)?;
        self.cluster_glyphs.insert(cluster.into(), info);
        Some(info)
    }

    /// Insert a rendered bitmap into the atlas and return glyph info.
    fn insert_bitmap(&mut self, c: char, bmp_buf: &[u8], bmp_w: usize, bmp_h: usize, is_color: bool) -> Option<GlyphInfo> {
        let info = self.insert_bitmap_raw(bmp_buf, bmp_w, bmp_h, is_color)?;
        self.glyphs.insert(c, info);
        log::trace!("Rasterized '{}' (U+{:04X}) into atlas", c, c as u32);
        Some(info)
    }

    /// Insert a rendered bitmap into the atlas, returning the GlyphInfo without storing it.
    fn insert_bitmap_raw(&mut self, bmp_buf: &[u8], bmp_w: usize, bmp_h: usize, is_color: bool) -> Option<GlyphInfo> {
        let bmp_bpr = bmp_w * 4;
        let slot_h = (bmp_h as u32).max(self.glyph_cell_h);

        // Check if we need to wrap to next row
        let slot_w = bmp_w as u32;
        if self.next_x + slot_w > self.atlas_width {
            self.next_x = 0;
            self.next_y += slot_h;
        }

        // Grow atlas if needed
        while self.next_y + slot_h > self.atlas_height {
            self.grow_atlas();
        }

        let atlas_x = self.next_x;
        let atlas_y = self.next_y;

        // Copy cell bitmap to atlas
        let atlas_bpr = self.atlas_width as usize * 4;
        for py in 0..bmp_h {
            let dst_y = atlas_y as usize + py;
            if dst_y >= self.atlas_height as usize { break; }
            let src_off = py * bmp_bpr;
            let dst_off = dst_y * atlas_bpr + atlas_x as usize * 4;
            let copy_bytes = (bmp_w * 4).min(atlas_bpr - atlas_x as usize * 4);
            self.atlas_buf[dst_off..dst_off + copy_bytes]
                .copy_from_slice(&bmp_buf[src_off..src_off + copy_bytes]);
        }

        // Upload the cell region to GPU
        let region = MTLRegion {
            origin: MTLOrigin { x: atlas_x as usize, y: atlas_y as usize, z: 0 },
            size: MTLSize { width: bmp_w, height: bmp_h, depth: 1 },
        };
        let region_start = atlas_y as usize * atlas_bpr + atlas_x as usize * 4;
        unsafe {
            self.texture.replaceRegion_mipmapLevel_withBytes_bytesPerRow(
                region,
                0,
                NonNull::new(self.atlas_buf[region_start..].as_ptr() as *mut c_void).unwrap(),
                atlas_bpr,
            );
        }

        let info = GlyphInfo {
            x: atlas_x,
            y: atlas_y,
            width: bmp_w as u32,
            height: bmp_h as u32,
            is_color,
        };

        // Advance cursor
        self.next_x += bmp_w as u32;

        Some(info)
    }

    /// Double the atlas height, creating a new texture and re-uploading.
    fn grow_atlas(&mut self) {
        let new_height = self.atlas_height * 2;
        let atlas_bpr = self.atlas_width as usize * 4;
        self.atlas_buf.resize(atlas_bpr * new_height as usize, 0);

        let desc = unsafe {
            let d = MTLTextureDescriptor::texture2DDescriptorWithPixelFormat_width_height_mipmapped(
                MTLPixelFormat::RGBA8Unorm,
                self.atlas_width as usize,
                new_height as usize,
                false,
            );
            d.setUsage(MTLTextureUsage::ShaderRead);
            d
        };

        let new_texture = self.device
            .newTextureWithDescriptor(&desc)
            .expect("failed to grow atlas texture");

        let region = MTLRegion {
            origin: MTLOrigin { x: 0, y: 0, z: 0 },
            size: MTLSize {
                width: self.atlas_width as usize,
                height: new_height as usize,
                depth: 1,
            },
        };

        unsafe {
            new_texture.replaceRegion_mipmapLevel_withBytes_bytesPerRow(
                region,
                0,
                NonNull::new(self.atlas_buf.as_mut_ptr() as *mut c_void).unwrap(),
                atlas_bpr,
            );
        }

        self.atlas_height = new_height;
        self.texture = new_texture;
        log::info!("Atlas grew to {}x{}", self.atlas_width, self.atlas_height);
    }

    /// Estimated heap bytes used by the atlas CPU buffer.
    pub fn mem_bytes(&self) -> usize {
        self.atlas_buf.capacity()
    }

    /// Atlas texture dimensions.
    pub fn texture_size(&self) -> (u32, u32) {
        (self.atlas_width, self.atlas_height)
    }
}
