use parking_lot::RwLock;
use std::cell::Cell;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use crate::config::Config;
use crate::renderer::PaneViewport;
use crate::terminal::pty::Pty;
use crate::terminal::TerminalState;

pub type PaneId = u32;

/// Distribute `total` among entries by weight, giving minimized entries zero.
/// Minimized panes take no layout space at all — they are reachable through
/// the pane switcher and the status-bar minimized counter instead.
pub fn distribute_visible(weights: &[f32], minimized: &[bool], total: f32) -> Vec<f32> {
    let visible_sum: f32 = weights
        .iter()
        .zip(minimized.iter())
        .filter(|&(_, &m)| !m)
        .map(|(w, _)| w)
        .sum();
    let visible_count = minimized.iter().filter(|&&m| !m).count();
    weights
        .iter()
        .zip(minimized.iter())
        .map(|(w, &m)| {
            if m {
                0.0
            } else if visible_sum > 0.0 {
                total * w / visible_sum
            } else {
                total / visible_count.max(1) as f32
            }
        })
        .collect()
}

/// Per-pane open-latency instrumentation. Splits the new-pane critical path so
/// "time to rectangle" (pane visible) and "time to prompt" (shell usable) can be
/// attributed separately — see notes/pane-open-perf.md.
///
/// The reference instant `entry` is captured at the very start of `Pane::spawn`.
/// The pre-spawn work in the split handlers (viewport float math, no syscalls)
/// is sub-microsecond and folded in. `entry` is set once and only read after,
/// so it is safe to share with the PTY reader thread, which records `shell-ready`.
/// Each milestone logs at most once (atomic-guarded) under the `PANE-OPEN` prefix;
/// grep the log for the full per-pane timeline.
pub struct PaneOpenTimer {
    entry: std::time::Instant,
    inserted_logged: AtomicBool,
    paint_logged: AtomicBool,
    ready_logged: AtomicBool,
}

impl PaneOpenTimer {
    pub fn new() -> Self {
        PaneOpenTimer {
            entry: std::time::Instant::now(),
            inserted_logged: AtomicBool::new(false),
            paint_logged: AtomicBool::new(false),
            ready_logged: AtomicBool::new(false),
        }
    }

    fn elapsed_ms(&self) -> f64 {
        self.entry.elapsed().as_secs_f64() * 1e3
    }

    /// Tree mutation done: the new pane now exists in the layout (main thread).
    /// Logged only for interactive splits (the split handlers call this);
    /// restore/initial panes never reach it, so its absence flags a restore.
    pub fn mark_inserted(&self, pane_id: PaneId) {
        if self.inserted_logged.swap(true, Ordering::Relaxed) {
            return;
        }
        log::info!("PANE-OPEN id={} tree-inserted +{:.1}ms", pane_id, self.elapsed_ms());
    }

    /// First frame the pane is submitted to the renderer = pane becomes visible
    /// (its loading overlay paints). This is the "time to rectangle".
    pub fn mark_first_paint(&self, pane_id: PaneId) {
        if self.paint_logged.swap(true, Ordering::Relaxed) {
            return;
        }
        log::info!("PANE-OPEN id={} first-paint +{:.1}ms (time-to-rectangle)", pane_id, self.elapsed_ms());
    }

    /// Shell emitted its first byte (first prompt) = shell is usable. This is
    /// the "time to prompt". Called from the PTY reader thread.
    pub fn mark_shell_ready(&self, pane_id: PaneId) {
        if self.ready_logged.swap(true, Ordering::Relaxed) {
            return;
        }
        log::info!("PANE-OPEN id={} shell-ready +{:.1}ms (time-to-prompt)", pane_id, self.elapsed_ms());
    }
}

impl Default for PaneOpenTimer {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum SplitDirection {
    Horizontal, // side by side (left | right)
    Vertical,   // stacked (top / bottom)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SplitAxis {
    Horizontal, // resize left/right
    Vertical,   // resize up/down
}

/// Info about a separator line, used for mouse hit-testing and dragging.
#[derive(Clone, Copy)]
pub struct SeparatorInfo {
    /// Pixel position of the separator line (x for column sep, y for row sep).
    pub pos: f32,
    /// Start of the separator extent on the cross-axis.
    pub cross_start: f32,
    /// End of the separator extent on the cross-axis.
    pub cross_end: f32,
    /// Whether this is a column separator (vertical line between columns).
    pub is_column_sep: bool,
    /// Parent dimension along the split axis (width for column, height for row).
    pub parent_dim: f32,
    /// Column separator index: Some(i) means separator between columns[i] and columns[i+1].
    pub column_sep_index: Option<usize>,
    /// Index of the column this separator belongs to.
    pub col_index: usize,
    /// Row separator index within the column: Some(i) means separator between panes[i] and panes[i+1].
    pub row_sep_index: Option<usize>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NavDirection {
    Left,
    Right,
    Up,
    Down,
}


pub type TabId = u32;

static NEXT_PANE_ID: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(1);
static NEXT_TAB_ID: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(1);

fn alloc_pane_id() -> PaneId {
    NEXT_PANE_ID.fetch_add(1, Ordering::Relaxed)
}

pub(crate) fn alloc_tab_id() -> TabId {
    NEXT_TAB_ID.fetch_add(1, Ordering::Relaxed)
}

/// A tab: owns a flat list of columns and tracks which pane is focused.
#[allow(dead_code)]
pub struct Tab {
    pub id: TabId,
    pub columns: Vec<Column>,
    pub column_weights: Vec<f32>,
    /// true = column was manually resized ("pinned"), keeps its weight during redistribution.
    pub custom_weights: Vec<bool>,
    pub focused_pane: PaneId,
    pub custom_title: Option<String>,
    /// Index into TAB_COLORS palette, None = default bg.
    pub color: Option<usize>,
    /// Bell received on a non-focused tab — show attention indicator.
    pub has_bell: bool,
    /// Command completed in a non-focused pane/tab — show completion indicator.
    pub has_completion: bool,
    /// A command is running in any pane of the tab (OSC 133;C/D or a
    /// foreground process other than the shell, e.g. claude, vim).
    pub has_running: bool,
    /// Cached "a pane has a foreground process" result — tcgetpgrp is an
    /// ioctl per pane, so it's refreshed on a throttle, not every tick.
    pub fg_running_cache: bool,
    /// FILO stack of minimized pane IDs.
    pub minimized_stack: Vec<PaneId>,
    /// Horizontal scroll offset in pixels (0 = no scroll).
    pub scroll_offset_x: f32,
    /// Manual override of virtual width (0.0 = auto from min_split_width).
    pub virtual_width_override: f32,
    /// Cell height in pixels, used to snap row heights to cell boundaries.
    /// Set by the window before layout; 0.0 = no snapping.
    pub cell_h: Cell<f32>,
}

/// Rewrite column weights so that, after a horizontal split performed while
/// already scrolling, every existing column keeps its pre-split pixel width and
/// the just-inserted column (at `new_col_idx`) gets `new_col_px`. Weights are
/// stored in pixel units (Tab::column_widths normalizes by their sum), so the
/// returned value — the new virtual width, equal to the sum of the desired pixel
/// widths — reproduces those widths exactly when used as the override.
///
/// Returns `None` (no change) if the index is out of range or the pre-split
/// weight sum is non-positive.
fn reweight_for_scrolled_split(
    weights: &mut [f32],
    new_col_idx: usize,
    old_virtual: f32,
    new_col_px: f32,
) -> Option<f32> {
    if new_col_idx >= weights.len() { return None; }
    // Old weight sum excludes the just-inserted column, so dividing by it
    // reproduces each existing column's pre-split pixel width.
    let old_sum: f32 = weights.iter().enumerate()
        .filter(|&(i, _)| i != new_col_idx)
        .map(|(_, w)| *w)
        .sum();
    if old_sum <= 0.0 { return None; }
    for (i, w) in weights.iter_mut().enumerate() {
        *w = if i == new_col_idx { new_col_px } else { *w / old_sum * old_virtual };
    }
    Some(old_virtual + new_col_px)
}

/// New virtual-width override after a column of `col_px` pixels became fully
/// hidden: the virtual space shrinks by exactly that width so the remaining
/// columns keep their pixel sizes, floored at the screen width (`0.0` means
/// "no override": the layout is screen-sized again).
fn shrink_virtual_for_hidden_column(old_virtual: f32, col_px: f32, screen: f32) -> f32 {
    let new_vw = old_virtual - col_px;
    if new_vw > screen { new_vw } else { 0.0 }
}

/// New virtual-width override after a fully-hidden column becomes visible
/// again: the virtual space grows by the pixel share the column takes
/// (`w_col` vs the `w_others` weight sum of the other visible columns), so
/// those columns keep their exact pixel sizes. Inverse of
/// `shrink_virtual_for_hidden_column` when weights are unchanged.
fn grow_virtual_for_restored_column(w_col: f32, w_others: f32, old_virtual: f32, screen: f32) -> f32 {
    let new_vw = old_virtual * (w_others + w_col) / w_others;
    if new_vw > screen { new_vw } else { 0.0 }
}

impl Tab {
    /// Create a new tab with a single pane.
    pub fn new(config: &Config) -> Result<Self, Box<dyn std::error::Error>> {
        let pane = Pane::spawn(config.terminal.columns, config.terminal.rows, config, None)?;
        let focused = pane.id;
        Ok(Tab {
            id: alloc_tab_id(),
            columns: vec![Column::new(pane)],
            column_weights: vec![1.0],
            custom_weights: vec![false],
            focused_pane: focused,
            custom_title: None,
            color: None,
            has_bell: false,
            has_completion: false,
            has_running: false,
            fg_running_cache: false,
            minimized_stack: Vec::new(),
            scroll_offset_x: 0.0,
            virtual_width_override: 0.0,
            cell_h: Cell::new(0.0),
        })
    }

    /// Create a placeholder tab with a dummy pane (no shell process).
    /// Used for deferred tab restore to avoid shell contention at startup.
    pub fn placeholder(config: &Config) -> Result<Self, Box<dyn std::error::Error>> {
        let pane = Pane::placeholder(config.terminal.columns, config.terminal.rows, config)?;
        let focused = pane.id;
        Ok(Tab {
            id: alloc_tab_id(),
            columns: vec![Column::new(pane)],
            column_weights: vec![1.0],
            custom_weights: vec![false],
            focused_pane: focused,
            custom_title: None,
            color: None,
            has_bell: false,
            has_completion: false,
            has_running: false,
            fg_running_cache: false,
            minimized_stack: Vec::new(),
            scroll_offset_x: 0.0,
            virtual_width_override: 0.0,
            cell_h: Cell::new(0.0),
        })
    }

    /// Create a new tab inheriting the CWD from another pane.
    pub fn new_with_cwd(config: &Config, cwd: Option<&str>) -> Result<Self, Box<dyn std::error::Error>> {
        let pane = Pane::spawn(config.terminal.columns, config.terminal.rows, config, cwd)?;
        let focused = pane.id;
        Ok(Tab {
            id: alloc_tab_id(),
            columns: vec![Column::new(pane)],
            column_weights: vec![1.0],
            custom_weights: vec![false],
            focused_pane: focused,
            custom_title: None,
            color: None,
            has_bell: false,
            has_completion: false,
            has_running: false,
            fg_running_cache: false,
            minimized_stack: Vec::new(),
            scroll_offset_x: 0.0,
            virtual_width_override: 0.0,
            cell_h: Cell::new(0.0),
        })
    }

    /// Compute the virtual width for this tab's split layout.
    /// If a manual override is set, use it. Otherwise: max(screen_width, visible columns * min_split_width).
    /// Fully-minimized columns take no space, so they don't extend the virtual width.
    pub fn virtual_width(&self, screen_width: f32, min_split_width: f32) -> f32 {
        if self.virtual_width_override > 0.0 {
            self.virtual_width_override.max(screen_width)
        } else {
            let n = self.num_visible_columns() as f32;
            (n * min_split_width).max(screen_width)
        }
    }

    /// Scale virtual_width_override proportionally when column count changes (e.g. pane close).
    pub fn scale_virtual_width(&mut self, old_columns: usize, new_columns: usize) {
        if self.virtual_width_override > 0.0 && old_columns > 0 {
            self.virtual_width_override *= new_columns as f32 / old_columns as f32;
        }
    }

    /// Clamp scroll_offset_x after a tree change.
    pub fn clamp_scroll(&mut self, screen_width: f32, min_split_width: f32) {
        let vw = self.virtual_width(screen_width, min_split_width);
        let max_scroll = (vw - screen_width).max(0.0);
        self.scroll_offset_x = self.scroll_offset_x.clamp(0.0, max_scroll);
    }

    /// Adjust scroll_offset_x so that the given pane viewport is fully visible.
    /// `pane_vp` is in virtual-space coordinates (from panes_viewport_for_tab).
    pub fn scroll_to_reveal(&mut self, pane_vp: &PaneViewport, screen_width: f32) {
        let pane_left = pane_vp.x + self.scroll_offset_x;
        let pane_right = pane_left + pane_vp.width;
        if pane_left < self.scroll_offset_x {
            self.scroll_offset_x = pane_left;
        } else if pane_right > self.scroll_offset_x + screen_width {
            self.scroll_offset_x = pane_right - screen_width;
        }
    }

    /// Title for this tab: custom title if set, then focused pane's display title, or "shell".
    pub fn title(&self) -> String {
        if let Some(ref custom) = self.custom_title {
            return custom.clone();
        }
        if let Some(pane) = self.pane(self.focused_pane) {
            return pane.display_title("shell");
        }
        "shell".to_string()
    }

    /// Accumulate pane bell flags into the tab-level flag. The per-pane flag
    /// is NOT consumed here — it stays set until the pane gets focus (cleared
    /// in the render loop) so the pane-level dot survives across frames.
    /// Returns true if this tab needs attention.
    pub fn check_bell(&mut self) -> bool {
        let mut any_bell = false;
        for col in &self.columns {
            col.for_each_pane(&mut |pane| {
                if pane.terminal.read().bell.load(std::sync::atomic::Ordering::Relaxed) {
                    any_bell = true;
                }
            });
        }
        if any_bell {
            self.has_bell = true;
        }
        self.has_bell
    }

    /// Clear the bell/attention flag (call when switching to this tab).
    pub fn clear_bell(&mut self) {
        self.has_bell = false;
    }

    /// Check if any non-focused pane has a completed command. Sets tab-level flag.
    pub fn check_completion(&mut self) -> bool {
        let focused = self.focused_pane;
        let mut any = false;
        self.for_each_pane(&mut |pane| {
            if pane.id != focused
                && pane.terminal.read().command_completed.load(std::sync::atomic::Ordering::Relaxed)
            {
                any = true;
            }
        });
        self.has_completion = any;
        self.has_completion
    }

    /// Check if any pane in the tab is running a command. Two sources, OR'd:
    /// - OSC 133;C/D from shell integration (precise prompt cycles) — note
    ///   that Claude Code emits a 133;D at the end of each of its turns, so
    ///   this flag alone dies while claude is still open;
    /// - a foreground process group other than the shell (tcgetpgrp) — covers
    ///   claude, vim, any TUI, no shell integration needed. Only re-probed
    ///   when `refresh_fg` is true (one ioctl per pane).
    /// Unlike completion, the focused pane counts too: the indicator says
    /// "something occupies this tab". Panes whose shell exited are skipped —
    /// a shell killed mid-command never emits 133;D, which would strand the
    /// OSC flag.
    pub fn check_running(&mut self, refresh_fg: bool) -> bool {
        let mut osc_any = false;
        let mut fg_any = false;
        self.for_each_pane(&mut |pane| {
            if !pane.is_alive() {
                return;
            }
            if pane.terminal.read().command_running.load(std::sync::atomic::Ordering::Relaxed) {
                osc_any = true;
            }
            if refresh_fg && pane.pty.has_foreground_process() {
                fg_any = true;
            }
        });
        if refresh_fg {
            self.fg_running_cache = fg_any;
        }
        self.has_running = osc_any || self.fg_running_cache;
        self.has_running
    }

    /// Minimize the pane with given id. Refuses if it's the last non-minimized pane.
    pub fn minimize_pane(&mut self, id: PaneId) -> bool {
        // Count non-minimized panes
        let mut non_minimized = 0;
        self.for_each_pane(&mut |p| {
            if !p.minimized { non_minimized += 1; }
        });
        if non_minimized <= 1 {
            return false; // can't minimize the last visible pane
        }
        if let Some(pane) = self.pane_mut(id) {
            if pane.minimized {
                return false; // already minimized
            }
            pane.minimized = true;
            self.minimized_stack.push(id);
            // Move focus to a non-minimized sibling
            if self.focused_pane == id {
                let mut first_non_minimized = None;
                self.for_each_pane(&mut |p| {
                    if !p.minimized && first_non_minimized.is_none() {
                        first_non_minimized = Some(p.id);
                    }
                });
                if let Some(new_focus) = first_non_minimized {
                    self.focused_pane = new_focus;
                }
            }
            true
        } else {
            false
        }
    }

    /// Restore a specific minimized pane.
    pub fn restore_pane(&mut self, id: PaneId) {
        if let Some(pane) = self.pane_mut(id) {
            pane.minimized = false;
        }
        self.minimized_stack.retain(|&pid| pid != id);
    }

    /// Restore the last minimized pane (FILO), adjusting the virtual space.
    pub fn restore_last_minimized(&mut self, screen_width: f32, min_split_width: f32) -> bool {
        if let Some(id) = self.minimized_stack.last().copied() {
            self.restore_pane_adjust_virtual(id, screen_width, min_split_width);
            true
        } else {
            false
        }
    }

    /// Minimize pane `id` and adjust the virtual space: while the tab is
    /// scrolling (virtual width > screen), a column that becomes fully hidden
    /// gives its pixel width back to the virtual space, so the remaining
    /// panes keep their exact sizes — never shrinking below the screen width.
    /// When not scrolling, the visible panes simply reshare the screen.
    pub fn minimize_pane_adjust_virtual(&mut self, id: PaneId, screen_width: f32, min_split_width: f32) -> bool {
        let old_vw = self.virtual_width(screen_width, min_split_width);
        let col_px = self
            .column_index_of(id)
            .and_then(|ci| self.column_widths(old_vw).get(ci).copied());
        if !self.minimize_pane(id) {
            return false;
        }
        if old_vw > screen_width {
            if let (Some(ci), Some(px)) = (self.column_index_of(id), col_px) {
                if self.columns[ci].is_fully_minimized() && px > 0.0 {
                    self.virtual_width_override =
                        shrink_virtual_for_hidden_column(old_vw, px, screen_width);
                }
            }
        }
        true
    }

    /// Restore pane `id` and adjust the virtual space: while the tab is
    /// scrolling, a fully-hidden column coming back grows the virtual width
    /// by the share it takes, so the already-visible panes keep their sizes.
    pub fn restore_pane_adjust_virtual(&mut self, id: PaneId, screen_width: f32, min_split_width: f32) {
        let old_vw = self.virtual_width(screen_width, min_split_width);
        let hidden_col = self
            .column_index_of(id)
            .filter(|&ci| self.columns[ci].is_fully_minimized());
        self.restore_pane(id);
        if let Some(ci) = hidden_col {
            if old_vw > screen_width {
                let mut w_col = self.column_weights.get(ci).copied().unwrap_or(0.0);
                let mut w_others: f32 = self
                    .column_weights
                    .iter()
                    .enumerate()
                    .filter(|&(i, _)| i != ci && !self.columns[i].is_fully_minimized())
                    .map(|(_, &w)| w)
                    .sum();
                if w_col <= 0.0 || w_others <= 0.0 {
                    // Degenerate weights: fall back to equal shares.
                    let others = self.num_visible_columns().saturating_sub(1);
                    if others == 0 {
                        return;
                    }
                    w_col = 1.0;
                    w_others = others as f32;
                }
                self.virtual_width_override =
                    grow_virtual_for_restored_column(w_col, w_others, old_vw, screen_width);
            }
        }
    }

    /// First non-minimized pane id, if any.
    pub fn first_visible_pane(&self) -> Option<PaneId> {
        let mut found = None;
        self.for_each_pane(&mut |p| {
            if !p.minimized && found.is_none() {
                found = Some(p.id);
            }
        });
        found
    }

    /// If no visible (non-minimized) pane remains, restore the most recently
    /// minimized one (FILO) and return its id. Used after closing the last
    /// visible pane so the tab never ends up showing nothing.
    pub fn ensure_visible_pane(&mut self) -> Option<PaneId> {
        if self.first_visible_pane().is_some() {
            return None;
        }
        let id = self
            .minimized_stack
            .last()
            .copied()
            .unwrap_or_else(|| self.first_pane().id);
        self.restore_pane(id);
        Some(id)
    }

    /// Rebuild minimized_stack from the columns (depth-first order). Used after session restore.
    pub fn rebuild_minimized_stack(&mut self) {
        self.minimized_stack.clear();
        let mut ids = Vec::new();
        for col in &self.columns {
            col.for_each_pane(&mut |p| {
                if p.minimized {
                    ids.push(p.id);
                }
            });
        }
        self.minimized_stack = ids;
    }

    /// Clear the completion flag (call when switching to this tab).
    pub fn clear_completion(&mut self) {
        self.has_completion = false;
        // Also clear all pane-level flags
        self.for_each_pane(&mut |pane| {
            pane.terminal.read().command_completed.store(false, std::sync::atomic::Ordering::Relaxed);
        });
    }

    // ---------------------------------------------------------------
    // Pane lookup (delegated to columns)
    // ---------------------------------------------------------------

    /// Find a pane by id across all columns.
    pub fn pane(&self, id: PaneId) -> Option<&Pane> {
        for col in &self.columns {
            if let Some(p) = col.pane(id) {
                return Some(p);
            }
        }
        None
    }

    /// Find a mutable pane by id across all columns.
    pub fn pane_mut(&mut self, id: PaneId) -> Option<&mut Pane> {
        for col in &mut self.columns {
            if let Some(p) = col.pane_mut(id) {
                return Some(p);
            }
        }
        None
    }

    /// Check if any column contains a pane with the given id.
    pub fn contains(&self, id: PaneId) -> bool {
        self.columns.iter().any(|col| col.contains(id))
    }

    /// Return the first (leftmost/topmost) pane.
    pub fn first_pane(&self) -> &Pane {
        self.columns.first().unwrap().first_pane()
    }

    /// Return the last (rightmost/bottommost) pane.
    pub fn last_pane(&self) -> &Pane {
        self.columns.last().unwrap().last_pane()
    }

    /// Iterate over all panes (depth-first, left to right).
    pub fn for_each_pane<F: FnMut(&Pane)>(&self, f: &mut F) {
        for col in &self.columns {
            col.for_each_pane(f);
        }
    }

    /// Mark all panes as dirty (needs redraw).
    pub fn mark_all_dirty(&self) {
        self.for_each_pane(&mut |p| {
            p.terminal.read().dirty.store(true, std::sync::atomic::Ordering::Relaxed);
        });
    }

    /// Collect ids of all panes whose shell has exited.
    pub fn exited_pane_ids(&self) -> Vec<PaneId> {
        let mut ids = Vec::new();
        self.for_each_pane(&mut |p| {
            if !p.is_alive() {
                ids.push(p.id);
            }
        });
        ids
    }

    /// Return the 0-based column index containing the pane with given id.
    pub fn column_index_of(&self, id: PaneId) -> Option<usize> {
        self.columns.iter().position(|col| col.contains(id))
    }

    /// Return the number of columns.
    pub fn num_columns(&self) -> usize {
        self.columns.len()
    }

    // ---------------------------------------------------------------
    // Viewport computation
    // ---------------------------------------------------------------

    /// Compute column widths from weights and total width.
    /// Fully-minimized columns take zero width (no layout footprint).
    fn column_widths(&self, total_width: f32) -> Vec<f32> {
        let minimized: Vec<bool> = self.columns.iter()
            .map(|col| col.is_fully_minimized())
            .collect();
        distribute_visible(&self.column_weights, &minimized, total_width)
    }

    /// Number of columns that occupy layout space (not fully minimized).
    pub fn num_visible_columns(&self) -> usize {
        self.columns.iter().filter(|c| !c.is_fully_minimized()).count()
    }

    /// 1-based index of the pane's column among visible columns (for status bar).
    pub fn visible_column_index(&self, id: PaneId) -> Option<usize> {
        let idx = self.column_index_of(id)?;
        Some(
            self.columns[..idx]
                .iter()
                .filter(|c| !c.is_fully_minimized())
                .count()
                + 1,
        )
    }

    /// Count minimized panes across all columns.
    pub fn count_minimized(&self) -> usize {
        let mut n = 0;
        self.for_each_pane(&mut |p| if p.minimized { n += 1 });
        n
    }

    /// Walk columns, computing viewports for each pane.
    pub fn for_each_pane_with_viewport<F: FnMut(&Pane, PaneViewport)>(&self, vp: PaneViewport, f: &mut F) {
        let widths = self.column_widths(vp.width);
        let ch = self.cell_h.get();
        let mut x = vp.x;
        for (col, &w) in self.columns.iter().zip(widths.iter()) {
            let col_vp = PaneViewport { x, y: vp.y, width: w, height: vp.height };
            col.for_each_pane_with_viewport(col_vp, ch, f);
            x += w;
        }
    }

    /// Compute the viewport for a specific pane by id.
    pub fn viewport_for_pane(&self, id: PaneId, vp: PaneViewport) -> Option<PaneViewport> {
        let widths = self.column_widths(vp.width);
        let ch = self.cell_h.get();
        let mut x = vp.x;
        for (col, &w) in self.columns.iter().zip(widths.iter()) {
            let col_vp = PaneViewport { x, y: vp.y, width: w, height: vp.height };
            if let Some(result) = col.viewport_for_pane(id, col_vp, ch) {
                return Some(result);
            }
            x += w;
        }
        None
    }

    /// Hit-test: find which pane contains the pixel coordinate (x, y).
    pub fn hit_test(&self, x: f32, y: f32, vp: PaneViewport) -> Option<(&Pane, PaneViewport)> {
        let widths = self.column_widths(vp.width);
        let ch = self.cell_h.get();
        let mut col_x = vp.x;
        for (col, &w) in self.columns.iter().zip(widths.iter()) {
            let col_vp = PaneViewport { x: col_x, y: vp.y, width: w, height: vp.height };
            if let Some(result) = col.hit_test(x, y, col_vp, ch) {
                return Some(result);
            }
            col_x += w;
        }
        None
    }

    // ---------------------------------------------------------------
    // Separator collection
    // ---------------------------------------------------------------

    /// Collect separator lines between splits as (x1, y1, x2, y2) segments.
    /// Fully-minimized columns are zero-width: no separator is drawn for them
    /// (a visible column draws one only when a visible column precedes it).
    pub fn collect_separators(&self, vp: PaneViewport, out: &mut Vec<(f32, f32, f32, f32)>) {
        let widths = self.column_widths(vp.width);
        let ch = self.cell_h.get();
        let mut x = vp.x;
        let mut seen_visible = false;
        for (col, &w) in self.columns.iter().zip(widths.iter()) {
            let col_vp = PaneViewport { x, y: vp.y, width: w, height: vp.height };
            if !col.is_fully_minimized() {
                // Vertical separator between this visible column and the previous one
                if seen_visible {
                    out.push((x, vp.y, x, vp.y + vp.height));
                }
                seen_visible = true;
                // Horizontal separators within column
                col.collect_separators(col_vp, ch, out);
            }
            x += w;
        }
    }

    /// Collect separator info for mouse hit-testing and dragging.
    pub fn collect_separator_info(&self, vp: PaneViewport, out: &mut Vec<SeparatorInfo>) {
        let widths = self.column_widths(vp.width);
        let ch = self.cell_h.get();
        let mut x = vp.x;
        for (i, (col, &w)) in self.columns.iter().zip(widths.iter()).enumerate() {
            let col_vp = PaneViewport { x, y: vp.y, width: w, height: vp.height };
            // Column separator between columns[i-1] and columns[i]
            // Block resize when either adjacent column is fully minimized
            if i > 0 && !self.columns[i - 1].is_fully_minimized() && !col.is_fully_minimized() {
                out.push(SeparatorInfo {
                    pos: x,
                    cross_start: vp.y,
                    cross_end: vp.y + vp.height,
                    is_column_sep: true,
                    parent_dim: vp.width,
                    column_sep_index: Some(i - 1),
                    col_index: i,
                    row_sep_index: None,
                });
            }
            // Row separators within column
            col.collect_separator_info(i, col_vp, ch, out);
            x += w;
        }
    }

    // ---------------------------------------------------------------
    // Navigation
    // ---------------------------------------------------------------

    /// Find the neighbor pane in the given direction from the pane with `id`.
    pub fn neighbor(&self, id: PaneId, dir: NavDirection, total_vp: PaneViewport) -> Option<PaneId> {
        // Collect all non-minimized panes with their viewports
        let mut panes: Vec<(PaneId, PaneViewport)> = Vec::new();
        self.for_each_pane_with_viewport(total_vp, &mut |p, vp| {
            if !p.minimized {
                panes.push((p.id, vp));
            }
        });

        let source = panes.iter().find(|(pid, _)| *pid == id)?;
        let (_, src_vp) = source;
        let src_cx = src_vp.x + src_vp.width / 2.0;
        let src_cy = src_vp.y + src_vp.height / 2.0;

        let mut best_overlap: Option<(PaneId, f32)> = None;
        let mut best_fallback: Option<(PaneId, f32)> = None;
        for &(pid, ref vp) in &panes {
            if pid == id { continue; }
            let cx = vp.x + vp.width / 2.0;
            let cy = vp.y + vp.height / 2.0;

            let valid = match dir {
                NavDirection::Left => cx < src_cx,
                NavDirection::Right => cx > src_cx,
                NavDirection::Up => cy < src_cy,
                NavDirection::Down => cy > src_cy,
            };
            if !valid { continue; }

            let overlaps = match dir {
                NavDirection::Left | NavDirection::Right => {
                    let s_top = src_vp.y;
                    let s_bot = src_vp.y + src_vp.height;
                    let c_top = vp.y;
                    let c_bot = vp.y + vp.height;
                    s_top < c_bot && c_top < s_bot
                }
                NavDirection::Up | NavDirection::Down => {
                    let s_left = src_vp.x;
                    let s_right = src_vp.x + src_vp.width;
                    let c_left = vp.x;
                    let c_right = vp.x + vp.width;
                    s_left < c_right && c_left < s_right
                }
            };

            let main_dist = match dir {
                NavDirection::Left | NavDirection::Right => (cx - src_cx).abs(),
                NavDirection::Up | NavDirection::Down => (cy - src_cy).abs(),
            };

            if overlaps {
                if best_overlap.map_or(true, |(_, d)| main_dist < d) {
                    best_overlap = Some((pid, main_dist));
                }
            } else {
                let dist = (cx - src_cx).abs() + (cy - src_cy).abs();
                if best_fallback.map_or(true, |(_, d)| dist < d) {
                    best_fallback = Some((pid, dist));
                }
            }
        }
        best_overlap.or(best_fallback).map(|(pid, _)| pid)
    }

    // ---------------------------------------------------------------
    // Split operations
    // ---------------------------------------------------------------

    /// Insert a new column after the column containing the focused pane.
    /// Returns the new pane's id.
    pub fn insert_column_after_focused(&mut self, new_pane: Pane) -> PaneId {
        let new_id = new_pane.id;
        let idx = self.column_index_of(self.focused_pane).unwrap_or(self.columns.len() - 1);
        let avg_weight: f32 = self.column_weights.iter().sum::<f32>() / self.columns.len() as f32;
        self.columns.insert(idx + 1, Column::new(new_pane));
        self.column_weights.insert(idx + 1, avg_weight);
        self.custom_weights.insert(idx + 1, false);
        new_id
    }

    /// Append a new column at the end.
    /// Returns the new pane's id.
    pub fn append_column(&mut self, new_pane: Pane) -> PaneId {
        let new_id = new_pane.id;
        let avg_weight: f32 = self.column_weights.iter().sum::<f32>() / self.columns.len() as f32;
        self.columns.push(Column::new(new_pane));
        self.column_weights.push(avg_weight);
        self.custom_weights.push(false);
        new_id
    }

    /// Split the pane with target_id vertically (insert new pane below it within its column).
    pub fn vsplit_at_pane(&mut self, target_id: PaneId, new_pane: Pane) {
        if let Some(idx) = self.column_index_of(target_id) {
            self.columns[idx].insert_pane_after(target_id, new_pane);
        }
    }

    /// Split at the bottom of the column containing the focused pane.
    /// Appends the new pane at the bottom of the column.
    pub fn vsplit_root_at_column(&mut self, new_pane: Pane) {
        let focused_id = self.focused_pane;
        if let Some(idx) = self.column_index_of(focused_id) {
            self.columns[idx].append_pane(new_pane);
        }
    }

    // ---------------------------------------------------------------
    // Remove pane
    // ---------------------------------------------------------------

    /// Remove a pane by id. Returns true if the tab still has panes.
    /// Returns false if the tab became empty (caller should close it).
    pub fn remove_pane(&mut self, id: PaneId) -> bool {
        if let Some(col_idx) = self.column_index_of(id) {
            if self.columns[col_idx].panes.len() == 1 {
                // Sole pane in column — remove entire column
                self.columns.remove(col_idx);
                let removed_weight = self.column_weights.remove(col_idx);
                self.custom_weights.remove(col_idx);
                if self.columns.is_empty() {
                    return false;
                }
                // Redistribute weight proportionally
                let sum: f32 = self.column_weights.iter().sum();
                if sum > 0.0 {
                    let scale = (sum + removed_weight) / sum;
                    for w in &mut self.column_weights {
                        *w *= scale;
                    }
                }
            } else {
                // Multi-pane column — remove pane within column
                self.columns[col_idx].remove_pane(id);
            }
            true
        } else {
            true // pane not found, nothing to remove
        }
    }

    /// Extract a pane by id, returning it separately. The tab retains the remainder.
    pub fn extract_pane(&mut self, id: PaneId) -> Option<Pane> {
        let col_idx = self.column_index_of(id)?;

        if self.columns[col_idx].panes.len() == 1 {
            // Sole pane in column — remove entire column, return the pane
            let col = self.columns.remove(col_idx);
            let removed_weight = self.column_weights.remove(col_idx);
            self.custom_weights.remove(col_idx);
            if !self.columns.is_empty() {
                let sum: f32 = self.column_weights.iter().sum();
                if sum > 0.0 {
                    let scale = (sum + removed_weight) / sum;
                    for w in &mut self.column_weights {
                        *w *= scale;
                    }
                }
            }
            col.panes.into_iter().next()
        } else {
            // Multi-pane column — extract pane from within
            self.columns[col_idx].extract_pane(id)
        }
    }

    // ---------------------------------------------------------------
    // Resize
    // ---------------------------------------------------------------

    /// Adjust split ratio directionally.
    pub fn adjust_ratio_directional(&mut self, id: PaneId, delta: f32, axis: SplitAxis) -> bool {
        match axis {
            SplitAxis::Horizontal => {
                // Horizontal resize: adjust column weights
                self.adjust_column_weight_directional(id, delta)
            }
            SplitAxis::Vertical => {
                // Vertical resize: delegate to column's row weights
                if let Some(col_idx) = self.column_index_of(id) {
                    self.columns[col_idx].adjust_row_weight_directional(id, delta)
                } else {
                    false
                }
            }
        }
    }

    /// Fallback: adjust nearest separator. Directional already handles all cases for flat columns.
    pub fn adjust_ratio_nearest(&mut self, id: PaneId, _delta: f32, axis: SplitAxis) -> bool {
        match axis {
            SplitAxis::Horizontal => false,
            SplitAxis::Vertical => {
                // Flat columns: directional handles all cases
                let _ = id;
                false
            }
        }
    }

    /// Adjust column weight by moving the controlled edge of the focused column.
    /// Controlled edge = right edge, except for the last column (left edge).
    /// delta > 0 (Right): push edge rightward.  delta < 0 (Left): push edge leftward.
    /// The focused column becomes pinned.
    fn adjust_column_weight_directional(&mut self, id: PaneId, delta: f32) -> bool {
        let col_idx = match self.column_index_of(id) {
            Some(i) => i,
            None => return false,
        };
        if self.columns.len() < 2 { return false; }

        let is_last = col_idx == self.columns.len() - 1;
        let weight_sum: f32 = self.column_weights.iter().sum();
        let step = delta.abs() * 0.5; // scale down for weight transfer

        // Determine if focused column grows or shrinks.
        // Non-last: right edge. Right (delta>0) = grow, Left (delta<0) = shrink.
        // Last: left edge. Right (delta>0) = shrink, Left (delta<0) = grow.
        let growing = if is_last { delta < 0.0 } else { delta > 0.0 };

        // The "outer side" is where the other columns are (relative to the controlled edge).
        // Non-last: outer = right side (col_idx+1..)
        // Last: outer = left side (0..col_idx)
        let (outer_range, outer_fallback): (std::ops::Range<usize>, usize) = if is_last {
            (0..col_idx, if col_idx > 0 { col_idx - 1 } else { 0 })
        } else {
            (col_idx + 1..self.columns.len(), col_idx + 1)
        };

        if growing {
            // Focused grows: take weight from outer side
            // Calculate max available transfer from outer non-pinned columns
            let outer_unpinned: Vec<usize> = outer_range.clone()
                .filter(|&i| !self.custom_weights[i])
                .collect();
            let source_indices = if outer_unpinned.is_empty() {
                vec![outer_fallback] // fallback to adjacent
            } else {
                outer_unpinned
            };
            let avail: f32 = source_indices.iter().map(|&i| self.column_weights[i] * 0.8).sum();
            let transfer = (step * weight_sum).min(avail);
            if transfer > 0.001 {
                self.column_weights[col_idx] += transfer;
                self.custom_weights[col_idx] = true;
                self.redistribute_loss(transfer, outer_range, outer_fallback);
                return true;
            }
        } else {
            // Focused shrinks: give weight to outer side
            let transfer = (step * weight_sum).min(self.column_weights[col_idx] * 0.8);
            if transfer > 0.001 {
                self.column_weights[col_idx] -= transfer;
                self.custom_weights[col_idx] = true;
                self.redistribute_weight(transfer, outer_range, outer_fallback);
                return true;
            }
        }
        false
    }



    /// Redistribute `amount` of weight among non-pinned columns in `range`.
    /// If all columns in range are pinned, fallback to `fallback_idx`.
    fn redistribute_weight(&mut self, amount: f32, range: std::ops::Range<usize>, fallback_idx: usize) {
        let unpinned: Vec<usize> = range.clone()
            .filter(|&i| !self.custom_weights[i])
            .collect();
        if unpinned.is_empty() {
            // Fallback: all pinned → adjacent absorbs
            if fallback_idx < self.column_weights.len() {
                self.column_weights[fallback_idx] += amount;
            }
        } else {
            let share = amount / unpinned.len() as f32;
            for &i in &unpinned {
                self.column_weights[i] += share;
            }
        }
    }

    /// Remove `amount` of weight from non-pinned columns in `range` (equally shared).
    /// If all columns in range are pinned, fallback to `fallback_idx`.
    fn redistribute_loss(&mut self, amount: f32, range: std::ops::Range<usize>, fallback_idx: usize) {
        let sum: f32 = self.column_weights.iter().sum();
        let min_weight = sum * 0.05;
        let unpinned: Vec<usize> = range.clone()
            .filter(|&i| !self.custom_weights[i])
            .collect();
        if unpinned.is_empty() {
            if fallback_idx < self.column_weights.len() {
                self.column_weights[fallback_idx] = (self.column_weights[fallback_idx] - amount).max(min_weight);
            }
        } else {
            let share = amount / unpinned.len() as f32;
            for &i in &unpinned {
                self.column_weights[i] = (self.column_weights[i] - share).max(min_weight);
            }
        }
    }

    /// Returns the maximum leaf width as a fraction of total width (0.0–1.0).
    pub fn max_leaf_width_fraction(&self) -> f32 {
        let sum: f32 = self.column_weights.iter().sum();
        if sum <= 0.0 { return 1.0; }
        let mut max_frac = 0.0f32;
        for (col, &w) in self.columns.iter().zip(self.column_weights.iter()) {
            let col_frac = w / sum;
            // Within the column, VSplit doesn't change width
            let leaf_frac = col.max_leaf_width_fraction() * col_frac;
            max_frac = max_frac.max(leaf_frac);
        }
        max_frac
    }

    /// Post-validation: adjust weights so no leaf exceeds `max_w` pixels.
    pub fn clamp_pane_widths(&mut self, total: f32, max_w: f32) {
        let sum: f32 = self.column_weights.iter().sum();
        if sum <= 0.0 { return; }
        for i in 0..self.columns.len() {
            let col_w = total * self.column_weights[i] / sum;
            let col_max = col_w; // flat column: each pane has full column width
            if col_max > max_w && col_max > 0.0 {
                // Scale down the column weight so its widest pane = max_w
                let new_col_w = col_w * max_w / col_max;
                self.column_weights[i] = new_col_w / total * sum;
            }
        }
    }

    /// Scale ratios so that only `target_id` absorbs the size change (edge grow).
    pub fn scale_ratios_for_edge_grow(&mut self, target_id: PaneId, old_total: f32, new_total: f32) {
        if new_total <= 0.0 || old_total <= 0.0 { return; }
        let col_idx = match self.column_index_of(target_id) {
            Some(i) => i,
            None => return,
        };
        let sum: f32 = self.column_weights.iter().sum();
        if sum <= 0.0 { return; }

        // Keep all other columns at their old pixel widths, target absorbs the change
        let others_total_px: f32 = (0..self.columns.len())
            .filter(|&i| i != col_idx)
            .map(|i| self.column_weights[i] / sum * old_total)
            .sum();

        // Target gets the new total minus what others need
        let target_new_w = (new_total - others_total_px).max(1.0);

        // Convert pixel widths to weights (proportional to new_total)
        for i in 0..self.columns.len() {
            if i == col_idx {
                self.column_weights[i] = target_new_w;
            } else {
                self.column_weights[i] = self.column_weights[i] / sum * old_total;
            }
        }
    }

    /// After a horizontal split that happens while already scrolling (virtual
    /// width > screen), grow the virtual width by the new column's width instead
    /// of stealing space from the existing columns. Every existing column keeps
    /// the pixel width it had before the split; the just-inserted column (at
    /// `new_col_idx`) gets `new_col_px`.
    ///
    /// `old_virtual` is the virtual width before the split. Weights are stored in
    /// pixel units here (column_widths normalizes by their sum), so after this
    /// the new override equals the sum of the desired pixel widths and the layout
    /// reproduces them exactly.
    pub fn grow_virtual_for_scrolled_split(
        &mut self,
        new_col_idx: usize,
        old_virtual: f32,
        new_col_px: f32,
        screen: f32,
    ) {
        if new_col_idx >= self.columns.len() { return; }
        if let Some(new_virtual) = reweight_for_scrolled_split(
            &mut self.column_weights, new_col_idx, old_virtual, new_col_px,
        ) {
            self.virtual_width_override = if new_virtual > screen { new_virtual } else { 0.0 };
        }
    }

    /// Set column weights by dragging a column separator.
    /// `col_idx` is the index such that the separator is between columns[col_idx] and columns[col_idx+1].
    ///
    /// Redistribution: the "pushed" column (on the side the separator moves toward) absorbs the
    /// delta directly and becomes pinned. The freed/consumed space is redistributed among all
    /// non-pinned columns on the opposite side. If all opposite columns are pinned, only the
    /// adjacent one absorbs (fallback).
    pub fn set_column_weights_by_drag(&mut self, col_idx: usize, delta_px: f32, total_width: f32) {
        if col_idx + 1 >= self.columns.len() { return; }
        let sum: f32 = self.column_weights.iter().sum();
        if sum <= 0.0 || total_width <= 0.0 { return; }

        let delta_weight = delta_px / total_width * sum;
        let min_weight = sum * 0.05; // minimum 5% of total

        if delta_weight.abs() < 0.001 { return; }

        // Determine pushed side (shrinks) and free side (absorbs).
        // delta > 0 → separator moves right → right column (col_idx+1) is pushed, left side is free.
        // delta < 0 → separator moves left → left column (col_idx) is pushed, right side is free.
        let (pushed_idx, free_range): (usize, std::ops::Range<usize>) = if delta_weight > 0.0 {
            (col_idx + 1, 0..col_idx + 1)
        } else {
            (col_idx, col_idx + 1..self.columns.len())
        };

        let abs_delta = delta_weight.abs();

        // Pushed column shrinks
        let new_pushed = (self.column_weights[pushed_idx] - abs_delta).max(min_weight);
        let actual_delta = self.column_weights[pushed_idx] - new_pushed;
        if actual_delta < 0.001 { return; }

        // Find non-pinned columns on the free side
        let free_unpinned: Vec<usize> = free_range.clone()
            .filter(|&i| !self.custom_weights[i])
            .collect();

        if free_unpinned.is_empty() {
            // Fallback: all pinned on free side → only adjacent absorbs
            let adjacent = if delta_weight > 0.0 { col_idx } else { col_idx + 1 };
            let new_adj = self.column_weights[adjacent] + actual_delta;
            self.column_weights[pushed_idx] = new_pushed;
            self.column_weights[adjacent] = new_adj;
        } else {
            // Redistribute equally among non-pinned columns on the free side
            let share = actual_delta / free_unpinned.len() as f32;
            self.column_weights[pushed_idx] = new_pushed;
            for &i in &free_unpinned {
                self.column_weights[i] += share;
            }
        }

        // Mark pushed column as pinned
        self.custom_weights[pushed_idx] = true;
    }

    /// Swap the focused pane with its neighbor. For Left/Right, swap entire columns.
    pub fn swap_panes(&mut self, id1: PaneId, id2: PaneId, dir: NavDirection) -> bool {
        if id1 == id2 { return false; }
        match dir {
            NavDirection::Left | NavDirection::Right => {
                // Swap entire columns
                let idx1 = match self.column_index_of(id1) { Some(i) => i, None => return false };
                let idx2 = match self.column_index_of(id2) { Some(i) => i, None => return false };
                if idx1 == idx2 {
                    // Same column: swap within VSplit
                    return self.columns[idx1].swap_panes(id1, id2);
                }
                self.columns.swap(idx1, idx2);
                self.column_weights.swap(idx1, idx2);
                self.custom_weights.swap(idx1, idx2);
                true
            }
            NavDirection::Up | NavDirection::Down => {
                // Swap within column's VSplit
                let idx = match self.column_index_of(id1) { Some(i) => i, None => return false };
                self.columns[idx].swap_panes(id1, id2)
            }
        }
    }

    /// Reparent pane: move to adjacent column (Left/Right) or swap within column (Up/Down).
    pub fn reparent_pane(&mut self, focused_id: PaneId, dir: NavDirection) -> bool {
        match dir {
            NavDirection::Left | NavDirection::Right => {
                // Reparent across columns: move pane to adjacent column
                let col_idx = match self.column_index_of(focused_id) { Some(i) => i, None => return false };
                let target_idx = match dir {
                    NavDirection::Left if col_idx > 0 => col_idx - 1,
                    NavDirection::Right if col_idx + 1 < self.columns.len() => col_idx + 1,
                    _ => return false,
                };

                let is_sole_pane = self.columns[col_idx].panes.len() == 1;

                if is_sole_pane {
                    // Single pane column — remove column and append pane to target
                    let col = self.columns.remove(col_idx);
                    let _weight = self.column_weights.remove(col_idx);
                    self.custom_weights.remove(col_idx);
                    let adj_target = if target_idx > col_idx { target_idx - 1 } else { target_idx };
                    // Move the pane into the target column
                    let pane = col.panes.into_iter().next().unwrap();
                    self.columns[adj_target].append_pane(pane);
                } else {
                    // Extract pane from multi-pane column
                    if let Some(pane) = self.columns[col_idx].extract_pane(focused_id) {
                        // Add extracted pane to target column at bottom
                        self.columns[target_idx].append_pane(pane);
                    } else {
                        return false;
                    }
                }
                true
            }
            NavDirection::Up | NavDirection::Down => {
                // Reparent within column
                if let Some(col_idx) = self.column_index_of(focused_id) {
                    self.columns[col_idx].reparent_pane(focused_id, dir)
                } else {
                    false
                }
            }
        }
    }

    /// Equalize: reset all column weights to 1.0 and all VSplit ratios proportionally (by leaf count).
    pub fn equalize(&mut self) {
        for w in &mut self.column_weights {
            *w = 1.0;
        }
        for cw in &mut self.custom_weights {
            *cw = false;
        }
        for col in &mut self.columns {
            col.equalize();
        }
    }

    /// Check if this tab has only a single pane.
    pub fn is_single_pane(&self) -> bool {
        self.columns.len() == 1 && self.columns[0].panes.len() == 1
    }
}

/// A single terminal pane: owns its PTY, terminal state, and per-pane flags.
pub struct Pane {
    pub id: PaneId,
    pub terminal: Arc<RwLock<TerminalState>>,
    pub pty: Pty,
    pub shell_exited: Arc<AtomicBool>,
    pub shell_ready: Arc<AtomicBool>,
    pub scroll_accumulator: Cell<f64>,
    /// Command to inject into PTY once shell is ready (for session restore).
    pub pending_command: Cell<Option<String>>,
    /// Custom pane title set by user (overrides OSC title).
    pub custom_title: Option<String>,
    /// Whether this pane is minimized (collapsed to a thin bar).
    pub minimized: bool,
    /// Open-latency instrumentation (time-to-rectangle / time-to-prompt).
    pub open_timer: Arc<PaneOpenTimer>,
}

/// Resolve the label to show for a pane, in priority order:
/// user-set custom title → non-empty OSC title → cwd basename → `fallback`.
/// An empty or whitespace-only OSC title is treated as absent so we never
/// render a blank row (the "invisible white line" bug in the pane switcher).
/// True if `title` begins with a Claude Code *working* marker: an animated
/// Braille spinner glyph (U+2800–U+28FF) immediately followed by a space.
/// Claude Code prepends this spinner ONLY while it is actively generating or
/// running a tool; at the prompt it shows an asterisk-like idle marker
/// (`✳ Claude Code`) or a plain title instead. So this — and NOT the asterisk —
/// is the reliable "the app is busy" signal.
fn is_working_marker(title: &str) -> bool {
    let mut chars = title.chars();
    matches!(chars.next(), Some(c) if ('\u{2800}'..='\u{28FF}').contains(&c))
        && chars.next() == Some(' ')
}

/// True if `title` begins with any Claude Code status marker followed by a
/// space: the Braille working spinner OR an asterisk-like idle marker
/// (`*`, `✳ ` U+2733, `∗` U+2217). Used to strip the prefix for display so the
/// title neither jitters with the spinner nor carries a bare idle marker.
fn has_leading_marker(title: &str) -> bool {
    let mut chars = title.chars();
    matches!(
        chars.next(),
        Some(c) if ('\u{2800}'..='\u{28FF}').contains(&c)
            || matches!(c, '*' | '\u{2733}' | '\u{2217}')
    ) && chars.next() == Some(' ')
}

/// Strip a leading status marker that Claude Code prepends to the terminal
/// title (Braille working spinner or asterisk-like idle marker), plus its
/// trailing space. Only trims a single such prefix; leaves the rest untouched.
fn strip_activity_prefix(title: &str) -> &str {
    if has_leading_marker(title) {
        // Marker glyph (1–3 bytes) + one ASCII space (1 byte).
        let marker_len = title.chars().next().map_or(0, char::len_utf8);
        &title[marker_len + 1..]
    } else {
        title
    }
}

fn derive_display_title(
    custom_title: Option<&str>,
    osc_title: Option<&str>,
    cwd: Option<&str>,
    fallback: &str,
) -> String {
    if let Some(custom) = custom_title {
        return custom.to_string();
    }
    if let Some(title) = osc_title.filter(|t| !t.trim().is_empty()) {
        return strip_activity_prefix(title).to_string();
    }
    if let Some(cwd) = cwd {
        if let Some(base) = std::path::Path::new(cwd).file_name() {
            return base.to_string_lossy().to_string();
        }
    }
    fallback.to_string()
}

impl Pane {
    pub fn spawn(cols: u16, rows: u16, config: &Config, working_dir: Option<&str>) -> Result<Self, Box<dyn std::error::Error>> {
        // Reference instant for open-latency instrumentation — captured first so
        // it covers the whole spawn (TerminalState alloc + fork/exec + dups).
        let open_timer = Arc::new(PaneOpenTimer::new());
        let id = alloc_pane_id();
        let terminal = Arc::new(RwLock::new(TerminalState::new(
            cols,
            rows,
            config.terminal.scrollback,
            crate::terminal::color_to_u8(config.colors.foreground),
            crate::terminal::color_to_u8(config.colors.background),
        )));
        let shell_exited = Arc::new(AtomicBool::new(false));
        let shell_ready = Arc::new(AtomicBool::new(false));
        let pty = Pty::spawn(
            cols,
            rows,
            terminal.clone(),
            shell_exited.clone(),
            shell_ready.clone(),
            working_dir,
            id,
            open_timer.clone(),
        )?;
        Ok(Pane {
            id,
            terminal,
            pty,
            shell_exited,
            shell_ready,
            scroll_accumulator: Cell::new(0.0),
            pending_command: Cell::new(None),
            custom_title: None,
            minimized: false,
            open_timer,
        })
    }

    /// Create a lightweight placeholder pane with a dummy PTY (no shell process).
    /// Used for deferred tab restore — avoids spawning a shell that would
    /// compete with the active tab's shells for zshrc loading time.
    pub fn placeholder(cols: u16, rows: u16, config: &Config) -> Result<Self, Box<dyn std::error::Error>> {
        let id = alloc_pane_id();
        let terminal = Arc::new(RwLock::new(TerminalState::new(
            cols,
            rows,
            config.terminal.scrollback,
            crate::terminal::color_to_u8(config.colors.foreground),
            crate::terminal::color_to_u8(config.colors.background),
        )));
        let pty = Pty::dummy()?;
        Ok(Pane {
            id,
            terminal,
            pty,
            shell_exited: Arc::new(AtomicBool::new(false)),
            shell_ready: Arc::new(AtomicBool::new(true)), // placeholder is immediately "ready"
            scroll_accumulator: Cell::new(0.0),
            pending_command: Cell::new(None),
            custom_title: None,
            minimized: false,
            open_timer: Arc::new(PaneOpenTimer::new()),
        })
    }

    pub fn cwd(&self) -> Option<String> {
        self.pty.cwd()
    }

    pub fn foreground_process_name(&self) -> Option<String> {
        self.pty.foreground_process_name()
    }

    pub fn is_alive(&self) -> bool {
        !self.shell_exited.load(Ordering::Relaxed)
    }

    pub fn is_ready(&self) -> bool {
        self.shell_ready.load(Ordering::Relaxed)
    }

    pub fn last_command(&self) -> Option<String> {
        self.terminal.read().last_command.clone()
    }

    /// Display title for this pane: custom title > OSC title > CWD basename > fallback.
    pub fn display_title(&self, fallback: &str) -> String {
        let term = self.terminal.read();
        derive_display_title(
            self.custom_title.as_deref(),
            term.title.as_deref(),
            term.cwd.as_deref(),
            fallback,
        )
    }

    /// True if the app in this pane is actively working: its live OSC 0/2 title
    /// leads with Claude Code's animated Braille spinner (see `is_working_marker`).
    /// The asterisk idle marker (`✳ Claude Code`) does NOT count. Reads the live
    /// OSC 0/2 title even when a sticky custom title shadows it in the display.
    pub fn is_working(&self) -> bool {
        self.terminal
            .read()
            .title
            .as_deref()
            .map_or(false, is_working_marker)
    }

    /// If the shell is ready and there's a pending command, write it to the PTY
    /// (without \r so the user can review before pressing Enter).
    pub fn inject_pending_command(&self) {
        if !self.is_ready() {
            return;
        }
        let cmd = self.pending_command.take();
        if let Some(command) = cmd {
            self.pty.write(command.as_bytes());
        }
    }
}

// (split_sizes removed — replaced by Column::row_heights)

/// A column: flat list of panes stacked vertically with proportional weights.
pub struct Column {
    pub panes: Vec<Pane>,
    pub row_weights: Vec<f32>,
    pub custom_row_weights: Vec<bool>,
}

impl Column {
    /// Create a column with a single pane.
    pub fn new(pane: Pane) -> Self {
        Column { panes: vec![pane], row_weights: vec![1.0], custom_row_weights: vec![false] }
    }

    /// Returns true if all panes in this column are minimized.
    pub fn is_fully_minimized(&self) -> bool {
        self.panes.iter().all(|p| p.minimized)
    }

    pub fn pane(&self, id: PaneId) -> Option<&Pane> {
        self.panes.iter().find(|p| p.id == id)
    }

    pub fn pane_mut(&mut self, id: PaneId) -> Option<&mut Pane> {
        self.panes.iter_mut().find(|p| p.id == id)
    }

    pub fn first_pane(&self) -> &Pane {
        self.panes.first().unwrap()
    }

    pub fn last_pane(&self) -> &Pane {
        self.panes.last().unwrap()
    }

    pub fn for_each_pane<F: FnMut(&Pane)>(&self, f: &mut F) {
        for p in &self.panes { f(p); }
    }

    pub fn contains(&self, id: PaneId) -> bool {
        self.panes.iter().any(|p| p.id == id)
    }

    /// Find the index of a pane by id.
    pub fn pane_index_of(&self, id: PaneId) -> Option<usize> {
        self.panes.iter().position(|p| p.id == id)
    }

    /// Compute pixel heights for each pane from row_weights. Minimized panes
    /// get zero height (no layout footprint).
    /// When cell_h > 0, snap non-minimized heights to multiples of cell_h so that
    /// pane y-offsets always land on cell boundaries (prevents prompt drift during resize).
    pub fn row_heights(&self, total_height: f32, cell_h: f32) -> Vec<f32> {
        let n = self.panes.len();
        let minimized: Vec<bool> = self.panes.iter().map(|p| p.minimized).collect();
        let mut heights = distribute_visible(&self.row_weights, &minimized, total_height);
        // Snap non-minimized heights to multiples of cell_h
        if cell_h > 0.0 {
            let mut snapped_total = 0.0f32;
            let mut last_non_min = None;
            for i in 0..n {
                if !minimized[i] {
                    heights[i] = (heights[i] / cell_h).floor() * cell_h;
                    snapped_total += heights[i];
                    last_non_min = Some(i);
                }
            }
            // Give remaining pixels to the last non-minimized pane
            if let Some(last) = last_non_min {
                let leftover = total_height - snapped_total;
                if leftover > 0.0 {
                    heights[last] += leftover;
                }
            }
        }
        heights
    }

    pub fn for_each_pane_with_viewport<F: FnMut(&Pane, PaneViewport)>(&self, vp: PaneViewport, cell_h: f32, f: &mut F) {
        let heights = self.row_heights(vp.height, cell_h);
        let mut y = vp.y;
        for (i, pane) in self.panes.iter().enumerate() {
            f(pane, PaneViewport { x: vp.x, y, width: vp.width, height: heights[i] });
            y += heights[i];
        }
    }

    pub fn viewport_for_pane(&self, id: PaneId, vp: PaneViewport, cell_h: f32) -> Option<PaneViewport> {
        let heights = self.row_heights(vp.height, cell_h);
        let mut y = vp.y;
        for (i, pane) in self.panes.iter().enumerate() {
            if pane.id == id {
                return Some(PaneViewport { x: vp.x, y, width: vp.width, height: heights[i] });
            }
            y += heights[i];
        }
        None
    }

    pub fn hit_test(&self, x: f32, y: f32, vp: PaneViewport, cell_h: f32) -> Option<(&Pane, PaneViewport)> {
        if x < vp.x || x >= vp.x + vp.width || y < vp.y || y >= vp.y + vp.height {
            return None;
        }
        let heights = self.row_heights(vp.height, cell_h);
        let mut cur_y = vp.y;
        for (i, pane) in self.panes.iter().enumerate() {
            let pane_vp = PaneViewport { x: vp.x, y: cur_y, width: vp.width, height: heights[i] };
            if y >= cur_y && y < cur_y + heights[i] {
                return Some((pane, pane_vp));
            }
            cur_y += heights[i];
        }
        // Fallback: last visible pane (minimized panes are zero-height and unhittable)
        self.panes
            .iter()
            .enumerate()
            .rev()
            .find(|(_, p)| !p.minimized)
            .map(|(i, p)| {
                let last_y = vp.y + vp.height - heights[i];
                (p, PaneViewport { x: vp.x, y: last_y, width: vp.width, height: heights[i] })
            })
    }

    pub fn collect_separators(&self, vp: PaneViewport, cell_h: f32, out: &mut Vec<(f32, f32, f32, f32)>) {
        let heights = self.row_heights(vp.height, cell_h);
        let mut y = vp.y;
        for i in 0..self.panes.len().saturating_sub(1) {
            y += heights[i];
            // Minimized panes are zero-height: draw one separator per visible
            // boundary only (skip when either side is minimized, and only if a
            // visible pane follows below).
            let below_visible = self.panes[i + 1..].iter().any(|p| !p.minimized);
            if !self.panes[i].minimized && below_visible {
                out.push((vp.x, y, vp.x + vp.width, y));
            }
        }
    }

    pub fn collect_separator_info(&self, col_index: usize, vp: PaneViewport, cell_h: f32, out: &mut Vec<SeparatorInfo>) {
        let heights = self.row_heights(vp.height, cell_h);
        let mut y = vp.y;
        for i in 0..self.panes.len().saturating_sub(1) {
            y += heights[i];
            let top_min = self.panes[i].minimized;
            let bot_min = self.panes[i + 1].minimized;
            if !top_min && !bot_min {
                out.push(SeparatorInfo {
                    pos: y,
                    cross_start: vp.x,
                    cross_end: vp.x + vp.width,
                    is_column_sep: false,
                    parent_dim: vp.height,
                    column_sep_index: None,
                    col_index,
                    row_sep_index: Some(i),
                });
            }
        }
    }

    /// Insert a new pane after the pane with target_id.
    pub fn insert_pane_after(&mut self, target_id: PaneId, new_pane: Pane) {
        let idx = self.pane_index_of(target_id).unwrap_or(self.panes.len() - 1);
        let avg = self.row_weights.iter().sum::<f32>() / self.panes.len() as f32;
        self.panes.insert(idx + 1, new_pane);
        self.row_weights.insert(idx + 1, avg);
        self.custom_row_weights.insert(idx + 1, false);
    }

    /// Append a new pane at the bottom.
    pub fn append_pane(&mut self, new_pane: Pane) {
        let avg = self.row_weights.iter().sum::<f32>() / self.panes.len() as f32;
        self.panes.push(new_pane);
        self.row_weights.push(avg);
        self.custom_row_weights.push(false);
    }

    /// Remove a pane by id. Returns true if the column still has panes.
    pub fn remove_pane(&mut self, id: PaneId) -> bool {
        if let Some(idx) = self.pane_index_of(id) {
            self.panes.remove(idx);
            let removed_weight = self.row_weights.remove(idx);
            self.custom_row_weights.remove(idx);
            if !self.panes.is_empty() {
                let sum: f32 = self.row_weights.iter().sum();
                if sum > 0.0 {
                    let scale = (sum + removed_weight) / sum;
                    for w in &mut self.row_weights { *w *= scale; }
                }
            }
            !self.panes.is_empty()
        } else {
            true
        }
    }

    /// Extract a pane by id, returning it. Returns None if not found or sole pane.
    pub fn extract_pane(&mut self, id: PaneId) -> Option<Pane> {
        let idx = self.pane_index_of(id)?;
        if self.panes.len() < 2 { return None; }
        let pane = self.panes.remove(idx);
        let removed_weight = self.row_weights.remove(idx);
        self.custom_row_weights.remove(idx);
        let sum: f32 = self.row_weights.iter().sum();
        if sum > 0.0 {
            let scale = (sum + removed_weight) / sum;
            for w in &mut self.row_weights { *w *= scale; }
        }
        Some(pane)
    }

    pub fn equalize(&mut self) {
        for w in &mut self.row_weights { *w = 1.0; }
        for cw in &mut self.custom_row_weights { *cw = false; }
    }

    /// Adjust row weight by moving the controlled edge of the focused pane.
    /// Same logic as Tab::adjust_column_weight_directional but for the vertical axis.
    pub fn adjust_row_weight_directional(&mut self, id: PaneId, delta: f32) -> bool {
        let row_idx = match self.pane_index_of(id) {
            Some(i) => i,
            None => return false,
        };
        if self.panes.len() < 2 { return false; }

        let is_last = row_idx == self.panes.len() - 1;
        let weight_sum: f32 = self.row_weights.iter().sum();
        let step = delta.abs() * 0.5;

        // Controlled edge = bottom, except last pane (top).
        // delta > 0 (Down): push edge down. delta < 0 (Up): push edge up.
        let growing = if is_last { delta < 0.0 } else { delta > 0.0 };

        let (outer_range, outer_fallback): (std::ops::Range<usize>, usize) = if is_last {
            (0..row_idx, if row_idx > 0 { row_idx - 1 } else { 0 })
        } else {
            (row_idx + 1..self.panes.len(), row_idx + 1)
        };

        if growing {
            let outer_unpinned: Vec<usize> = outer_range.clone()
                .filter(|&i| !self.custom_row_weights[i])
                .collect();
            let source_indices = if outer_unpinned.is_empty() {
                vec![outer_fallback]
            } else {
                outer_unpinned
            };
            let avail: f32 = source_indices.iter().map(|&i| self.row_weights[i] * 0.8).sum();
            let transfer = (step * weight_sum).min(avail);
            if transfer > 0.001 {
                self.row_weights[row_idx] += transfer;
                self.custom_row_weights[row_idx] = true;
                Self::redistribute_loss_static(&mut self.row_weights, &self.custom_row_weights, transfer, outer_range, outer_fallback);
                return true;
            }
        } else {
            let transfer = (step * weight_sum).min(self.row_weights[row_idx] * 0.8);
            if transfer > 0.001 {
                self.row_weights[row_idx] -= transfer;
                self.custom_row_weights[row_idx] = true;
                Self::redistribute_gain_static(&mut self.row_weights, &self.custom_row_weights, transfer, outer_range, outer_fallback);
                return true;
            }
        }
        false
    }

    /// Set row weights by dragging a row separator.
    /// Same logic as Tab::set_column_weights_by_drag but for rows.
    pub fn set_row_weights_by_drag(&mut self, row_idx: usize, delta_px: f32, total_height: f32) {
        if row_idx + 1 >= self.panes.len() { return; }
        let sum: f32 = self.row_weights.iter().sum();
        if sum <= 0.0 || total_height <= 0.0 { return; }

        let delta_weight = delta_px / total_height * sum;
        let min_weight = sum * 0.05;
        if delta_weight.abs() < 0.001 { return; }

        let (pushed_idx, free_range): (usize, std::ops::Range<usize>) = if delta_weight > 0.0 {
            (row_idx + 1, 0..row_idx + 1)
        } else {
            (row_idx, row_idx + 1..self.panes.len())
        };

        let abs_delta = delta_weight.abs();
        let new_pushed = (self.row_weights[pushed_idx] - abs_delta).max(min_weight);
        let actual_delta = self.row_weights[pushed_idx] - new_pushed;
        if actual_delta < 0.001 { return; }

        let free_unpinned: Vec<usize> = free_range.clone()
            .filter(|&i| !self.custom_row_weights[i])
            .collect();

        if free_unpinned.is_empty() {
            let adjacent = if delta_weight > 0.0 { row_idx } else { row_idx + 1 };
            self.row_weights[pushed_idx] = new_pushed;
            self.row_weights[adjacent] += actual_delta;
        } else {
            let share = actual_delta / free_unpinned.len() as f32;
            self.row_weights[pushed_idx] = new_pushed;
            for &i in &free_unpinned {
                self.row_weights[i] += share;
            }
        }
        self.custom_row_weights[pushed_idx] = true;
    }

    /// Swap two panes within this column.
    pub fn swap_panes(&mut self, id1: PaneId, id2: PaneId) -> bool {
        if id1 == id2 { return false; }
        let idx1 = match self.pane_index_of(id1) { Some(i) => i, None => return false };
        let idx2 = match self.pane_index_of(id2) { Some(i) => i, None => return false };
        self.panes.swap(idx1, idx2);
        self.row_weights.swap(idx1, idx2);
        self.custom_row_weights.swap(idx1, idx2);
        true
    }

    /// Reparent pane within column (Up/Down swap with neighbor).
    pub fn reparent_pane(&mut self, focused_id: PaneId, dir: NavDirection) -> bool {
        let idx = match self.pane_index_of(focused_id) { Some(i) => i, None => return false };
        match dir {
            NavDirection::Down if idx + 1 < self.panes.len() => {
                self.panes.swap(idx, idx + 1);
                self.row_weights.swap(idx, idx + 1);
                self.custom_row_weights.swap(idx, idx + 1);
                true
            }
            NavDirection::Up if idx > 0 => {
                self.panes.swap(idx, idx - 1);
                self.row_weights.swap(idx, idx - 1);
                self.custom_row_weights.swap(idx, idx - 1);
                true
            }
            _ => false,
        }
    }

    pub fn max_leaf_width_fraction(&self) -> f32 { 1.0 }

    fn redistribute_loss_static(weights: &mut Vec<f32>, custom: &Vec<bool>, amount: f32, range: std::ops::Range<usize>, fallback: usize) {
        let sum: f32 = weights.iter().sum();
        let min_weight = sum * 0.05;
        let unpinned: Vec<usize> = range.filter(|&i| !custom[i]).collect();
        if unpinned.is_empty() {
            if fallback < weights.len() {
                weights[fallback] = (weights[fallback] - amount).max(min_weight);
            }
        } else {
            let share = amount / unpinned.len() as f32;
            for &i in &unpinned {
                weights[i] = (weights[i] - share).max(min_weight);
            }
        }
    }

    fn redistribute_gain_static(weights: &mut Vec<f32>, custom: &Vec<bool>, amount: f32, range: std::ops::Range<usize>, fallback: usize) {
        let unpinned: Vec<usize> = range.filter(|&i| !custom[i]).collect();
        if unpinned.is_empty() {
            if fallback < weights.len() {
                weights[fallback] += amount;
            }
        } else {
            let share = amount / unpinned.len() as f32;
            for &i in &unpinned {
                weights[i] += share;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{derive_display_title, distribute_visible, grow_virtual_for_restored_column, is_working_marker, reweight_for_scrolled_split, shrink_virtual_for_hidden_column, strip_activity_prefix};

    #[test]
    fn shrink_virtual_gives_back_hidden_column_width() {
        // Scrolling at 2×screen, a half-screen column hides → virtual shrinks by it.
        assert!((shrink_virtual_for_hidden_column(2000.0, 500.0, 1000.0) - 1500.0).abs() < 0.01);
    }

    #[test]
    fn shrink_virtual_floors_at_screen_width() {
        // Shrinking to (or below) the screen clears the override entirely.
        assert_eq!(shrink_virtual_for_hidden_column(1200.0, 400.0, 1000.0), 0.0);
        assert_eq!(shrink_virtual_for_hidden_column(1200.0, 200.0, 1000.0), 0.0);
    }

    #[test]
    fn grow_virtual_adds_restored_column_share() {
        // 3 visible columns (weight 3) at 1500px, restoring a weight-1 column:
        // grows by 500px so the restored column gets its 1/4 share of 2000px.
        assert!((grow_virtual_for_restored_column(1.0, 3.0, 1500.0, 1000.0) - 2000.0).abs() < 0.01);
    }

    #[test]
    fn grow_virtual_inverts_shrink_with_unchanged_weights() {
        // Round trip: minimize a column then restore it → original virtual width.
        // Weights [1, 1, 2], virtual 2000px, screen 1000px; hide the weight-2
        // column (1000px wide), then restore it.
        let after_min = shrink_virtual_for_hidden_column(2000.0, 1000.0, 1000.0);
        assert!((after_min - 1000.0).abs() < 0.01 || after_min == 0.0);
        // Still-scrolling variant: weights [1, 1, 1, 1], virtual 2000px, hide
        // one 500px column (→ 1500px), restore it (w_col=1 vs w_others=3).
        let after_min = shrink_virtual_for_hidden_column(2000.0, 500.0, 1000.0);
        let restored = grow_virtual_for_restored_column(1.0, 3.0, after_min, 1000.0);
        assert!((restored - 2000.0).abs() < 0.01);
    }

    #[test]
    fn distribute_visible_gives_minimized_zero_space() {
        // One minimized entry: it gets 0, the others share the full total by weight.
        let w = distribute_visible(&[1.0, 1.0, 2.0], &[false, true, false], 900.0);
        assert_eq!(w[1], 0.0);
        assert!((w[0] - 300.0).abs() < 0.01);
        assert!((w[2] - 600.0).abs() < 0.01);
        // Total is fully used by visible entries.
        assert!((w.iter().sum::<f32>() - 900.0).abs() < 0.01);
    }

    #[test]
    fn distribute_visible_no_minimized_is_plain_weights() {
        let w = distribute_visible(&[1.0, 3.0], &[false, false], 800.0);
        assert!((w[0] - 200.0).abs() < 0.01);
        assert!((w[1] - 600.0).abs() < 0.01);
    }

    #[test]
    fn distribute_visible_zero_weights_split_evenly() {
        // Degenerate zero weights: visible entries share evenly, minimized stays 0.
        let w = distribute_visible(&[0.0, 0.0, 0.0], &[false, true, false], 600.0);
        assert_eq!(w[1], 0.0);
        assert!((w[0] - 300.0).abs() < 0.01);
        assert!((w[2] - 300.0).abs() < 0.01);
    }

    #[test]
    fn is_working_marker_only_on_braille_spinner() {
        // Working: an animated Braille spinner glyph (U+2800–U+28FF) + space.
        assert!(is_working_marker("\u{2802} Revue de code")); // ⠂
        assert!(is_working_marker("\u{2810} Comprendre"));    // ⠐
        assert!(is_working_marker("\u{28FF} x"));             // last frame of range
        assert!(is_working_marker("\u{2800} ")); // spinner + trailing space only
        // NOT working: the asterisk idle marker (this was the earlier bug).
        assert!(!is_working_marker("* Claude Code"));
        assert!(!is_working_marker("\u{2733} Claude Code"));
        assert!(!is_working_marker("\u{2217} foo"));
        // NOT working: plain titles, spinner without a space, empty.
        assert!(!is_working_marker("plain title"));
        assert!(!is_working_marker("\u{2802}glued"));
        assert!(!is_working_marker("\u{2802}"));
        assert!(!is_working_marker(""));
    }

    #[test]
    fn strip_activity_prefix_trims_spinner_and_idle_marker() {
        // Braille working spinner stripped for a stable, non-jittering title.
        assert_eq!(strip_activity_prefix("\u{2802} Revue de code"), "Revue de code");
        assert_eq!(strip_activity_prefix("\u{2810} Comprendre"), "Comprendre");
        // Asterisk-like idle markers still stripped too.
        assert_eq!(strip_activity_prefix("* Add TimeComet.swift"), "Add TimeComet.swift");
        assert_eq!(strip_activity_prefix("\u{2733} Claude Code"), "Claude Code");
        assert_eq!(strip_activity_prefix("\u{2217} foo"), "foo");
        // No marker, or marker without a following space: left untouched.
        assert_eq!(strip_activity_prefix("plain title"), "plain title");
        assert_eq!(strip_activity_prefix("*already glued"), "*already glued");
        assert_eq!(strip_activity_prefix("\u{2802}glued"), "\u{2802}glued");
        // Only one prefix stripped; an inner asterisk stays.
        assert_eq!(strip_activity_prefix("* a * b"), "a * b");
        // Empty and marker-only edge cases don't panic.
        assert_eq!(strip_activity_prefix(""), "");
        assert_eq!(strip_activity_prefix("*"), "*");
        assert_eq!(strip_activity_prefix("* "), "");
    }

    // Emulate Tab::column_widths for the no-minimized fast path.
    fn col_widths(weights: &[f32], total: f32) -> Vec<f32> {
        let sum: f32 = weights.iter().sum();
        weights.iter().map(|w| total * w / sum).collect()
    }

    #[test]
    fn scrolled_split_keeps_existing_pixel_widths() {
        // 3 columns at 900px each, scrolling (virtual 2700 > screen 1512).
        // A 4th column was just inserted at the end with the average weight;
        // it should be born at the focused pane's width (900), like a sibling.
        let old_virtual = 2700.0;
        let new_col_px = 900.0; // focused pane width
        let mut weights = vec![900.0, 900.0, 900.0, 900.0]; // last = just-inserted (avg)
        let new_virtual = reweight_for_scrolled_split(&mut weights, 3, old_virtual, new_col_px).unwrap();
        assert_eq!(new_virtual, 3600.0);
        let widths = col_widths(&weights, new_virtual);
        // All four columns end up at the same width, nothing shrunk.
        assert!((widths[0] - 900.0).abs() < 0.01);
        assert!((widths[1] - 900.0).abs() < 0.01);
        assert!((widths[2] - 900.0).abs() < 0.01);
        assert!((widths[3] - 900.0).abs() < 0.01);
    }

    #[test]
    fn scrolled_split_insert_in_middle_preserves_others() {
        // Unequal columns: 1200, 600, scrolling; insert a new column at idx 1.
        let old_virtual = 1800.0;
        let new_col_px = 600.0;
        let mut weights = vec![1200.0, 900.0, 600.0]; // idx 1 = just-inserted (avg of 1200+600)
        let new_virtual = reweight_for_scrolled_split(&mut weights, 1, old_virtual, new_col_px).unwrap();
        assert_eq!(new_virtual, 2400.0);
        let widths = col_widths(&weights, new_virtual);
        assert!((widths[0] - 1200.0).abs() < 0.01);
        assert!((widths[1] - 600.0).abs() < 0.01); // new column
        assert!((widths[2] - 600.0).abs() < 0.01);
    }

    #[test]
    fn scrolled_split_bad_index_is_noop() {
        let mut weights = vec![1.0, 1.0];
        assert!(reweight_for_scrolled_split(&mut weights, 5, 1000.0, 300.0).is_none());
        assert_eq!(weights, vec![1.0, 1.0]);
    }

    #[test]
    fn custom_title_wins_over_everything() {
        let t = derive_display_title(Some("my pane"), Some("osc"), Some("/home/x/proj"), "shell");
        assert_eq!(t, "my pane");
    }

    #[test]
    fn non_empty_osc_title_used() {
        let t = derive_display_title(None, Some("vim"), Some("/home/x/proj"), "shell");
        assert_eq!(t, "vim");
    }

    #[test]
    fn empty_osc_title_falls_back_to_cwd_basename() {
        let t = derive_display_title(None, Some(""), Some("/home/x/proj"), "shell");
        assert_eq!(t, "proj");
    }

    #[test]
    fn whitespace_osc_title_falls_back_to_cwd_basename() {
        let t = derive_display_title(None, Some("   "), Some("/home/x/proj"), "shell");
        assert_eq!(t, "proj");
    }

    #[test]
    fn no_title_no_cwd_uses_fallback() {
        assert_eq!(derive_display_title(None, None, None, "shell"), "shell");
        // Empty OSC title with no cwd must also reach the fallback, never blank.
        assert_eq!(derive_display_title(None, Some(""), None, "shell"), "shell");
    }
}
