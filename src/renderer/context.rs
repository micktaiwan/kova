//! What every pane of a host shares: the Metal command queue and pipeline, the
//! glyph atlas of the screen the host draws on, and the text helpers that read
//! that atlas. A `PaneRenderer` draws through it; the host (Kova's window
//! renderer today) owns it and the drawable.
//!
//! Kova.app keeps one context per window, as before the split: each window
//! rebuilds its atlas when it moves to a screen with another scale.

use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2_metal::*;
use std::ptr::NonNull;

use super::glyph_atlas::GlyphAtlas;
use super::glyph_fits;
use super::pipeline;
use super::vertex::Vertex;
use super::PANE_H_PADDING;
use crate::config::Config;

pub struct RenderContext {
    pub(super) command_queue: Retained<ProtocolObject<dyn MTLCommandQueue>>,
    pub(super) pipeline: Retained<ProtocolObject<dyn MTLRenderPipelineState>>,
    pub(super) atlas: GlyphAtlas,
    /// Atlas dimensions as the shader reads them, rewritten when the atlas grows.
    pub(super) atlas_size_buf: Retained<ProtocolObject<dyn MTLBuffer>>,
    last_atlas_size: [f32; 2],
    pub(super) font_size: f64,
    pub(super) font_name: String,
    /// Backing scale factor of the display the renderer currently draws on.
    pub(super) scale: f32,
}

impl RenderContext {
    pub fn new(
        device: &ProtocolObject<dyn MTLDevice>,
        pixel_format: MTLPixelFormat,
        scale: f64,
        config: &Config,
    ) -> Self {
        let command_queue = device
            .newCommandQueue()
            .expect("failed to create command queue");

        let pipeline = pipeline::create_pipeline(device, pixel_format);
        let atlas = GlyphAtlas::new(device, config.font.size * scale, scale, &config.font.family);

        let atlas_size = [atlas.atlas_width as f32, atlas.atlas_height as f32];
        let atlas_size_buf = unsafe {
            device.newBufferWithBytes_length_options(
                NonNull::new(atlas_size.as_ptr() as *mut _).unwrap(),
                std::mem::size_of_val(&atlas_size),
                MTLResourceOptions::CPUCacheModeDefaultCache,
            )
        }.unwrap();

        RenderContext {
            command_queue,
            pipeline,
            atlas,
            atlas_size_buf,
            last_atlas_size: atlas_size,
            font_size: config.font.size,
            font_name: config.font.family.clone(),
            scale: scale as f32,
        }
    }

    /// Rasterize a fresh atlas for a display of `scale`. Every vertex built
    /// before carries UVs into the old texture: the caller drops them.
    pub fn rebuild_atlas(&mut self, scale: f64) {
        let device = self.atlas.device.clone();
        self.scale = scale as f32;
        self.atlas = GlyphAtlas::new(&device, self.font_size * scale, scale, &self.font_name);
        // Update atlas size buffer
        let atlas_size = [self.atlas.atlas_width as f32, self.atlas.atlas_height as f32];
        self.last_atlas_size = atlas_size;
        unsafe {
            let ptr = self.atlas_size_buf.contents().as_ptr() as *mut [f32; 2];
            *ptr = atlas_size;
        }
    }

    /// Update atlas size buffer if atlas grew
    pub(super) fn sync_atlas_size(&mut self) {
        let atlas_size = [self.atlas.atlas_width as f32, self.atlas.atlas_height as f32];
        if atlas_size != self.last_atlas_size {
            self.last_atlas_size = atlas_size;
            unsafe {
                let ptr = self.atlas_size_buf.contents().as_ptr() as *mut [f32; 2];
                *ptr = atlas_size;
            }
        }
    }

    /// Horizontal pane padding in pixels for the current display scale.
    pub fn h_padding(&self) -> f32 {
        PANE_H_PADDING * self.scale
    }

    pub fn cell_size(&self) -> (f32, f32) {
        (self.atlas.cell_width, self.atlas.cell_height)
    }

    /// Render a string at the given position with optional scale factor.
    /// Returns the x position after the last rendered character.
    /// Stops rendering if x exceeds max_x.
    pub(super) fn render_text(
        &mut self,
        vertices: &mut Vec<Vertex>,
        text: &str,
        start_x: f32,
        y: f32,
        max_x: f32,
        fg: [f32; 4],
        no_bg: [f32; 4],
        scale: f32,
    ) -> f32 {
        let cell_w = self.atlas.cell_width * scale;
        let atlas_w = self.atlas.atlas_width as f32;
        let atlas_h = self.atlas.atlas_height as f32;

        for c in text.chars() {
            if self.atlas.glyph(c).is_none() {
                self.atlas.rasterize_char(c);
            }
        }

        let mut x = start_x;
        for c in text.chars() {
            if !glyph_fits(x, cell_w, max_x) { break; }
            let glyph = match self.atlas.glyph(c) {
                Some(g) => *g,
                None => { x += cell_w; continue; }
            };
            if glyph.width == 0 || glyph.height == 0 { x += cell_w; continue; }

            let gw = glyph.width as f32 * scale;
            let gh = glyph.height as f32 * scale;
            let tx = glyph.x as f32 / atlas_w;
            let ty = glyph.y as f32 / atlas_h;
            let tw = glyph.width as f32 / atlas_w;
            let th = glyph.height as f32 / atlas_h;

            vertices.push(Vertex { position: [x, y], tex_coords: [tx, ty], color: fg, bg_color: no_bg });
            vertices.push(Vertex { position: [x + gw, y], tex_coords: [tx + tw, ty], color: fg, bg_color: no_bg });
            vertices.push(Vertex { position: [x, y + gh], tex_coords: [tx, ty + th], color: fg, bg_color: no_bg });
            vertices.push(Vertex { position: [x + gw, y], tex_coords: [tx + tw, ty], color: fg, bg_color: no_bg });
            vertices.push(Vertex { position: [x + gw, y + gh], tex_coords: [tx + tw, ty + th], color: fg, bg_color: no_bg });
            vertices.push(Vertex { position: [x, y + gh], tex_coords: [tx, ty + th], color: fg, bg_color: no_bg });
            x += cell_w;
        }
        x
    }

    pub(super) fn render_status_text(
        &mut self,
        vertices: &mut Vec<Vertex>,
        text: &str,
        start_x: f32,
        y: f32,
        max_x: f32,
        fg: [f32; 4],
        no_bg: [f32; 4],
    ) -> f32 {
        self.render_text(vertices, text, start_x, y, max_x, fg, no_bg, 1.0)
    }

    /// Render text using the overlay font (rasterized at larger size, no bitmap stretching).
    /// `scale` upscales the overlay glyphs (1.0 = native overlay size). Keep close to 1.0:
    /// large factors reintroduce bilinear blur. Used >1.0 only for the help title.
    pub(super) fn render_overlay_text(
        &mut self,
        vertices: &mut Vec<Vertex>,
        text: &str,
        start_x: f32,
        y: f32,
        max_x: f32,
        fg: [f32; 4],
        no_bg: [f32; 4],
        scale: f32,
    ) -> f32 {
        let cell_w = self.atlas.overlay_cell_width * scale;
        let atlas_w = self.atlas.atlas_width as f32;
        let atlas_h = self.atlas.atlas_height as f32;

        for c in text.chars() {
            if self.atlas.overlay_glyph(c).is_none() {
                self.atlas.rasterize_overlay_char(c);
            }
        }

        let mut x = start_x;
        for c in text.chars() {
            if !glyph_fits(x, cell_w, max_x) { break; }
            let glyph = match self.atlas.overlay_glyph(c) {
                Some(g) => *g,
                None => { x += cell_w; continue; }
            };
            if glyph.width == 0 || glyph.height == 0 { x += cell_w; continue; }

            let gw = glyph.width as f32 * scale;
            let gh = glyph.height as f32 * scale;
            let tx = glyph.x as f32 / atlas_w;
            let ty = glyph.y as f32 / atlas_h;
            let tw = glyph.width as f32 / atlas_w;
            let th = glyph.height as f32 / atlas_h;

            // Snap to integer pixels so the nearest-sampled glyph stays crisp
            // (fractional positions smear the bitmap edges).
            let px = x.round();
            let py = y.round();

            vertices.push(Vertex { position: [px, py], tex_coords: [tx, ty], color: fg, bg_color: no_bg });
            vertices.push(Vertex { position: [px + gw, py], tex_coords: [tx + tw, ty], color: fg, bg_color: no_bg });
            vertices.push(Vertex { position: [px, py + gh], tex_coords: [tx, ty + th], color: fg, bg_color: no_bg });
            vertices.push(Vertex { position: [px + gw, py], tex_coords: [tx + tw, ty], color: fg, bg_color: no_bg });
            vertices.push(Vertex { position: [px + gw, py + gh], tex_coords: [tx + tw, ty + th], color: fg, bg_color: no_bg });
            vertices.push(Vertex { position: [px, py + gh], tex_coords: [tx, ty + th], color: fg, bg_color: no_bg });
            x += cell_w;
        }
        x
    }
}
