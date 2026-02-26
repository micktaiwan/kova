pub mod glyph_atlas;
pub mod pipeline;
pub mod vertex;

pub const PANE_H_PADDING: f32 = 10.0;

/// Predefined tab color palette (macOS Finder-style tags).
/// Each entry is [R, G, B] in 0.0–1.0.
pub const TAB_COLORS: [[f32; 3]; 6] = [
    [0.82, 0.22, 0.22], // Red
    [0.90, 0.55, 0.15], // Orange
    [0.85, 0.75, 0.15], // Yellow
    [0.30, 0.70, 0.30], // Green
    [0.25, 0.50, 0.85], // Blue
    [0.60, 0.35, 0.75], // Violet
];

use glyph_atlas::GlyphAtlas;
use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2_metal::*;
use objc2_quartz_core::{CAMetalDrawable, CAMetalLayer};
use parking_lot::RwLock;
use std::ptr::NonNull;
use std::sync::Arc;
use std::time::SystemTime;
use vertex::Vertex;

use crate::config::Config;
use crate::terminal::{CursorShape, FilterMatch, TerminalState};

/// Data passed to the renderer for drawing filter overlay.
pub struct FilterRenderData {
    pub query: String,
    pub matches: Vec<FilterMatch>,
}

/// Sub-region of the drawable where a pane is rendered (in pixels).
#[derive(Clone, Copy)]
pub struct PaneViewport {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

const MAX_VERTEX_BYTES: usize = 16 * 1024 * 1024; // 16MB

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
    status_bar_enabled: bool,
    status_bar_bg: [f32; 3],
    status_bar_fg: [f32; 3],
    status_bar_cwd_color: [f32; 3],
    status_bar_branch_color: [f32; 3],
    status_bar_scroll_color: [f32; 3],
    status_bar_time_color: [f32; 3],
    last_minute: u32,
    selection_color: [f32; 3],
    tab_bar_bg: [f32; 3],
    tab_bar_fg: [f32; 3],
    tab_bar_active_bg: [f32; 3],
    /// Hovered URL: (focused_pane_id, visible_row, col_start, col_end)
    pub hovered_url: Option<(usize, u16, u16)>,
    /// Hovered URL text (for status bar display)
    pub hovered_url_text: Option<String>,
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
            status_bar_enabled: config.status_bar.enabled,
            status_bar_bg: config.status_bar.bg_color,
            status_bar_fg: config.status_bar.fg_color,
            status_bar_cwd_color: config.status_bar.cwd_color,
            status_bar_branch_color: config.status_bar.branch_color,
            status_bar_scroll_color: config.status_bar.scroll_color,
            status_bar_time_color: config.status_bar.time_color,
            last_minute: u32::MAX,
            selection_color: [0.45, 0.42, 0.20],
            tab_bar_bg: config.tab_bar.bg_color,
            tab_bar_fg: config.tab_bar.fg_color,
            tab_bar_active_bg: config.tab_bar.active_bg,
            hovered_url: None,
            hovered_url_text: None,
        }
    }


    /// Render multiple panes. Each entry: (terminal, viewport, shell_ready, is_focused).
    /// `separators` are line segments (x1, y1, x2, y2) drawn between splits.
    pub fn render_panes(
        &mut self,
        layer: &CAMetalLayer,
        panes: &[(Arc<RwLock<TerminalState>>, PaneViewport, bool, bool)],
        separators: &[(f32, f32, f32, f32)],
        tab_titles: &[(String, bool, Option<usize>, bool, bool)],
        filter: Option<&FilterRenderData>,
        tab_bar_left_inset: f32,
    ) {
        // Reset blink on cursor movement of focused pane
        if let Some((term, _, _, _)) = panes.iter().find(|(_, _, _, focused)| *focused) {
            let epoch = term.read().cursor_move_epoch.load(std::sync::atomic::Ordering::Relaxed);
            if epoch != self.last_cursor_epoch {
                self.last_cursor_epoch = epoch;
                self.blink_counter = 0;
            }
        }

        self.blink_counter = self.blink_counter.wrapping_add(1);
        let (blink_on, blink_changed) = if self.cursor_blink_frames >= 2 {
            let half = self.cursor_blink_frames / 2;
            (
                self.blink_counter % self.cursor_blink_frames < half,
                (self.blink_counter % half) == 0,
            )
        } else {
            (true, false)
        };

        // Check if minute changed for status bar update
        let minute_changed = if self.status_bar_enabled {
            let now = SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap_or_default();
            let current_minute = (now.as_secs() / 60) as u32;
            if current_minute != self.last_minute {
                self.last_minute = current_minute;
                true
            } else {
                false
            }
        } else {
            false
        };

        // Check if any pane is dirty (consume ALL flags, no short-circuit)
        let mut any_dirty = false;
        let mut any_not_ready = false;
        let mut any_sync_deferred = false;
        for (term, _, ready, _) in panes {
            if !ready { any_not_ready = true; }
            let t = term.read();
            // Synchronized output: this pane wants to defer, but don't block others
            if t.synchronized_output {
                if let Some(since) = t.sync_output_since {
                    if since.elapsed().as_millis() < 100 {
                        any_sync_deferred = true;
                        continue; // Don't consume dirty flag — pane will render later
                    }
                }
            }
            drop(t);
            if term.read().dirty.swap(false, std::sync::atomic::Ordering::Relaxed) {
                any_dirty = true;
            }
        }
        // If only sync-deferred panes were dirty, still need to render the others
        let all_ready = !any_not_ready;
        let has_filter = filter.is_some();
        if all_ready && !any_dirty && !any_sync_deferred && !blink_changed && !minute_changed && !has_filter {
            return;
        }

        let drawable = match layer.nextDrawable() {
            Some(d) => d,
            None => return,
        };

        let drawable_size = layer.drawableSize();
        let viewport_w = drawable_size.width as f32;
        let viewport_h = drawable_size.height as f32;

        // Build vertices for all panes
        let mut all_vertices = Vec::new();
        for (term, vp, shell_ready, is_focused) in panes {
            // Skip rendering focused pane content when filter overlay covers it
            if *is_focused && filter.is_some() {
                continue;
            }
            if *shell_ready {
                let t = term.read();
                // Only blink cursor on focused pane
                let show_blink = if *is_focused { blink_on } else { true };
                let mut verts = self.build_vertices(&t, vp, show_blink, *is_focused);
                all_vertices.append(&mut verts);
            } else {
                let mut verts = self.build_loading_vertices(vp);
                all_vertices.append(&mut verts);
            }
        }

        // Draw split separators (1px lines)
        if !separators.is_empty() {
            let no_tex = [0.0_f32, 0.0];
            let white = [1.0_f32, 1.0, 1.0, 0.0]; // unused (bg_color path)
            let sep_bg = [1.0_f32, 1.0, 1.0, 0.15]; // light grey via bg_color
            let thickness = 1.0_f32;
            for &(x1, y1, x2, y2) in separators {
                if (x1 - x2).abs() < 0.1 {
                    // Vertical line
                    let lx = x1 - thickness * 0.5;
                    let rx = x1 + thickness * 0.5;
                    all_vertices.push(Vertex { position: [lx, y1], tex_coords: no_tex, color: white, bg_color: sep_bg });
                    all_vertices.push(Vertex { position: [rx, y1], tex_coords: no_tex, color: white, bg_color: sep_bg });
                    all_vertices.push(Vertex { position: [lx, y2], tex_coords: no_tex, color: white, bg_color: sep_bg });
                    all_vertices.push(Vertex { position: [rx, y1], tex_coords: no_tex, color: white, bg_color: sep_bg });
                    all_vertices.push(Vertex { position: [rx, y2], tex_coords: no_tex, color: white, bg_color: sep_bg });
                    all_vertices.push(Vertex { position: [lx, y2], tex_coords: no_tex, color: white, bg_color: sep_bg });
                } else {
                    // Horizontal line
                    let ty = y1 - thickness * 0.5;
                    let by = y1 + thickness * 0.5;
                    all_vertices.push(Vertex { position: [x1, ty], tex_coords: no_tex, color: white, bg_color: sep_bg });
                    all_vertices.push(Vertex { position: [x2, ty], tex_coords: no_tex, color: white, bg_color: sep_bg });
                    all_vertices.push(Vertex { position: [x1, by], tex_coords: no_tex, color: white, bg_color: sep_bg });
                    all_vertices.push(Vertex { position: [x2, ty], tex_coords: no_tex, color: white, bg_color: sep_bg });
                    all_vertices.push(Vertex { position: [x2, by], tex_coords: no_tex, color: white, bg_color: sep_bg });
                    all_vertices.push(Vertex { position: [x1, by], tex_coords: no_tex, color: white, bg_color: sep_bg });
                }
            }
        }

        // Draw tab bar
        if tab_titles.len() > 0 {
            self.build_tab_bar_vertices(&mut all_vertices, viewport_w, tab_titles, tab_bar_left_inset);
        }

        // Draw filter overlay on focused pane
        if let Some(filter_data) = filter {
            if let Some((_, vp, _, _)) = panes.iter().find(|(_, _, _, focused)| *focused) {
                self.build_filter_overlay_vertices(&mut all_vertices, vp, filter_data);
            }
        }

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

        if !all_vertices.is_empty() {
            let vertex_bytes = unsafe {
                std::slice::from_raw_parts(
                    all_vertices.as_ptr() as *const u8,
                    std::mem::size_of_val(all_vertices.as_slice()),
                )
            };

            let buf_idx = self.vertex_buf_idx;
            self.vertex_buf_idx = 1 - buf_idx;
            let vertex_buf = &self.vertex_bufs[buf_idx];

            if vertex_bytes.len() > MAX_VERTEX_BYTES {
                log::error!("Vertex data ({} bytes) exceeds buffer size ({} bytes), skipping frame", vertex_bytes.len(), MAX_VERTEX_BYTES);
                encoder.endEncoding();
                cmd_buf.commit();
                return;
            }
            unsafe {
                let ptr = vertex_buf.contents().as_ptr() as *mut u8;
                std::ptr::copy_nonoverlapping(vertex_bytes.as_ptr(), ptr, vertex_bytes.len());
            }

            encoder.setRenderPipelineState(&self.pipeline);
            encoder.setScissorRect(MTLScissorRect {
                x: 0,
                y: 0,
                width: viewport_w as usize,
                height: viewport_h as usize,
            });
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
                    all_vertices.len(),
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
        vp: &PaneViewport,
        blink_on: bool,
        is_focused: bool,
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
        let ox = vp.x + PANE_H_PADDING;
        let oy = vp.y;

        // Calculate y_offset: push content to bottom when screen isn't full
        let max_rows = term.rows as usize;
        let last_used = display.iter().rposition(|line|
            line.iter().any(|c| c.c != ' ' && c.c != '\0')
        ).map_or(0, |i| i + 1);
        let y_offset = if last_used < max_rows && term.scroll_offset() == 0 {
            let raw = (max_rows - last_used) as f32 * cell_h;
            // Clamp: never push content beyond the viewport
            let max_offset = (vp.height - last_used as f32 * cell_h).max(0.0);
            raw.min(max_offset)
        } else {
            0.0
        };

        let mut vertices = Vec::with_capacity(display.len() * term.cols as usize * 6);

        // Precompute selection abs_line base if selection is active
        let has_selection = term.selection.is_some();
        let abs_line_base = if has_selection {
            term.scrollback_len() as i64 - term.scroll_offset() as i64
        } else {
            0
        };

        // Pass 1: backgrounds + selection highlights (under text)
        for (row_idx, line) in display.iter().enumerate() {
            let abs_line = (abs_line_base + row_idx as i64) as usize;
            let y = oy + y_offset + row_idx as f32 * cell_h;

            for col_idx in 0..term.cols as usize {
                let x = ox + col_idx as f32 * cell_w;

                // Cell background
                if col_idx < line.len() && line[col_idx].bg != self.bg_color {
                    Self::push_bg_quad(&mut vertices, x, y, cell_w, cell_h, line[col_idx].bg);
                }

                // Selection highlight (rendered on top of cell bg, under glyphs)
                if has_selection && term.is_selected(abs_line, col_idx as u16) {
                    Self::push_bg_quad(&mut vertices, x, y, cell_w, cell_h, self.selection_color);
                }
            }
        }

        // Pass 2: glyphs (on top of backgrounds and selection)
        for (row_idx, line) in display.iter().enumerate() {
            for col_idx in 0..term.cols as usize {
                let cell = if col_idx < line.len() {
                    &line[col_idx]
                } else {
                    continue;
                };

                let c = cell.c;
                if c == ' ' || c == '\0' {
                    continue;
                }

                if c == '─' && row_idx == 2 && col_idx < 3 {
                    log::trace!("render ─ at col={} row={} fg={:?} bg={:?}", col_idx, row_idx, cell.fg, cell.bg);
                }

                let glyph = match self.atlas.glyph(c) {
                    Some(g) => *g,
                    None => continue,
                };

                if glyph.width == 0 || glyph.height == 0 {
                    continue;
                }

                let gx = ox + col_idx as f32 * cell_w;
                let gy = oy + y_offset + row_idx as f32 * cell_h;
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

        // Draw URL underline for hovered URL
        if let Some((hover_row, col_start, col_end)) = self.hovered_url {
            let uy = oy + y_offset + hover_row as f32 * cell_h + cell_h - 1.0;
            let ux = ox + col_start as f32 * cell_w;
            let uw = (col_end - col_start) as f32 * cell_w;
            // Use a subtle blue underline color
            let url_color = [0.4, 0.6, 1.0];
            Self::push_bg_quad(&mut vertices, ux, uy, uw, 1.0, url_color);
        }

        // Draw cursor (adjusted for scroll offset and y_offset)
        if term.cursor_visible && blink_on {
            let offset = term.scroll_offset();
            let screen_y = offset + term.cursor_y as i32;
            if screen_y >= 0 && screen_y < term.rows as i32 {
                let cx = ox + term.cursor_x as f32 * cell_w;
                let cy = oy + y_offset + screen_y as f32 * cell_h;
                match term.cursor_shape {
                    CursorShape::Block => {
                        Self::push_bg_quad(&mut vertices, cx, cy, cell_w, cell_h, self.cursor_color);
                    }
                    CursorShape::Underline => {
                        let thickness = (cell_h * 0.1).max(1.0);
                        Self::push_bg_quad(&mut vertices, cx, cy + cell_h - thickness, cell_w, thickness, self.cursor_color);
                    }
                    CursorShape::Bar => {
                        let thickness = (cell_w * 0.1).max(1.0);
                        Self::push_bg_quad(&mut vertices, cx, cy, thickness, cell_h, self.cursor_color);
                    }
                }
            }
        }

        // Dim overlay on unfocused panes
        if !is_focused {
            let dim = [0.0, 0.0, 0.0]; // black overlay
            let dim4 = [dim[0], dim[1], dim[2], 0.3]; // 30% opacity
            let no_tex = [0.0, 0.0];
            let white = [1.0, 1.0, 1.0, 0.0];
            // Cover the whole pane area (excluding status bar)
            let dim_h = if self.status_bar_enabled {
                vp.height - self.atlas.cell_height
            } else {
                vp.height
            };
            vertices.push(Vertex { position: [vp.x, vp.y], tex_coords: no_tex, color: white, bg_color: dim4 });
            vertices.push(Vertex { position: [vp.x + vp.width, vp.y], tex_coords: no_tex, color: white, bg_color: dim4 });
            vertices.push(Vertex { position: [vp.x, vp.y + dim_h], tex_coords: no_tex, color: white, bg_color: dim4 });
            vertices.push(Vertex { position: [vp.x + vp.width, vp.y], tex_coords: no_tex, color: white, bg_color: dim4 });
            vertices.push(Vertex { position: [vp.x + vp.width, vp.y + dim_h], tex_coords: no_tex, color: white, bg_color: dim4 });
            vertices.push(Vertex { position: [vp.x, vp.y + dim_h], tex_coords: no_tex, color: white, bg_color: dim4 });
        }

        // Status bar
        if self.status_bar_enabled {
            self.build_status_bar_vertices(&mut vertices, vp, term);
        }

        vertices
    }

    fn build_status_bar_vertices(
        &mut self,
        vertices: &mut Vec<Vertex>,
        vp: &PaneViewport,
        term: &TerminalState,
    ) {
        let cell_w = self.atlas.cell_width;
        let cell_h = self.atlas.cell_height;
        let bar_y = vp.y + vp.height - cell_h;

        // Background quad for the full status bar
        Self::push_bg_quad(vertices, vp.x, bar_y, vp.width, cell_h, self.status_bar_bg);

        let no_bg = [0.0, 0.0, 0.0, 0.0];
        let cwd_fg = [self.status_bar_cwd_color[0], self.status_bar_cwd_color[1], self.status_bar_cwd_color[2], 1.0];
        let branch_fg = [self.status_bar_branch_color[0], self.status_bar_branch_color[1], self.status_bar_branch_color[2], 1.0];
        let scroll_fg = [self.status_bar_scroll_color[0], self.status_bar_scroll_color[1], self.status_bar_scroll_color[2], 1.0];
        let time_fg = [self.status_bar_time_color[0], self.status_bar_time_color[1], self.status_bar_time_color[2], 1.0];
        let title_fg = [self.status_bar_fg[0], self.status_bar_fg[1], self.status_bar_fg[2], 1.0];

        // Render CWD aligned to the left
        let mut cursor_x = vp.x + PANE_H_PADDING + cell_w; // 1 cell padding from left
        if let Some(ref cwd) = term.cwd {
            let home = std::env::var("HOME").unwrap_or_default();
            let display_path = if !home.is_empty() && cwd.starts_with(&home) {
                format!("~{}", &cwd[home.len()..])
            } else {
                cwd.clone()
            };
            cursor_x = self.render_status_text(vertices, &display_path, cursor_x, bar_y, vp.x + vp.width * 0.4, cwd_fg, no_bg);
        }

        // Render git branch after CWD
        cursor_x += cell_w * 2.0; // 2 cell gap
        let branch_display = match term.git_branch {
            Some(ref b) => format!(" {}", b),
            None => " no git".to_string(),
        };
        let actual_branch_fg = match term.git_branch {
            Some(_) => branch_fg,
            None => [branch_fg[0] * 0.5, branch_fg[1] * 0.5, branch_fg[2] * 0.5, 0.5],
        };
        let left_end = self.render_status_text(vertices, &branch_display, cursor_x, bar_y, vp.x + vp.width * 0.6, actual_branch_fg, no_bg);

        // Render hovered URL or title centered (only if it doesn't overlap with left content)
        let center_text: Option<(String, [f32; 4])> = if let Some(ref url) = self.hovered_url_text {
            Some((url.clone(), [0.4, 0.6, 1.0, 1.0]))
        } else {
            term.title.as_ref().map(|t| (t.clone(), title_fg))
        };
        if let Some((text, fg)) = center_text {
            let char_count = text.chars().count();
            let text_w = char_count as f32 * cell_w;
            let center_x = vp.x + (vp.width - text_w) / 2.0;
            let min_x = vp.x + vp.width * 0.3;
            let max_x = vp.x + vp.width * 0.7;
            let start_x = center_x.max(min_x);
            // Don't render if left content (cwd + branch) would overlap
            if start_x >= left_end + cell_w {
                self.render_status_text(vertices, &text, start_x, bar_y, max_x, fg, no_bg);
            }
        }

        // Right side: scroll indicator + time
        let time_str = {
            let secs = SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
            let t = secs as libc::time_t;
            let mut tm: libc::tm = unsafe { std::mem::zeroed() };
            unsafe { libc::localtime_r(&t, &mut tm) };
            format!("{:02}:{:02}", tm.tm_hour, tm.tm_min)
        };

        let scroll_off = term.scroll_offset();
        let scroll_str = if scroll_off > 0 {
            format!("↑{}", scroll_off)
        } else {
            String::new()
        };

        // Calculate total right width to align from right edge
        let time_w = time_str.chars().count() as f32 * cell_w;
        let scroll_w = scroll_str.chars().count() as f32 * cell_w;
        let gap = if scroll_off > 0 { cell_w * 2.0 } else { 0.0 };
        let total_right_w = scroll_w + gap + time_w;
        let right_edge = vp.x + vp.width;
        let mut right_x = right_edge - total_right_w - cell_w;

        if scroll_off > 0 {
            right_x = self.render_status_text(vertices, &scroll_str, right_x, bar_y, right_edge, scroll_fg, no_bg);
            right_x += cell_w * 2.0; // gap
        }
        self.render_status_text(vertices, &time_str, right_x, bar_y, right_edge, time_fg, no_bg);
    }

    fn build_tab_bar_vertices(
        &mut self,
        vertices: &mut Vec<Vertex>,
        viewport_w: f32,
        tab_titles: &[(String, bool, Option<usize>, bool, bool)],
        left_inset: f32,
    ) {
        let cell_w = self.atlas.cell_width;
        let cell_h = self.atlas.cell_height;
        let bar_h = (cell_h * 2.0).round();
        let tab_count = tab_titles.len();

        // Full-width background
        Self::push_bg_quad(vertices, 0.0, 0.0, viewport_w, bar_h, self.tab_bar_bg);

        // Fixed width per tab, capped at cell_w * 15
        let max_tab_w = cell_w * 20.0;
        let available_w = viewport_w - left_inset;
        let tab_width = (available_w / tab_count as f32).min(max_tab_w);
        let no_bg = [0.0, 0.0, 0.0, 0.0];

        for (i, (title, is_active, color_idx, is_renaming, has_bell)) in tab_titles.iter().enumerate() {
            let x = left_inset + i as f32 * tab_width;

            // Tab background color
            let tab_bg: Option<[f32; 3]> = if let Some(idx) = color_idx {
                Some(TAB_COLORS[*idx % TAB_COLORS.len()])
            } else if *is_active {
                Some(self.tab_bar_active_bg)
            } else {
                None // transparent, shows bar bg
            };

            if let Some(bg) = tab_bg {
                Self::push_bg_quad(vertices, x, 0.0, tab_width, bar_h, bg);
            }

            // Active tab: bright border at bottom
            if *is_active {
                let border_h = 6.0_f32;
                let border_color = if let Some(idx) = color_idx {
                    let c = TAB_COLORS[*idx % TAB_COLORS.len()];
                    [(c[0] + 1.0) * 0.5, (c[1] + 1.0) * 0.5, (c[2] + 1.0) * 0.5]
                } else {
                    [0.7, 0.7, 0.7]
                };
                Self::push_bg_quad(vertices, x, bar_h - border_h, tab_width, border_h, border_color);
            }

            // Tab number + title: "1: title"
            // When renaming, show the end of the text so the cursor is visible
            let truncated: String;
            let max_title_chars = 25;
            let display_title = if title.chars().count() > max_title_chars {
                if *is_renaming {
                    // Show last N chars to keep cursor visible
                    let skip = title.chars().count() - max_title_chars;
                    truncated = title.chars().skip(skip).collect();
                    &truncated
                } else {
                    truncated = title.chars().take(max_title_chars).collect();
                    &truncated
                }
            } else {
                title
            };
            let label = format!("{}:{}", i + 1, display_title);
            // White text on colored tabs (active or not), grey on default bg
            let fg = if color_idx.is_some() || *is_active {
                [1.0, 1.0, 1.0, 1.0]
            } else {
                [self.tab_bar_fg[0], self.tab_bar_fg[1], self.tab_bar_fg[2], 1.0]
            };

            // Center text vertically and horizontally in the tab
            let text_w = label.chars().count() as f32 * cell_w;
            let text_x = x + (tab_width - text_w) / 2.0;
            let text_y = (bar_h - cell_h) / 2.0;
            let max_x = x + tab_width - cell_w;
            self.render_status_text(vertices, &label, text_x.max(x + cell_w * 0.5), text_y, max_x, fg, no_bg);

            // Bell indicator: orange dot in the top-right of the tab
            if *has_bell && !is_active {
                let dot_x = x + tab_width - cell_w * 1.2;
                let dot_y = (bar_h - cell_h) / 2.0;
                let dot_color = [1.0, 0.45, 0.1, 1.0]; // orange
                self.render_status_text(vertices, "●", dot_x, dot_y, x + tab_width, dot_color, no_bg);
            }
        }
    }

    /// Render a string in the status bar at the given x position.
    /// Returns the x position after the last rendered character.
    /// Stops rendering if x exceeds max_x.
    fn render_status_text(
        &mut self,
        vertices: &mut Vec<Vertex>,
        text: &str,
        start_x: f32,
        y: f32,
        max_x: f32,
        fg: [f32; 4],
        no_bg: [f32; 4],
    ) -> f32 {
        let cell_w = self.atlas.cell_width;
        let atlas_w = self.atlas.atlas_width as f32;
        let atlas_h = self.atlas.atlas_height as f32;

        for c in text.chars() {
            if self.atlas.glyph(c).is_none() {
                self.atlas.rasterize_char(c);
            }
        }

        let mut x = start_x;
        for c in text.chars() {
            if x + cell_w > max_x { break; }
            let glyph = match self.atlas.glyph(c) {
                Some(g) => *g,
                None => { x += cell_w; continue; }
            };
            if glyph.width == 0 || glyph.height == 0 { x += cell_w; continue; }

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
            x += cell_w;
        }
        x
    }

    fn build_filter_overlay_vertices(
        &mut self,
        vertices: &mut Vec<Vertex>,
        vp: &PaneViewport,
        filter: &FilterRenderData,
    ) {
        let cell_w = self.atlas.cell_width;
        let cell_h = self.atlas.cell_height;

        // 1. Semi-transparent dark overlay covering the entire pane
        let overlay_bg = [0.0, 0.0, 0.0, 0.85];
        let no_tex = [0.0_f32, 0.0];
        let white = [1.0_f32, 1.0, 1.0, 0.0];
        vertices.push(Vertex { position: [vp.x, vp.y], tex_coords: no_tex, color: white, bg_color: overlay_bg });
        vertices.push(Vertex { position: [vp.x + vp.width, vp.y], tex_coords: no_tex, color: white, bg_color: overlay_bg });
        vertices.push(Vertex { position: [vp.x, vp.y + vp.height], tex_coords: no_tex, color: white, bg_color: overlay_bg });
        vertices.push(Vertex { position: [vp.x + vp.width, vp.y], tex_coords: no_tex, color: white, bg_color: overlay_bg });
        vertices.push(Vertex { position: [vp.x + vp.width, vp.y + vp.height], tex_coords: no_tex, color: white, bg_color: overlay_bg });
        vertices.push(Vertex { position: [vp.x, vp.y + vp.height], tex_coords: no_tex, color: white, bg_color: overlay_bg });

        let no_bg = [0.0, 0.0, 0.0, 0.0];

        // 2. Search bar background
        let bar_bg = [0.2, 0.2, 0.25];
        Self::push_bg_quad(vertices, vp.x, vp.y, vp.width, cell_h, bar_bg);

        // 3. Search bar text: "/ query▏"
        let bar_text = format!("/ {}▏", &filter.query);
        let bar_fg = [1.0, 0.8, 0.2, 1.0]; // accent yellow
        self.render_status_text(vertices, &bar_text, vp.x + PANE_H_PADDING, vp.y, vp.x + vp.width - cell_w, bar_fg, no_bg);

        // Match count
        let count_text = format!("{} matches", filter.matches.len());
        let count_fg = [0.6, 0.6, 0.6, 1.0];
        let count_w = count_text.chars().count() as f32 * cell_w;
        self.render_status_text(vertices, &count_text, vp.x + vp.width - count_w - PANE_H_PADDING, vp.y, vp.x + vp.width, count_fg, no_bg);

        // 4. List matched lines — truncate text to visible columns to limit vertices
        let max_visible = ((vp.height / cell_h).floor() as usize).saturating_sub(1);
        let match_fg = [0.85, 0.85, 0.85, 1.0];
        let highlight_fg = [1.0, 0.8, 0.2, 1.0];
        let query_lower = filter.query.to_lowercase();
        let max_chars = ((vp.width - 2.0 * PANE_H_PADDING) / cell_w) as usize;

        for (i, m) in filter.matches.iter().take(max_visible).enumerate() {
            let y = vp.y + (i + 1) as f32 * cell_h;
            let max_x = vp.x + vp.width - PANE_H_PADDING;

            // Line number prefix
            let prefix = format!("{:>6}: ", m.abs_line);
            let prefix_fg = [0.5, 0.5, 0.5, 1.0];
            let after_prefix = self.render_status_text(vertices, &prefix, vp.x + PANE_H_PADDING, y, max_x, prefix_fg, no_bg);

            // Truncate line text to what fits on screen
            let prefix_chars = prefix.chars().count();
            let text_limit = max_chars.saturating_sub(prefix_chars);
            let display_text: String = m.text.chars().take(text_limit).collect();

            if query_lower.is_empty() {
                self.render_status_text(vertices, &display_text, after_prefix, y, max_x, match_fg, no_bg);
            } else {
                // Split text into spans: alternating normal/highlighted
                let text_lower: String = display_text.to_lowercase();
                let mut spans: Vec<(&str, bool)> = Vec::new();
                let mut pos = 0;
                while pos < display_text.len() {
                    if let Some(found) = text_lower[pos..].find(&query_lower) {
                        if found > 0 {
                            spans.push((&display_text[pos..pos + found], false));
                        }
                        let end = pos + found + filter.query.len();
                        spans.push((&display_text[pos + found..end], true));
                        pos = end;
                    } else {
                        spans.push((&display_text[pos..], false));
                        break;
                    }
                }

                let mut x = after_prefix;
                for (span, is_hl) in spans {
                    let fg = if is_hl { highlight_fg } else { match_fg };
                    x = self.render_status_text(vertices, span, x, y, max_x, fg, no_bg);
                }
            }
        }
    }

    fn build_loading_vertices(&mut self, vp: &PaneViewport) -> Vec<Vertex> {
        let text = "starting...";
        let cell_w = self.atlas.cell_width;
        let cell_h = self.atlas.cell_height;
        let atlas_w = self.atlas.atlas_width as f32;
        let atlas_h = self.atlas.atlas_height as f32;

        let text_w = text.len() as f32 * cell_w;
        let start_x = vp.x + (vp.width - text_w) / 2.0;
        let start_y = vp.y + (vp.height - cell_h) / 2.0;

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

    pub fn status_bar_enabled(&self) -> bool {
        self.status_bar_enabled
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
