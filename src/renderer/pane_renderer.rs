//! Drawing one pane: its terminal grid, its status bar, the veil over an
//! unfocused pane and the outline of the focused one, built into a vertex list
//! that is kept between frames and reused while nothing it shows has changed.
//!
//! A `PaneRenderer` belongs to one pane. It draws into the rectangle the host
//! hands it (`PaneRenderData::viewport`) through the shared `RenderContext`;
//! the host owns the drawable, the clock and the per-pane clipping. Window
//! chrome (tab bar, global bar, overlays, separators) is not drawn here.
//!
//! The cache holds ABSOLUTE drawable pixels, and the pane's position is part of
//! its key: a pane that moves is rebuilt. Vertices built at the origin and
//! shifted afterwards would not be the same pixels — the build snaps cells to
//! whole pixels relative to the drawable (`(vp.x + pad + col * cell_w).round()`),
//! so a pane at a fractional x would have its glyphs moved by up to half a
//! pixel, and the unrounded sums (status-bar text, italic shear) land on other
//! f32 values. See `a_shifted_build_is_not_a_build_in_place` below.

use super::context::RenderContext;
use super::vertex::Vertex;
use super::{
    bg_runs, fade_toward, format_count, format_last_activity, push_bg_quad, PaneAttention,
    PaneRenderData, PaneViewport, TooltipZone, ITALIC_SHEAR,
};
use crate::config::{Config, DimMode};
use crate::pane::PaneId;
use crate::terminal::paste_block::RowPaint;
use crate::terminal::{CellAttrs, CursorShape, TerminalState};

/// The appearance a pane is drawn with, read from the config. Shared by every
/// pane of a host; the window chrome reads some of it too (background, bar
/// colors, bookmark colors).
pub struct PaneStyle {
    pub(super) bg_color: [f32; 3],
    /// Compact version of bg_color for comparing with Cell.bg ([u8; 3]).
    pub(super) bg_color_u8: [u8; 3],
    pub(super) cursor_color: [f32; 3],
    /// Colour of a block tagged to be copied out (see `terminal::paste_block`).
    pub(super) paste_block_color: [f32; 3],
    pub(super) selection_color: [f32; 3],
    pub(super) status_bar_enabled: bool,
    /// How much an unfocused pane is faded (see `DimMode`).
    pub(super) dim_opacity: f32,
    pub(super) dim_mode: DimMode,
    /// Outline drawn around the focused pane; width 0.0 disables it.
    pub(super) focus_border_width: f32,
    pub(super) focus_border_color: [f32; 3],
    pub(super) status_bar_bg: [f32; 3],
    pub(super) status_bar_fg: [f32; 3],
    pub(super) status_bar_cwd_color: [f32; 3],
    pub(super) status_bar_branch_color: [f32; 3],
    pub(super) status_bar_scroll_color: [f32; 3],
    /// Colors of a bookmarked conversation (status bar and switcher row).
    pub(super) bookmarks: crate::config::BookmarksConfig,
}

impl PaneStyle {
    pub fn from_config(config: &Config) -> Self {
        PaneStyle {
            bg_color: config.colors.background,
            bg_color_u8: crate::terminal::color_to_u8(config.colors.background),
            cursor_color: config.colors.cursor,
            paste_block_color: config.colors.paste_block,
            selection_color: [0.45, 0.42, 0.20],
            status_bar_enabled: config.status_bar.enabled,
            dim_opacity: config.splits.dim_opacity,
            dim_mode: config.splits.dim_mode,
            focus_border_width: config.splits.focus_border_width,
            focus_border_color: config.splits.focus_border_color,
            status_bar_bg: config.status_bar.bg_color,
            status_bar_fg: config.status_bar.fg_color,
            status_bar_cwd_color: config.status_bar.cwd_color,
            status_bar_branch_color: config.status_bar.branch_color,
            status_bar_scroll_color: config.status_bar.scroll_color,
            bookmarks: config.bookmarks.clone(),
        }
    }
}

/// Cached vertex list for one pane, with the conditions it was built under.
/// Reused as the pane's "previous coherent frame" while the pane defers
/// rendering inside a DEC-2026 sync burst, and on any frame where the pane
/// is clean and its key unchanged — see `can_reuse_pane_cache`.
struct PaneVertexEntry {
    verts: Vec<Vertex>,
    mid_sync: bool,
    ready: bool,
    /// Everything the build read besides the grid (which `dirty` covers).
    key: PaneDrawKey,
    /// Status-bar tooltip zones the build registered, replayed on reuse.
    zones: Vec<TooltipZone>,
}

/// What a pane's vertices depend on apart from its terminal grid. These
/// change without the pane being marked dirty (a cursor blink, a hovered
/// URL, a focus change, the input counter…), so a cached build is only
/// reused when the pane is clean AND this key is unchanged.
#[derive(Clone, PartialEq)]
pub(super) struct PaneDrawKey {
    pub(super) vp: [f32; 4],
    pub(super) is_focused: bool,
    pub(super) show_blink: bool,
    pub(super) hover: Option<(Vec<(usize, u16, u16)>, Option<String>)>,
    pub(super) custom_title: Option<String>,
    pub(super) has_bell: bool,
    pub(super) has_completion: bool,
    pub(super) input_chars: u64,
    pub(super) printable_chars: u64,
    pub(super) last_activity: u64,
    pub(super) fg_process: Option<String>,
    pub(super) bookmarked: bool,
    pub(super) atlas_generation: u64,
}

/// Whether a pane can be drawn from its cached vertices this frame.
/// Inside a DEC 2026 burst the previous coherent frame is drawn whatever
/// changed (only the viewport must match: vertices are absolute pixels).
/// Outside one, the pane must be clean and nothing it shows may have moved.
/// A build made mid-burst or for the loading placeholder is never reused.
pub(super) fn can_reuse_pane_cache(
    entry: Option<(&PaneDrawKey, bool, bool)>, // (key, mid_sync, ready)
    key: &PaneDrawKey,
    shell_ready: bool,
    in_sync: bool,
    dirty: bool,
) -> bool {
    let Some((cached, mid_sync, ready)) = entry else { return false };
    if !shell_ready || !ready || mid_sync {
        return false;
    }
    if in_sync {
        cached.vp == key.vp
    } else {
        !dirty && cached == key
    }
}

/// Draws one pane and keeps its last build. Owned by the host, one per pane;
/// dropping it (or `invalidate`) forgets the cache, nothing else.
#[derive(Default)]
pub struct PaneRenderer {
    entry: Option<PaneVertexEntry>,
}

impl PaneRenderer {
    /// True while a pane is inside a synchronized-output burst (DEC 2026)
    /// and should be drawn from its cached previous frame. Capped so a lost
    /// ?2026l can't freeze the pane; a full TUI repaint spanning several PTY
    /// chunks fits comfortably within the window.
    pub(super) fn in_sync_window(pane: &PaneRenderData) -> bool {
        let t = pane.terminal.read();
        t.synchronized_output
            && t.sync_output_since.is_some_and(|s| s.elapsed().as_millis() < 150)
    }

    /// Everything `pane`'s build reads besides its grid (see `PaneDrawKey`).
    /// `hovered_url` / `hovered_url_text` are this pane's hover, `None` when
    /// the hovered URL belongs to another pane.
    pub(super) fn draw_key(
        pane: &PaneRenderData,
        hovered_url: Option<&[(usize, u16, u16)]>,
        hovered_url_text: Option<&str>,
        blink_on: bool,
        atlas_generation: u64,
    ) -> PaneDrawKey {
        let vp = &pane.viewport;
        let hover = hovered_url.map(|segs| (segs.to_vec(), hovered_url_text.map(str::to_string)));
        let t = pane.terminal.read();
        PaneDrawKey {
            vp: [vp.x, vp.y, vp.width, vp.height],
            is_focused: pane.is_focused,
            show_blink: if pane.is_focused { blink_on } else { true },
            hover,
            custom_title: pane.custom_title.clone(),
            has_bell: pane.has_bell,
            has_completion: pane.has_completion,
            input_chars: pane.input_chars.load(std::sync::atomic::Ordering::Relaxed),
            printable_chars: t.printable_chars.load(std::sync::atomic::Ordering::Relaxed),
            last_activity: t.last_activity_secs.load(std::sync::atomic::Ordering::Relaxed),
            fg_process: pane.fg_process.clone(),
            bookmarked: pane.bookmarked,
            atlas_generation,
        }
    }

    /// The pane's half of "does this frame need drawing": something its last
    /// build read besides the grid has moved (input counter, fg process,
    /// bookmark, blink…). Those never mark the terminal dirty; the grid's own
    /// changes are the terminal's `dirty` flag. False without a cached build.
    pub(super) fn key_changed(&self, key: &PaneDrawKey) -> bool {
        self.entry.as_ref().is_some_and(|e| e.key != *key)
    }

    /// Whether a build is cached (false before the first one, or after the
    /// cache was dropped).
    pub(super) fn is_built(&self) -> bool {
        self.entry.is_some()
    }

    /// The vertices of the last build, empty before the first one.
    pub(super) fn vertices(&self) -> &[Vertex] {
        self.entry.as_ref().map_or(&[], |e| &e.verts)
    }

    /// Make sure the cache holds this frame's vertices: reuse the last build
    /// when `can_reuse_pane_cache` allows it, rebuild otherwise. Tooltip zones
    /// of the pane's status bar are appended to `zones` either way.
    pub(super) fn prepare(
        &mut self,
        ctx: &mut RenderContext,
        style: &PaneStyle,
        pane: &PaneRenderData,
        hovered_url: Option<&[(usize, u16, u16)]>,
        hovered_url_text: Option<&str>,
        key: PaneDrawKey,
        in_sync: bool,
        dirty: bool,
        zones: &mut Vec<TooltipZone>,
    ) {
        let vp = &pane.viewport;
        let show_blink = key.show_blink;
        let pin = key.input_chars;
        // Sync-deferred pane: draw its previous coherent frame from
        // the cache instead of rebuilding from the mid-update grid.
        // Clean pane whose inputs did not move: its last build is
        // still exact, skip the rebuild (see `can_reuse_pane_cache`).
        let reuse_cached = can_reuse_pane_cache(
            self.entry.as_ref().map(|e| (&e.key, e.mid_sync, e.ready)),
            &key,
            pane.shell_ready,
            in_sync,
            dirty,
        );
        if reuse_cached {
            if let Some(e) = &self.entry {
                zones.extend(e.zones.iter().cloned());
            }
            return;
        }
        let (cell_w, cell_h) = ctx.cell_size();
        let zones_before = zones.len();
        let pane_verts = {
            let mut build = PaneBuild { ctx, style, hovered_url, hovered_url_text, zones };
            let pane_attention = PaneAttention::from_flags(pane.has_bell, pane.has_completion);
            let mut verts = if pane.shell_ready {
                let t = pane.terminal.read();
                build.build_vertices(&t, vp, show_blink, pane.is_focused, pane.custom_title.as_deref(), pane_attention, pin, pane.pane_id, pane.fg_process.as_deref(), pane.bookmarked)
            } else {
                build.build_loading_vertices(vp)
            };
            // Attention indicator dot on non-focused panes
            if let Some(color) = pane_attention.dot_color() {
                let dot_x = vp.x + vp.width - cell_w * 2.5;
                let dot_y = vp.y + cell_h * 0.5;
                let no_bg = [0.0_f32, 0.0, 0.0, 0.0];
                build.ctx.render_status_text(&mut verts, "●", dot_x, dot_y, vp.x + vp.width, color, no_bg);
            }
            verts
        };
        self.entry = Some(PaneVertexEntry {
            verts: pane_verts,
            // A cache-miss build during a sync burst is possibly
            // torn: never reuse it as a "coherent previous frame";
            // it refreshes every tick until a clean build replaces it.
            mid_sync: in_sync,
            ready: pane.shell_ready,
            key,
            zones: zones[zones_before..].to_vec(),
        });
    }
}

/// One build of one pane: the shared context, the style, this pane's hover,
/// and where the status bar registers its tooltip zones.
pub(super) struct PaneBuild<'a> {
    pub(super) ctx: &'a mut RenderContext,
    pub(super) style: &'a PaneStyle,
    /// Hovered URL: per-row segments [(visible_row, col_start, col_end)]
    pub(super) hovered_url: Option<&'a [(usize, u16, u16)]>,
    /// Hovered URL text (for status bar display)
    pub(super) hovered_url_text: Option<&'a str>,
    pub(super) zones: &'a mut Vec<TooltipZone>,
}

impl PaneBuild<'_> {
    fn push_tooltip_zone(&mut self, x: f32, y: f32, width: f32, height: f32, text: &'static str) {
        self.zones.push(TooltipZone { x, y, width, height, text });
    }

    pub(super) fn build_vertices(
        &mut self,
        term: &TerminalState,
        vp: &PaneViewport,
        blink_on: bool,
        is_focused: bool,
        custom_title: Option<&str>,
        attention: PaneAttention,
        pane_input_chars: u64,
        pane_id: PaneId,
        fg_process: Option<&str>,
        bookmarked: bool,
    ) -> Vec<Vertex> {
        // Pass 1: collect unknown chars/clusters for dynamic rasterization
        let display = term.visible_lines();
        let mut unknown_chars: Vec<char> = Vec::new();
        let mut unknown_italic_chars: Vec<char> = Vec::new();
        let mut unknown_clusters: Vec<Box<str>> = Vec::new();
        let has_italic = self.ctx.atlas.has_italic();
        {
            let mut seen_chars = std::collections::HashSet::new();
            let mut seen_italic = std::collections::HashSet::new();
            let mut seen_clusters = std::collections::HashSet::new();
            for line in display.iter() {
                for cell in line.iter() {
                    if let Some(ref cluster) = cell.cluster {
                        if self.ctx.atlas.cluster_glyph(cluster).is_none() && seen_clusters.insert(cluster.clone()) {
                            unknown_clusters.push(cluster.clone());
                        }
                    } else {
                        let c = cell.c;
                        if c == ' ' || c == '\0' {
                            continue;
                        }
                        // Regular glyph is always needed (non-italic cells, and the
                        // synthetic-shear fallback for chars with no italic form).
                        if self.ctx.atlas.glyph(c).is_none() && seen_chars.insert(c) {
                            unknown_chars.push(c);
                        }
                        if has_italic
                            && cell.attrs.contains(CellAttrs::ITALIC)
                            && self.ctx.atlas.italic_glyph(c).is_none()
                            && seen_italic.insert(c)
                        {
                            unknown_italic_chars.push(c);
                        }
                    }
                }
            }
        }

        // Pass 2: rasterize unknowns
        for c in unknown_chars {
            self.ctx.atlas.rasterize_char(c);
        }
        for c in unknown_italic_chars {
            self.ctx.atlas.rasterize_italic_char(c);
        }
        for cluster in unknown_clusters {
            self.ctx.atlas.rasterize_cluster(&cluster);
        }

        // Pass 3: build vertices
        // Amount by which this pane's text is faded. Non-zero only for an
        // unfocused pane in `text` dim mode; `full` mode fades with a veil
        // quad at the end instead.
        let text_fade = if !is_focused && self.style.dim_mode == DimMode::Text {
            self.style.dim_opacity
        } else {
            0.0
        };
        let cell_w = self.ctx.atlas.cell_width;
        let cell_h = self.ctx.atlas.cell_height;
        let baseline_from_top = self.ctx.atlas.baseline_from_top();
        let atlas_w = self.ctx.atlas.atlas_width as f32;
        let atlas_h = self.ctx.atlas.atlas_height as f32;
        let ox = vp.x + self.ctx.h_padding();
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

            // Cell backgrounds, one quad per run of same-colour cells: a TUI's
            // coloured band is a single rectangle, not one per column. Covers
            // exactly the union of the per-cell quads it replaces.
            let cols = (term.cols as usize).min(line.len());
            for (start, end, bg) in bg_runs(&line[..cols], self.style.bg_color_u8) {
                let x = (ox + start as f32 * cell_w).round();
                let x_end = (ox + (end - 1) as f32 * cell_w).round() + cell_w;
                push_bg_quad(&mut vertices, x, y, x_end - x, cell_h, crate::terminal::color_to_f32(bg));
            }

            // Selection highlight (on top of every cell bg of the row, under glyphs)
            if has_selection {
                for col_idx in 0..term.cols as usize {
                    if term.is_selected(abs_line, col_idx as u16) {
                        let x = (ox + col_idx as f32 * cell_w).round();
                        push_bg_quad(&mut vertices, x, y, cell_w, cell_h, self.style.selection_color);
                    }
                }
            }
        }

        // Rows holding a block Claude tagged as meant to be pasted elsewhere. Read from
        // the cells rather than tracked as the bytes arrive: Claude Code redraws lines
        // while it streams, and a state machine fed by those redraws would drift.
        let paste_rows = {
            let lines: Vec<&[crate::terminal::Cell]> = display.iter().map(|l| l.as_ref()).collect();
            crate::terminal::paste_block::paste_block_rows(&lines, term.default_fg)
        };

        // Pass 2: glyphs (on top of backgrounds and selection)
        for (row_idx, line) in display.iter().enumerate() {
            // The line that closes a paste block: it delimited, it is done, and drawing it
            // would put a marker on screen for every message.
            if paste_rows[row_idx] == RowPaint::Hidden {
                continue;
            }
            for col_idx in 0..term.cols as usize {
                let cell = if col_idx < line.len() {
                    &line[col_idx]
                } else {
                    continue;
                };

                // Underline / strikethrough: horizontal rules in the cell's fg
                // color. Drawn before the blank skip so runs of underlined
                // spaces (and wide-char continuation cells) stay continuous.
                if cell.attrs.intersects(CellAttrs::UNDERLINE | CellAttrs::STRIKETHROUGH) {
                    let lx = (ox + col_idx as f32 * cell_w).round();
                    let ly = (oy + y_offset + row_idx as f32 * cell_h).round();
                    let rule_fg = fade_toward(crate::terminal::color_to_f32(cell.fg), self.style.bg_color, text_fade);
                    let thickness = (cell_h * 0.07).max(1.0).round();
                    if cell.attrs.contains(CellAttrs::UNDERLINE) {
                        let uy = (ly + cell_h - thickness).round();
                        push_bg_quad(&mut vertices, lx, uy, cell_w, thickness, rule_fg);
                    }
                    if cell.attrs.contains(CellAttrs::STRIKETHROUGH) {
                        let sy = (ly + cell_h * 0.5 - thickness * 0.5).round();
                        push_bg_quad(&mut vertices, lx, sy, cell_w, thickness, rule_fg);
                    }
                }

                if cell.is_blank() {
                    continue;
                }
                let c = cell.c;

                if c == '─' && row_idx == 2 && col_idx < 3 {
                    log::trace!("render ─ at col={} row={} fg={:?} bg={:?}", col_idx, row_idx, cell.fg, cell.bg);
                }

                // Look up glyph: cluster first, then single char. Italic cells
                // prefer the real italic glyph; when there is none (no italic
                // face, or a char with no italic form like box-drawing) we fall
                // back to the upright glyph and shear it synthetically below.
                let mut real_italic = false;
                let glyph = if let Some(ref cluster) = cell.cluster {
                    match self.ctx.atlas.cluster_glyph(cluster) {
                        Some(g) => *g,
                        None => continue,
                    }
                } else if cell.attrs.contains(CellAttrs::ITALIC) {
                    match self.ctx.atlas.italic_glyph(c) {
                        Some(g) => {
                            real_italic = true;
                            *g
                        }
                        None => match self.ctx.atlas.glyph(c) {
                            Some(g) => *g,
                            None => continue,
                        },
                    }
                } else {
                    match self.ctx.atlas.glyph(c) {
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
                // Inside a tagged block, text that carries no colour of its own takes the
                // paste colour; anything Claude coloured itself keeps what it was given.
                let fg_f = if paste_rows[row_idx] == RowPaint::Body && cell.fg == term.default_fg {
                    self.style.paste_block_color
                } else {
                    crate::terminal::color_to_f32(cell.fg)
                };
                let fg_f = fade_toward(fg_f, self.style.bg_color, text_fade);
                let fg = [fg_f[0], fg_f[1], fg_f[2], alpha];
                let no_bg = [0.0, 0.0, 0.0, 0.0];

                // Synthetic italic: shear the glyph quad around the baseline —
                // the top edge leans right, the descender edge leans left. Emoji
                // and box-drawing keep their shape only insofar as the quad is
                // slanted; color glyphs are left as-is to avoid smearing.
                let italic = cell.attrs.contains(CellAttrs::ITALIC) && !glyph.is_color && !real_italic;
                let (shear_top, shear_bot) = if italic {
                    (ITALIC_SHEAR * baseline_from_top, ITALIC_SHEAR * (baseline_from_top - gh))
                } else {
                    (0.0, 0.0)
                };

                // Faux-bold: draw the glyph a second time shifted +1px in x. A
                // real bold font would need a (char, bold)-keyed atlas; the
                // synthetic double-draw is cheap and reads clearly as bold.
                // Color emoji are already color glyphs — don't embolden them.
                let bold = cell.attrs.contains(CellAttrs::BOLD) && !glyph.is_color;
                let x_offsets: &[f32] = if bold { &[0.0, 1.0] } else { &[0.0] };
                for &dx in x_offsets {
                    let xtl = gx + dx + shear_top;
                    let xtr = gx + gw + dx + shear_top;
                    let xbl = gx + dx + shear_bot;
                    let xbr = gx + gw + dx + shear_bot;
                    vertices.push(Vertex { position: [xtl, gy], tex_coords: [tx, ty], color: fg, bg_color: no_bg });
                    vertices.push(Vertex { position: [xtr, gy], tex_coords: [tx + tw, ty], color: fg, bg_color: no_bg });
                    vertices.push(Vertex { position: [xbl, gy + gh], tex_coords: [tx, ty + th], color: fg, bg_color: no_bg });
                    vertices.push(Vertex { position: [xtr, gy], tex_coords: [tx + tw, ty], color: fg, bg_color: no_bg });
                    vertices.push(Vertex { position: [xbr, gy + gh], tex_coords: [tx + tw, ty + th], color: fg, bg_color: no_bg });
                    vertices.push(Vertex { position: [xbl, gy + gh], tex_coords: [tx, ty + th], color: fg, bg_color: no_bg });
                }
            }
        }

        // Draw URL underline for hovered URL (may span multiple wrapped rows)
        if let Some(segments) = self.hovered_url {
            let url_color = [0.4, 0.6, 1.0];
            for &(hover_row, col_start, col_end) in segments {
                let uy = (oy + y_offset + hover_row as f32 * cell_h + cell_h - 1.0).round();
                let ux = (ox + col_start as f32 * cell_w).round();
                let uw = (col_end - col_start) as f32 * cell_w;
                push_bg_quad(&mut vertices, ux, uy, uw, 1.0, url_color);
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
                    // Filled block only where the keystrokes go. Elsewhere the
                    // cursor is drawn hollow, the way a text field marks that it
                    // no longer has the caret.
                    CursorShape::Block if is_focused => {
                        push_bg_quad(&mut vertices, cx, cy, cell_w, cell_h, self.style.cursor_color);
                    }
                    CursorShape::Block => {
                        let t = (cell_h * 0.08).max(1.0).round();
                        push_bg_quad(&mut vertices, cx, cy, cell_w, t, self.style.cursor_color);
                        push_bg_quad(&mut vertices, cx, cy + cell_h - t, cell_w, t, self.style.cursor_color);
                        push_bg_quad(&mut vertices, cx, cy + t, t, cell_h - 2.0 * t, self.style.cursor_color);
                        push_bg_quad(&mut vertices, cx + cell_w - t, cy + t, t, cell_h - 2.0 * t, self.style.cursor_color);
                    }
                    CursorShape::Underline => {
                        let thickness = (cell_h * 0.1).max(1.0);
                        push_bg_quad(&mut vertices, cx, cy + cell_h - thickness, cell_w, thickness, self.style.cursor_color);
                    }
                    CursorShape::Bar => {
                        let thickness = (cell_w * 0.1).max(1.0);
                        push_bg_quad(&mut vertices, cx, cy, thickness, cell_h, self.style.cursor_color);
                    }
                }
            }
        }

        // Status bar. Built before the veil so the veil covers it too: leaving
        // it out made every unfocused bar as bright as the focused one, and with
        // four splits nothing pointed at the pane that had the keyboard.
        if self.style.status_bar_enabled {
            self.build_status_bar_vertices(&mut vertices, vp, term, custom_title, attention, pane_input_chars, pane_id, fg_process, text_fade, bookmarked);
        }

        // Veil over an unfocused pane, status bar included.
        if !is_focused && self.style.dim_mode == DimMode::Full && self.style.dim_opacity > 0.0 {
            let dim4 = [0.0, 0.0, 0.0, self.style.dim_opacity]; // black overlay
            let no_tex = [0.0, 0.0];
            let white = [1.0, 1.0, 1.0, 0.0];
            let dim_h = vp.height;
            vertices.push(Vertex { position: [vp.x, vp.y], tex_coords: no_tex, color: white, bg_color: dim4 });
            vertices.push(Vertex { position: [vp.x + vp.width, vp.y], tex_coords: no_tex, color: white, bg_color: dim4 });
            vertices.push(Vertex { position: [vp.x, vp.y + dim_h], tex_coords: no_tex, color: white, bg_color: dim4 });
            vertices.push(Vertex { position: [vp.x + vp.width, vp.y], tex_coords: no_tex, color: white, bg_color: dim4 });
            vertices.push(Vertex { position: [vp.x + vp.width, vp.y + dim_h], tex_coords: no_tex, color: white, bg_color: dim4 });
            vertices.push(Vertex { position: [vp.x, vp.y + dim_h], tex_coords: no_tex, color: white, bg_color: dim4 });
        }

        // Outline around the focused pane, drawn last so nothing covers it.
        if is_focused && self.style.focus_border_width > 0.0 {
            let w = self.style.focus_border_width.min(vp.width * 0.5).min(vp.height * 0.5);
            let c = self.style.focus_border_color;
            push_bg_quad(&mut vertices, vp.x, vp.y, vp.width, w, c);
            push_bg_quad(&mut vertices, vp.x, vp.y + vp.height - w, vp.width, w, c);
            push_bg_quad(&mut vertices, vp.x, vp.y + w, w, vp.height - 2.0 * w, c);
            push_bg_quad(&mut vertices, vp.x + vp.width - w, vp.y + w, w, vp.height - 2.0 * w, c);
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
        pane_id: PaneId,
        fg_process: Option<&str>,
        text_fade: f32,
        bookmarked: bool,
    ) {
        let cell_w = self.ctx.atlas.cell_width;
        let cell_h = self.ctx.atlas.cell_height;
        let bar_y = vp.y + vp.height - cell_h;

        // Background quad: orange for bell, green for completion, default otherwise.
        // In `text` dim mode the veil never comes, so the bar of an unfocused
        // pane fades here — bar included, or the brightest thing on screen ends
        // up being a pane nobody is typing in.
        // A bookmarked pane paints its whole bar very dark blue: the mark has to
        // be readable at a glance across four splits, and a single glyph is not.
        // Darker than the default bar, so the usual text colors keep their
        // contrast — the bar changes hue, not its readability. Attention still
        // wins: a bell or a finished run is news, a bookmark is a standing fact.
        let base_bar_bg = if bookmarked { self.style.bookmarks.bar_bg } else { self.style.status_bar_bg };
        let bar_bg = fade_toward(attention.bar_bg(base_bar_bg), self.style.bg_color, text_fade);
        push_bg_quad(vertices, vp.x, bar_y, vp.width, cell_h, bar_bg);

        let no_bg = [0.0, 0.0, 0.0, 0.0];
        let cwd = fade_toward(self.style.status_bar_cwd_color, bar_bg, text_fade);
        let branch = fade_toward(self.style.status_bar_branch_color, bar_bg, text_fade);
        let scroll = fade_toward(self.style.status_bar_scroll_color, bar_bg, text_fade);
        let fg = fade_toward(self.style.status_bar_fg, bar_bg, text_fade);
        let cwd_fg = [cwd[0], cwd[1], cwd[2], 1.0];
        let branch_fg = [branch[0], branch[1], branch[2], 1.0];
        let scroll_fg = [scroll[0], scroll[1], scroll[2], 1.0];
        let title_fg = [fg[0], fg[1], fg[2], 1.0];
        let id_fg = [fg[0], fg[1], fg[2], 0.6];
        let process_fg = [fg[0], fg[1], fg[2], 0.9];

        // Pane ID first, always visible: it is the handle used to address the
        // pane over IPC, so it never gets dropped when the bar runs out of room.
        let mut cursor_x = vp.x + self.ctx.h_padding() + cell_w; // 1 cell padding from left
        {
            let id_str = format!("#{}", pane_id);
            let id_w = id_str.chars().count() as f32 * cell_w;
            let id_x = cursor_x;
            cursor_x = self.ctx.render_status_text(vertices, &id_str, id_x, bar_y, vp.x + vp.width, id_fg, no_bg);
            cursor_x += cell_w; // 1 cell gap before the CWD
            self.push_tooltip_zone(id_x, bar_y, id_w, cell_h, "Pane ID (KOVA_PANE_ID — use it with the IPC socket)");
        }

        // Render CWD aligned to the left
        if let Some(ref cwd) = term.cwd {
            let home = std::env::var("HOME").unwrap_or_default();
            let display_path = if !home.is_empty() && cwd.starts_with(&home) {
                format!("~{}", &cwd[home.len()..])
            } else {
                cwd.clone()
            };
            cursor_x = self.ctx.render_status_text(vertices, &display_path, cursor_x, bar_y, vp.x + vp.width * 0.4, cwd_fg, no_bg);
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
        let mut left_end = self.ctx.render_status_text(vertices, &branch_display, cursor_x, bar_y, vp.x + vp.width * 0.6, actual_branch_fg, no_bg);

        // Foreground process after the branch: what is actually running in the
        // pane right now (claude, nvim, ssh…). Absent at a bare shell prompt,
        // so the bar stays quiet when nothing runs.
        if let Some(process) = fg_process {
            let proc_x = left_end + cell_w * 2.0;
            let proc_w = process.chars().count() as f32 * cell_w;
            left_end = self.ctx.render_status_text(vertices, process, proc_x, bar_y, vp.x + vp.width * 0.75, process_fg, no_bg);
            self.push_tooltip_zone(proc_x, bar_y, proc_w, cell_h, "Foreground process running in this pane");
        }

        // Right side: title (custom or hovered URL or OSC) + scroll indicator
        let right_edge = vp.x + vp.width - cell_w; // 1 cell padding from right

        // Scroll indicator (rightmost)
        let scroll_off = term.scroll_offset();
        let right_after_scroll = if scroll_off > 0 {
            let scroll_str = format!("↑{}", scroll_off);
            let scroll_w = scroll_str.chars().count() as f32 * cell_w;
            let right_x = right_edge - scroll_w;
            self.ctx.render_status_text(vertices, &scroll_str, right_x, bar_y, right_edge + cell_w, scroll_fg, no_bg);
            self.push_tooltip_zone(right_x, bar_y, scroll_w, cell_h, "Scroll offset (lines above visible area)");
            right_x - cell_w * 2.0 // gap before title
        } else {
            right_edge
        };

        // Title: hovered URL > custom_title > OSC title
        let right_text: Option<(String, [f32; 4])> = if let Some(url) = self.hovered_url_text {
            Some((url.to_string(), [0.4, 0.6, 1.0, 1.0]))
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
                self.ctx.render_status_text(vertices, text, title_x, bar_y, right_after_scroll, fg, no_bg);
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
        let io_rendered = io_x >= left_end + cell_w * 2.0;
        let dim_fg = [self.style.status_bar_fg[0], self.style.status_bar_fg[1], self.style.status_bar_fg[2], 0.4];
        if io_rendered {
            self.ctx.render_status_text(vertices, &io_str, io_x, bar_y, right_content_start, dim_fg, no_bg);
            self.push_tooltip_zone(io_x, bar_y, io_w, cell_h, "Pane I/O — ↑ chars sent  ↓ chars received");
        }

        // Last interaction timestamp (DD/MMM HH:mm) — shown to the left of the I/O counters.
        let last_activity = term.last_activity_secs.load(std::sync::atomic::Ordering::Relaxed);
        if last_activity > 0 {
            let ts_str = format_last_activity(last_activity);
            let ts_w = ts_str.chars().count() as f32 * cell_w;
            let ts_right = if io_rendered { io_x - cell_w * 2.0 } else { right_content_start };
            let ts_x = ts_right - ts_w;
            if ts_x >= left_end + cell_w * 2.0 {
                self.ctx.render_status_text(vertices, &ts_str, ts_x, bar_y, ts_right, dim_fg, no_bg);
                self.push_tooltip_zone(ts_x, bar_y, ts_w, cell_h, "Last input or output on this pane");
            }
        }
    }

    pub(super) fn build_loading_vertices(&mut self, vp: &PaneViewport) -> Vec<Vertex> {
        let text = "starting...";
        let cell_w = self.ctx.atlas.cell_width;
        let cell_h = self.ctx.atlas.cell_height;
        let atlas_w = self.ctx.atlas.atlas_width as f32;
        let atlas_h = self.ctx.atlas.atlas_height as f32;

        let text_w = text.len() as f32 * cell_w;
        let start_x = vp.x + (vp.width - text_w) / 2.0;
        let start_y = vp.y + (vp.height - cell_h) / 2.0;

        let fg = [0.4, 0.4, 0.45, 1.0];
        let no_bg = [0.0, 0.0, 0.0, 0.0];
        let mut vertices = Vec::new();

        for (i, c) in text.chars().enumerate() {
            let glyph = match self.ctx.atlas.glyph(c) {
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use objc2_metal::{MTLCreateSystemDefaultDevice, MTLPixelFormat};
    use parking_lot::RwLock;
    use std::sync::atomic::AtomicU64;
    use std::sync::Arc;

    fn context(config: &Config) -> RenderContext {
        let device = MTLCreateSystemDefaultDevice().expect("no Metal device");
        RenderContext::new(&device, MTLPixelFormat::BGRA8Unorm, 2.0, config)
    }

    /// A terminal with a coloured first cell, so the build starts with a
    /// background quad snapped to the pixel grid.
    fn terminal() -> TerminalState {
        let c = Config::default();
        let mut t = TerminalState::new(
            20,
            4,
            10,
            crate::terminal::color_to_u8(c.colors.foreground),
            crate::terminal::color_to_u8(c.colors.background),
        );
        t.set_sgr(&[44]);
        t.put_char('x');
        t.set_sgr(&[0]);
        for c in "yz ab".chars() {
            t.put_char(c);
        }
        t.cwd = Some("/tmp".into());
        t
    }

    fn pane(t: TerminalState, vp: PaneViewport) -> PaneRenderData {
        PaneRenderData {
            terminal: Arc::new(RwLock::new(t)),
            viewport: vp,
            shell_ready: true,
            is_focused: true,
            pane_id: 7,
            custom_title: None,
            has_completion: false,
            has_bell: false,
            minimized: false,
            input_chars: Arc::new(AtomicU64::new(0)),
            fg_process: Some("claude".into()),
            bookmarked: false,
        }
    }

    fn vp_at(x: f32, y: f32) -> PaneViewport {
        PaneViewport { x, y, width: 400.0, height: 160.0 }
    }

    /// One frame of the host's loop for this pane, outside a sync burst.
    fn prepare(
        r: &mut PaneRenderer,
        ctx: &mut RenderContext,
        style: &PaneStyle,
        p: &PaneRenderData,
        dirty: bool,
    ) -> Vec<TooltipZone> {
        let key = PaneRenderer::draw_key(p, None, None, true, ctx.atlas.generation);
        let mut zones = Vec::new();
        r.prepare(ctx, style, p, None, None, key, false, dirty, &mut zones);
        zones
    }

    /// Every float of every vertex, bit for bit.
    fn bits(vs: &[Vertex]) -> Vec<[u32; 12]> {
        vs.iter()
            .map(|v| {
                let [x, y] = v.position;
                let [u, w] = v.tex_coords;
                let [r, g, b, a] = v.color;
                let [br, bg, bb, ba] = v.bg_color;
                [x, y, u, w, r, g, b, a, br, bg, bb, ba].map(f32::to_bits)
            })
            .collect()
    }

    fn zone_list(zones: &[TooltipZone]) -> Vec<(u32, u32, u32, u32, &'static str)> {
        zones
            .iter()
            .map(|z| (z.x.to_bits(), z.y.to_bits(), z.width.to_bits(), z.height.to_bits(), z.text))
            .collect()
    }

    #[test]
    fn a_clean_pane_is_drawn_from_its_cache_with_its_tooltip_zones() {
        let config = Config::default();
        let mut ctx = context(&config);
        let style = PaneStyle::from_config(&config);
        let p = pane(terminal(), vp_at(100.0, 30.0));
        let mut r = PaneRenderer::default();
        let first_zones = prepare(&mut r, &mut ctx, &style, &p, true);
        let first = bits(r.vertices());
        assert!(!first.is_empty());
        assert!(!first_zones.is_empty(), "the status bar registers its zones");
        // The grid changes without the dirty flag: a reuse must not see it.
        p.terminal.write().put_char('Q');
        let zones = prepare(&mut r, &mut ctx, &style, &p, false);
        assert_eq!(bits(r.vertices()), first, "clean and unchanged: drawn from the cache");
        assert_eq!(zone_list(&zones), zone_list(&first_zones), "zones replayed on reuse");
        // Dirty: rebuilt, and the new cell shows.
        prepare(&mut r, &mut ctx, &style, &p, true);
        assert_ne!(bits(r.vertices()), first);
    }

    #[test]
    fn a_moved_pane_is_rebuilt_exactly_where_it_now_is() {
        // Columns scrolling move a pane by a fractional amount. Its cache is
        // keyed on the position, so the frame after the move is a fresh build
        // there — the same floats as a pane that had always been there.
        let config = Config::default();
        let mut ctx = context(&config);
        let style = PaneStyle::from_config(&config);
        let mut p = pane(terminal(), vp_at(100.0, 30.0));
        let mut moved = PaneRenderer::default();
        prepare(&mut moved, &mut ctx, &style, &p, true);
        let before = bits(moved.vertices());
        p.viewport = vp_at(-63.37, 30.0);
        let moved_zones = prepare(&mut moved, &mut ctx, &style, &p, false);
        assert_ne!(bits(moved.vertices()), before, "a clean pane that moved is not reused");

        let mut fresh = PaneRenderer::default();
        let fresh_zones = prepare(&mut fresh, &mut ctx, &style, &p, true);
        assert_eq!(bits(moved.vertices()), bits(fresh.vertices()));
        assert_eq!(zone_list(&moved_zones), zone_list(&fresh_zones));
    }

    #[test]
    fn a_shifted_build_is_not_a_build_in_place() {
        // Why the cache is not kept in pane-local coordinates: the build snaps
        // cells to whole drawable pixels, `(vp.x + pad + col * cell_w).round()`.
        // Built at x = 0 and shifted by 100.5, the first cell lands on 120.5;
        // built in place it lands on 121. Translating a cached build would move
        // every glyph of a pane at a fractional x by half a pixel.
        let mut config = Config::default();
        config.status_bar.enabled = false;
        let mut ctx = context(&config);
        let style = PaneStyle::from_config(&config);
        let t = terminal();
        let mut zones = Vec::new();
        let mut build = |vp: PaneViewport| {
            PaneBuild { ctx: &mut ctx, style: &style, hovered_url: None, hovered_url_text: None, zones: &mut zones }
                .build_vertices(&t, &vp, true, true, None, PaneAttention::None, 0, 7, None, false)
        };
        let in_place = build(vp_at(100.5, 30.0));
        let at_origin = build(vp_at(0.0, 30.0));
        assert_eq!(in_place.len(), at_origin.len());
        let pad = 2.0 * crate::renderer::PANE_H_PADDING;
        // Quad 0 is the coloured background of the first cell.
        assert_eq!(in_place[0].position[0], (100.5 + pad).round());
        assert_eq!(at_origin[0].position[0] + 100.5, 100.5 + pad);
        assert_ne!(in_place[0].position[0], at_origin[0].position[0] + 100.5);
    }
}
