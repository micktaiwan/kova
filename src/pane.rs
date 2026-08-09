use parking_lot::RwLock;
use std::cell::{Cell, RefCell};
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

/// Sum of the weights that actually occupy layout space. A minimized entry
/// keeps its weight (so it can come back to its old size) but renders at zero,
/// so every ratio must be taken against this sum — never against the raw total.
fn visible_weight_sum(weights: &[f32], minimized: &[bool]) -> f32 {
    weights
        .iter()
        .zip(minimized.iter())
        .filter(|&(_, &m)| !m)
        .map(|(w, _)| *w)
        .sum()
}

/// Indices of the entries that occupy layout space, in order.
fn visible_indices(minimized: &[bool]) -> Vec<usize> {
    minimized
        .iter()
        .enumerate()
        .filter(|&(_, &m)| !m)
        .map(|(i, _)| i)
        .collect()
}

/// Index pairs of adjacent *visible* entries — one per boundary actually drawn
/// on screen. A minimized entry between two visible ones is skipped over
/// instead of hiding the separator that sits there.
fn adjacent_visible_pairs(minimized: &[bool]) -> Vec<(usize, usize)> {
    let vis = visible_indices(minimized);
    vis.windows(2).map(|w| (w[0], w[1])).collect()
}

/// Weight for a newly inserted entry so it renders at the same size as the
/// entries already on screen: the average of the *visible* weights.
fn new_entry_weight(weights: &[f32], minimized: &[bool]) -> f32 {
    let visible = visible_indices(minimized).len();
    if visible == 0 {
        let n = weights.len().max(1);
        return weights.iter().sum::<f32>() / n as f32;
    }
    let avg = visible_weight_sum(weights, minimized) / visible as f32;
    if avg > 0.0 { avg } else { 1.0 }
}

/// Largest share of the rendered space taken by a single entry, as a fraction
/// of the total (0.0–1.0).
fn max_visible_fraction(weights: &[f32], minimized: &[bool]) -> f32 {
    let sum = visible_weight_sum(weights, minimized);
    if sum <= 0.0 {
        return 1.0;
    }
    weights
        .iter()
        .zip(minimized.iter())
        .filter(|&(_, &m)| !m)
        .map(|(w, _)| w / sum)
        .fold(0.0, f32::max)
}

/// Shrink the weights of the entries wider than `max_px` so they render at
/// exactly `max_px`, leaving the others alone. Shrinking one entry grows every
/// other one's share, so the sum is re-read at each step.
fn clamp_weights_to_max(weights: &mut [f32], minimized: &[bool], total: f32, max_px: f32) {
    if total <= 0.0 || max_px <= 0.0 || max_px >= total {
        return;
    }
    for i in 0..weights.len() {
        if minimized[i] {
            continue;
        }
        let sum = visible_weight_sum(weights, minimized);
        if sum <= 0.0 {
            return;
        }
        if total * weights[i] / sum <= max_px {
            continue;
        }
        let others = sum - weights[i];
        if others <= 0.0 {
            // Sole visible entry: it always fills the space, whatever its
            // weight. Capping it is the caller's job (shrink the total).
            continue;
        }
        weights[i] = max_px * others / (total - max_px);
    }
}

/// Rewrite the weights so that every entry except `idx` keeps its current pixel
/// size while the total goes from `old_total` to `new_total` — `idx` absorbs the
/// whole change (edge grow). Weights come out in pixel units.
fn reweight_for_edge_grow(
    weights: &mut [f32],
    minimized: &[bool],
    idx: usize,
    old_total: f32,
    new_total: f32,
) {
    if idx >= weights.len() || old_total <= 0.0 || new_total <= 0.0 {
        return;
    }
    let sum = visible_weight_sum(weights, minimized);
    if sum <= 0.0 {
        return;
    }
    // Only the visible entries share `old_total`, so only they set the pixel
    // value of one weight unit — and only they have a size to preserve.
    let px_per_weight = old_total / sum;
    let others_px: f32 = weights
        .iter()
        .enumerate()
        .filter(|&(i, _)| i != idx && !minimized[i])
        .map(|(_, w)| *w * px_per_weight)
        .sum();
    let target_px = (new_total - others_px).max(1.0);
    for (i, w) in weights.iter_mut().enumerate() {
        // Minimized entries are rescaled too, so the weight they hold stays
        // comparable with the others when they come back.
        *w = if i == idx { target_px } else { *w * px_per_weight };
    }
}

/// Move the separator sitting to the right of the visible entry `left_idx` by
/// `delta_px`. The entry the separator is pushed into shrinks and becomes
/// pinned; the freed space goes to the non-pinned entries on the other side
/// (or, if they are all pinned, to the adjacent one).
fn apply_separator_drag(
    weights: &mut [f32],
    custom: &mut [bool],
    minimized: &[bool],
    left_idx: usize,
    delta_px: f32,
    total: f32,
) {
    let vis = visible_indices(minimized);
    let p = match vis.iter().position(|&i| i == left_idx) {
        Some(p) if p + 1 < vis.len() => p,
        _ => return,
    };
    let right_idx = vis[p + 1];
    let sum = visible_weight_sum(weights, minimized);
    if sum <= 0.0 || total <= 0.0 {
        return;
    }
    // `total` is shared by the visible entries only: that is what turns a
    // cursor movement in pixels into the right amount of weight.
    let delta_weight = delta_px / total * sum;
    let min_weight = sum * 0.05;
    if delta_weight.abs() < 0.001 {
        return;
    }
    // delta > 0 → the separator moves right → the entry on its right is pushed
    // and the ones on its left absorb. delta < 0 → mirror image.
    let (pushed_idx, free): (usize, Vec<usize>) = if delta_weight > 0.0 {
        (right_idx, vis[..=p].to_vec())
    } else {
        (left_idx, vis[p + 1..].to_vec())
    };
    let new_pushed = (weights[pushed_idx] - delta_weight.abs()).max(min_weight);
    let actual_delta = weights[pushed_idx] - new_pushed;
    if actual_delta < 0.001 {
        return;
    }
    let free_unpinned: Vec<usize> = free.iter().copied().filter(|&i| !custom[i]).collect();
    weights[pushed_idx] = new_pushed;
    if free_unpinned.is_empty() {
        let adjacent = if delta_weight > 0.0 { left_idx } else { right_idx };
        weights[adjacent] += actual_delta;
    } else {
        let share = actual_delta / free_unpinned.len() as f32;
        for &i in &free_unpinned {
            weights[i] += share;
        }
    }
    custom[pushed_idx] = true;
}

/// Keyboard resize: push the edge of entry `idx` by `delta` (positive = to the
/// right / downward). Returns false when nothing could move.
fn apply_directional_resize(
    weights: &mut [f32],
    custom: &mut [bool],
    minimized: &[bool],
    idx: usize,
    delta: f32,
) -> bool {
    let vis = visible_indices(minimized);
    if vis.len() < 2 {
        return false;
    }
    let p = match vis.iter().position(|&i| i == idx) {
        Some(p) => p,
        None => return false,
    };
    // "Last" means last *on screen*: an entry followed only by minimized ones
    // controls its left edge, like any rightmost entry.
    let is_last = p == vis.len() - 1;
    let weight_sum = visible_weight_sum(weights, minimized);
    let min_weight = weight_sum * 0.05;
    let step = delta.abs() * 0.5;
    let growing = if is_last { delta < 0.0 } else { delta > 0.0 };
    // The "outer side" is where the other visible entries are, relative to the
    // controlled edge. Minimized entries are never a source nor a target: they
    // have no size to give or take.
    let (outer, fallback): (Vec<usize>, usize) = if is_last {
        (vis[..p].to_vec(), vis[p - 1])
    } else {
        (vis[p + 1..].to_vec(), vis[p + 1])
    };

    if growing {
        let unpinned: Vec<usize> = outer.iter().copied().filter(|&i| !custom[i]).collect();
        let sources = if unpinned.is_empty() { vec![fallback] } else { unpinned };
        let avail: f32 = sources.iter().map(|&i| weights[i] * 0.8).sum();
        let transfer = (step * weight_sum).min(avail);
        if transfer > 0.001 {
            weights[idx] += transfer;
            custom[idx] = true;
            redistribute_loss(weights, custom, transfer, &outer, fallback, min_weight);
            return true;
        }
    } else {
        let transfer = (step * weight_sum).min(weights[idx] * 0.8);
        if transfer > 0.001 {
            weights[idx] -= transfer;
            custom[idx] = true;
            redistribute_gain(weights, custom, transfer, &outer, fallback);
            return true;
        }
    }
    false
}

/// Hand `amount` of weight to the non-pinned entries of `targets` (equal
/// shares). If they are all pinned, `fallback` takes it all.
fn redistribute_gain(
    weights: &mut [f32],
    custom: &[bool],
    amount: f32,
    targets: &[usize],
    fallback: usize,
) {
    let unpinned: Vec<usize> = targets.iter().copied().filter(|&i| !custom[i]).collect();
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

/// Take `amount` of weight from the non-pinned entries of `targets` (equal
/// shares), never below `min_weight`. If they are all pinned, `fallback`
/// gives it all.
fn redistribute_loss(
    weights: &mut [f32],
    custom: &[bool],
    amount: f32,
    targets: &[usize],
    fallback: usize,
    min_weight: f32,
) {
    let unpinned: Vec<usize> = targets.iter().copied().filter(|&i| !custom[i]).collect();
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
    minimized: &[bool],
    new_col_idx: usize,
    old_virtual: f32,
    new_col_px: f32,
) -> Option<f32> {
    if new_col_idx >= weights.len() { return None; }
    // Old weight sum: the columns that shared `old_virtual` before the split,
    // so the just-inserted one and the minimized ones stay out of it. Dividing
    // by it reproduces each existing column's pre-split pixel width.
    let old_sum: f32 = weights.iter().enumerate()
        .filter(|&(i, _)| i != new_col_idx && !minimized[i])
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
            if pane.id != focused && pane.terminal.read().unread_completion() {
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
    ///   when `refresh_fg` is true (one ioctl per pane); the same probe caches
    ///   the process name the status bar and the pane switcher display.
    /// Unlike completion, the focused pane counts too: the indicator says
    /// "something occupies this tab". Panes whose shell exited are skipped —
    /// a shell killed mid-command never emits 133;D, which would strand the
    /// OSC flag.
    ///
    /// This pass also reaps stale "waiting for the user" flags, because it is
    /// already paying for the one probe that answers the question: a pane
    /// marked waiting whose foreground process is gone (its Claude Code died
    /// without ever retracting the flag) is dropped here. Piggybacking costs
    /// nothing; a separate sweep would double the ioctls.
    pub fn check_running(&mut self, refresh_fg: bool) -> bool {
        let mut osc_any = false;
        let mut fg_any = false;
        self.for_each_pane(&mut |pane| {
            if !pane.is_alive() {
                pane.clear_awaiting();
                pane.fg_process.replace(None);
                pane.claude_name.replace(None);
                return;
            }
            if pane.terminal.read().command_running.load(std::sync::atomic::Ordering::Relaxed) {
                osc_any = true;
            }
            if refresh_fg {
                pane.refresh_claude_name();
                let fg = pane.refresh_fg_process();
                if fg {
                    fg_any = true;
                } else {
                    // Back to a bare shell prompt: whatever was waiting is gone.
                    pane.clear_awaiting();
                }
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

    /// Acknowledge the completion indicator (call when switching to this tab):
    /// every pane of the tab becomes visible at once, so all of them are "seen".
    /// Only the attention state is acked — `command_completed` itself stays set
    /// for IPC `wait-for-completion`.
    pub fn clear_completion(&mut self) {
        self.has_completion = false;
        self.for_each_pane(&mut |pane| {
            pane.terminal.read().ack_completion();
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

    // ---------------------------------------------------------------
    // Viewport computation
    // ---------------------------------------------------------------

    /// Per-column "takes no layout space" flags, in column order. Every weight
    /// computation needs these: a fully-minimized column keeps its weight but
    /// renders at zero width, so it must stay out of the sums and the ratios.
    fn minimized_columns(&self) -> Vec<bool> {
        self.columns.iter().map(|col| col.is_fully_minimized()).collect()
    }

    /// Compute column widths from weights and total width.
    /// Fully-minimized columns take zero width (no layout footprint).
    fn column_widths(&self, total_width: f32) -> Vec<f32> {
        distribute_visible(&self.column_weights, &self.minimized_columns(), total_width)
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
        let minimized = self.minimized_columns();
        let ch = self.cell_h.get();
        // One vertical separator per boundary between two columns actually on
        // screen — minimized columns are zero-width and simply skipped over.
        for (left, _) in adjacent_visible_pairs(&minimized) {
            let x = vp.x + widths[..=left].iter().sum::<f32>();
            out.push((x, vp.y, x, vp.y + vp.height));
        }
        // Horizontal separators within each visible column
        let mut x = vp.x;
        for (col, &w) in self.columns.iter().zip(widths.iter()) {
            if !col.is_fully_minimized() {
                let col_vp = PaneViewport { x, y: vp.y, width: w, height: vp.height };
                col.collect_separators(col_vp, ch, out);
            }
            x += w;
        }
    }

    /// Collect separator info for mouse hit-testing and dragging. Every
    /// separator drawn by `collect_separators` is listed here, so none of them
    /// looks draggable without being so: `column_sep_index` is the visible
    /// column on the left of the separator, and its partner is the next visible
    /// one (a minimized column in between changes nothing).
    pub fn collect_separator_info(&self, vp: PaneViewport, out: &mut Vec<SeparatorInfo>) {
        let widths = self.column_widths(vp.width);
        let minimized = self.minimized_columns();
        let ch = self.cell_h.get();
        for (left, right) in adjacent_visible_pairs(&minimized) {
            let x = vp.x + widths[..=left].iter().sum::<f32>();
            out.push(SeparatorInfo {
                pos: x,
                cross_start: vp.y,
                cross_end: vp.y + vp.height,
                is_column_sep: true,
                parent_dim: vp.width,
                column_sep_index: Some(left),
                col_index: right,
                row_sep_index: None,
            });
        }
        let mut x = vp.x;
        for (i, (col, &w)) in self.columns.iter().zip(widths.iter()).enumerate() {
            if !col.is_fully_minimized() {
                let col_vp = PaneViewport { x, y: vp.y, width: w, height: vp.height };
                col.collect_separator_info(i, col_vp, ch, out);
            }
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
        let avg_weight = new_entry_weight(&self.column_weights, &self.minimized_columns());
        self.columns.insert(idx + 1, Column::new(new_pane));
        self.column_weights.insert(idx + 1, avg_weight);
        self.custom_weights.insert(idx + 1, false);
        new_id
    }

    /// Append a new column at the end.
    /// Returns the new pane's id.
    pub fn append_column(&mut self, new_pane: Pane) -> PaneId {
        let new_id = new_pane.id;
        let avg_weight = new_entry_weight(&self.column_weights, &self.minimized_columns());
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
        let minimized = self.minimized_columns();
        apply_directional_resize(
            &mut self.column_weights,
            &mut self.custom_weights,
            &minimized,
            col_idx,
            delta,
        )
    }

    /// Returns the maximum leaf width as a fraction of total width (0.0–1.0).
    pub fn max_leaf_width_fraction(&self) -> f32 {
        max_visible_fraction(&self.column_weights, &self.minimized_columns())
    }

    /// Post-validation: adjust weights so no leaf exceeds `max_w` pixels.
    pub fn clamp_pane_widths(&mut self, total: f32, max_w: f32) {
        let minimized = self.minimized_columns();
        clamp_weights_to_max(&mut self.column_weights, &minimized, total, max_w);
    }

    /// Scale ratios so that only `target_id` absorbs the size change (edge grow).
    pub fn scale_ratios_for_edge_grow(&mut self, target_id: PaneId, old_total: f32, new_total: f32) {
        let col_idx = match self.column_index_of(target_id) {
            Some(i) => i,
            None => return,
        };
        let minimized = self.minimized_columns();
        reweight_for_edge_grow(&mut self.column_weights, &minimized, col_idx, old_total, new_total);
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
        let minimized = self.minimized_columns();
        if let Some(new_virtual) = reweight_for_scrolled_split(
            &mut self.column_weights, &minimized, new_col_idx, old_virtual, new_col_px,
        ) {
            self.virtual_width_override = if new_virtual > screen { new_virtual } else { 0.0 };
        }
    }

    /// Set column weights by dragging a column separator.
    /// `col_idx` is the index of the visible column left of the separator (the
    /// right-hand partner is the next visible column, skipping minimized ones).
    ///
    /// Redistribution: the "pushed" column (on the side the separator moves toward) absorbs the
    /// delta directly and becomes pinned. The freed/consumed space is redistributed among all
    /// non-pinned columns on the opposite side. If all opposite columns are pinned, only the
    /// adjacent one absorbs (fallback).
    pub fn set_column_weights_by_drag(&mut self, col_idx: usize, delta_px: f32, total_width: f32) {
        let minimized = self.minimized_columns();
        apply_separator_drag(
            &mut self.column_weights,
            &mut self.custom_weights,
            &minimized,
            col_idx,
            delta_px,
            total_width,
        );
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
    /// The app in this pane told us it is waiting for the user (Claude Code
    /// pushes this from its `Stop` / permission-prompt hooks over IPC).
    /// `Some(epoch_secs)` = waiting since that time; `None` = not waiting.
    /// Never trusted blindly: see `is_awaiting` and `Tab::check_running`.
    pub awaiting: Cell<Option<u64>>,
    /// Name of the Claude Code session running in this pane, as set by its
    /// `/rename` command. Refreshed on the same throttle as the foreground
    /// probe (`Tab::check_running`) rather than read per frame, because the
    /// lookup goes through a directory scan. `None` = no Claude session here,
    /// or one the user never named.
    pub claude_name: RefCell<Option<String>>,
    /// Name of the binary running in the foreground (`claude`, `nvim`, `ssh`…),
    /// `None` at a bare shell prompt. Cached because the status bar reads it on
    /// every frame while resolving it costs two syscalls: it is refreshed on the
    /// same ~0.5s throttle as the running-state probe, in `Tab::check_running`.
    fg_process: RefCell<Option<String>>,
}

/// Resolve the label to show for a pane, in priority order: user-set custom
/// title → Claude Code session name (its `/rename`) → non-empty OSC title →
/// cwd basename → `fallback`.
/// An empty or whitespace-only OSC title is treated as absent so we never
/// render a blank row (the "invisible white line" bug in the pane switcher).
/// True if `title` begins with a Claude Code *working* marker: an animated
/// Braille spinner glyph (U+2800–U+28FF) immediately followed by a space.
/// Claude Code prepends this spinner ONLY while it is actively generating or
/// running a tool; at the prompt it shows an asterisk-like idle marker
/// (`✳ Claude Code`) or a plain title instead. So this — and NOT the asterisk —
/// is the reliable "the app is busy" signal.
/// Wall-clock seconds since the epoch. Used to stamp when a pane started
/// waiting; a jump in system time only skews a displayed age, never a decision.
fn now_epoch_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

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

/// Clean up a raw foreground process name for display: drop surrounding
/// whitespace, keep only the last path component (some processes report a full
/// path), and drop the leading `-` a login shell carries (`-zsh`). Returns
/// `None` when nothing displayable is left, so callers can treat "no name" and
/// "empty name" the same way.
fn normalize_process_name(raw: &str) -> Option<String> {
    let name = raw.trim();
    let name = name.rsplit('/').next().unwrap_or(name);
    let name = name.strip_prefix('-').unwrap_or(name);
    (!name.is_empty()).then(|| name.to_string())
}

fn derive_display_title(
    custom_title: Option<&str>,
    claude_name: Option<&str>,
    osc_title: Option<&str>,
    cwd: Option<&str>,
    fallback: &str,
) -> String {
    if let Some(custom) = custom_title {
        return custom.to_string();
    }
    // A Claude session name beats the OSC title, which is only ever the generic
    // "Claude Code" plus a status marker, but stays below a title the user set
    // by hand here — Kova's own rename must win over the app's.
    if let Some(name) = claude_name.map(str::trim).filter(|n| !n.is_empty()) {
        return name.to_string();
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
            awaiting: Cell::new(None),
            claude_name: RefCell::new(None),
            fg_process: RefCell::new(None),
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
            awaiting: Cell::new(None),
            claude_name: RefCell::new(None),
            fg_process: RefCell::new(None),
        })
    }

    pub fn cwd(&self) -> Option<String> {
        self.pty.cwd()
    }

    pub fn foreground_process_name(&self) -> Option<String> {
        self.pty.foreground_process_name()
    }

    /// Re-probe the foreground process and refresh the cached name. Returns
    /// whether a process other than the shell owns the terminal — the answer
    /// `Tab::check_running` needs, so both come out of a single probe.
    /// An unresolvable name caches as `None` (nothing to display) without
    /// changing the returned yes/no.
    fn refresh_fg_process(&self) -> bool {
        let probe = self.pty.foreground_process();
        let running = probe.is_some();
        self.fg_process
            .replace(probe.and_then(|name| normalize_process_name(&name)));
        running
    }

    /// Cached name of the foreground binary, `None` at a bare shell prompt.
    /// Up to ~0.5s stale (see `Tab::check_running`), which is what makes it
    /// free to read on every frame.
    pub fn fg_process(&self) -> Option<String> {
        self.fg_process.borrow().clone()
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

    /// Title set by the running app (OSC 0/2), without the activity marker
    /// Claude Code prepends — so a saved title does not freeze a spinner frame.
    pub fn osc_title(&self) -> Option<String> {
        self.terminal
            .read()
            .title
            .as_deref()
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .map(|t| strip_activity_prefix(t).to_string())
    }

    /// Display title for this pane: custom title > Claude session name > OSC
    /// title > CWD basename > fallback.
    pub fn display_title(&self, fallback: &str) -> String {
        let term = self.terminal.read();
        derive_display_title(
            self.custom_title.as_deref(),
            self.claude_name.borrow().as_deref(),
            term.title.as_deref(),
            term.cwd.as_deref(),
            fallback,
        )
    }

    /// Re-read the name of the Claude Code session running in this pane (the
    /// one its `/rename` sets). Called from the throttled probe pass, never per
    /// frame: the lookup is a cached scan of `~/.claude/sessions/`.
    pub fn refresh_claude_name(&self) {
        *self.claude_name.borrow_mut() = crate::claude_session::name_for_shell(self.pty.pid());
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

    /// True if the app in this pane says it is waiting for the user, and
    /// nothing observed since contradicts it.
    ///
    /// The flag is *pushed* by Claude Code's hooks, but a pushed flag can
    /// outlive its truth (a session killed with -9 never gets to retract it),
    /// so it is only ever reported through checks Kova makes itself. Here: a
    /// pane whose Claude went back to work cannot be waiting, whatever the
    /// last hook said. The slower liveness reap lives in `Tab::check_running`.
    pub fn is_awaiting(&self) -> bool {
        self.awaiting.get().is_some() && !self.is_working()
    }

    /// Epoch seconds since which this pane has been waiting, if it is.
    pub fn awaiting_since(&self) -> Option<u64> {
        self.is_awaiting().then(|| self.awaiting.get()).flatten()
    }

    /// Mark the pane as waiting for the user, starting now (idempotent: an
    /// already-waiting pane keeps its original timestamp, so a second `Stop`
    /// on the same unanswered turn does not reset how long it has waited).
    pub fn set_awaiting(&self) {
        if self.awaiting.get().is_none() {
            self.awaiting.set(Some(now_epoch_secs()));
        }
    }

    /// Drop the waiting flag — the user engaged, or the session is gone.
    pub fn clear_awaiting(&self) {
        self.awaiting.set(None);
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
    /// Per-pane "takes no layout space" flags, in row order.
    fn minimized_rows(&self) -> Vec<bool> {
        self.panes.iter().map(|p| p.minimized).collect()
    }

    pub fn row_heights(&self, total_height: f32, cell_h: f32) -> Vec<f32> {
        let n = self.panes.len();
        let minimized = self.minimized_rows();
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
        // Minimized panes are zero-height: one separator per boundary between
        // two panes actually on screen.
        for (top, _) in adjacent_visible_pairs(&self.minimized_rows()) {
            let y = vp.y + heights[..=top].iter().sum::<f32>();
            out.push((vp.x, y, vp.x + vp.width, y));
        }
    }

    pub fn collect_separator_info(&self, col_index: usize, vp: PaneViewport, cell_h: f32, out: &mut Vec<SeparatorInfo>) {
        let heights = self.row_heights(vp.height, cell_h);
        for (top, _) in adjacent_visible_pairs(&self.minimized_rows()) {
            let y = vp.y + heights[..=top].iter().sum::<f32>();
            out.push(SeparatorInfo {
                pos: y,
                cross_start: vp.x,
                cross_end: vp.x + vp.width,
                is_column_sep: false,
                parent_dim: vp.height,
                column_sep_index: None,
                col_index,
                row_sep_index: Some(top),
            });
        }
    }

    /// Insert a new pane after the pane with target_id.
    pub fn insert_pane_after(&mut self, target_id: PaneId, new_pane: Pane) {
        let idx = self.pane_index_of(target_id).unwrap_or(self.panes.len() - 1);
        let avg = new_entry_weight(&self.row_weights, &self.minimized_rows());
        self.panes.insert(idx + 1, new_pane);
        self.row_weights.insert(idx + 1, avg);
        self.custom_row_weights.insert(idx + 1, false);
    }

    /// Append a new pane at the bottom.
    pub fn append_pane(&mut self, new_pane: Pane) {
        let avg = new_entry_weight(&self.row_weights, &self.minimized_rows());
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
        let minimized = self.minimized_rows();
        apply_directional_resize(
            &mut self.row_weights,
            &mut self.custom_row_weights,
            &minimized,
            row_idx,
            delta,
        )
    }

    /// Set row weights by dragging a row separator.
    /// `row_idx` is the index of the visible pane above the separator (the pane
    /// below is the next visible one, skipping minimized ones).
    pub fn set_row_weights_by_drag(&mut self, row_idx: usize, delta_px: f32, total_height: f32) {
        let minimized = self.minimized_rows();
        apply_separator_drag(
            &mut self.row_weights,
            &mut self.custom_row_weights,
            &minimized,
            row_idx,
            delta_px,
            total_height,
        );
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

}

#[cfg(test)]
mod tests {
    use super::{adjacent_visible_pairs, apply_directional_resize, apply_separator_drag, clamp_weights_to_max, derive_display_title, distribute_visible, grow_virtual_for_restored_column, is_working_marker, max_visible_fraction, new_entry_weight, normalize_process_name, reweight_for_edge_grow, reweight_for_scrolled_split, shrink_virtual_for_hidden_column, strip_activity_prefix};

    // ------------------------------------------------------------------
    // Minimized entries must not pollute the layout of the visible ones.
    //
    // A minimized column keeps its weight so it can come back at its old
    // size, but it renders at zero width. Every one of these tests puts a
    // minimized entry next to visible ones and checks the visible ones are
    // laid out exactly as if it were not there.
    // ------------------------------------------------------------------

    /// Rendered sizes, minimized entries included (0.0 each).
    fn rendered(weights: &[f32], minimized: &[bool], total: f32) -> Vec<f32> {
        distribute_visible(weights, minimized, total)
    }

    #[test]
    fn split_while_scrolling_keeps_the_visible_widths_with_a_minimized_column() {
        // Columns [A, M(minimized)], A alone fills the 2000px virtual space.
        // Splitting A inserts N between them, born at A's width (2000px), and
        // the virtual space grows to hold both: A and N keep 2000px each.
        let mut weights = vec![1.0, 1.0, 1.0];
        let minimized = [false, false, true]; // [A, N, M]
        let new_virtual =
            reweight_for_scrolled_split(&mut weights, &minimized, 1, 2000.0, 2000.0).unwrap();
        assert!((new_virtual - 4000.0).abs() < 0.01, "virtual = {}", new_virtual);
        let px = rendered(&weights, &minimized, new_virtual);
        assert!((px[0] - 2000.0).abs() < 1.0, "A = {}px, expected 2000", px[0]);
        assert!((px[1] - 2000.0).abs() < 1.0, "N = {}px, expected 2000", px[1]);
    }

    #[test]
    fn a_new_column_is_born_the_size_of_the_visible_ones() {
        // One wide visible column (weight 3) and two minimized leftovers.
        // The new column should share the space with the visible one: 50/50.
        let weights = [3.0, 0.5, 0.5];
        let minimized = [false, true, true];
        let w = new_entry_weight(&weights, &minimized);
        let after = [3.0, w, 0.5, 0.5];
        let after_min = [false, false, true, true];
        let px = rendered(&after, &after_min, 1000.0);
        assert!((px[0] - 500.0).abs() < 1.0, "old = {}px, expected 500", px[0]);
        assert!((px[1] - 500.0).abs() < 1.0, "new = {}px, expected 500", px[1]);
    }

    #[test]
    fn dragging_a_separator_follows_the_cursor_with_a_minimized_column() {
        // Two visible columns at 500px each, plus a minimized one holding
        // weight 2. Dragging the separator 100px right must move it 100px.
        let mut weights = vec![1.0, 1.0, 2.0];
        let mut custom = vec![false, false, false];
        let minimized = [false, false, true];
        apply_separator_drag(&mut weights, &mut custom, &minimized, 0, 100.0, 1000.0);
        let px = rendered(&weights, &minimized, 1000.0);
        assert!((px[0] - 600.0).abs() < 1.0, "left = {}px, expected 600", px[0]);
        assert!((px[1] - 400.0).abs() < 1.0, "right = {}px, expected 400", px[1]);
    }

    #[test]
    fn the_widest_visible_column_is_measured_against_the_rendered_space() {
        // Visible weights 1 and 3 → the wide one takes 3/4 of the screen.
        // The minimized weight-4 column must not dilute that fraction.
        let frac = max_visible_fraction(&[1.0, 3.0, 4.0], &[false, false, true]);
        assert!((frac - 0.75).abs() < 0.001, "fraction = {}, expected 0.75", frac);
    }

    #[test]
    fn clamping_caps_the_width_actually_rendered() {
        // Same layout: the wide column renders at 750px of a 1000px space.
        // Capping at 600px must bring it to 600px on screen.
        let mut weights = vec![1.0, 3.0, 4.0];
        let minimized = [false, false, true];
        clamp_weights_to_max(&mut weights, &minimized, 1000.0, 600.0);
        let px = rendered(&weights, &minimized, 1000.0);
        assert!(px[1] <= 601.0, "wide column = {}px, expected ≤ 600", px[1]);
    }

    #[test]
    fn edge_grow_leaves_the_other_visible_column_alone() {
        // Two visible columns at 500px each in a 1000px space (plus a
        // minimized one). Growing the first to a 1200px total: it takes the
        // whole +200px, the second stays at 500px.
        let mut weights = vec![1.0, 1.0, 2.0];
        let minimized = [false, false, true];
        reweight_for_edge_grow(&mut weights, &minimized, 0, 1000.0, 1200.0);
        let px = rendered(&weights, &minimized, 1200.0);
        assert!((px[0] - 700.0).abs() < 1.0, "grown = {}px, expected 700", px[0]);
        assert!((px[1] - 500.0).abs() < 1.0, "other = {}px, expected 500", px[1]);
    }

    #[test]
    fn keyboard_resize_only_moves_weight_between_visible_columns() {
        // Growing the first column must take from the visible column next to
        // it, never from the minimized one hiding behind.
        let mut weights = vec![1.0, 1.0, 1.0];
        let mut custom = vec![false, false, false];
        let minimized = [false, false, true];
        let before_visible = weights[0] + weights[1];
        assert!(apply_directional_resize(&mut weights, &mut custom, &minimized, 0, 1.0));
        assert!((weights[2] - 1.0).abs() < 0.001, "minimized weight moved: {}", weights[2]);
        assert!(
            ((weights[0] + weights[1]) - before_visible).abs() < 0.001,
            "visible weight sum changed: {} → {}",
            before_visible,
            weights[0] + weights[1]
        );
        assert!(weights[0] > 1.0, "focused column did not grow: {}", weights[0]);
    }

    #[test]
    fn the_last_visible_column_resizes_from_its_left_edge() {
        // Column 1 is the last one on screen (column 2 is minimized), so it
        // controls its left edge: Right shrinks it.
        let mut weights = vec![1.0, 1.0, 1.0];
        let mut custom = vec![false, false, false];
        let minimized = [false, false, true];
        assert!(apply_directional_resize(&mut weights, &mut custom, &minimized, 1, 1.0));
        assert!(weights[1] < 1.0, "last visible column grew instead: {}", weights[1]);
        assert!(weights[0] > 1.0, "its left neighbour did not grow: {}", weights[0]);
    }

    #[test]
    fn a_minimized_column_does_not_swallow_the_separator_it_sits_on() {
        // [A, M(minimized), B] draws one separator, between A and B.
        assert_eq!(adjacent_visible_pairs(&[false, true, false]), vec![(0, 2)]);
        // Trailing and leading minimized entries add no separator.
        assert_eq!(adjacent_visible_pairs(&[false, false, true]), vec![(0, 1)]);
        assert_eq!(adjacent_visible_pairs(&[true, false]), vec![]);
    }

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
    fn process_name_kept_as_is_when_already_a_bare_binary() {
        assert_eq!(normalize_process_name("claude").as_deref(), Some("claude"));
        assert_eq!(normalize_process_name("nvim").as_deref(), Some("nvim"));
    }

    #[test]
    fn process_name_keeps_only_the_last_path_component() {
        assert_eq!(normalize_process_name("/usr/bin/ssh").as_deref(), Some("ssh"));
    }

    #[test]
    fn process_name_drops_the_login_shell_dash() {
        assert_eq!(normalize_process_name("-zsh").as_deref(), Some("zsh"));
    }

    #[test]
    fn process_name_trims_surrounding_whitespace() {
        assert_eq!(normalize_process_name("  cargo \n").as_deref(), Some("cargo"));
    }

    #[test]
    fn unresolvable_process_name_yields_nothing_to_display() {
        // proc_name failing gives an empty string — must not render a blank label.
        assert_eq!(normalize_process_name(""), None);
        assert_eq!(normalize_process_name("   "), None);
        assert_eq!(normalize_process_name("-"), None);
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
        let minimized = [false; 4];
        let new_virtual = reweight_for_scrolled_split(&mut weights, &minimized, 3, old_virtual, new_col_px).unwrap();
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
        let minimized = [false; 3];
        let new_virtual = reweight_for_scrolled_split(&mut weights, &minimized, 1, old_virtual, new_col_px).unwrap();
        assert_eq!(new_virtual, 2400.0);
        let widths = col_widths(&weights, new_virtual);
        assert!((widths[0] - 1200.0).abs() < 0.01);
        assert!((widths[1] - 600.0).abs() < 0.01); // new column
        assert!((widths[2] - 600.0).abs() < 0.01);
    }

    #[test]
    fn scrolled_split_bad_index_is_noop() {
        let mut weights = vec![1.0, 1.0];
        assert!(reweight_for_scrolled_split(&mut weights, &[false, false], 5, 1000.0, 300.0).is_none());
        assert_eq!(weights, vec![1.0, 1.0]);
    }

    #[test]
    fn custom_title_wins_over_everything() {
        let t = derive_display_title(
            Some("my pane"),
            Some("session"),
            Some("osc"),
            Some("/home/x/proj"),
            "shell",
        );
        assert_eq!(t, "my pane");
    }

    #[test]
    fn claude_session_name_wins_over_osc_title() {
        // The OSC title a Claude pane carries is the generic "✳ Claude Code";
        // the name the user gave the session is what identifies the pane.
        let t = derive_display_title(
            None,
            Some("pane switcher"),
            Some("✳ Claude Code"),
            Some("/home/x/proj"),
            "shell",
        );
        assert_eq!(t, "pane switcher");
    }

    #[test]
    fn blank_claude_session_name_falls_through_to_osc_title() {
        let t = derive_display_title(None, Some("   "), Some("vim"), Some("/home/x/proj"), "shell");
        assert_eq!(t, "vim");
    }

    #[test]
    fn non_empty_osc_title_used() {
        let t = derive_display_title(None, None, Some("vim"), Some("/home/x/proj"), "shell");
        assert_eq!(t, "vim");
    }

    #[test]
    fn empty_osc_title_falls_back_to_cwd_basename() {
        let t = derive_display_title(None, None, Some(""), Some("/home/x/proj"), "shell");
        assert_eq!(t, "proj");
    }

    #[test]
    fn whitespace_osc_title_falls_back_to_cwd_basename() {
        let t = derive_display_title(None, None, Some("   "), Some("/home/x/proj"), "shell");
        assert_eq!(t, "proj");
    }

    #[test]
    fn claude_session_name_used_when_the_pane_has_no_other_title() {
        let t = derive_display_title(None, Some("kova-bc"), None, None, "shell");
        assert_eq!(t, "kova-bc");
    }

    #[test]
    fn no_title_no_cwd_uses_fallback() {
        assert_eq!(derive_display_title(None, None, None, None, "shell"), "shell");
        // Empty OSC title with no cwd must also reach the fallback, never blank.
        assert_eq!(derive_display_title(None, None, Some(""), None, "shell"), "shell");
    }
}
