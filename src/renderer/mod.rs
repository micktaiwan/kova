pub mod glyph_atlas;
pub mod pipeline;
pub mod vertex;

use glyph_atlas::GlyphAtlas;
use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2_metal::*;
use objc2_quartz_core::{CAMetalDrawable, CAMetalLayer};
use parking_lot::RwLock;
use std::ptr::NonNull;
use std::sync::Arc;
use vertex::Vertex;

use crate::config::Config;
use crate::terminal::TerminalState;

const MAX_VERTEX_BYTES: usize = 4 * 1024 * 1024; // 4MB

pub struct Renderer {
    command_queue: Retained<ProtocolObject<dyn MTLCommandQueue>>,
    pipeline: Retained<ProtocolObject<dyn MTLRenderPipelineState>>,
    atlas: GlyphAtlas,
    // Pre-allocated buffers
    viewport_buf: Retained<ProtocolObject<dyn MTLBuffer>>,
    atlas_size_buf: Retained<ProtocolObject<dyn MTLBuffer>>,
    vertex_bufs: [Retained<ProtocolObject<dyn MTLBuffer>>; 2],
    vertex_buf_idx: usize,
    last_viewport: [f32; 2],
    last_atlas_size: [f32; 2],
    blink_counter: u32,
    last_cursor_epoch: u32,
    bg_color: [f32; 3],
    cursor_color: [f32; 3],
    font_size: f64,
    font_name: String,
    cursor_blink_frames: u32,
}

impl Renderer {
    pub fn new(
        device: &ProtocolObject<dyn MTLDevice>,
        layer: &CAMetalLayer,
        _terminal: Arc<RwLock<TerminalState>>,
        scale: f64,
        config: &Config,
    ) -> Self {
        let command_queue = device
            .newCommandQueue()
            .expect("failed to create command queue");

        let pixel_format = layer.pixelFormat();
        let pipeline = pipeline::create_pipeline(device, pixel_format);
        let atlas = GlyphAtlas::new(device, config.font.size * scale, &config.font.family);

        let make_vertex_buf = || {
            device.newBufferWithLength_options(
                MAX_VERTEX_BYTES,
                MTLResourceOptions(
                    MTLResourceOptions::CPUCacheModeDefaultCache.0
                        | MTLResourceOptions::StorageModeShared.0
                ),
            ).expect("failed to allocate vertex buffer")
        };

        let viewport = [0.0f32; 2];
        let viewport_buf = unsafe {
            device.newBufferWithBytes_length_options(
                NonNull::new(viewport.as_ptr() as *mut _).unwrap(),
                std::mem::size_of_val(&viewport),
                MTLResourceOptions::CPUCacheModeDefaultCache,
            )
        }.unwrap();

        let atlas_size = [atlas.atlas_width as f32, atlas.atlas_height as f32];
        let atlas_size_buf = unsafe {
            device.newBufferWithBytes_length_options(
                NonNull::new(atlas_size.as_ptr() as *mut _).unwrap(),
                std::mem::size_of_val(&atlas_size),
                MTLResourceOptions::CPUCacheModeDefaultCache,
            )
        }.unwrap();

        Renderer {
            command_queue,
            pipeline,
            atlas,
            viewport_buf,
            atlas_size_buf,
            vertex_bufs: [make_vertex_buf(), make_vertex_buf()],
            vertex_buf_idx: 0,
            last_viewport: [0.0; 2],
            last_atlas_size: atlas_size,
            blink_counter: 0,
            last_cursor_epoch: 0,
            bg_color: config.colors.background,
            cursor_color: config.colors.cursor,
            font_size: config.font.size,
            font_name: config.font.family.clone(),
            cursor_blink_frames: config.terminal.cursor_blink_frames,
        }
    }


    pub fn render(&mut self, layer: &CAMetalLayer, terminal: &Arc<RwLock<TerminalState>>, shell_ready: bool) {
        // Reset blink on cursor movement so cursor is immediately visible
        let epoch = terminal.read().cursor_move_epoch.load(std::sync::atomic::Ordering::Relaxed);
        if epoch != self.last_cursor_epoch {
            self.last_cursor_epoch = epoch;
            self.blink_counter = 0;
        }

        self.blink_counter = self.blink_counter.wrapping_add(1);
        let (blink_on, blink_changed) = if self.cursor_blink_frames >= 2 {
            let half = self.cursor_blink_frames / 2;
            (
                self.blink_counter % self.cursor_blink_frames < half,
                (self.blink_counter % half) == 0,
            )
        } else {
            // cursor_blink_frames 0 or 1: cursor always on, never triggers blink refresh
            (true, false)
        };

        let is_dirty = terminal.read().dirty.swap(false, std::sync::atomic::Ordering::Relaxed);
        if shell_ready && !is_dirty && !blink_changed {
            return;
        }

        let drawable = match layer.nextDrawable() {
            Some(d) => d,
            None => return,
        };

        let drawable_size = layer.drawableSize();
        let viewport_w = drawable_size.width as f32;
        let viewport_h = drawable_size.height as f32;

        let vertices = if shell_ready {
            let term = terminal.read();
            self.build_vertices(&term, viewport_w, viewport_h, blink_on)
        } else {
            self.build_loading_vertices(viewport_w, viewport_h)
        };

        // Update viewport buffer if changed
        let viewport = [viewport_w, viewport_h];
        if viewport != self.last_viewport {
            self.last_viewport = viewport;
            unsafe {
                let ptr = self.viewport_buf.contents().as_ptr() as *mut [f32; 2];
                *ptr = viewport;
            }
        }

        // Update atlas size buffer if atlas grew
        let atlas_size = [self.atlas.atlas_width as f32, self.atlas.atlas_height as f32];
        if atlas_size != self.last_atlas_size {
            self.last_atlas_size = atlas_size;
            unsafe {
                let ptr = self.atlas_size_buf.contents().as_ptr() as *mut [f32; 2];
                *ptr = atlas_size;
            }
        }

        let pass_desc = {
            let desc = MTLRenderPassDescriptor::new();
            let color = unsafe {
                desc.colorAttachments().objectAtIndexedSubscript(0)
            };
            let tex = drawable.texture();
            color.setTexture(Some(&tex));
            color.setLoadAction(MTLLoadAction::Clear);
            color.setClearColor(MTLClearColor {
                red: self.bg_color[0] as f64,
                green: self.bg_color[1] as f64,
                blue: self.bg_color[2] as f64,
                alpha: 1.0,
            });
            color.setStoreAction(MTLStoreAction::Store);
            desc
        };

        let cmd_buf = self.command_queue.commandBuffer().unwrap();
        let encoder = cmd_buf.renderCommandEncoderWithDescriptor(&pass_desc).unwrap();

        if !vertices.is_empty() {
            let vertex_bytes = unsafe {
                std::slice::from_raw_parts(
                    vertices.as_ptr() as *const u8,
                    std::mem::size_of_val(vertices.as_slice()),
                )
            };

            // Use pre-allocated double-buffered vertex buffer
            let buf_idx = self.vertex_buf_idx;
            self.vertex_buf_idx = 1 - buf_idx;
            let vertex_buf = &self.vertex_bufs[buf_idx];

            assert!(vertex_bytes.len() <= MAX_VERTEX_BYTES, "vertex data exceeds buffer size");
            unsafe {
                let ptr = vertex_buf.contents().as_ptr() as *mut u8;
                std::ptr::copy_nonoverlapping(vertex_bytes.as_ptr(), ptr, vertex_bytes.len());
            }

            encoder.setRenderPipelineState(&self.pipeline);
            unsafe {
                encoder.setVertexBuffer_offset_atIndex(Some(vertex_buf), 0, 0);
                encoder.setVertexBuffer_offset_atIndex(Some(&self.viewport_buf), 0, 1);
                encoder.setVertexBuffer_offset_atIndex(Some(&self.atlas_size_buf), 0, 2);
                encoder.setFragmentTexture_atIndex(Some(&*self.atlas.texture), 0);
            }

            unsafe {
                encoder.drawPrimitives_vertexStart_vertexCount(
                    MTLPrimitiveType::Triangle,
                    0,
                    vertices.len(),
                );
            }
        }

        encoder.endEncoding();
        let mtl_drawable: &ProtocolObject<dyn MTLDrawable> =
            ProtocolObject::from_ref(&*drawable);
        cmd_buf.presentDrawable(mtl_drawable);
        cmd_buf.commit();
    }

    fn build_vertices(
        &mut self,
        term: &TerminalState,
        _viewport_w: f32,
        _viewport_h: f32,
        blink_on: bool,
    ) -> Vec<Vertex> {
        // Pass 1: collect unknown chars for dynamic rasterization
        let display = term.visible_lines();
        let unknown: Vec<char> = {
            let mut seen = std::collections::HashSet::new();
            display.iter()
                .flat_map(|line| line.iter())
                .filter_map(|cell| {
                    let c = cell.c;
                    if c != ' ' && c != '\0' && self.atlas.glyph(c).is_none() && seen.insert(c) {
                        Some(c)
                    } else {
                        None
                    }
                })
                .collect()
        };

        // Pass 2: rasterize unknowns
        for c in unknown {
            self.atlas.rasterize_char(c);
        }

        // Pass 3: build vertices
        let cell_w = self.atlas.cell_width;
        let cell_h = self.atlas.cell_height;
        let atlas_w = self.atlas.atlas_width as f32;
        let atlas_h = self.atlas.atlas_height as f32;

        let mut vertices = Vec::with_capacity(display.len() * term.cols as usize * 6);

        for (row_idx, line) in display.iter().enumerate() {
            for col_idx in 0..term.cols as usize {
                let cell = if col_idx < line.len() {
                    &line[col_idx]
                } else {
                    continue;
                };

                let c = cell.c;
                if c == ' ' || c == '\0' {
                    if cell.bg != self.bg_color {
                        let x = col_idx as f32 * cell_w;
                        let y = row_idx as f32 * cell_h;
                        Self::push_bg_quad(&mut vertices, x, y, cell_w, cell_h, cell.bg);
                    }
                    continue;
                }

                let x = col_idx as f32 * cell_w;
                let y = row_idx as f32 * cell_h;

                if cell.bg != self.bg_color {
                    Self::push_bg_quad(&mut vertices, x, y, cell_w, cell_h, cell.bg);
                }

                let glyph = match self.atlas.glyph(c) {
                    Some(g) => *g,
                    None => continue,
                };

                if glyph.width == 0 || glyph.height == 0 {
                    continue;
                }

                let gx = x;
                let gy = y;
                let gw = glyph.width as f32;
                let gh = glyph.height as f32;

                let tx = glyph.x as f32 / atlas_w;
                let ty = glyph.y as f32 / atlas_h;
                let tw = glyph.width as f32 / atlas_w;
                let th = glyph.height as f32 / atlas_h;

                let fg = [cell.fg[0], cell.fg[1], cell.fg[2], 1.0];
                let no_bg = [0.0, 0.0, 0.0, 0.0];

                vertices.push(Vertex { position: [gx, gy], tex_coords: [tx, ty], color: fg, bg_color: no_bg });
                vertices.push(Vertex { position: [gx + gw, gy], tex_coords: [tx + tw, ty], color: fg, bg_color: no_bg });
                vertices.push(Vertex { position: [gx, gy + gh], tex_coords: [tx, ty + th], color: fg, bg_color: no_bg });
                vertices.push(Vertex { position: [gx + gw, gy], tex_coords: [tx + tw, ty], color: fg, bg_color: no_bg });
                vertices.push(Vertex { position: [gx + gw, gy + gh], tex_coords: [tx + tw, ty + th], color: fg, bg_color: no_bg });
                vertices.push(Vertex { position: [gx, gy + gh], tex_coords: [tx, ty + th], color: fg, bg_color: no_bg });
            }
        }

        // Draw cursor (adjusted for scroll offset)
        if term.cursor_visible && blink_on {
            let offset = term.scroll_offset();
            // In visible_lines(), grid starts at screen row `offset`,
            // so cursor_y in the grid maps to screen row `offset + cursor_y`
            let screen_y = offset + term.cursor_y as i32;
            if screen_y >= 0 && screen_y < term.rows as i32 {
                let cx = term.cursor_x as f32 * cell_w;
                let cy = screen_y as f32 * cell_h;
                Self::push_bg_quad(&mut vertices, cx, cy, cell_w, cell_h, self.cursor_color);
            }
        }

        vertices
    }

    fn build_loading_vertices(&mut self, viewport_w: f32, viewport_h: f32) -> Vec<Vertex> {
        let text = "starting...";
        let cell_w = self.atlas.cell_width;
        let cell_h = self.atlas.cell_height;
        let atlas_w = self.atlas.atlas_width as f32;
        let atlas_h = self.atlas.atlas_height as f32;

        let text_w = text.len() as f32 * cell_w;
        let start_x = (viewport_w - text_w) / 2.0;
        let start_y = (viewport_h - cell_h) / 2.0;

        let fg = [0.4, 0.4, 0.45, 1.0];
        let no_bg = [0.0, 0.0, 0.0, 0.0];
        let mut vertices = Vec::new();

        for (i, c) in text.chars().enumerate() {
            let glyph = match self.atlas.glyph(c) {
                Some(g) => *g,
                None => continue,
            };
            if glyph.width == 0 || glyph.height == 0 { continue; }

            let x = start_x + i as f32 * cell_w;
            let y = start_y;
            let gw = glyph.width as f32;
            let gh = glyph.height as f32;
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
        }

        vertices
    }

    pub fn rebuild_atlas(&mut self, scale: f64) {
        let device = self.atlas.device.clone();
        self.atlas = GlyphAtlas::new(&device, self.font_size * scale, &self.font_name);
        // Update atlas size buffer
        let atlas_size = [self.atlas.atlas_width as f32, self.atlas.atlas_height as f32];
        self.last_atlas_size = atlas_size;
        unsafe {
            let ptr = self.atlas_size_buf.contents().as_ptr() as *mut [f32; 2];
            *ptr = atlas_size;
        }
    }

    pub fn cell_size(&self) -> (f32, f32) {
        (self.atlas.cell_width, self.atlas.cell_height)
    }

    fn push_bg_quad(
        vertices: &mut Vec<Vertex>,
        x: f32,
        y: f32,
        w: f32,
        h: f32,
        bg: [f32; 3],
    ) {
        let bg4 = [bg[0], bg[1], bg[2], 1.0];
        let no_tex = [0.0, 0.0];
        let white = [1.0, 1.0, 1.0, 0.0];

        vertices.push(Vertex { position: [x, y], tex_coords: no_tex, color: white, bg_color: bg4 });
        vertices.push(Vertex { position: [x + w, y], tex_coords: no_tex, color: white, bg_color: bg4 });
        vertices.push(Vertex { position: [x, y + h], tex_coords: no_tex, color: white, bg_color: bg4 });
        vertices.push(Vertex { position: [x + w, y], tex_coords: no_tex, color: white, bg_color: bg4 });
        vertices.push(Vertex { position: [x + w, y + h], tex_coords: no_tex, color: white, bg_color: bg4 });
        vertices.push(Vertex { position: [x, y + h], tex_coords: no_tex, color: white, bg_color: bg4 });
    }
}
