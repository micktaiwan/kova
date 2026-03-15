use parking_lot::RwLock;
use std::cell::Cell;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use crate::config::Config;
use crate::renderer::PaneViewport;
use crate::terminal::pty::Pty;
use crate::terminal::TerminalState;

pub type PaneId = u32;

/// Height of a minimized pane bar (in pixels).
pub const MINIMIZED_BAR_PX: f32 = 24.0;

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
    /// Pixel position of the separator line (x for column sep, y for vsplit).
    pub pos: f32,
    /// Start of the separator extent on the cross-axis.
    pub cross_start: f32,
    /// End of the separator extent on the cross-axis.
    pub cross_end: f32,
    /// Whether this is a column separator (vertical line between columns).
    pub is_hsplit: bool,
    /// Current ratio of the parent node (for VSplit seps).
    pub origin_ratio: f32,
    /// Parent dimension along the split axis (width for column, height for vsplit).
    pub parent_dim: f32,
    /// Pointer address of the split node, used as a stable identifier for VSplit seps.
    pub node_ptr: usize,
    /// Column separator index: Some(i) means separator between columns[i] and columns[i+1].
    pub column_sep_index: Option<usize>,
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
    pub columns: Vec<ColumnTree>,
    pub column_weights: Vec<f32>,
    pub focused_pane: PaneId,
    pub custom_title: Option<String>,
    /// Index into TAB_COLORS palette, None = default bg.
    pub color: Option<usize>,
    /// Bell received on a non-focused tab — show attention indicator.
    pub has_bell: bool,
    /// Command completed in a non-focused pane/tab — show completion indicator.
    pub has_completion: bool,
    /// FILO stack of minimized pane IDs.
    pub minimized_stack: Vec<PaneId>,
    /// Horizontal scroll offset in pixels (0 = no scroll).
    pub scroll_offset_x: f32,
    /// Manual override of virtual width (0.0 = auto from min_split_width).
    pub virtual_width_override: f32,
}

impl Tab {
    /// Create a new tab with a single pane.
    pub fn new(config: &Config) -> Result<Self, Box<dyn std::error::Error>> {
        let pane = Pane::spawn(config.terminal.columns, config.terminal.rows, config, None)?;
        let focused = pane.id;
        Ok(Tab {
            id: alloc_tab_id(),
            columns: vec![ColumnTree::Leaf(pane)],
            column_weights: vec![1.0],
            focused_pane: focused,
            custom_title: None,
            color: None,
            has_bell: false,
            has_completion: false,
            minimized_stack: Vec::new(),
            scroll_offset_x: 0.0,
            virtual_width_override: 0.0,
        })
    }

    /// Create a new tab inheriting the CWD from another pane.
    pub fn new_with_cwd(config: &Config, cwd: Option<&str>) -> Result<Self, Box<dyn std::error::Error>> {
        let pane = Pane::spawn(config.terminal.columns, config.terminal.rows, config, cwd)?;
        let focused = pane.id;
        Ok(Tab {
            id: alloc_tab_id(),
            columns: vec![ColumnTree::Leaf(pane)],
            column_weights: vec![1.0],
            focused_pane: focused,
            custom_title: None,
            color: None,
            has_bell: false,
            has_completion: false,
            minimized_stack: Vec::new(),
            scroll_offset_x: 0.0,
            virtual_width_override: 0.0,
        })
    }

    /// Compute the virtual width for this tab's split layout.
    /// If a manual override is set, use it. Otherwise: max(screen_width, columns * min_split_width).
    pub fn virtual_width(&self, screen_width: f32, min_split_width: f32) -> f32 {
        if self.virtual_width_override > 0.0 {
            self.virtual_width_override.max(screen_width)
        } else {
            let n = self.columns.len() as f32;
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

    /// Drain bell flags from panes and accumulate into tab-level flag.
    /// Returns true if this tab needs attention.
    pub fn check_bell(&mut self) -> bool {
        let mut any_bell = false;
        for col in &self.columns {
            col.for_each_pane(&mut |pane| {
                if pane.terminal.read().bell.swap(false, std::sync::atomic::Ordering::Relaxed) {
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

    /// Restore the last minimized pane (FILO).
    pub fn restore_last_minimized(&mut self) -> bool {
        if let Some(id) = self.minimized_stack.pop() {
            if let Some(pane) = self.pane_mut(id) {
                pane.minimized = false;
            }
            true
        } else {
            false
        }
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

    /// Return the 1-based column index of the pane (for status bar display).
    pub fn column_index(&self, id: PaneId) -> Option<usize> {
        self.column_index_of(id).map(|i| i + 1)
    }

    // ---------------------------------------------------------------
    // Viewport computation
    // ---------------------------------------------------------------

    /// Compute column widths from weights and total width.
    fn column_widths(&self, total_width: f32) -> Vec<f32> {
        let sum: f32 = self.column_weights.iter().sum();
        if sum <= 0.0 {
            return vec![total_width / self.columns.len() as f32; self.columns.len()];
        }
        self.column_weights.iter().map(|w| total_width * w / sum).collect()
    }

    /// Walk columns, computing viewports for each pane.
    pub fn for_each_pane_with_viewport<F: FnMut(&Pane, PaneViewport)>(&self, vp: PaneViewport, f: &mut F) {
        let widths = self.column_widths(vp.width);
        let mut x = vp.x;
        for (col, &w) in self.columns.iter().zip(widths.iter()) {
            let col_vp = PaneViewport { x, y: vp.y, width: w, height: vp.height };
            col.for_each_pane_with_viewport(col_vp, f);
            x += w;
        }
    }

    /// Compute the viewport for a specific pane by id.
    pub fn viewport_for_pane(&self, id: PaneId, vp: PaneViewport) -> Option<PaneViewport> {
        let widths = self.column_widths(vp.width);
        let mut x = vp.x;
        for (col, &w) in self.columns.iter().zip(widths.iter()) {
            let col_vp = PaneViewport { x, y: vp.y, width: w, height: vp.height };
            if let Some(result) = col.viewport_for_pane(id, col_vp) {
                return Some(result);
            }
            x += w;
        }
        None
    }

    /// Hit-test: find which pane contains the pixel coordinate (x, y).
    pub fn hit_test(&self, x: f32, y: f32, vp: PaneViewport) -> Option<(&Pane, PaneViewport)> {
        let widths = self.column_widths(vp.width);
        let mut col_x = vp.x;
        for (col, &w) in self.columns.iter().zip(widths.iter()) {
            let col_vp = PaneViewport { x: col_x, y: vp.y, width: w, height: vp.height };
            if let Some(result) = col.hit_test(x, y, col_vp) {
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
    pub fn collect_separators(&self, vp: PaneViewport, out: &mut Vec<(f32, f32, f32, f32)>) {
        let widths = self.column_widths(vp.width);
        let mut x = vp.x;
        for (i, (col, &w)) in self.columns.iter().zip(widths.iter()).enumerate() {
            let col_vp = PaneViewport { x, y: vp.y, width: w, height: vp.height };
            // Vertical separator between columns
            if i > 0 {
                out.push((x, vp.y, x, vp.y + vp.height));
            }
            // Horizontal separators within column
            col.collect_separators(col_vp, out);
            x += w;
        }
    }

    /// Collect separator info for mouse hit-testing and dragging.
    pub fn collect_separator_info(&self, vp: PaneViewport, out: &mut Vec<SeparatorInfo>) {
        let widths = self.column_widths(vp.width);
        let mut x = vp.x;
        for (i, (col, &w)) in self.columns.iter().zip(widths.iter()).enumerate() {
            let col_vp = PaneViewport { x, y: vp.y, width: w, height: vp.height };
            // Column separator between columns[i-1] and columns[i]
            if i > 0 {
                out.push(SeparatorInfo {
                    pos: x,
                    cross_start: vp.y,
                    cross_end: vp.y + vp.height,
                    is_hsplit: true,
                    origin_ratio: 0.0, // not used for column seps
                    parent_dim: vp.width,
                    node_ptr: 0, // not used for column seps
                    column_sep_index: Some(i - 1),
                });
            }
            // VSplit separators within column
            col.collect_separator_info(col_vp, out);
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
        self.columns.insert(idx + 1, ColumnTree::Leaf(new_pane));
        self.column_weights.insert(idx + 1, avg_weight);
        new_id
    }

    /// Append a new column at the end.
    /// Returns the new pane's id.
    pub fn append_column(&mut self, new_pane: Pane) -> PaneId {
        let new_id = new_pane.id;
        let avg_weight: f32 = self.column_weights.iter().sum::<f32>() / self.columns.len() as f32;
        self.columns.push(ColumnTree::Leaf(new_pane));
        self.column_weights.push(avg_weight);
        new_id
    }

    /// Split the pane with target_id vertically (insert new pane below it within its column).
    pub fn vsplit_at_pane(&mut self, target_id: PaneId, new_pane: Pane) {
        if let Some(idx) = self.column_index_of(target_id) {
            // Create a temporary leaf to swap in
            let old_col = unsafe {
                let ptr = &mut self.columns[idx] as *mut ColumnTree;
                std::ptr::read(ptr)
            };
            let new_col = old_col.with_vsplit(target_id, new_pane);
            unsafe {
                let ptr = &mut self.columns[idx] as *mut ColumnTree;
                std::ptr::write(ptr, new_col);
            }
        }
    }

    /// Split at the bottom of the column containing the focused pane.
    /// Wraps the entire column in a VSplit with the new pane at the bottom.
    pub fn vsplit_root_at_column(&mut self, new_pane: Pane) {
        let focused_id = self.focused_pane;
        if let Some(idx) = self.column_index_of(focused_id) {
            let old_col = unsafe {
                let ptr = &mut self.columns[idx] as *mut ColumnTree;
                std::ptr::read(ptr)
            };
            let new_col = ColumnTree::VSplit {
                top: Box::new(old_col),
                bottom: Box::new(ColumnTree::Leaf(new_pane)),
                ratio: 0.5,
                custom_ratio: false,
            };
            unsafe {
                let ptr = &mut self.columns[idx] as *mut ColumnTree;
                std::ptr::write(ptr, new_col);
            }
        }
    }

    // ---------------------------------------------------------------
    // Remove pane
    // ---------------------------------------------------------------

    /// Remove a pane by id. Returns true if the tab still has panes.
    /// Returns false if the tab became empty (caller should close it).
    pub fn remove_pane(&mut self, id: PaneId) -> bool {
        if let Some(col_idx) = self.column_index_of(id) {
            // Check if it's the sole occupant of this column
            if matches!(self.columns[col_idx], ColumnTree::Leaf(ref p) if p.id == id) {
                // Remove entire column
                self.columns.remove(col_idx);
                let removed_weight = self.column_weights.remove(col_idx);
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
                // Remove pane from within the column's VSplit
                let old_col = unsafe {
                    let ptr = &mut self.columns[col_idx] as *mut ColumnTree;
                    std::ptr::read(ptr)
                };
                match old_col.remove_pane(id) {
                    Some(new_col) => {
                        unsafe {
                            let ptr = &mut self.columns[col_idx] as *mut ColumnTree;
                            std::ptr::write(ptr, new_col);
                        }
                    }
                    None => {
                        // Column became empty (remove_pane consumed the tree).
                        // The slot is now invalid; remove it from the Vec.
                        // Vec::remove shifts elements, so the invalid slot is overwritten.
                        // We must NOT drop the old value (already consumed), so use
                        // ManuallyDrop-style approach: overwrite the slot first.
                        // Actually, remove_pane returned None meaning the Leaf was the target.
                        // The column is gone. Just remove the slot (it's been consumed, not dropped).
                        // Vec::remove will shift elements over the invalid slot.
                        // To be safe, we need to write a valid value before remove can drop it.
                        // But remove_pane already consumed self, so there's nothing to drop.
                        // We can just remove the entry since it points to consumed memory.
                        // However, Vec::remove will try to drop the element. We need to prevent that.
                        // Use set_len trick:
                        let len = self.columns.len();
                        // Shift everything after col_idx left by 1
                        if col_idx + 1 < len {
                            unsafe {
                                let ptr = self.columns.as_mut_ptr();
                                std::ptr::copy(ptr.add(col_idx + 1), ptr.add(col_idx), len - col_idx - 1);
                            }
                        }
                        unsafe { self.columns.set_len(len - 1); }
                        let removed_weight = self.column_weights.remove(col_idx);
                        if self.columns.is_empty() {
                            return false;
                        }
                        let sum: f32 = self.column_weights.iter().sum();
                        if sum > 0.0 {
                            let scale = (sum + removed_weight) / sum;
                            for w in &mut self.column_weights {
                                *w *= scale;
                            }
                        }
                    }
                }
            }
            true
        } else {
            true // pane not found, nothing to remove
        }
    }

    /// Extract a pane by id, returning true if successful.
    /// The extracted pane is returned separately, the tab retains the remainder.
    pub fn extract_pane(&mut self, id: PaneId) -> Option<ColumnTree> {
        let col_idx = self.column_index_of(id)?;

        if matches!(self.columns[col_idx], ColumnTree::Leaf(ref p) if p.id == id) {
            // Sole occupant: remove column
            let extracted = self.columns.remove(col_idx);
            let removed_weight = self.column_weights.remove(col_idx);
            if !self.columns.is_empty() {
                let sum: f32 = self.column_weights.iter().sum();
                if sum > 0.0 {
                    let scale = (sum + removed_weight) / sum;
                    for w in &mut self.column_weights {
                        *w *= scale;
                    }
                }
            }
            Some(extracted)
        } else {
            // Extract from within VSplit — need a dummy to swap in
            // Use ptr::read/write pattern to take ownership without Clone
            let old_col = unsafe {
                let ptr = &mut self.columns[col_idx] as *mut ColumnTree;
                std::ptr::read(ptr)
            };
            match old_col.extract_pane(id) {
                Some((extracted, remainder)) => {
                    unsafe {
                        let ptr = &mut self.columns[col_idx] as *mut ColumnTree;
                        std::ptr::write(ptr, remainder);
                    }
                    Some(extracted)
                }
                None => {
                    // extract_pane consumed old_col and returned None
                    // This means the column was a Leaf (shouldn't reach here)
                    // or pane wasn't found. Either way, column is gone.
                    // Remove it to avoid invalid memory.
                    self.columns.remove(col_idx);
                    self.column_weights.remove(col_idx);
                    None
                }
            }
        }
    }

    // ---------------------------------------------------------------
    // Resize
    // ---------------------------------------------------------------

    /// Adjust VSplit ratio directionally (Mode 1 vertical).
    pub fn adjust_ratio_directional(&mut self, id: PaneId, delta: f32, axis: SplitAxis) -> bool {
        match axis {
            SplitAxis::Horizontal => {
                // Horizontal resize: adjust column weights
                self.adjust_column_weight_directional(id, delta)
            }
            SplitAxis::Vertical => {
                // Vertical resize: delegate to column's VSplit
                if let Some(col_idx) = self.column_index_of(id) {
                    self.columns[col_idx].adjust_ratio_directional(id, delta)
                } else {
                    false
                }
            }
        }
    }

    /// Fallback: adjust nearest separator.
    pub fn adjust_ratio_nearest(&mut self, id: PaneId, delta: f32, axis: SplitAxis) -> bool {
        match axis {
            SplitAxis::Horizontal => {
                // Try the other direction for column weights
                self.adjust_column_weight_nearest(id, delta)
            }
            SplitAxis::Vertical => {
                if let Some(col_idx) = self.column_index_of(id) {
                    self.columns[col_idx].adjust_ratio_nearest(id, delta)
                } else {
                    false
                }
            }
        }
    }

    /// Adjust column weight: move separator in the arrow direction.
    /// delta > 0 (Right): separator to the right of focused column — column grows.
    /// delta < 0 (Left): separator to the left — column grows to the left.
    fn adjust_column_weight_directional(&mut self, id: PaneId, delta: f32) -> bool {
        let col_idx = match self.column_index_of(id) {
            Some(i) => i,
            None => return false,
        };
        if self.columns.len() < 2 { return false; }

        let step = delta.abs() * 0.5; // scale down for weight transfer
        if delta > 0.0 {
            // Grow right: transfer from column[col_idx + 1] to column[col_idx]
            if col_idx + 1 < self.columns.len() {
                let transfer = (self.column_weights[col_idx + 1] * step).min(self.column_weights[col_idx + 1] * 0.8);
                if transfer > 0.001 {
                    self.column_weights[col_idx] += transfer;
                    self.column_weights[col_idx + 1] -= transfer;
                    return true;
                }
            }
        } else {
            // Grow left: transfer from column[col_idx - 1] to column[col_idx]
            if col_idx > 0 {
                let transfer = (self.column_weights[col_idx - 1] * step).min(self.column_weights[col_idx - 1] * 0.8);
                if transfer > 0.001 {
                    self.column_weights[col_idx] += transfer;
                    self.column_weights[col_idx - 1] -= transfer;
                    return true;
                }
            }
        }
        false
    }

    /// Fallback: push the nearest separator in the arrow direction.
    ///
    /// Semantics: "move the separator", NOT "grow the focused pane".
    /// Example: focused is rightmost column, user presses →.
    /// Directional fails (no right neighbor). Nearest finds the left separator
    /// and pushes it rightward — the left neighbor grows, focused shrinks.
    /// The separator moves in the direction of the arrow, which is the expected UX.
    fn adjust_column_weight_nearest(&mut self, id: PaneId, delta: f32) -> bool {
        let col_idx = match self.column_index_of(id) {
            Some(i) => i,
            None => return false,
        };
        if self.columns.len() < 2 { return false; }

        let step = delta.abs() * 0.5;
        if delta > 0.0 {
            // → arrow, no right neighbor: push left separator rightward
            // (left neighbor grows, focused shrinks)
            if col_idx > 0 {
                let transfer = (self.column_weights[col_idx - 1] * step).min(self.column_weights[col_idx - 1] * 0.8);
                if transfer > 0.001 {
                    self.column_weights[col_idx - 1] += transfer;
                    self.column_weights[col_idx] -= transfer;
                    return true;
                }
            }
        } else {
            // ← arrow, no left neighbor: push right separator leftward
            // (right neighbor grows, focused shrinks)
            if col_idx + 1 < self.columns.len() {
                let transfer = (self.column_weights[col_idx + 1] * step).min(self.column_weights[col_idx + 1] * 0.8);
                if transfer > 0.001 {
                    self.column_weights[col_idx + 1] += transfer;
                    self.column_weights[col_idx] -= transfer;
                    return true;
                }
            }
        }
        false
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
            let col_max = self.columns[i].max_leaf_width_px(col_w);
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

    /// Set the ratio of a split node identified by its pointer address (VSplit only).
    pub fn set_ratio_by_ptr(&mut self, ptr: usize, new_ratio: f32) -> bool {
        for col in &mut self.columns {
            if col.set_ratio_by_ptr(ptr, new_ratio) {
                return true;
            }
        }
        false
    }

    /// Set column weights by dragging a column separator.
    /// `col_idx` is the index such that the separator is between columns[col_idx] and columns[col_idx+1].
    pub fn set_column_weights_by_drag(&mut self, col_idx: usize, delta_px: f32, total_width: f32) {
        if col_idx + 1 >= self.columns.len() { return; }
        let sum: f32 = self.column_weights.iter().sum();
        if sum <= 0.0 || total_width <= 0.0 { return; }

        let delta_weight = delta_px / total_width * sum;
        let min_weight = sum * 0.05; // minimum 5% of total

        let new_left = (self.column_weights[col_idx] + delta_weight).max(min_weight);
        let new_right = (self.column_weights[col_idx + 1] - delta_weight).max(min_weight);

        // Only apply if both remain above minimum
        if new_left >= min_weight && new_right >= min_weight {
            self.column_weights[col_idx] = new_left;
            self.column_weights[col_idx + 1] = new_right;
        }
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
                true
            }
            NavDirection::Up | NavDirection::Down => {
                // Swap within column's VSplit
                let idx = match self.column_index_of(id1) { Some(i) => i, None => return false };
                self.columns[idx].swap_panes(id1, id2)
            }
        }
    }

    /// Reparent pane: for 2-leaf VSplits, rotate orientation or swap.
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

                let is_sole_leaf = matches!(self.columns[col_idx], ColumnTree::Leaf(ref p) if p.id == focused_id);

                if is_sole_leaf {
                    // Single leaf column — remove column and merge into target
                    let col = self.columns.remove(col_idx);
                    let _weight = self.column_weights.remove(col_idx);
                    let adj_target = if target_idx > col_idx { target_idx - 1 } else { target_idx };
                    let old_target = unsafe {
                        let ptr = &mut self.columns[adj_target] as *mut ColumnTree;
                        std::ptr::read(ptr)
                    };
                    let new_target = ColumnTree::VSplit {
                        top: Box::new(old_target),
                        bottom: Box::new(col),
                        ratio: 0.5,
                        custom_ratio: false,
                    };
                    unsafe {
                        let ptr = &mut self.columns[adj_target] as *mut ColumnTree;
                        std::ptr::write(ptr, new_target);
                    }
                } else {
                    // Extract pane from within a VSplit column
                    let old_col = unsafe {
                        let ptr = &mut self.columns[col_idx] as *mut ColumnTree;
                        std::ptr::read(ptr)
                    };
                    match old_col.extract_pane(focused_id) {
                        Some((extracted, remainder)) => {
                            unsafe {
                                let ptr = &mut self.columns[col_idx] as *mut ColumnTree;
                                std::ptr::write(ptr, remainder);
                            }
                            // Add extracted pane to target column at bottom
                            let old_target = unsafe {
                                let ptr = &mut self.columns[target_idx] as *mut ColumnTree;
                                std::ptr::read(ptr)
                            };
                            let new_target = ColumnTree::VSplit {
                                top: Box::new(old_target),
                                bottom: Box::new(extracted),
                                ratio: 0.5,
                                custom_ratio: false,
                            };
                            unsafe {
                                let ptr = &mut self.columns[target_idx] as *mut ColumnTree;
                                std::ptr::write(ptr, new_target);
                            }
                        }
                        None => {
                            // Shouldn't happen since we checked is_sole_leaf above
                            // But extract_pane consumed old_col. Remove the invalid column.
                            let len = self.columns.len();
                            if col_idx + 1 < len {
                                unsafe {
                                    let ptr = self.columns.as_mut_ptr();
                                    std::ptr::copy(ptr.add(col_idx + 1), ptr.add(col_idx), len - col_idx - 1);
                                }
                            }
                            unsafe { self.columns.set_len(len - 1); }
                            self.column_weights.remove(col_idx);
                            return false;
                        }
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

    /// Equalize: reset all column weights to 1.0 and all VSplit ratios to 0.5.
    pub fn equalize(&mut self) {
        for w in &mut self.column_weights {
            *w = 1.0;
        }
        for col in &mut self.columns {
            col.equalize();
        }
    }

    /// Check if this tab has only a single pane.
    pub fn is_single_pane(&self) -> bool {
        self.columns.len() == 1 && matches!(self.columns[0], ColumnTree::Leaf(_))
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
}

impl Pane {
    pub fn spawn(cols: u16, rows: u16, config: &Config, working_dir: Option<&str>) -> Result<Self, Box<dyn std::error::Error>> {
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
        )?;

        let id = alloc_pane_id();
        log::debug!("Pane spawned: id={}, cols={}, rows={}", id, cols, rows);
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
        if let Some(ref custom) = self.custom_title {
            return custom.clone();
        }
        let term = self.terminal.read();
        if let Some(ref title) = term.title {
            return title.clone();
        }
        if let Some(ref cwd) = term.cwd {
            if let Some(base) = std::path::Path::new(cwd).file_name() {
                return base.to_string_lossy().to_string();
            }
        }
        fallback.to_string()
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

/// Compute split sizes accounting for minimized children.
/// When a subtree is fully minimized, it collapses to MINIMIZED_BAR_PX.
fn split_sizes(total: f32, ratio: f32, first_minimized: bool, second_minimized: bool) -> (f32, f32) {
    match (first_minimized, second_minimized) {
        (true, true) => (MINIMIZED_BAR_PX, MINIMIZED_BAR_PX),
        (true, false) => (MINIMIZED_BAR_PX, total - MINIMIZED_BAR_PX),
        (false, true) => (total - MINIMIZED_BAR_PX, MINIMIZED_BAR_PX),
        (false, false) => (total * ratio, total * (1.0 - ratio)),
    }
}

/// Vertical split tree within a single column.
pub enum ColumnTree {
    Leaf(Pane),
    VSplit {
        top: Box<ColumnTree>,
        bottom: Box<ColumnTree>,
        /// Fraction of height allocated to the top child (0.0–1.0).
        ratio: f32,
        /// Whether the ratio was manually adjusted by the user.
        custom_ratio: bool,
    },
}

impl ColumnTree {
    /// Returns true if this subtree is fully minimized (all leaves are minimized).
    pub fn is_fully_minimized(&self) -> bool {
        match self {
            ColumnTree::Leaf(p) => p.minimized,
            ColumnTree::VSplit { top, bottom, .. } => {
                top.is_fully_minimized() && bottom.is_fully_minimized()
            }
        }
    }

    /// Find a pane by id.
    pub fn pane(&self, id: PaneId) -> Option<&Pane> {
        match self {
            ColumnTree::Leaf(p) => {
                if p.id == id { Some(p) } else { None }
            }
            ColumnTree::VSplit { top, bottom, .. } => {
                top.pane(id).or_else(|| bottom.pane(id))
            }
        }
    }

    pub fn pane_mut(&mut self, id: PaneId) -> Option<&mut Pane> {
        match self {
            ColumnTree::Leaf(p) => {
                if p.id == id { Some(p) } else { None }
            }
            ColumnTree::VSplit { top, bottom, .. } => {
                top.pane_mut(id).or_else(|| bottom.pane_mut(id))
            }
        }
    }

    /// Return the first (topmost) pane.
    pub fn first_pane(&self) -> &Pane {
        match self {
            ColumnTree::Leaf(p) => p,
            ColumnTree::VSplit { top, .. } => top.first_pane(),
        }
    }

    /// Return the last (bottommost) pane.
    pub fn last_pane(&self) -> &Pane {
        match self {
            ColumnTree::Leaf(p) => p,
            ColumnTree::VSplit { bottom, .. } => bottom.last_pane(),
        }
    }

    /// Iterate over all panes (depth-first).
    pub fn for_each_pane<F: FnMut(&Pane)>(&self, f: &mut F) {
        match self {
            ColumnTree::Leaf(p) => f(p),
            ColumnTree::VSplit { top, bottom, .. } => {
                top.for_each_pane(f);
                bottom.for_each_pane(f);
            }
        }
    }

    /// Check if this tree contains a pane with the given id.
    pub fn contains(&self, id: PaneId) -> bool {
        self.pane(id).is_some()
    }

    /// Count leaves in this column tree (for VSplit chain counting).
    pub fn leaf_count(&self) -> usize {
        match self {
            ColumnTree::Leaf(_) => 1,
            ColumnTree::VSplit { top, bottom, .. } => {
                top.leaf_count() + bottom.leaf_count()
            }
        }
    }

    /// Walk the tree, computing viewports by splitting according to ratios.
    pub fn for_each_pane_with_viewport<F: FnMut(&Pane, PaneViewport)>(&self, vp: PaneViewport, f: &mut F) {
        match self {
            ColumnTree::Leaf(p) => f(p, vp),
            ColumnTree::VSplit { top, bottom, ratio, .. } => {
                let (top_h, bot_h) = split_sizes(vp.height, *ratio, top.is_fully_minimized(), bottom.is_fully_minimized());
                let top_vp = PaneViewport { x: vp.x, y: vp.y, width: vp.width, height: top_h };
                let bot_vp = PaneViewport { x: vp.x, y: vp.y + top_h, width: vp.width, height: bot_h };
                top.for_each_pane_with_viewport(top_vp, f);
                bottom.for_each_pane_with_viewport(bot_vp, f);
            }
        }
    }

    /// Compute the viewport for a specific pane by id.
    pub fn viewport_for_pane(&self, id: PaneId, vp: PaneViewport) -> Option<PaneViewport> {
        match self {
            ColumnTree::Leaf(p) => {
                if p.id == id { Some(vp) } else { None }
            }
            ColumnTree::VSplit { top, bottom, ratio, .. } => {
                let (top_h, bot_h) = split_sizes(vp.height, *ratio, top.is_fully_minimized(), bottom.is_fully_minimized());
                let top_vp = PaneViewport { x: vp.x, y: vp.y, width: vp.width, height: top_h };
                let bot_vp = PaneViewport { x: vp.x, y: vp.y + top_h, width: vp.width, height: bot_h };
                top.viewport_for_pane(id, top_vp)
                    .or_else(|| bottom.viewport_for_pane(id, bot_vp))
            }
        }
    }

    /// Hit-test within this column.
    pub fn hit_test(&self, x: f32, y: f32, vp: PaneViewport) -> Option<(&Pane, PaneViewport)> {
        if x < vp.x || x >= vp.x + vp.width || y < vp.y || y >= vp.y + vp.height {
            return None;
        }
        match self {
            ColumnTree::Leaf(p) => Some((p, vp)),
            ColumnTree::VSplit { top, bottom, ratio, .. } => {
                let (top_h, bot_h) = split_sizes(vp.height, *ratio, top.is_fully_minimized(), bottom.is_fully_minimized());
                let top_vp = PaneViewport { x: vp.x, y: vp.y, width: vp.width, height: top_h };
                let bot_vp = PaneViewport { x: vp.x, y: vp.y + top_h, width: vp.width, height: bot_h };
                top.hit_test(x, y, top_vp)
                    .or_else(|| bottom.hit_test(x, y, bot_vp))
            }
        }
    }

    /// Collect horizontal separator lines within this column.
    pub fn collect_separators(&self, vp: PaneViewport, out: &mut Vec<(f32, f32, f32, f32)>) {
        match self {
            ColumnTree::Leaf(_) => {}
            ColumnTree::VSplit { top, bottom, ratio, .. } => {
                let (top_h, bot_h) = split_sizes(vp.height, *ratio, top.is_fully_minimized(), bottom.is_fully_minimized());
                let split_y = vp.y + top_h;
                out.push((vp.x, split_y, vp.x + vp.width, split_y));
                let top_vp = PaneViewport { x: vp.x, y: vp.y, width: vp.width, height: top_h };
                let bot_vp = PaneViewport { x: vp.x, y: split_y, width: vp.width, height: bot_h };
                top.collect_separators(top_vp, out);
                bottom.collect_separators(bot_vp, out);
            }
        }
    }

    /// Collect separator info for hit-testing within this column (VSplit separators only).
    pub fn collect_separator_info(&self, vp: PaneViewport, out: &mut Vec<SeparatorInfo>) {
        match self {
            ColumnTree::Leaf(_) => {}
            ColumnTree::VSplit { top, bottom, ratio, .. } => {
                let first_min = top.is_fully_minimized();
                let second_min = bottom.is_fully_minimized();
                let (top_h, bot_h) = split_sizes(vp.height, *ratio, first_min, second_min);
                let split_y = vp.y + top_h;
                if !first_min && !second_min {
                    out.push(SeparatorInfo {
                        pos: split_y,
                        cross_start: vp.x,
                        cross_end: vp.x + vp.width,
                        is_hsplit: false,
                        origin_ratio: *ratio,
                        parent_dim: vp.height,
                        node_ptr: self as *const ColumnTree as usize,
                        column_sep_index: None,
                    });
                }
                let top_vp = PaneViewport { x: vp.x, y: vp.y, width: vp.width, height: top_h };
                let bot_vp = PaneViewport { x: vp.x, y: split_y, width: vp.width, height: bot_h };
                top.collect_separator_info(top_vp, out);
                bottom.collect_separator_info(bot_vp, out);
            }
        }
    }

    /// Remove a pane by id. Returns `None` if the tree becomes empty (was a Leaf),
    /// or `Some(new_tree)` with the pane removed and its sibling promoted.
    pub fn remove_pane(self, id: PaneId) -> Option<ColumnTree> {
        match self {
            ColumnTree::Leaf(p) => {
                if p.id == id { None } else { Some(ColumnTree::Leaf(p)) }
            }
            ColumnTree::VSplit { top, bottom, ratio, custom_ratio } => {
                if top.contains(id) {
                    match top.remove_pane(id) {
                        Some(new_top) => {
                            let new_ratio = if custom_ratio { ratio } else {
                                let tc = new_top.leaf_count() as f32;
                                tc / (tc + bottom.leaf_count() as f32)
                            };
                            Some(ColumnTree::VSplit { top: Box::new(new_top), bottom, ratio: new_ratio, custom_ratio })
                        }
                        None => Some(*bottom),
                    }
                } else {
                    match bottom.remove_pane(id) {
                        Some(new_bottom) => {
                            let new_ratio = if custom_ratio { ratio } else {
                                let tc = top.leaf_count() as f32;
                                tc / (tc + new_bottom.leaf_count() as f32)
                            };
                            Some(ColumnTree::VSplit { top, bottom: Box::new(new_bottom), ratio: new_ratio, custom_ratio })
                        }
                        None => Some(*top),
                    }
                }
            }
        }
    }

    /// Extract a pane by id, returning (extracted_pane, remaining_tree).
    pub fn extract_pane(self, id: PaneId) -> Option<(ColumnTree, ColumnTree)> {
        match self {
            ColumnTree::Leaf(_) => None,
            ColumnTree::VSplit { top, bottom, ratio, custom_ratio } => {
                if let ColumnTree::Leaf(ref p) = *top {
                    if p.id == id {
                        return Some((*top, *bottom));
                    }
                }
                if let ColumnTree::Leaf(ref p) = *bottom {
                    if p.id == id {
                        return Some((*bottom, *top));
                    }
                }
                if top.contains(id) {
                    match top.extract_pane(id) {
                        Some((extracted, remaining_top)) => {
                            let new_ratio = if custom_ratio { ratio } else {
                                let tc = remaining_top.leaf_count() as f32;
                                tc / (tc + bottom.leaf_count() as f32)
                            };
                            let remainder = ColumnTree::VSplit {
                                top: Box::new(remaining_top),
                                bottom,
                                ratio: new_ratio,
                                custom_ratio,
                            };
                            Some((extracted, remainder))
                        }
                        None => None,
                    }
                } else {
                    match bottom.extract_pane(id) {
                        Some((extracted, remaining_bottom)) => {
                            let new_ratio = if custom_ratio { ratio } else {
                                let tc = top.leaf_count() as f32;
                                tc / (tc + remaining_bottom.leaf_count() as f32)
                            };
                            let remainder = ColumnTree::VSplit {
                                top,
                                bottom: Box::new(remaining_bottom),
                                ratio: new_ratio,
                                custom_ratio,
                            };
                            Some((extracted, remainder))
                        }
                        None => None,
                    }
                }
            }
        }
    }

    /// Split the pane with given id: old stays on top, new goes below.
    pub fn with_vsplit(self, id: PaneId, new_pane: Pane) -> ColumnTree {
        match self {
            ColumnTree::Leaf(p) if p.id == id => {
                ColumnTree::VSplit {
                    top: Box::new(ColumnTree::Leaf(p)),
                    bottom: Box::new(ColumnTree::Leaf(new_pane)),
                    ratio: 0.5,
                    custom_ratio: false,
                }
            }
            ColumnTree::Leaf(_) => self,
            ColumnTree::VSplit { top, bottom, ratio, custom_ratio } => {
                if top.contains(id) {
                    ColumnTree::VSplit {
                        top: Box::new(top.with_vsplit(id, new_pane)),
                        bottom,
                        ratio,
                        custom_ratio,
                    }
                } else {
                    ColumnTree::VSplit {
                        top,
                        bottom: Box::new(bottom.with_vsplit(id, new_pane)),
                        ratio,
                        custom_ratio,
                    }
                }
            }
        }
    }

    /// Equalize VSplit ratios within this column.
    pub fn equalize(&mut self) {
        match self {
            ColumnTree::Leaf(_) => {}
            ColumnTree::VSplit { top, bottom, ratio, custom_ratio, .. } => {
                top.equalize();
                bottom.equalize();
                let top_count = top.leaf_count();
                let total = top_count + bottom.leaf_count();
                *ratio = top_count as f32 / total as f32;
                *custom_ratio = false;
            }
        }
    }

    /// Adjust VSplit ratio directionally.
    pub fn adjust_ratio_directional(&mut self, id: PaneId, delta: f32) -> bool {
        match self {
            ColumnTree::Leaf(_) => false,
            ColumnTree::VSplit { top, bottom, ratio, custom_ratio, .. } => {
                let blocked = top.is_fully_minimized() || bottom.is_fully_minimized();
                if top.adjust_ratio_directional(id, delta) { return true; }
                if bottom.adjust_ratio_directional(id, delta) { return true; }
                if blocked { return false; }
                // delta > 0 (Down): pane in top child → separator moves down
                // delta < 0 (Up): pane in bottom child → separator moves up
                if delta > 0.0 && top.contains(id) {
                    *ratio = (*ratio + delta).clamp(0.1, 0.9);
                    *custom_ratio = true;
                    true
                } else if delta < 0.0 && bottom.contains(id) {
                    *ratio = (*ratio + delta).clamp(0.1, 0.9);
                    *custom_ratio = true;
                    true
                } else {
                    false
                }
            }
        }
    }

    /// Fallback ratio adjust (nearest separator).
    pub fn adjust_ratio_nearest(&mut self, id: PaneId, delta: f32) -> bool {
        match self {
            ColumnTree::Leaf(_) => false,
            ColumnTree::VSplit { top, bottom, ratio, custom_ratio, .. } => {
                let blocked = top.is_fully_minimized() || bottom.is_fully_minimized();
                if top.contains(id) {
                    if top.adjust_ratio_nearest(id, delta) { return true; }
                    if blocked { return false; }
                    *ratio = (*ratio + delta).clamp(0.1, 0.9);
                    *custom_ratio = true;
                    true
                } else if bottom.contains(id) {
                    if bottom.adjust_ratio_nearest(id, delta) { return true; }
                    if blocked { return false; }
                    *ratio = (*ratio + delta).clamp(0.1, 0.9);
                    *custom_ratio = true;
                    true
                } else {
                    false
                }
            }
        }
    }

    /// Returns the maximum leaf width as a fraction of column width (always 1.0 since VSplit doesn't split width).
    pub fn max_leaf_width_fraction(&self) -> f32 {
        1.0
    }

    /// Returns the maximum leaf width in pixels.
    fn max_leaf_width_px(&self, total: f32) -> f32 {
        // VSplit doesn't split width, so all leaves have the same width
        total
    }

    /// Set the ratio of a split node identified by its pointer address.
    pub fn set_ratio_by_ptr(&mut self, ptr: usize, new_ratio: f32) -> bool {
        let self_ptr = self as *const ColumnTree as usize;
        match self {
            ColumnTree::Leaf(_) => false,
            ColumnTree::VSplit { top, bottom, ratio, custom_ratio, .. } => {
                if self_ptr == ptr {
                    *ratio = new_ratio.clamp(0.1, 0.9);
                    *custom_ratio = true;
                    true
                } else {
                    top.set_ratio_by_ptr(ptr, new_ratio)
                        || bottom.set_ratio_by_ptr(ptr, new_ratio)
                }
            }
        }
    }

    /// Swap the Pane values of two leaves.
    pub fn swap_panes(&mut self, id1: PaneId, id2: PaneId) -> bool {
        if id1 == id2 { return false; }
        let ptr1 = self.pane_leaf_mut(id1);
        let ptr2 = self.pane_leaf_mut(id2);
        match (ptr1, ptr2) {
            (Some(p1), Some(p2)) => {
                unsafe { std::ptr::swap(p1, p2) };
                true
            }
            _ => false,
        }
    }

    /// Reparent pane within a column's VSplit.
    pub fn reparent_pane(&mut self, focused_id: PaneId, dir: NavDirection) -> bool {
        match self {
            ColumnTree::Leaf(_) => false,
            ColumnTree::VSplit { top, bottom, .. } => {
                if let (ColumnTree::Leaf(_), ColumnTree::Leaf(_)) = (top.as_ref(), bottom.as_ref()) {
                    let focused_is_first = top.contains(focused_id);
                    if !focused_is_first && !bottom.contains(focused_id) {
                        return false;
                    }
                    self.reparent_2leaf(focused_is_first, dir)
                } else {
                    top.reparent_pane(focused_id, dir)
                        || bottom.reparent_pane(focused_id, dir)
                }
            }
        }
    }

    /// Core reparent logic for a VSplit with exactly 2 Leaf children.
    fn reparent_2leaf(&mut self, focused_is_first: bool, dir: NavDirection) -> bool {
        // VSplit only does vertical reparent (up/down swap)
        match dir {
            NavDirection::Up | NavDirection::Down => {
                let moving_forward = matches!(dir, NavDirection::Down);
                if moving_forward == focused_is_first {
                    self.swap_children();
                    true
                } else {
                    false
                }
            }
            _ => false,
        }
    }

    /// Swap the two children of this VSplit.
    fn swap_children(&mut self) {
        match self {
            ColumnTree::VSplit { top, bottom, .. } => std::mem::swap(top, bottom),
            ColumnTree::Leaf(_) => {}
        }
    }

    /// Return a raw mutable pointer to the Pane inside a Leaf node.
    fn pane_leaf_mut(&mut self, id: PaneId) -> Option<*mut Pane> {
        match self {
            ColumnTree::Leaf(p) => {
                if p.id == id { Some(p as *mut Pane) } else { None }
            }
            ColumnTree::VSplit { top, bottom, .. } => {
                top.pane_leaf_mut(id).or_else(|| bottom.pane_leaf_mut(id))
            }
        }
    }
}
