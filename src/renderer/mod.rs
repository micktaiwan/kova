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

/// Format a count as human-readable string (e.g. "1.2K", "3.4M").
fn format_count(n: u64) -> String {
    if n < 1_000 {
        format!("{}", n)
    } else if n < 1_000_000 {
        format!("{:.1}K", n as f64 / 1_000.0)
    } else if n < 1_000_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else {
        format!("{:.1}G", n as f64 / 1_000_000_000.0)
    }
}

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

use crate::config::{Config, KeysConfig};
use crate::pane::PaneId;

/// Attention state for a non-focused pane (bell > completion > none).
#[derive(Clone, Copy, PartialEq)]
enum PaneAttention {
    None,
    Completion,
    Bell,
}

impl PaneAttention {
    fn from_flags(has_bell: bool, has_completion: bool) -> Self {
        if has_bell { Self::Bell } else if has_completion { Self::Completion } else { Self::None }
    }

    fn dot_color(self) -> Option<[f32; 4]> {
        match self {
            Self::Bell => Some([0.9, 0.6, 0.2, 1.0]),
            Self::Completion => Some([0.2, 0.8, 0.3, 1.0]),
            Self::None => None,
        }
    }

    fn bar_bg(self, default: [f32; 3]) -> [f32; 3] {
        match self {
            Self::Bell => [0.35, 0.22, 0.10],
            Self::Completion => [0.15, 0.30, 0.15],
            Self::None => default,
        }
    }
}
use crate::terminal::{CursorShape, FilterMatch, TerminalState};

/// Data passed to the renderer for drawing filter overlay.
pub struct FilterRenderData {
    pub query: String,
    pub matches: Vec<FilterMatch>,
}

/// A single entry in the recent projects overlay.
pub struct RecentProjectEntry {
    pub path: String,
    pub time_ago: String,
    pub pane_count: usize,
    pub invalid: bool,
}

/// Data passed to the renderer for drawing recent projects overlay.
pub struct RecentProjectsRenderData<'a> {
    pub entries: &'a [&'a RecentProjectEntry],
    pub selected: usize,
    pub scroll: usize,
}

/// Per-pane data passed from window to renderer.
pub struct PaneRenderData {
    pub terminal: Arc<RwLock<TerminalState>>,
    pub viewport: PaneViewport,
    pub shell_ready: bool,
    pub is_focused: bool,
    pub pane_id: PaneId,
    pub custom_title: Option<String>,
    /// Pre-computed display title (custom > OSC > CWD basename > "shell").
    pub display_title: String,
    pub has_completion: bool,
    pub has_bell: bool,
    pub minimized: bool,
    pub input_chars: Arc<std::sync::atomic::AtomicU64>,
}

/// Sub-region of the drawable where a pane is rendered (in pixels).
#[derive(Clone, Copy)]
pub struct PaneViewport {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

const MAX_VERTEX_BYTES: usize = 8 * 1024 * 1024; // 8MB

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
    /// Compact version of bg_color for comparing with Cell.bg ([u8; 3]).
    bg_color_u8: [u8; 3],
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
    global_bar_bg: [f32; 3],
    global_bar_time_color: [f32; 3],
    global_bar_scroll_color: [f32; 3],
    last_minute: u32,
    cached_time_str: String,
    last_rss_epoch: u32,
    cached_rss_str: String,
    cached_proc_count: u32,
    cached_proc_str: String,
    cached_io_str: String,
    /// Cached memory report lines for overlay (set by window on Cmd+Shift+I).
    cached_mem_report: Vec<String>,
    selection_color: [f32; 3],
    tab_bar_bg: [f32; 3],
    tab_bar_fg: [f32; 3],
    tab_bar_active_bg: [f32; 3],
    /// Hovered URL: per-row segments [(visible_row, col_start, col_end)]
    pub hovered_url: Option<Vec<(usize, u16, u16)>>,
    /// Hovered URL text (for status bar display)
    pub hovered_url_text: Option<String>,
    /// Pane ID of the hovered URL (to show URL only in that pane's status bar)
    pub hovered_url_pane_id: Option<PaneId>,
    /// Cached help hint text for status bar (avoid per-frame allocation).
    cached_help_hint: String,
    /// Cached formatted shortcuts for help overlay (label, formatted key combo).
    cached_help_shortcuts: Vec<(String, String)>,
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
            bg_color_u8: crate::terminal::color_to_u8(config.colors.background),
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
            global_bar_bg: config.global_status_bar.bg_color,
            global_bar_time_color: config.global_status_bar.time_color,
            global_bar_scroll_color: config.global_status_bar.scroll_indicator_color,
            last_minute: u32::MAX,
            cached_time_str: String::new(),
            last_rss_epoch: u32::MAX,
            cached_rss_str: String::new(),
            cached_proc_count: 0,
            cached_proc_str: String::from("▶0"),
            cached_io_str: String::new(),
            cached_mem_report: Vec::new(),
            selection_color: [0.45, 0.42, 0.20],
            tab_bar_bg: config.tab_bar.bg_color,
            tab_bar_fg: config.tab_bar.fg_color,
            tab_bar_active_bg: config.tab_bar.active_bg,
            hovered_url: None,
            hovered_url_text: None,
            hovered_url_pane_id: None,
            cached_help_hint: String::new(),
            cached_help_shortcuts: Vec::new(),
        }
    }


    /// Render multiple panes. Each entry: (terminal, viewport, shell_ready, is_focused, pane_id, custom_title, has_completion, has_bell, minimized).
    /// `separators` are line segments (x1, y1, x2, y2) drawn between splits.
    pub fn render_panes(
        &mut self,
        layer: &CAMetalLayer,
        panes: &[PaneRenderData],
        separators: &[(f32, f32, f32, f32)],
        tab_titles: &[(String, bool, Option<usize>, bool, bool, bool)],
        filter: Option<&FilterRenderData>,
        tab_bar_left_inset: f32,
        hidden_left: usize,
        hidden_right: usize,
        focused_column: usize,
        total_columns: usize,
        active_tab: usize,
        total_tabs: usize,
        show_help: bool,
        show_mem_report: bool,
        recent_projects: Option<&RecentProjectsRenderData<'_>>,
        help_hint_remaining: u32,
        keys_config: Option<&KeysConfig>,
    ) {
        // Reset blink on cursor movement of focused pane
        if let Some(focused_pane) = panes.iter().find(|p| p.is_focused) {
            let epoch = focused_pane.terminal.read().cursor_move_epoch.load(std::sync::atomic::Ordering::Relaxed);
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

        // Shared timestamp for time + RSS checks
        let now_secs = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        // Check if minute changed for global status bar time
        let minute_changed = {
            let current_minute = (now_secs / 60) as u32;
            if current_minute != self.last_minute {
                self.last_minute = current_minute;
                let t = now_secs as libc::time_t;
                let mut tm: libc::tm = unsafe { std::mem::zeroed() };
                unsafe { libc::localtime_r(&t, &mut tm) };
                self.cached_time_str = format!("{:02}:{:02}", tm.tm_hour, tm.tm_min);
                true
            } else {
                false
            }
        };

        // Update RSS + process count every 2 seconds
        let rss_changed = {
            let epoch_2s = (now_secs / 2) as u32;
            if epoch_2s != self.last_rss_epoch {
                self.last_rss_epoch = epoch_2s;
                let rss_mb = crate::get_rss_mb();
                self.cached_rss_str = if rss_mb >= 0.0 {
                    format!("{:.1}M", rss_mb)
                } else {
                    String::new()
                };
                self.cached_proc_count = crate::terminal::pty::foreground_process_count();
                self.cached_proc_str = format!("▶{}", self.cached_proc_count);
                true
            } else {
                false
            }
        };

        // Check if any pane is dirty (consume ALL flags, no short-circuit)
        let mut any_dirty = false;
        let mut any_not_ready = false;
        let mut any_sync_deferred = false;
        for pane in panes {
            if !pane.shell_ready { any_not_ready = true; }
            let t = pane.terminal.read();
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
            if pane.terminal.read().dirty.swap(false, std::sync::atomic::Ordering::Relaxed) {
                any_dirty = true;
            }
        }
        // If only sync-deferred panes were dirty, still need to render the others
        let all_ready = !any_not_ready;
        let has_filter = filter.is_some();
        let has_recent_projects = recent_projects.is_some();
        if all_ready && !any_dirty && !any_sync_deferred && !blink_changed && !minute_changed && !rss_changed && !has_filter && !show_help && !show_mem_report && !has_recent_projects && help_hint_remaining == 0 {
            return;
        }

        let drawable = match layer.nextDrawable() {
            Some(d) => d,
            None => return,
        };

        let drawable_size = layer.drawableSize();
        let viewport_w = drawable_size.width as f32;
        let viewport_h = drawable_size.height as f32;

        // Build vertices for each pane with its own scissor rect for clipping
        let mut pane_draws: Vec<(Vec<Vertex>, MTLScissorRect)> = Vec::new();
        let mut overlay_vertices = Vec::new();
        let (cell_w, cell_h) = self.cell_size();
        let saved_hover_text = self.hovered_url_text.clone();
        let saved_hover_segments = self.hovered_url.clone();
        for pane in panes {
            let vp = &pane.viewport;
            // Scope hovered URL to the pane that owns it
            let is_hover_pane = self.hovered_url_pane_id == Some(pane.pane_id);
            self.hovered_url_text = if is_hover_pane { saved_hover_text.clone() } else { None };
            self.hovered_url = if is_hover_pane { saved_hover_segments.clone() } else { None };
            // Skip panes entirely off-screen (hidden by horizontal scroll)
            if vp.x + vp.width <= 0.0 || vp.x >= viewport_w {
                continue;
            }
            // Build pane vertices (minimized bar or full content)
            let pane_verts = if pane.minimized {
                self.build_minimized_bar_vertices(vp, &pane.display_title, pane.has_bell, pane.has_completion)
            } else if pane.is_focused && filter.is_some() {
                continue; // Skip: filter overlay covers focused pane
            } else {
                let pane_attention = PaneAttention::from_flags(pane.has_bell, pane.has_completion);
                let mut verts = if pane.shell_ready {
                    let t = pane.terminal.read();
                    let show_blink = if pane.is_focused { blink_on } else { true };
                    let pin = pane.input_chars.load(std::sync::atomic::Ordering::Relaxed);
                    self.build_vertices(&t, vp, show_blink, pane.is_focused, pane.custom_title.as_deref(), pane_attention, pin)
                } else {
                    self.build_loading_vertices(vp)
                };
                // Attention indicator dot on non-focused panes
                if let Some(color) = pane_attention.dot_color() {
                    let dot_x = vp.x + vp.width - cell_w * 2.5;
                    let dot_y = vp.y + cell_h * 0.5;
                    let no_bg = [0.0_f32, 0.0, 0.0, 0.0];
                    self.render_status_text(&mut verts, "●", dot_x, dot_y, vp.x + vp.width, color, no_bg);
                }
                verts
            };
            // Compute scissor rect clamped to drawable bounds
            let sx = (vp.x.max(0.0)) as usize;
            let sy = (vp.y.max(0.0)) as usize;
            let sw = ((vp.width).min(viewport_w - sx as f32).max(0.0)) as usize;
            let sh = ((vp.height).min(viewport_h - sy as f32).max(0.0)) as usize;
            if !pane_verts.is_empty() && sw > 0 && sh > 0 {
                pane_draws.push((pane_verts, MTLScissorRect { x: sx, y: sy, width: sw, height: sh }));
            }
        }
        self.hovered_url_text = saved_hover_text;
        self.hovered_url = saved_hover_segments;

        // Build overlay vertices (separators, tab bar, status bar, filter, help)
        // These are drawn with a global scissor rect (no per-pane clipping needed)

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
                    overlay_vertices.push(Vertex { position: [lx, y1], tex_coords: no_tex, color: white, bg_color: sep_bg });
                    overlay_vertices.push(Vertex { position: [rx, y1], tex_coords: no_tex, color: white, bg_color: sep_bg });
                    overlay_vertices.push(Vertex { position: [lx, y2], tex_coords: no_tex, color: white, bg_color: sep_bg });
                    overlay_vertices.push(Vertex { position: [rx, y1], tex_coords: no_tex, color: white, bg_color: sep_bg });
                    overlay_vertices.push(Vertex { position: [rx, y2], tex_coords: no_tex, color: white, bg_color: sep_bg });
                    overlay_vertices.push(Vertex { position: [lx, y2], tex_coords: no_tex, color: white, bg_color: sep_bg });
                } else {
                    // Horizontal line
                    let ty = y1 - thickness * 0.5;
                    let by = y1 + thickness * 0.5;
                    overlay_vertices.push(Vertex { position: [x1, ty], tex_coords: no_tex, color: white, bg_color: sep_bg });
                    overlay_vertices.push(Vertex { position: [x2, ty], tex_coords: no_tex, color: white, bg_color: sep_bg });
                    overlay_vertices.push(Vertex { position: [x1, by], tex_coords: no_tex, color: white, bg_color: sep_bg });
                    overlay_vertices.push(Vertex { position: [x2, ty], tex_coords: no_tex, color: white, bg_color: sep_bg });
                    overlay_vertices.push(Vertex { position: [x2, by], tex_coords: no_tex, color: white, bg_color: sep_bg });
                    overlay_vertices.push(Vertex { position: [x1, by], tex_coords: no_tex, color: white, bg_color: sep_bg });
                }
            }
        }

        // Draw tab bar
        if tab_titles.len() > 0 {
            self.build_tab_bar_vertices(&mut overlay_vertices, viewport_w, tab_titles, tab_bar_left_inset);
        }

        // Update I/O char counters (global totals, persist across pane closures)
        if rss_changed {
            let total_in = crate::terminal::pty::GLOBAL_INPUT_CHARS.load(std::sync::atomic::Ordering::Relaxed);
            let total_out = crate::terminal::pty::GLOBAL_PRINTABLE_CHARS.load(std::sync::atomic::Ordering::Relaxed);
            self.cached_io_str = format!("↑{} ↓{}", format_count(total_in), format_count(total_out));
        }

        // Draw global status bar
        self.build_global_status_bar_vertices(&mut overlay_vertices, viewport_w, viewport_h, hidden_left, hidden_right, focused_column, total_columns, active_tab, total_tabs, help_hint_remaining, keys_config);

        // Draw filter overlay on focused pane
        if let Some(filter_data) = filter {
            if let Some(focused_pane) = panes.iter().find(|p| p.is_focused) {
                self.build_filter_overlay_vertices(&mut overlay_vertices, &focused_pane.viewport, filter_data);
            }
        }

        // Draw help overlay (on top of everything)
        if show_help {
            if let Some(keys_config) = keys_config {
                self.build_help_overlay_vertices(&mut overlay_vertices, viewport_w, viewport_h, keys_config);
            }
        }

        // Draw memory report overlay
        if show_mem_report {
            self.build_mem_report_overlay_vertices(&mut overlay_vertices, viewport_w, viewport_h);
        }

        // Draw recent projects overlay
        if let Some(rp) = recent_projects {
            self.build_recent_projects_overlay_vertices(&mut overlay_vertices, viewport_w, viewport_h, rp);
        }

        // Flatten all pane vertices + overlay into a single buffer, tracking draw ranges
        let mut all_vertices: Vec<Vertex> = Vec::new();
        let mut draw_calls: Vec<(usize, usize, MTLScissorRect)> = Vec::new(); // (start, count, scissor)
        let global_scissor = MTLScissorRect {
            x: 0,
            y: 0,
            width: viewport_w as usize,
            height: viewport_h as usize,
        };

        for (verts, scissor) in &pane_draws {
            let start = all_vertices.len();
            all_vertices.extend_from_slice(verts);
            draw_calls.push((start, verts.len(), *scissor));
        }
        if !overlay_vertices.is_empty() {
            let start = all_vertices.len();
            let count = overlay_vertices.len();
            all_vertices.extend(overlay_vertices);
            draw_calls.push((start, count, global_scissor));
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

        let cmd_buf = match self.command_queue.commandBuffer() {
            Some(buf) => buf,
            None => { log::error!("Metal: failed to create command buffer, skipping frame"); return; }
        };
        let encoder = match cmd_buf.renderCommandEncoderWithDescriptor(&pass_desc) {
            Some(enc) => enc,
            None => { log::error!("Metal: failed to create render encoder, skipping frame"); return; }
        };

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
            unsafe {
                encoder.setVertexBuffer_offset_atIndex(Some(vertex_buf), 0, 0);
                encoder.setVertexBuffer_offset_atIndex(Some(&self.viewport_buf), 0, 1);
                encoder.setVertexBuffer_offset_atIndex(Some(&self.atlas_size_buf), 0, 2);
                encoder.setFragmentTexture_atIndex(Some(&*self.atlas.texture), 0);
            }

            // Draw each group with its own scissor rect
            for &(start, count, ref scissor) in &draw_calls {
                encoder.setScissorRect(MTLScissorRect {
                    x: scissor.x,
                    y: scissor.y,
                    width: scissor.width,
                    height: scissor.height,
                });
                unsafe {
                    encoder.drawPrimitives_vertexStart_vertexCount(
                        MTLPrimitiveType::Triangle,
                        start,
                        count,
                    );
                }
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
        custom_title: Option<&str>,
        attention: PaneAttention,
        pane_input_chars: u64,
    ) -> Vec<Vertex> {
        // Pass 1: collect unknown chars/clusters for dynamic rasterization
        let display = term.visible_lines();
        let mut unknown_chars: Vec<char> = Vec::new();
        let mut unknown_clusters: Vec<Box<str>> = Vec::new();
        {
            let mut seen_chars = std::collections::HashSet::new();
            let mut seen_clusters = std::collections::HashSet::new();
            for line in display.iter() {
                for cell in line.iter() {
                    if let Some(ref cluster) = cell.cluster {
                        if self.atlas.cluster_glyph(cluster).is_none() && seen_clusters.insert(cluster.clone()) {
                            unknown_clusters.push(cluster.clone());
                        }
                    } else {
                        let c = cell.c;
                        if c != ' ' && c != '\0' && self.atlas.glyph(c).is_none() && seen_chars.insert(c) {
                            unknown_chars.push(c);
                        }
                    }
                }
            }
        }

        // Pass 2: rasterize unknowns
        for c in unknown_chars {
            self.atlas.rasterize_char(c);
        }
        for cluster in unknown_clusters {
            self.atlas.rasterize_cluster(&cluster);
        }

        // Pass 3: build vertices
        let cell_w = self.atlas.cell_width;
        let cell_h = self.atlas.cell_height;
        let atlas_w = self.atlas.atlas_width as f32;
        let atlas_h = self.atlas.atlas_height as f32;
        let ox = vp.x + PANE_H_PADDING;
        let oy = vp.y;

        // Push content to bottom when screen isn't full (single source of truth in Terminal)
        let y_offset_rows = term.y_offset_rows() as f32;
        let content_height = (term.rows as f32 - y_offset_rows) * cell_h;
        let y_offset = (y_offset_rows * cell_h).min((vp.height - content_height).max(0.0));

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
            let y = (oy + y_offset + row_idx as f32 * cell_h).round();

            for col_idx in 0..term.cols as usize {
                let x = (ox + col_idx as f32 * cell_w).round();

                // Cell background
                if col_idx < line.len() && line[col_idx].bg != self.bg_color_u8 {
                    Self::push_bg_quad(&mut vertices, x, y, cell_w, cell_h, crate::terminal::color_to_f32(line[col_idx].bg));
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

                if cell.is_blank() {
                    continue;
                }
                let c = cell.c;

                if c == '─' && row_idx == 2 && col_idx < 3 {
                    log::trace!("render ─ at col={} row={} fg={:?} bg={:?}", col_idx, row_idx, cell.fg, cell.bg);
                }

                // Look up glyph: cluster first, then single char
                let glyph = if let Some(ref cluster) = cell.cluster {
                    match self.atlas.cluster_glyph(cluster) {
                        Some(g) => *g,
                        None => continue,
                    }
                } else {
                    match self.atlas.glyph(c) {
                        Some(g) => *g,
                        None => continue,
                    }
                };

                if glyph.width == 0 || glyph.height == 0 {
                    continue;
                }

                let gx = (ox + col_idx as f32 * cell_w).round();
                let gy = (oy + y_offset + row_idx as f32 * cell_h).round();
                let gw = glyph.width as f32;
                let gh = glyph.height as f32;

                let tx = glyph.x as f32 / atlas_w;
                let ty = glyph.y as f32 / atlas_h;
                let tw = glyph.width as f32 / atlas_w;
                let th = glyph.height as f32 / atlas_h;

                let alpha = if glyph.is_color { 2.0 } else { 1.0 };
                let fg_f = crate::terminal::color_to_f32(cell.fg);
                let fg = [fg_f[0], fg_f[1], fg_f[2], alpha];
                let no_bg = [0.0, 0.0, 0.0, 0.0];

                vertices.push(Vertex { position: [gx, gy], tex_coords: [tx, ty], color: fg, bg_color: no_bg });
                vertices.push(Vertex { position: [gx + gw, gy], tex_coords: [tx + tw, ty], color: fg, bg_color: no_bg });
                vertices.push(Vertex { position: [gx, gy + gh], tex_coords: [tx, ty + th], color: fg, bg_color: no_bg });
                vertices.push(Vertex { position: [gx + gw, gy], tex_coords: [tx + tw, ty], color: fg, bg_color: no_bg });
                vertices.push(Vertex { position: [gx + gw, gy + gh], tex_coords: [tx + tw, ty + th], color: fg, bg_color: no_bg });
                vertices.push(Vertex { position: [gx, gy + gh], tex_coords: [tx, ty + th], color: fg, bg_color: no_bg });
            }
        }

        // Draw URL underline for hovered URL (may span multiple wrapped rows)
        if let Some(ref segments) = self.hovered_url {
            let url_color = [0.4, 0.6, 1.0];
            for &(hover_row, col_start, col_end) in segments {
                let uy = (oy + y_offset + hover_row as f32 * cell_h + cell_h - 1.0).round();
                let ux = (ox + col_start as f32 * cell_w).round();
                let uw = (col_end - col_start) as f32 * cell_w;
                Self::push_bg_quad(&mut vertices, ux, uy, uw, 1.0, url_color);
            }
        }

        // Draw cursor (adjusted for scroll offset and y_offset)
        if term.cursor_visible && blink_on {
            let offset = term.scroll_offset();
            let screen_y = offset + term.cursor_y as i32;
            if screen_y >= 0 && screen_y < term.rows as i32 {
                let cx = (ox + term.cursor_x as f32 * cell_w).round();
                let cy = (oy + y_offset + screen_y as f32 * cell_h).round();
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
            self.build_status_bar_vertices(&mut vertices, vp, term, custom_title, attention, pane_input_chars);
        }

        vertices
    }

    fn build_status_bar_vertices(
        &mut self,
        vertices: &mut Vec<Vertex>,
        vp: &PaneViewport,
        term: &TerminalState,
        custom_title: Option<&str>,
        attention: PaneAttention,
        pane_input_chars: u64,
    ) {
        let cell_w = self.atlas.cell_width;
        let cell_h = self.atlas.cell_height;
        let bar_y = vp.y + vp.height - cell_h;

        // Background quad: orange for bell, green for completion, default otherwise
        Self::push_bg_quad(vertices, vp.x, bar_y, vp.width, cell_h, attention.bar_bg(self.status_bar_bg));

        let no_bg = [0.0, 0.0, 0.0, 0.0];
        let cwd_fg = [self.status_bar_cwd_color[0], self.status_bar_cwd_color[1], self.status_bar_cwd_color[2], 1.0];
        let branch_fg = [self.status_bar_branch_color[0], self.status_bar_branch_color[1], self.status_bar_branch_color[2], 1.0];
        let scroll_fg = [self.status_bar_scroll_color[0], self.status_bar_scroll_color[1], self.status_bar_scroll_color[2], 1.0];
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

        // Right side: title (custom or hovered URL or OSC) + scroll indicator
        let right_edge = vp.x + vp.width - cell_w; // 1 cell padding from right

        // Scroll indicator (rightmost)
        let scroll_off = term.scroll_offset();
        let right_after_scroll = if scroll_off > 0 {
            let scroll_str = format!("↑{}", scroll_off);
            let scroll_w = scroll_str.chars().count() as f32 * cell_w;
            let right_x = right_edge - scroll_w;
            self.render_status_text(vertices, &scroll_str, right_x, bar_y, right_edge + cell_w, scroll_fg, no_bg);
            right_x - cell_w * 2.0 // gap before title
        } else {
            right_edge
        };

        // Title: hovered URL > custom_title > OSC title
        let right_text: Option<(String, [f32; 4])> = if let Some(ref url) = self.hovered_url_text {
            Some((url.clone(), [0.4, 0.6, 1.0, 1.0]))
        } else if let Some(title) = custom_title {
            Some((title.to_string(), title_fg))
        } else {
            term.title.as_ref().map(|t| (t.clone(), title_fg))
        };
        let right_content_start = if let Some((ref text, fg)) = right_text {
            let char_count = text.chars().count();
            let text_w = char_count as f32 * cell_w;
            let title_x = right_after_scroll - text_w;
            // Only render if it doesn't overlap with left content
            if title_x >= left_end + cell_w * 2.0 {
                self.render_status_text(vertices, text, title_x, bar_y, right_after_scroll, fg, no_bg);
                title_x
            } else {
                right_after_scroll
            }
        } else {
            right_after_scroll
        };

        // Per-pane I/O counters — only if enough space between left and right content
        let pane_out = term.printable_chars.load(std::sync::atomic::Ordering::Relaxed);
        let io_str = format!("↑{} ↓{}", format_count(pane_input_chars), format_count(pane_out));
        let io_w = io_str.chars().count() as f32 * cell_w;
        let io_x = right_content_start - io_w - cell_w * 2.0;
        if io_x >= left_end + cell_w * 2.0 {
            let io_fg = [self.status_bar_fg[0], self.status_bar_fg[1], self.status_bar_fg[2], 0.4];
            self.render_status_text(vertices, &io_str, io_x, bar_y, right_content_start, io_fg, no_bg);
        }
    }

    fn build_global_status_bar_vertices(
        &mut self,
        vertices: &mut Vec<Vertex>,
        viewport_w: f32,
        viewport_h: f32,
        hidden_left: usize,
        hidden_right: usize,
        focused_column: usize,
        total_columns: usize,
        active_tab: usize,
        total_tabs: usize,
        help_hint_remaining: u32,
        keys_config: Option<&KeysConfig>,
    ) {
        let cell_w = self.atlas.cell_width;
        let cell_h = self.atlas.cell_height;
        let bar_y = viewport_h - cell_h;

        // Background quad
        Self::push_bg_quad(vertices, 0.0, bar_y, viewport_w, cell_h, self.global_bar_bg);

        let no_bg = [0.0, 0.0, 0.0, 0.0];
        let time_fg = [self.global_bar_time_color[0], self.global_bar_time_color[1], self.global_bar_time_color[2], 1.0];
        let scroll_fg = [self.global_bar_scroll_color[0], self.global_bar_scroll_color[1], self.global_bar_scroll_color[2], 1.0];
        let tab_fg = [self.global_bar_time_color[0], self.global_bar_time_color[1], self.global_bar_time_color[2], 0.5];

        // Center: [tab/total] - col/total, combined with scroll arrows
        // Pre-compute the full text to measure total width for centering
        {
            let tab_text = format!("[{}/{}]", active_tab, total_tabs);
            let sep = " - ";
            let col_text = format!("{}/{}", focused_column, total_columns);

            // Build optional left/right scroll parts
            let left_arrow = if hidden_left > 0 { format!("⟵ {} | ", hidden_left) } else { String::new() };
            let right_arrow = if hidden_right > 0 { format!(" | {} ⟶", hidden_right) } else { String::new() };

            // Total char width for centering
            let total_chars = left_arrow.chars().count() + tab_text.chars().count() + sep.chars().count() + col_text.chars().count() + right_arrow.chars().count();
            let text_w = total_chars as f32 * cell_w;
            let mut x = (viewport_w - text_w) / 2.0;

            if hidden_left > 0 {
                x = self.render_status_text(vertices, &left_arrow, x, bar_y, viewport_w, scroll_fg, no_bg);
            }
            x = self.render_status_text(vertices, &tab_text, x, bar_y, viewport_w, tab_fg, no_bg);
            x = self.render_status_text(vertices, sep, x, bar_y, viewport_w, tab_fg, no_bg);
            x = self.render_status_text(vertices, &col_text, x, bar_y, viewport_w, scroll_fg, no_bg);
            if hidden_right > 0 {
                self.render_status_text(vertices, &right_arrow, x, bar_y, viewport_w, scroll_fg, no_bg);
            }
        }

        // Left: help hint with fade
        if help_hint_remaining > 0 {
            if self.cached_help_hint.is_empty() {
                if let Some(kc) = keys_config {
                    self.cached_help_hint = format_key_combo(&kc.toggle_help);
                }
            }
            if !self.cached_help_hint.is_empty() {
                let fade_frames = 30u32;
                let alpha = if help_hint_remaining <= fade_frames {
                    help_hint_remaining as f32 / fade_frames as f32
                } else {
                    1.0
                };
                let hint_fg = [0.6, 0.75, 1.0, alpha];
                let hint_text = format!("{} for help", &self.cached_help_hint);
                self.render_status_text(vertices, &hint_text, cell_w, bar_y, viewport_w, hint_fg, no_bg);
            }
        }

        // Right: proc count + RSS + time (e.g. "▶2  14.2M  17:42")
        if !self.cached_time_str.is_empty() {
            let time_str = self.cached_time_str.clone();
            let rss_str = self.cached_rss_str.clone();
            let time_w = time_str.chars().count() as f32 * cell_w;
            let right_x = viewport_w - time_w - cell_w;
            self.render_status_text(vertices, &time_str, right_x, bar_y, viewport_w, time_fg, no_bg);

            let mut left_edge = right_x;
            let gap = cell_w * 2.0;

            if !rss_str.is_empty() {
                let rss_fg = [self.global_bar_time_color[0], self.global_bar_time_color[1], self.global_bar_time_color[2], 0.6];
                let rss_w = rss_str.chars().count() as f32 * cell_w;
                left_edge = left_edge - rss_w - gap;
                self.render_status_text(vertices, &rss_str, left_edge, bar_y, viewport_w, rss_fg, no_bg);
            }

            if !self.cached_io_str.is_empty() {
                let io_fg = [self.global_bar_time_color[0], self.global_bar_time_color[1], self.global_bar_time_color[2], 0.5];
                let io_w = self.cached_io_str.chars().count() as f32 * cell_w;
                left_edge = left_edge - io_w - gap;
                let io_str = self.cached_io_str.clone();
                self.render_status_text(vertices, &io_str, left_edge, bar_y, viewport_w, io_fg, no_bg);
            }

            let proc_fg = if self.cached_proc_count > 0 {
                [0.6, 0.85, 0.6, 1.0]
            } else {
                [self.global_bar_time_color[0], self.global_bar_time_color[1], self.global_bar_time_color[2], 0.4]
            };
            let proc_w = self.cached_proc_str.chars().count() as f32 * cell_w;
            left_edge = left_edge - proc_w - gap;
            let proc_str = self.cached_proc_str.clone();
            self.render_status_text(vertices, &proc_str, left_edge, bar_y, viewport_w, proc_fg, no_bg);
        }
    }

    fn build_tab_bar_vertices(
        &mut self,
        vertices: &mut Vec<Vertex>,
        viewport_w: f32,
        tab_titles: &[(String, bool, Option<usize>, bool, bool, bool)],
        left_inset: f32,
    ) {
        let cell_w = self.atlas.cell_width;
        let cell_h = self.atlas.cell_height;
        let bar_h = (cell_h * 2.0).round();
        let tab_count = tab_titles.len();

        // Full-width background
        Self::push_bg_quad(vertices, 0.0, 0.0, viewport_w, bar_h, self.tab_bar_bg);

        // Fixed width per tab, capped at cell_w * 20
        let max_tab_w = cell_w * 20.0;
        let full_available_w = viewport_w - left_inset;
        let tab_width = (full_available_w / tab_count as f32).min(max_tab_w);

        // Version label: show only if tabs don't reach its area
        let version_label = format!("Kova v{}", env!("CARGO_PKG_VERSION"));
        let version_chars = version_label.chars().count() as f32;
        let version_padding = cell_w * (version_chars + 2.0);
        let tabs_right_edge = left_inset + tab_count as f32 * tab_width;
        let show_version = tabs_right_edge <= viewport_w - version_padding;
        let no_bg = [0.0, 0.0, 0.0, 0.0];

        for (i, (title, is_active, color_idx, is_renaming, has_bell, has_completion)) in tab_titles.iter().enumerate() {
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

            // Tab indicator dot: bell (orange) takes priority, then completion (green)
            let indicator_color = if *has_bell && !is_active {
                Some([1.0_f32, 0.45, 0.1])
            } else if *has_completion && !is_active {
                Some([0.2_f32, 0.8, 0.3])
            } else {
                None
            };
            if let Some(color) = indicator_color {
                let dot_x = x + tab_width - cell_w * 2.0;
                let dot_y = (bar_h - cell_h) / 2.0;
                let dot_color = if let Some(bg) = tab_bg {
                    let lum = |c: [f32; 3]| 0.299 * c[0] + 0.587 * c[1] + 0.114 * c[2];
                    if (lum(color) - lum(bg)).abs() < 0.25 {
                        [1.0, 1.0, 1.0, 1.0]
                    } else {
                        [color[0], color[1], color[2], 1.0]
                    }
                } else {
                    [color[0], color[1], color[2], 1.0]
                };
                self.render_status_text(vertices, "●", dot_x, dot_y, x + tab_width, dot_color, no_bg);
            }
        }

        // Render version label on the right (if space allows)
        if show_version {
            let version_fg = [self.tab_bar_fg[0], self.tab_bar_fg[1], self.tab_bar_fg[2], 0.5];
            let version_x = viewport_w - version_padding + cell_w;
            let version_y = (bar_h - cell_h) / 2.0;
            self.render_status_text(vertices, &version_label, version_x, version_y, viewport_w - cell_w * 0.5, version_fg, no_bg);
        }
    }

    /// Render a string at the given position with optional scale factor.
    /// Returns the x position after the last rendered character.
    /// Stops rendering if x exceeds max_x.
    fn render_text(
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
            if x + cell_w > max_x { break; }
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
        self.render_text(vertices, text, start_x, y, max_x, fg, no_bg, 1.0)
    }

    /// Render text using the overlay font (rasterized at larger size, no bitmap stretching).
    fn render_overlay_text(
        &mut self,
        vertices: &mut Vec<Vertex>,
        text: &str,
        start_x: f32,
        y: f32,
        max_x: f32,
        fg: [f32; 4],
        no_bg: [f32; 4],
    ) -> f32 {
        let cell_w = self.atlas.overlay_cell_width;
        let atlas_w = self.atlas.atlas_width as f32;
        let atlas_h = self.atlas.atlas_height as f32;

        for c in text.chars() {
            if self.atlas.overlay_glyph(c).is_none() {
                self.atlas.rasterize_overlay_char(c);
            }
        }

        let mut x = start_x;
        for c in text.chars() {
            if x + cell_w > max_x { break; }
            let glyph = match self.atlas.overlay_glyph(c) {
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
        Self::push_bg_quad_alpha(vertices, vp.x, vp.y, vp.width, vp.height, [0.0, 0.0, 0.0], 0.85);

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

    /// Build vertices for a minimized pane bar (24px thin dimension).
    /// Detects orientation from viewport aspect ratio:
    /// - narrow & tall (HSplit minimized) → vertical bar, text rotated 90°
    /// - wide & short (VSplit minimized) → horizontal bar, text horizontal
    fn build_minimized_bar_vertices(
        &mut self,
        vp: &PaneViewport,
        label: &str,
        has_bell: bool,
        has_completion: bool,
    ) -> Vec<Vertex> {
        let mut vertices = Vec::new();
        let attention = PaneAttention::from_flags(has_bell, has_completion);
        let bar_bg = attention.bar_bg(self.status_bar_bg);

        // Background quad
        Self::push_bg_quad_alpha(&mut vertices, vp.x, vp.y, vp.width, vp.height, bar_bg, 1.0);

        let cell_w = self.atlas.cell_width;
        let cell_h = self.atlas.cell_height;
        let fg = [0.6, 0.6, 0.65, 1.0];
        let no_bg = [0.0, 0.0, 0.0, 0.0];
        let is_vertical_bar = vp.width < vp.height;

        if is_vertical_bar {
            // Vertical bar: render each character top-to-bottom
            let char_x = vp.x + (vp.width - cell_w) / 2.0;
            let start_y = vp.y + cell_h * 0.5;
            let max_chars = ((vp.height - cell_h) / cell_h).floor() as usize;
            let dot = attention.dot_color();
            let text_slots = if dot.is_some() { max_chars.saturating_sub(1) } else { max_chars };
            let mut buf = [0u8; 4];

            for (i, c) in label.chars().take(text_slots).enumerate() {
                let cy = start_y + i as f32 * cell_h;
                let s = c.encode_utf8(&mut buf);
                self.render_status_text(&mut vertices, s, char_x, cy, char_x + cell_w, fg, no_bg);
            }

            if let Some(color) = dot {
                let dot_y = start_y + text_slots as f32 * cell_h;
                self.render_status_text(&mut vertices, "●", char_x, dot_y, char_x + cell_w, color, no_bg);
            }
        } else {
            // Horizontal bar: render text left-to-right
            let padding = PANE_H_PADDING;
            let text_y = vp.y + (vp.height - cell_h) / 2.0;
            self.render_status_text(&mut vertices, &label, vp.x + padding, text_y, vp.x + vp.width - padding, fg, no_bg);

            if let Some(color) = attention.dot_color() {
                let dot_x = vp.x + vp.width - cell_w * 2.0;
                self.render_status_text(&mut vertices, "●", dot_x, text_y, vp.x + vp.width, color, no_bg);
            }
        }

        vertices
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

    /// Memory report for the renderer (atlas + vertex buffers).
    pub fn mem_report(&self) -> (usize, (u32, u32), usize, usize) {
        let atlas_bytes = self.atlas.mem_bytes();
        let atlas_dims = self.atlas.texture_size();
        let glyph_count = self.atlas.glyphs.len() + self.atlas.cluster_glyphs.len();
        let vertex_bytes = MAX_VERTEX_BYTES * 2; // double-buffered
        (atlas_bytes, atlas_dims, glyph_count, vertex_bytes)
    }

    fn build_help_overlay_vertices(
        &mut self,
        vertices: &mut Vec<Vertex>,
        viewport_w: f32,
        viewport_h: f32,
        keys_config: &KeysConfig,
    ) {
        let cell_w = self.atlas.cell_width;
        let cell_h = self.atlas.cell_height;

        // Semi-transparent dark overlay
        Self::push_bg_quad_alpha(vertices, 0.0, 0.0, viewport_w, viewport_h, [0.0, 0.0, 0.0], 0.9);

        let no_bg = [0.0, 0.0, 0.0, 0.0];
        let title_fg = [1.0, 0.85, 0.3, 1.0]; // accent yellow
        let label_fg = [0.7, 0.7, 0.75, 1.0];
        let key_fg = [1.0, 1.0, 1.0, 1.0];

        let title_scale = 1.8_f32;
        let body_scale = 1.3_f32;
        let scaled_cell_w = cell_w * body_scale;
        let scaled_cell_h = cell_h * body_scale;

        // Title centered
        let title = "Keyboard Shortcuts";
        let title_chars = title.chars().count() as f32;
        let title_x = (viewport_w - title_chars * cell_w * title_scale) / 2.0;
        let mut y = cell_h * 3.0;
        self.render_text(vertices, title, title_x, y, viewport_w, title_fg, no_bg, title_scale);
        y += cell_h * title_scale * 2.0;

        // Subtitle
        if self.cached_help_hint.is_empty() {
            self.cached_help_hint = format_key_combo(&keys_config.toggle_help);
        }
        let subtitle = format!("Press {} or Esc to close", &self.cached_help_hint);
        let sub_chars = subtitle.chars().count() as f32;
        let sub_x = (viewport_w - sub_chars * scaled_cell_w) / 2.0;
        self.render_text(vertices, &subtitle, sub_x, y, viewport_w, label_fg, no_bg, body_scale);
        drop(subtitle);
        y += scaled_cell_h * 2.5;

        // Build shortcut list (cached to avoid per-frame allocation)
        if self.cached_help_shortcuts.is_empty() {
            let raw: Vec<(&str, &str)> = vec![
                ("New Tab", &keys_config.new_tab),
                ("Close Pane/Tab", &keys_config.close_pane_or_tab),
                ("Close Tab", &keys_config.close_tab),
                ("Open Recent", &keys_config.open_recent_project),
                ("Vertical Split", &keys_config.vsplit),
                ("Horizontal Split", &keys_config.hsplit),
                ("V Split (Root)", &keys_config.vsplit_root),
                ("H Split (Root)", &keys_config.hsplit_root),
                ("New Window", &keys_config.new_window),
                ("Close Window", &keys_config.close_window),
                ("Copy", &keys_config.copy),
                ("Copy Raw", &keys_config.copy_raw),
                ("Paste", &keys_config.paste),
                ("Find", &keys_config.toggle_filter),
                ("Clear Scrollback", &keys_config.clear_scrollback),
                ("Previous Tab", &keys_config.prev_tab),
                ("Next Tab", &keys_config.next_tab),
                ("Rename Tab", &keys_config.rename_tab),
                ("Rename Pane", &keys_config.rename_pane),
                ("Detach Tab", &keys_config.detach_tab),
                ("Merge Window", &keys_config.merge_window),
                ("Navigate", &keys_config.navigate_up),
                ("Swap Pane", &keys_config.swap_up),
                ("Reparent Pane", &keys_config.reparent_up),
                ("Resize Pane", &keys_config.resize_up),
                ("Minimize Pane", &keys_config.minimize_pane),
                ("Restore Minimized", &keys_config.restore_minimized),
                ("Help", &keys_config.toggle_help),
            ];
            self.cached_help_shortcuts = raw.into_iter()
                .map(|(label, key)| (label.to_string(), format_key_combo_arrows(key)))
                .collect();
        }
        // Take shortcuts out of self to avoid borrow conflict with render_text
        let shortcuts = std::mem::take(&mut self.cached_help_shortcuts);

        // Render in 2 columns
        let col_width = viewport_w / 2.0;
        let label_offset = scaled_cell_w * 2.0;
        let max_label_len = shortcuts.iter().map(|(l, _)| l.chars().count()).max().unwrap_or(0) as f32;
        let key_offset = label_offset + (max_label_len + 2.0) * scaled_cell_w;

        let rows_per_col = (shortcuts.len() + 1) / 2;
        for (i, (label, formatted)) in shortcuts.iter().enumerate() {
            let col = if i < rows_per_col { 0 } else { 1 };
            let row = if i < rows_per_col { i } else { i - rows_per_col };
            let base_x = col as f32 * col_width;
            let row_y = y + row as f32 * (scaled_cell_h * 1.4);

            if row_y + scaled_cell_h > viewport_h - cell_h {
                break; // Don't overflow past global status bar
            }

            // Label
            self.render_text(vertices, label, base_x + label_offset, row_y, base_x + key_offset - scaled_cell_w, label_fg, no_bg, body_scale);

            // Key combo
            self.render_text(vertices, formatted, base_x + key_offset, row_y, base_x + col_width - scaled_cell_w, key_fg, no_bg, body_scale);
        }

        // Put shortcuts back
        self.cached_help_shortcuts = shortcuts;
    }

    fn build_mem_report_overlay_vertices(
        &mut self,
        vertices: &mut Vec<Vertex>,
        viewport_w: f32,
        viewport_h: f32,
    ) {
        let overlay_cw = self.atlas.overlay_cell_width;
        let overlay_ch = self.atlas.overlay_cell_height;

        // Semi-transparent dark overlay
        Self::push_bg_quad_alpha(vertices, 0.0, 0.0, viewport_w, viewport_h, [0.0, 0.0, 0.0], 0.9);

        let no_bg = [0.0, 0.0, 0.0, 0.0];
        let title_fg = [1.0, 0.85, 0.3, 1.0];
        let label_fg = [0.8, 0.85, 0.9, 1.0];
        let dim_fg = [0.55, 0.6, 0.65, 1.0];

        // Title — rendered natively at overlay font size, no stretching
        let title = "Memory Report";
        let title_chars = title.chars().count() as f32;
        let title_x = (viewport_w - title_chars * overlay_cw) / 2.0;
        let mut y = overlay_ch * 2.0;
        self.render_overlay_text(vertices, title, title_x, y, viewport_w, title_fg, no_bg);
        y += overlay_ch * 2.0;

        // Subtitle
        let subtitle = "Press Esc to close";
        let sub_chars = subtitle.chars().count() as f32;
        let sub_x = (viewport_w - sub_chars * overlay_cw) / 2.0;
        self.render_overlay_text(vertices, subtitle, sub_x, y, viewport_w, dim_fg, no_bg);
        y += overlay_ch * 2.5;

        // Report lines
        let report = std::mem::take(&mut self.cached_mem_report);
        let left_margin = overlay_cw * 3.0;
        for line in &report {
            if y + overlay_ch > viewport_h - overlay_ch {
                break;
            }
            let fg = if line.starts_with("===") || line.starts_with("RSS") {
                title_fg
            } else if line.starts_with("  ") {
                dim_fg
            } else {
                label_fg
            };
            self.render_overlay_text(vertices, line, left_margin, y, viewport_w - overlay_cw, fg, no_bg);
            y += overlay_ch * 1.3;
        }
        self.cached_mem_report = report;
    }

    fn build_recent_projects_overlay_vertices(
        &mut self,
        vertices: &mut Vec<Vertex>,
        viewport_w: f32,
        viewport_h: f32,
        data: &RecentProjectsRenderData<'_>,
    ) {
        let cell_w = self.atlas.cell_width;
        let cell_h = self.atlas.cell_height;

        // Semi-transparent dark overlay
        Self::push_bg_quad_alpha(vertices, 0.0, 0.0, viewport_w, viewport_h, [0.0, 0.0, 0.0], 0.9);

        let no_bg = [0.0, 0.0, 0.0, 0.0];
        let title_fg = [1.0, 0.85, 0.3, 1.0];
        let label_fg = [0.85, 0.85, 0.9, 1.0];
        let dim_fg = [0.45, 0.45, 0.5, 1.0];
        let time_fg = [0.5, 0.5, 0.55, 1.0];
        let selected_bg = [0.25, 0.35, 0.55];
        let invalid_fg = [0.4, 0.4, 0.42, 1.0];

        let title_scale = 1.8_f32;
        let body_scale = 1.3_f32;
        let scaled_cell_w = cell_w * body_scale;
        let scaled_cell_h = cell_h * body_scale;
        let row_height = scaled_cell_h * 1.6;

        // Title centered
        let title = "Open Recent Project";
        let title_chars = title.chars().count() as f32;
        let title_x = (viewport_w - title_chars * cell_w * title_scale) / 2.0;
        let mut y = cell_h * 3.0;
        self.render_text(vertices, title, title_x, y, viewport_w, title_fg, no_bg, title_scale);
        y += cell_h * title_scale * 2.0;

        // Subtitle
        let subtitle = "↑↓ Navigate  ⏎ Open  ⌘⌫ Remove  esc Cancel";
        let sub_chars = subtitle.chars().count() as f32;
        let sub_x = (viewport_w - sub_chars * scaled_cell_w) / 2.0;
        self.render_text(vertices, subtitle, sub_x, y, viewport_w, dim_fg, no_bg, body_scale);
        y += scaled_cell_h * 2.0;

        let content_top = y;
        let content_bottom = viewport_h - cell_h * 2.0;
        let max_visible = ((content_bottom - content_top) / row_height) as usize;

        // Compute scroll to keep selected visible
        let scroll = if data.selected >= data.scroll + max_visible {
            data.selected - max_visible + 1
        } else {
            data.scroll
        };

        let left_margin = scaled_cell_w * 3.0;
        let right_margin = viewport_w - scaled_cell_w * 3.0;

        if data.entries.is_empty() {
            let msg = "No recent projects to open";
            let msg_w = msg.chars().count() as f32 * scaled_cell_w;
            let msg_x = (viewport_w - msg_w) / 2.0;
            self.render_text(vertices, msg, msg_x, content_top + row_height, viewport_w, dim_fg, no_bg, body_scale);
            return;
        }

        for (i, entry) in data.entries.iter().enumerate().skip(scroll).take(max_visible) {
            let row_y = content_top + (i - scroll) as f32 * row_height;
            let text_y = row_y + (row_height - scaled_cell_h) / 2.0;

            // Selected row background
            if i == data.selected {
                Self::push_bg_quad_alpha(vertices, left_margin - scaled_cell_w, row_y, right_margin - left_margin + scaled_cell_w * 2.0, row_height, selected_bg, 0.8);
            }

            let fg = if entry.invalid { invalid_fg } else { label_fg };

            // Path
            self.render_text(vertices, &entry.path, left_margin, text_y, right_margin - scaled_cell_w * 12.0, fg, no_bg, body_scale);

            // Pane count (if > 1)
            let info = if entry.pane_count > 1 {
                format!("{}p  {}", entry.pane_count, entry.time_ago)
            } else {
                entry.time_ago.clone()
            };
            let info_w = info.chars().count() as f32 * scaled_cell_w;
            let info_x = right_margin - info_w;
            self.render_text(vertices, &info, info_x, text_y, right_margin, time_fg, no_bg, body_scale);
        }

        // Scroll indicators
        if scroll > 0 {
            let arrow = "▲";
            let ax = (viewport_w - scaled_cell_w) / 2.0;
            self.render_text(vertices, arrow, ax, content_top - scaled_cell_h, viewport_w, dim_fg, no_bg, body_scale);
        }
        if scroll + max_visible < data.entries.len() {
            let arrow = "▼";
            let ax = (viewport_w - scaled_cell_w) / 2.0;
            self.render_text(vertices, arrow, ax, content_bottom, viewport_w, dim_fg, no_bg, body_scale);
        }
    }

    pub fn cell_size(&self) -> (f32, f32) {
        (self.atlas.cell_width, self.atlas.cell_height)
    }

    pub fn set_mem_report(&mut self, report: Vec<String>) {
        self.cached_mem_report = report;
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
        Self::push_bg_quad_alpha(vertices, x, y, w, h, bg, 1.0);
    }

    fn push_bg_quad_alpha(
        vertices: &mut Vec<Vertex>,
        x: f32,
        y: f32,
        w: f32,
        h: f32,
        bg: [f32; 3],
        alpha: f32,
    ) {
        let bg4 = [bg[0], bg[1], bg[2], alpha];
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

/// Format a key combo string like "cmd+shift+d" into "⌘⇧D" for display.
/// Like `format_key_combo` but replaces a trailing arrow direction with "Arrows".
fn format_key_combo_arrows(s: &str) -> String {
    let parts: Vec<&str> = s.split('+').collect();
    if let Some(last) = parts.last() {
        match last.trim().to_ascii_lowercase().as_str() {
            "up" | "down" | "left" | "right" => {
                let prefix: Vec<&str> = parts[..parts.len() - 1].to_vec();
                let rebuilt = if prefix.is_empty() {
                    "arrows".to_string()
                } else {
                    format!("{}+arrows", prefix.join("+"))
                };
                return format_key_combo(&rebuilt);
            }
            _ => {}
        }
    }
    format_key_combo(s)
}

fn format_key_combo(s: &str) -> String {
    let mut result = String::new();
    let parts: Vec<&str> = s.split('+').collect();
    for (i, part) in parts.iter().enumerate() {
        let trimmed = part.trim();
        if i < parts.len() - 1 {
            // Modifier
            match trimmed.to_ascii_lowercase().as_str() {
                "cmd" | "command" => result.push('\u{2318}'),
                "ctrl" | "control" => result.push('\u{2303}'),
                "option" | "alt" | "opt" => result.push('\u{2325}'),
                "shift" => result.push('\u{21E7}'),
                _ => { result.push_str(trimmed); }
            }
        } else {
            // Key
            match trimmed.to_ascii_lowercase().as_str() {
                "up" => result.push('\u{2191}'),
                "down" => result.push('\u{2193}'),
                "left" => result.push('\u{2190}'),
                "right" => result.push('\u{2192}'),
                "backspace" | "delete" => result.push('\u{232B}'),
                "enter" | "return" => result.push('\u{21A9}'),
                "/" => result.push('/'),
                "[" => result.push('['),
                "]" => result.push(']'),
                s => result.push_str(&s.to_ascii_uppercase()),
            }
        }
    }
    result
}
