use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2::Message;
use objc2_core_foundation::{CFString, CGPoint, CGSize};
use objc2_core_graphics::*;
use objc2_core_text::*;
use objc2_metal::{
    MTLDevice, MTLPixelFormat, MTLRegion, MTLSize, MTLTexture, MTLTextureDescriptor,
    MTLTextureUsage, MTLOrigin,
};
use std::collections::HashMap;
use std::ffi::c_void;
use std::ptr::{self, NonNull};

#[derive(Copy, Clone, Debug)]
pub struct GlyphInfo {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
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
    glyph_cell_w: u32,
    glyph_cell_h: u32,
    font: Retained<CTFont>,
    descent: f64,
    pub device: Retained<ProtocolObject<dyn MTLDevice>>,
    color_space: Retained<CGColorSpace>,
}

impl GlyphAtlas {
    pub fn new(device: &ProtocolObject<dyn MTLDevice>, font_size: f64) -> Self {
        let font_name = CFString::from_static_str("Menlo");
        let font = unsafe { CTFont::with_name(&font_name, font_size, ptr::null()) };

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
            glyph_cell_w,
            glyph_cell_h,
            font: font.clone().into(),
            descent,
            device: device.retain(),
            color_space: color_space.into(),
        }
    }

    pub fn glyph(&self, c: char) -> Option<&GlyphInfo> {
        self.glyphs.get(&c)
    }

    /// Rasterize a single character on-demand and add it to the atlas.
    pub fn rasterize_char(&mut self, c: char) -> Option<GlyphInfo> {
        if let Some(g) = self.glyphs.get(&c) {
            return Some(*g);
        }

        // Get glyph ID
        let mut uni_buf = [0u16; 2];
        let encoded = c.encode_utf16(&mut uni_buf);
        let count = encoded.len();

        // For now only handle BMP characters (single UTF-16 unit)
        if count != 1 {
            return None;
        }

        let mut uni_char = uni_buf[0];
        let mut glyph_id: CGGlyph = 0;
        let ok = unsafe {
            self.font.glyphs_for_characters(
                NonNull::new(&mut uni_char).unwrap(),
                NonNull::new(&mut glyph_id).unwrap(),
                1,
            )
        };

        if !ok || glyph_id == 0 {
            return None;
        }

        // Check if we need to wrap to next row
        if self.next_x + self.glyph_cell_w > self.atlas_width {
            self.next_x = 0;
            self.next_y += self.glyph_cell_h;
        }

        // Grow atlas if needed
        if self.next_y + self.glyph_cell_h > self.atlas_height {
            self.grow_atlas();
        }

        let atlas_x = self.next_x;
        let atlas_y = self.next_y;

        // Render glyph into cell-sized bitmap with fixed baseline
        let bmp_w = self.cell_width as usize;
        let bmp_h = self.cell_height as usize;
        let bmp_bpr = bmp_w * 4;
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

        let mut pos = CGPoint { x: 0.0, y: self.descent };
        unsafe {
            self.font.draw_glyphs(
                NonNull::new(&mut glyph_id).unwrap(),
                NonNull::new(&mut pos).unwrap(),
                1,
                &bmp_ctx,
            );
        }

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
            width: self.glyph_cell_w,
            height: self.glyph_cell_h,
        };
        self.glyphs.insert(c, info);

        // Advance cursor
        self.next_x += self.glyph_cell_w;

        log::debug!("Rasterized '{}' (U+{:04X}) into atlas", c, c as u32);
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
