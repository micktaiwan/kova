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
}

impl GlyphAtlas {
    pub fn new(device: &ProtocolObject<dyn MTLDevice>, font_size: f64, font_name: &str) -> Self {
        let cf_font_name = CFString::from_str(font_name);
        let font = unsafe { CTFont::with_name(&cf_font_name, font_size, ptr::null()) };

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
        let glyph_cell_w = cell_width as u32;
        let glyph_cell_h = cell_height as u32;
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

        GlyphAtlas {
            texture,
            glyphs,
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
        }
    }

    pub fn glyph(&self, c: char) -> Option<&GlyphInfo> {
        self.glyphs.get(&c)
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

        // Helper: fill a rectangular region with white (alpha=255 in channel 0)
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

            // === Box-Drawing (U+2500-U+257F) ===
            '\u{2500}' | '\u{2501}' => { // ─ ━ horizontal line
                let thick = if c == '\u{2501}' { 3 } else { 1 };
                let y0 = hh.saturating_sub(thick / 2);
                fill_rect(buf, 0, y0, w, y0 + thick);
                true
            }
            '\u{2502}' | '\u{2503}' => { // │ ┃ vertical line
                let thick = if c == '\u{2503}' { 3 } else { 1 };
                let x0 = hw.saturating_sub(thick / 2);
                fill_rect(buf, x0, 0, x0 + thick, h);
                true
            }
            '\u{250C}' => { // ┌
                fill_rect(buf, hw, hh, hw + 1, h);
                fill_rect(buf, hw, hh, w, hh + 1);
                true
            }
            '\u{2510}' => { // ┐
                fill_rect(buf, hw, hh, hw + 1, h);
                fill_rect(buf, 0, hh, hw + 1, hh + 1);
                true
            }
            '\u{2514}' => { // └
                fill_rect(buf, hw, 0, hw + 1, hh + 1);
                fill_rect(buf, hw, hh, w, hh + 1);
                true
            }
            '\u{2518}' => { // ┘
                fill_rect(buf, hw, 0, hw + 1, hh + 1);
                fill_rect(buf, 0, hh, hw + 1, hh + 1);
                true
            }
            '\u{251C}' => { // ├
                fill_rect(buf, hw, 0, hw + 1, h);
                fill_rect(buf, hw, hh, w, hh + 1);
                true
            }
            '\u{2524}' => { // ┤
                fill_rect(buf, hw, 0, hw + 1, h);
                fill_rect(buf, 0, hh, hw + 1, hh + 1);
                true
            }
            '\u{252C}' => { // ┬
                fill_rect(buf, 0, hh, w, hh + 1);
                fill_rect(buf, hw, hh, hw + 1, h);
                true
            }
            '\u{2534}' => { // ┴
                fill_rect(buf, 0, hh, w, hh + 1);
                fill_rect(buf, hw, 0, hw + 1, hh + 1);
                true
            }
            '\u{253C}' => { // ┼
                fill_rect(buf, 0, hh, w, hh + 1);
                fill_rect(buf, hw, 0, hw + 1, h);
                true
            }
            // Rounded corners (same as sharp corners visually at terminal scale)
            '\u{256D}' => { // ╭
                fill_rect(buf, hw, hh, hw + 1, h);
                fill_rect(buf, hw, hh, w, hh + 1);
                true
            }
            '\u{256E}' => { // ╮
                fill_rect(buf, hw, hh, hw + 1, h);
                fill_rect(buf, 0, hh, hw + 1, hh + 1);
                true
            }
            '\u{256F}' => { // ╯
                fill_rect(buf, hw, 0, hw + 1, hh + 1);
                fill_rect(buf, 0, hh, hw + 1, hh + 1);
                true
            }
            '\u{2570}' => { // ╰
                fill_rect(buf, hw, 0, hw + 1, hh + 1);
                fill_rect(buf, hw, hh, w, hh + 1);
                true
            }
            // Double lines
            '\u{2550}' => { // ═ double horizontal
                let y0 = hh.saturating_sub(1);
                fill_rect(buf, 0, y0, w, y0 + 1);
                fill_rect(buf, 0, y0 + 2, w, y0 + 3);
                true
            }
            '\u{2551}' => { // ║ double vertical
                let x0 = hw.saturating_sub(1);
                fill_rect(buf, x0, 0, x0 + 1, h);
                fill_rect(buf, x0 + 2, 0, x0 + 3, h);
                true
            }
            '\u{2552}'..='\u{256C}' => {
                // For remaining double-line box-drawing, fall through to font rendering
                // (less commonly used, can be added later)
                false
            }
            // Dashes
            '\u{2504}' | '\u{2505}' | '\u{2508}' | '\u{2509}' | // ┄ ┅ ┈ ┉
            '\u{254C}' | '\u{254D}' | '\u{254E}' | '\u{254F}' => { // ╌ ╍ ╎ ╏
                // Dashed lines - fall through to font for now
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

    /// Insert a rendered bitmap into the atlas and return glyph info.
    fn insert_bitmap(&mut self, c: char, bmp_buf: &[u8], bmp_w: usize, bmp_h: usize, is_color: bool) -> Option<GlyphInfo> {
        let bmp_bpr = bmp_w * 4;

        // Check if we need to wrap to next row
        let slot_w = bmp_w as u32;
        if self.next_x + slot_w > self.atlas_width {
            self.next_x = 0;
            self.next_y += self.glyph_cell_h;
        }

        // Grow atlas if needed
        if self.next_y + self.glyph_cell_h > self.atlas_height {
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
            height: self.glyph_cell_h,
            is_color,
        };
        self.glyphs.insert(c, info);

        // Advance cursor
        self.next_x += bmp_w as u32;

        log::trace!("Rasterized '{}' (U+{:04X}) into atlas", c, c as u32);
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
}
