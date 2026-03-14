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
    /// Pixel position of the separator line (x for hsplit, y for vsplit).
    pub pos: f32,
    /// Start of the separator extent on the cross-axis.
    pub cross_start: f32,
    /// End of the separator extent on the cross-axis.
    pub cross_end: f32,
    /// Whether this is an HSplit separator (vertical line).
    pub is_hsplit: bool,
    /// Current ratio of the parent node.
    pub origin_ratio: f32,
    /// Parent dimension along the split axis (width for hsplit, height for vsplit).
    pub parent_dim: f32,
    /// Pointer address of the split node, used as a stable identifier.
    pub node_ptr: usize,
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

/// A tab: owns a split tree and tracks which pane is focused within it.
#[allow(dead_code)]
pub struct Tab {
    pub id: TabId,
    pub tree: SplitTree,
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
            tree: SplitTree::Leaf(pane),
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
            tree: SplitTree::Leaf(pane),
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
    /// If a manual override is set, use it. Otherwise: max(screen_width, root_columns * min_split_width).
    pub fn virtual_width(&self, screen_width: f32, min_split_width: f32) -> f32 {
        if self.virtual_width_override > 0.0 {
            self.virtual_width_override.max(screen_width)
        } else {
            let columns = self.tree.chain_count(true, true) as f32;
            (columns * min_split_width).max(screen_width)
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
        if let Some(pane) = self.tree.pane(self.focused_pane) {
            return pane.display_title("shell");
        }
        "shell".to_string()
    }

    /// Drain bell flags from panes and accumulate into tab-level flag.
    /// Returns true if this tab needs attention.
    pub fn check_bell(&mut self) -> bool {
        self.tree.for_each_pane(&mut |pane| {
            if pane.terminal.read().bell.swap(false, std::sync::atomic::Ordering::Relaxed) {
                self.has_bell = true;
            }
        });
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
        self.tree.for_each_pane(&mut |pane| {
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
        self.tree.for_each_pane(&mut |p| {
            if !p.minimized { non_minimized += 1; }
        });
        if non_minimized <= 1 {
            return false; // can't minimize the last visible pane
        }
        if let Some(pane) = self.tree.pane_mut(id) {
            if pane.minimized {
                return false; // already minimized
            }
            pane.minimized = true;
            self.minimized_stack.push(id);
            // Move focus to a non-minimized sibling
            if self.focused_pane == id {
                let mut first_non_minimized = None;
                self.tree.for_each_pane(&mut |p| {
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
        if let Some(pane) = self.tree.pane_mut(id) {
            pane.minimized = false;
        }
        self.minimized_stack.retain(|&pid| pid != id);
    }

    /// Restore the last minimized pane (FILO).
    pub fn restore_last_minimized(&mut self) -> bool {
        if let Some(id) = self.minimized_stack.pop() {
            if let Some(pane) = self.tree.pane_mut(id) {
                pane.minimized = false;
            }
            true
        } else {
            false
        }
    }

    /// Rebuild minimized_stack from the tree (depth-first order). Used after session restore.
    pub fn rebuild_minimized_stack(&mut self) {
        self.minimized_stack.clear();
        self.tree.for_each_pane(&mut |p| {
            if p.minimized {
                self.minimized_stack.push(p.id);
            }
        });
    }

    /// Clear the completion flag (call when switching to this tab).
    pub fn clear_completion(&mut self) {
        self.has_completion = false;
        // Also clear all pane-level flags
        self.tree.for_each_pane(&mut |pane| {
            pane.terminal.read().command_completed.store(false, std::sync::atomic::Ordering::Relaxed);
        });
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

/// Binary tree of splits. Each leaf is a Pane.
pub enum SplitTree {
    Leaf(Pane),
    HSplit {
        left: Box<SplitTree>,
        right: Box<SplitTree>,
        /// Fraction of width allocated to the left child (0.0–1.0).
        ratio: f32,
        /// Whether this split was created at root level (Cmd+E).
        root: bool,
        /// Whether the ratio was manually adjusted by the user (Ctrl+Cmd+Arrow).
        custom_ratio: bool,
    },
    VSplit {
        top: Box<SplitTree>,
        bottom: Box<SplitTree>,
        /// Fraction of height allocated to the top child (0.0–1.0).
        ratio: f32,
        /// Whether this split was created at root level (Cmd+Shift+E).
        root: bool,
        /// Whether the ratio was manually adjusted by the user (Ctrl+Cmd+Arrow).
        custom_ratio: bool,
    },
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

impl SplitTree {
    /// Returns true if this subtree is fully minimized (all leaves are minimized).
    pub fn is_fully_minimized(&self) -> bool {
        match self {
            SplitTree::Leaf(p) => p.minimized,
            SplitTree::HSplit { left, right, .. }
            | SplitTree::VSplit { top: left, bottom: right, .. } => {
                left.is_fully_minimized() && right.is_fully_minimized()
            }
        }
    }

    /// Find a pane by id.
    pub fn pane(&self, id: PaneId) -> Option<&Pane> {
        match self {
            SplitTree::Leaf(p) => {
                if p.id == id { Some(p) } else { None }
            }
            SplitTree::HSplit { left, right, .. }
            | SplitTree::VSplit { top: left, bottom: right, .. } => {
                left.pane(id).or_else(|| right.pane(id))
            }
        }
    }

    pub fn pane_mut(&mut self, id: PaneId) -> Option<&mut Pane> {
        match self {
            SplitTree::Leaf(p) => {
                if p.id == id { Some(p) } else { None }
            }
            SplitTree::HSplit { left, right, .. }
            | SplitTree::VSplit { top: left, bottom: right, .. } => {
                left.pane_mut(id).or_else(|| right.pane_mut(id))
            }
        }
    }

    /// Return the first (leftmost/topmost) pane — useful as the initial focus.
    pub fn first_pane(&self) -> &Pane {
        match self {
            SplitTree::Leaf(p) => p,
            SplitTree::HSplit { left, .. } | SplitTree::VSplit { top: left, .. } => {
                left.first_pane()
            }
        }
    }

    /// Return the last (rightmost/bottommost) pane.
    pub fn last_pane(&self) -> &Pane {
        match self {
            SplitTree::Leaf(p) => p,
            SplitTree::HSplit { right, .. } | SplitTree::VSplit { bottom: right, .. } => {
                right.last_pane()
            }
        }
    }

    /// Iterate over all panes (depth-first).
    pub fn for_each_pane<F: FnMut(&Pane)>(&self, f: &mut F) {
        match self {
            SplitTree::Leaf(p) => f(p),
            SplitTree::HSplit { left, right, .. }
            | SplitTree::VSplit { top: left, bottom: right, .. } => {
                left.for_each_pane(f);
                right.for_each_pane(f);
            }
        }
    }

    /// Mark all panes in the tree as dirty (needs redraw).
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

    /// Remove a pane by id. Returns `None` if the tree becomes empty (was a Leaf),
    /// or `Some(new_tree)` with the pane removed and its sibling promoted.
    pub fn remove_pane(self, id: PaneId) -> Option<SplitTree> {
        match self {
            SplitTree::Leaf(p) => {
                if p.id == id { None } else { Some(SplitTree::Leaf(p)) }
            }
            SplitTree::HSplit { left, right, ratio, root, custom_ratio } => {
                if left.contains(id) {
                    match left.remove_pane(id) {
                        Some(new_left) => {
                            let new_ratio = if custom_ratio { ratio } else {
                                let lc = new_left.chain_count(true, false) as f32;
                                lc / (lc + right.chain_count(true, false) as f32)
                            };
                            Some(SplitTree::HSplit { left: Box::new(new_left), right, ratio: new_ratio, root, custom_ratio })
                        }
                        None => Some(*right),
                    }
                } else {
                    match right.remove_pane(id) {
                        Some(new_right) => {
                            let new_ratio = if custom_ratio { ratio } else {
                                let lc = left.chain_count(true, false) as f32;
                                lc / (lc + new_right.chain_count(true, false) as f32)
                            };
                            Some(SplitTree::HSplit { left, right: Box::new(new_right), ratio: new_ratio, root, custom_ratio })
                        }
                        None => Some(*left),
                    }
                }
            }
            SplitTree::VSplit { top, bottom, ratio, root, custom_ratio } => {
                if top.contains(id) {
                    match top.remove_pane(id) {
                        Some(new_top) => {
                            let new_ratio = if custom_ratio { ratio } else {
                                let tc = new_top.chain_count(false, false) as f32;
                                tc / (tc + bottom.chain_count(false, false) as f32)
                            };
                            Some(SplitTree::VSplit { top: Box::new(new_top), bottom, ratio: new_ratio, root, custom_ratio })
                        }
                        None => Some(*bottom),
                    }
                } else {
                    match bottom.remove_pane(id) {
                        Some(new_bottom) => {
                            let new_ratio = if custom_ratio { ratio } else {
                                let tc = top.chain_count(false, false) as f32;
                                tc / (tc + new_bottom.chain_count(false, false) as f32)
                            };
                            Some(SplitTree::VSplit { top, bottom: Box::new(new_bottom), ratio: new_ratio, root, custom_ratio })
                        }
                        None => Some(*top),
                    }
                }
            }
        }
    }

    /// Extract a pane by id, returning (extracted_pane, remaining_tree).
    /// Returns `None` if pane not found or if the tree is a single leaf (nothing to extract from).
    pub fn extract_pane(self, id: PaneId) -> Option<(SplitTree, SplitTree)> {
        match self {
            SplitTree::Leaf(_) => {
                // Single leaf — can't extract, caller should check first
                None
            }
            SplitTree::HSplit { left, right, ratio, root, custom_ratio } => {
                if let SplitTree::Leaf(ref p) = *left {
                    if p.id == id {
                        return Some((*left, *right));
                    }
                }
                if let SplitTree::Leaf(ref p) = *right {
                    if p.id == id {
                        return Some((*right, *left));
                    }
                }
                // Recurse into subtrees
                if left.contains(id) {
                    match left.extract_pane(id) {
                        Some((extracted, remaining_left)) => {
                            let new_ratio = if custom_ratio { ratio } else {
                                let lc = remaining_left.chain_count(true, false) as f32;
                                lc / (lc + right.chain_count(true, false) as f32)
                            };
                            let remainder = SplitTree::HSplit {
                                left: Box::new(remaining_left),
                                right,
                                ratio: new_ratio,
                                root,
                                custom_ratio,
                            };
                            Some((extracted, remainder))
                        }
                        None => None,
                    }
                } else {
                    match right.extract_pane(id) {
                        Some((extracted, remaining_right)) => {
                            let new_ratio = if custom_ratio { ratio } else {
                                let lc = left.chain_count(true, false) as f32;
                                lc / (lc + remaining_right.chain_count(true, false) as f32)
                            };
                            let remainder = SplitTree::HSplit {
                                left,
                                right: Box::new(remaining_right),
                                ratio: new_ratio,
                                root,
                                custom_ratio,
                            };
                            Some((extracted, remainder))
                        }
                        None => None,
                    }
                }
            }
            SplitTree::VSplit { top, bottom, ratio, root, custom_ratio } => {
                if let SplitTree::Leaf(ref p) = *top {
                    if p.id == id {
                        return Some((*top, *bottom));
                    }
                }
                if let SplitTree::Leaf(ref p) = *bottom {
                    if p.id == id {
                        return Some((*bottom, *top));
                    }
                }
                if top.contains(id) {
                    match top.extract_pane(id) {
                        Some((extracted, remaining_top)) => {
                            let new_ratio = if custom_ratio { ratio } else {
                                let tc = remaining_top.chain_count(false, false) as f32;
                                tc / (tc + bottom.chain_count(false, false) as f32)
                            };
                            let remainder = SplitTree::VSplit {
                                top: Box::new(remaining_top),
                                bottom,
                                ratio: new_ratio,
                                root,
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
                                let tc = top.chain_count(false, false) as f32;
                                tc / (tc + remaining_bottom.chain_count(false, false) as f32)
                            };
                            let remainder = SplitTree::VSplit {
                                top,
                                bottom: Box::new(remaining_bottom),
                                ratio: new_ratio,
                                root,
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

    /// Split the pane with given id. The old pane stays in the first position
    /// (left/top) and the new pane goes in the second (right/bottom).
    /// Consumes self and returns the new tree. Use via `tree = tree.with_split(...)`.
    pub fn with_split(self, id: PaneId, new_pane: Pane, direction: SplitDirection) -> SplitTree {
        match self {
            SplitTree::Leaf(p) if p.id == id => {
                let old = Box::new(SplitTree::Leaf(p));
                let new = Box::new(SplitTree::Leaf(new_pane));
                match direction {
                    SplitDirection::Horizontal => SplitTree::HSplit { left: old, right: new, ratio: 0.5, root: false, custom_ratio: false },
                    SplitDirection::Vertical => SplitTree::VSplit { top: old, bottom: new, ratio: 0.5, root: false, custom_ratio: false },
                }
            }
            SplitTree::Leaf(_) => self,
            SplitTree::HSplit { left, right, ratio, root, custom_ratio } => {
                if left.contains(id) {
                    SplitTree::HSplit { left: Box::new(left.with_split(id, new_pane, direction)), right, ratio, root, custom_ratio }
                } else {
                    SplitTree::HSplit { left, right: Box::new(right.with_split(id, new_pane, direction)), ratio, root, custom_ratio }
                }
            }
            SplitTree::VSplit { top, bottom, ratio, root, custom_ratio } => {
                if top.contains(id) {
                    SplitTree::VSplit { top: Box::new(top.with_split(id, new_pane, direction)), bottom, ratio, root, custom_ratio }
                } else {
                    SplitTree::VSplit { top, bottom: Box::new(bottom.with_split(id, new_pane, direction)), ratio, root, custom_ratio }
                }
            }
        }
    }

    /// Count units in a chain of same-direction splits.
    /// When `root_only` is true, only root-flagged splits are traversed (non-root subtrees count as 1).
    /// When `root_only` is false, all same-direction splits are traversed (counts all leaves).
    pub(crate) fn chain_count(&self, horizontal: bool, root_only: bool) -> usize {
        match self {
            SplitTree::Leaf(_) => 1,
            SplitTree::HSplit { left, right, root, .. } if horizontal && (!root_only || *root) => {
                left.chain_count(true, root_only) + right.chain_count(true, root_only)
            }
            SplitTree::VSplit { top, bottom, root, .. } if !horizontal && (!root_only || *root) => {
                top.chain_count(false, root_only) + bottom.chain_count(false, root_only)
            }
            _ => 1,
        }
    }

    /// Count the number of visual columns (minimum horizontal partitions).
    pub fn columns(&self) -> usize {
        match self {
            SplitTree::Leaf(_) => 1,
            SplitTree::HSplit { left, right, .. } => left.columns() + right.columns(),
            SplitTree::VSplit { top, bottom, .. } => top.columns().min(bottom.columns()),
        }
    }

    /// Find the 1-based column index of the pane with given id.
    /// Returns None if the pane is not found.
    pub fn column_index(&self, id: PaneId) -> Option<usize> {
        match self {
            SplitTree::Leaf(p) => {
                if p.id == id { Some(1) } else { None }
            }
            SplitTree::HSplit { left, right, .. } => {
                if let Some(idx) = left.column_index(id) {
                    Some(idx)
                } else {
                    right.column_index(id).map(|idx| left.columns() + idx)
                }
            }
            SplitTree::VSplit { top, bottom, .. } => {
                let ct = top.columns();
                let cb = bottom.columns();
                let min_c = ct.min(cb);
                if let Some(idx) = top.column_index(id) {
                    Some(((idx - 1) * min_c / ct) + 1)
                } else {
                    bottom.column_index(id).map(|idx| ((idx - 1) * min_c / cb) + 1)
                }
            }
        }
    }

    /// Equalize ratios so that all panes along a same-direction chain get equal space.
    /// For example, HSplit(A, HSplit(B, C)) gets ratios 1/3 and 1/2, giving each pane 1/3.
    pub fn equalize(&mut self) {
        match self {
            SplitTree::Leaf(_) => {}
            SplitTree::HSplit { left, right, ratio, custom_ratio, .. } => {
                left.equalize();
                right.equalize();
                if !*custom_ratio {
                    let left_count = left.chain_count(true, false);
                    let total = left_count + right.chain_count(true, false);
                    *ratio = left_count as f32 / total as f32;
                }
            }
            SplitTree::VSplit { top, bottom, ratio, custom_ratio, .. } => {
                top.equalize();
                bottom.equalize();
                if !*custom_ratio {
                    let top_count = top.chain_count(false, false);
                    let total = top_count + bottom.chain_count(false, false);
                    *ratio = top_count as f32 / total as f32;
                }
            }
        }
    }

    /// Walk the tree, computing viewports by splitting according to ratios.
    /// Calls `f` for each leaf with its pane and computed viewport.
    pub fn for_each_pane_with_viewport<F: FnMut(&Pane, PaneViewport)>(&self, vp: PaneViewport, f: &mut F) {
        match self {
            SplitTree::Leaf(p) => f(p, vp),
            SplitTree::HSplit { left, right, ratio, .. } => {
                let (left_w, right_w) = split_sizes(vp.width, *ratio, left.is_fully_minimized(), right.is_fully_minimized());
                let left_vp = PaneViewport { x: vp.x, y: vp.y, width: left_w, height: vp.height };
                let right_vp = PaneViewport { x: vp.x + left_w, y: vp.y, width: right_w, height: vp.height };
                left.for_each_pane_with_viewport(left_vp, f);
                right.for_each_pane_with_viewport(right_vp, f);
            }
            SplitTree::VSplit { top, bottom, ratio, .. } => {
                let (top_h, bot_h) = split_sizes(vp.height, *ratio, top.is_fully_minimized(), bottom.is_fully_minimized());
                let top_vp = PaneViewport { x: vp.x, y: vp.y, width: vp.width, height: top_h };
                let bot_vp = PaneViewport { x: vp.x, y: vp.y + top_h, width: vp.width, height: bot_h };
                top.for_each_pane_with_viewport(top_vp, f);
                bottom.for_each_pane_with_viewport(bot_vp, f);
            }
        }
    }

    /// Collect separator lines between splits as (x1, y1, x2, y2) segments.
    pub fn collect_separators(&self, vp: PaneViewport, out: &mut Vec<(f32, f32, f32, f32)>) {
        match self {
            SplitTree::Leaf(_) => {}
            SplitTree::HSplit { left, right, ratio, .. } => {
                let (left_w, right_w) = split_sizes(vp.width, *ratio, left.is_fully_minimized(), right.is_fully_minimized());
                let split_x = vp.x + left_w;
                out.push((split_x, vp.y, split_x, vp.y + vp.height));
                let left_vp = PaneViewport { x: vp.x, y: vp.y, width: left_w, height: vp.height };
                let right_vp = PaneViewport { x: split_x, y: vp.y, width: right_w, height: vp.height };
                left.collect_separators(left_vp, out);
                right.collect_separators(right_vp, out);
            }
            SplitTree::VSplit { top, bottom, ratio, .. } => {
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

    /// Find the neighbor pane in the given direction from the pane with `id`.
    /// Uses viewport geometry: finds the pane whose center is closest in the given direction.
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

        // For directional navigation, prioritize panes that overlap on the
        // perpendicular axis (e.g. going Down, prefer panes whose x-range
        // overlaps with ours). Among overlapping panes, pick the closest on
        // the main axis. Fall back to Manhattan distance if no overlap.
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
                    // Check y-range overlap
                    let s_top = src_vp.y;
                    let s_bot = src_vp.y + src_vp.height;
                    let c_top = vp.y;
                    let c_bot = vp.y + vp.height;
                    s_top < c_bot && c_top < s_bot
                }
                NavDirection::Up | NavDirection::Down => {
                    // Check x-range overlap
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

    /// Check if this tree contains a pane with the given id.
    pub fn contains(&self, id: PaneId) -> bool {
        self.pane(id).is_some()
    }

    /// Fallback for adjust_ratio_directional: move the nearest separator in the
    /// arrow direction (no flip). Used when no separator exists in the arrow direction
    /// — the pane shrinks because the separator moves away from it.
    pub fn adjust_ratio_nearest(&mut self, id: PaneId, delta: f32, axis: SplitAxis) -> bool {
        match self {
            SplitTree::Leaf(_) => false,
            SplitTree::HSplit { left, right, ratio, custom_ratio, .. } if axis == SplitAxis::Horizontal => {
                let blocked = left.is_fully_minimized() || right.is_fully_minimized();
                if left.contains(id) {
                    if left.adjust_ratio_nearest(id, delta, axis) { return true; }
                    if blocked { return false; }
                    *ratio = (*ratio + delta).clamp(0.1, 0.9);
                    *custom_ratio = true;
                    true
                } else if right.contains(id) {
                    if right.adjust_ratio_nearest(id, delta, axis) { return true; }
                    if blocked { return false; }
                    *ratio = (*ratio + delta).clamp(0.1, 0.9);
                    *custom_ratio = true;
                    true
                } else {
                    false
                }
            }
            SplitTree::VSplit { top, bottom, ratio, custom_ratio, .. } if axis == SplitAxis::Vertical => {
                let blocked = top.is_fully_minimized() || bottom.is_fully_minimized();
                if top.contains(id) {
                    if top.adjust_ratio_nearest(id, delta, axis) { return true; }
                    if blocked { return false; }
                    *ratio = (*ratio + delta).clamp(0.1, 0.9);
                    *custom_ratio = true;
                    true
                } else if bottom.contains(id) {
                    if bottom.adjust_ratio_nearest(id, delta, axis) { return true; }
                    if blocked { return false; }
                    *ratio = (*ratio + delta).clamp(0.1, 0.9);
                    *custom_ratio = true;
                    true
                } else {
                    false
                }
            }
            SplitTree::HSplit { left, right, .. } | SplitTree::VSplit { top: left, bottom: right, .. } => {
                left.adjust_ratio_nearest(id, delta, axis)
                    || right.adjust_ratio_nearest(id, delta, axis)
            }
        }
    }

    /// Move the separator that is in the direction of the arrow.
    /// delta > 0 (Right/Down): find a split where pane is in the first child (separator is to pane's right/bottom).
    /// delta < 0 (Left/Up): find a split where pane is in the second child (separator is to pane's left/top).
    /// No delta flip — `ratio += delta` always moves the separator in the arrow direction.
    /// Returns false if no matching separator found (caller should fall back to adjust_ratio_for_pane).
    pub fn adjust_ratio_directional(&mut self, id: PaneId, delta: f32, axis: SplitAxis) -> bool {
        match self {
            SplitTree::Leaf(_) => false,
            SplitTree::HSplit { left, right, ratio, custom_ratio, .. } if axis == SplitAxis::Horizontal => {
                let blocked = left.is_fully_minimized() || right.is_fully_minimized();
                // Recurse first (deeper splits take priority)
                if left.adjust_ratio_directional(id, delta, axis) { return true; }
                if right.adjust_ratio_directional(id, delta, axis) { return true; }
                if blocked { return false; }
                // delta > 0: separator should be to pane's right → pane in left child
                // delta < 0: separator should be to pane's left → pane in right child
                if delta > 0.0 && left.contains(id) {
                    *ratio = (*ratio + delta).clamp(0.1, 0.9);
                    *custom_ratio = true;
                    true
                } else if delta < 0.0 && right.contains(id) {
                    *ratio = (*ratio + delta).clamp(0.1, 0.9);
                    *custom_ratio = true;
                    true
                } else {
                    false
                }
            }
            SplitTree::VSplit { top, bottom, ratio, custom_ratio, .. } if axis == SplitAxis::Vertical => {
                let blocked = top.is_fully_minimized() || bottom.is_fully_minimized();
                if top.adjust_ratio_directional(id, delta, axis) { return true; }
                if bottom.adjust_ratio_directional(id, delta, axis) { return true; }
                if blocked { return false; }
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
            // Wrong axis — recurse through children
            SplitTree::HSplit { left, right, .. } | SplitTree::VSplit { top: left, bottom: right, .. } => {
                left.adjust_ratio_directional(id, delta, axis)
                    || right.adjust_ratio_directional(id, delta, axis)
            }
        }
    }

    /// Returns the maximum leaf width as a fraction of total width (0.0–1.0).
    /// Used to cap virtual width so no pane exceeds screen width.
    pub fn max_leaf_width_fraction(&self) -> f32 {
        match self {
            SplitTree::Leaf(_) => 1.0,
            SplitTree::HSplit { left, right, ratio, .. } => {
                let left_frac = left.max_leaf_width_fraction() * ratio;
                let right_frac = right.max_leaf_width_fraction() * (1.0 - ratio);
                left_frac.max(right_frac)
            }
            SplitTree::VSplit { top, bottom, .. } => {
                // VSplit doesn't split width — both children get the full width
                top.max_leaf_width_fraction().max(bottom.max_leaf_width_fraction())
            }
        }
    }

    /// Adjust ratios so that only `target_id` absorbs the size change when
    /// virtual width changes from `old_total` to `new_total`.
    /// All other panes keep their absolute pixel size.
    ///
    /// At each HSplit ancestor of the target pane:
    ///   sibling_px = sibling_share * old_total  →  must equal sibling_share' * new_total
    ///   → new_ratio preserves the sibling's absolute pixel size
    pub fn scale_ratios_for_edge_grow(&mut self, target_id: PaneId, old_total: f32, new_total: f32) {
        if new_total <= 0.0 || old_total <= 0.0 { return; }
        match self {
            SplitTree::Leaf(_) => {}
            SplitTree::HSplit { left, right, ratio, custom_ratio, .. } => {
                if left.contains(target_id) {
                    // Right sibling keeps its pixel size
                    let right_px = (1.0 - *ratio) * old_total;
                    let new_ratio = (1.0 - right_px / new_total).clamp(0.1, 0.9);
                    let old_left = *ratio * old_total;
                    let new_left = new_ratio * new_total;
                    *ratio = new_ratio;
                    *custom_ratio = true;
                    left.scale_ratios_for_edge_grow(target_id, old_left, new_left);
                } else if right.contains(target_id) {
                    // Left sibling keeps its pixel size
                    let left_px = *ratio * old_total;
                    let new_ratio = (left_px / new_total).clamp(0.1, 0.9);
                    let old_right = (1.0 - *ratio) * old_total;
                    let new_right = (1.0 - new_ratio) * new_total;
                    *ratio = new_ratio;
                    *custom_ratio = true;
                    right.scale_ratios_for_edge_grow(target_id, old_right, new_right);
                }
            }
            SplitTree::VSplit { top, bottom, .. } => {
                top.scale_ratios_for_edge_grow(target_id, old_total, new_total);
                bottom.scale_ratios_for_edge_grow(target_id, old_total, new_total);
            }
        }
    }

    /// Collect separator info for mouse hit-testing and dragging.
    pub fn collect_separator_info(&self, vp: PaneViewport, out: &mut Vec<SeparatorInfo>) {
        match self {
            SplitTree::Leaf(_) => {}
            SplitTree::HSplit { left, right, ratio, .. } => {
                let first_min = left.is_fully_minimized();
                let second_min = right.is_fully_minimized();
                let (left_w, right_w) = split_sizes(vp.width, *ratio, first_min, second_min);
                let split_x = vp.x + left_w;
                // Only allow dragging if neither child is minimized
                if !first_min && !second_min {
                    out.push(SeparatorInfo {
                        pos: split_x,
                        cross_start: vp.y,
                        cross_end: vp.y + vp.height,
                        is_hsplit: true,
                        origin_ratio: *ratio,
                        parent_dim: vp.width,
                        node_ptr: self as *const SplitTree as usize,
                    });
                }
                let left_vp = PaneViewport { x: vp.x, y: vp.y, width: left_w, height: vp.height };
                let right_vp = PaneViewport { x: split_x, y: vp.y, width: right_w, height: vp.height };
                left.collect_separator_info(left_vp, out);
                right.collect_separator_info(right_vp, out);
            }
            SplitTree::VSplit { top, bottom, ratio, .. } => {
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
                        node_ptr: self as *const SplitTree as usize,
                    });
                }
                let top_vp = PaneViewport { x: vp.x, y: vp.y, width: vp.width, height: top_h };
                let bot_vp = PaneViewport { x: vp.x, y: split_y, width: vp.width, height: bot_h };
                top.collect_separator_info(top_vp, out);
                bottom.collect_separator_info(bot_vp, out);
            }
        }
    }

    /// Set the ratio of a split node identified by its pointer address.
    pub fn set_ratio_by_ptr(&mut self, ptr: usize, new_ratio: f32) -> bool {
        let self_ptr = self as *const SplitTree as usize;
        match self {
            SplitTree::Leaf(_) => false,
            SplitTree::HSplit { left, right, ratio, custom_ratio, .. } => {
                if self_ptr == ptr {
                    *ratio = new_ratio.clamp(0.1, 0.9);
                    *custom_ratio = true;
                    true
                } else {
                    left.set_ratio_by_ptr(ptr, new_ratio)
                        || right.set_ratio_by_ptr(ptr, new_ratio)
                }
            }
            SplitTree::VSplit { top, bottom, ratio, custom_ratio, .. } => {
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

    /// Hit-test: find which pane contains the pixel coordinate (x, y)
    /// within the given total viewport. Returns the pane and its viewport.
    pub fn hit_test(&self, x: f32, y: f32, vp: PaneViewport) -> Option<(&Pane, PaneViewport)> {
        if x < vp.x || x >= vp.x + vp.width || y < vp.y || y >= vp.y + vp.height {
            return None;
        }
        match self {
            SplitTree::Leaf(p) => Some((p, vp)),
            SplitTree::HSplit { left, right, ratio, .. } => {
                let (left_w, right_w) = split_sizes(vp.width, *ratio, left.is_fully_minimized(), right.is_fully_minimized());
                let left_vp = PaneViewport { x: vp.x, y: vp.y, width: left_w, height: vp.height };
                let right_vp = PaneViewport { x: vp.x + left_w, y: vp.y, width: right_w, height: vp.height };
                left.hit_test(x, y, left_vp)
                    .or_else(|| right.hit_test(x, y, right_vp))
            }
            SplitTree::VSplit { top, bottom, ratio, .. } => {
                let (top_h, bot_h) = split_sizes(vp.height, *ratio, top.is_fully_minimized(), bottom.is_fully_minimized());
                let top_vp = PaneViewport { x: vp.x, y: vp.y, width: vp.width, height: top_h };
                let bot_vp = PaneViewport { x: vp.x, y: vp.y + top_h, width: vp.width, height: bot_h };
                top.hit_test(x, y, top_vp)
                    .or_else(|| bottom.hit_test(x, y, bot_vp))
            }
        }
    }

    /// Swap the Pane values of two leaves identified by their PaneIds.
    /// Returns true if both were found and swapped.
    pub fn swap_panes(&mut self, id1: PaneId, id2: PaneId) -> bool {
        if id1 == id2 {
            return false;
        }
        // Get raw pointers to the two leaf Panes, then swap.
        let ptr1 = self.pane_leaf_mut(id1);
        let ptr2 = self.pane_leaf_mut(id2);
        match (ptr1, ptr2) {
            (Some(p1), Some(p2)) => {
                // SAFETY: PaneIds are globally unique (monotonic allocator) so id1 != id2
                // guarantees p1 and p2 point to non-overlapping Pane values.
                // Both pointers remain valid: self is exclusively borrowed and no
                // structural mutation occurs between pointer acquisition and swap.
                unsafe { std::ptr::swap(p1, p2) };
                true
            }
            _ => false,
        }
    }

    /// Reparent pane: rotate split orientation or swap children (2-leaf case only).
    /// Returns true if the tree was modified.
    pub fn reparent_pane(&mut self, focused_id: PaneId, dir: NavDirection) -> bool {
        match self {
            SplitTree::Leaf(_) => false,
            SplitTree::HSplit { left, right, .. } => {
                if let (SplitTree::Leaf(_), SplitTree::Leaf(_)) = (left.as_ref(), right.as_ref()) {
                    let focused_is_first = left.contains(focused_id);
                    if !focused_is_first && !right.contains(focused_id) {
                        return false;
                    }
                    self.reparent_2leaf(focused_is_first, dir)
                } else {
                    left.reparent_pane(focused_id, dir)
                        || right.reparent_pane(focused_id, dir)
                }
            }
            SplitTree::VSplit { top, bottom, .. } => {
                if let (SplitTree::Leaf(_), SplitTree::Leaf(_)) = (top.as_ref(), bottom.as_ref()) {
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

    /// Core reparent logic for a split node with exactly 2 Leaf children.
    /// `focused_is_first`: whether the focused pane is the first child (left/top).
    /// Handles both HSplit and VSplit uniformly via axis abstraction.
    fn reparent_2leaf(&mut self, focused_is_first: bool, dir: NavDirection) -> bool {
        let is_hsplit = matches!(self, SplitTree::HSplit { .. });
        let is_perpendicular = match dir {
            NavDirection::Up | NavDirection::Down => is_hsplit,
            NavDirection::Left | NavDirection::Right => !is_hsplit,
        };

        if is_perpendicular {
            // Rotate orientation: HSplit↔VSplit
            // Focused goes to the "target" position (e.g. Right→right, Down→bottom).
            // "target is second" means the direction points to the second slot.
            let target_is_second = matches!(dir,
                NavDirection::Right | NavDirection::Down);
            let swap = target_is_second == focused_is_first;
            if swap {
                self.swap_children();
            }
            self.flip_orientation();
            true
        } else {
            // Aligned direction: swap if moving toward sibling, no-op if at border.
            let moving_forward = matches!(dir,
                NavDirection::Right | NavDirection::Down);
            if moving_forward == focused_is_first {
                self.swap_children();
                true
            } else {
                false
            }
        }
    }

    /// Swap the two children of a split node.
    fn swap_children(&mut self) {
        match self {
            SplitTree::HSplit { left, right, .. } => std::mem::swap(left, right),
            SplitTree::VSplit { top, bottom, .. } => std::mem::swap(top, bottom),
            SplitTree::Leaf(_) => {}
        }
    }

    /// Flip HSplit↔VSplit in place, preserving children, ratio, and root flag.
    fn flip_orientation(&mut self) {
        // SAFETY: ptr::read + ptr::write avoids needing a valid intermediate value.
        // The old value is consumed (not dropped) and a new value is written immediately.
        let old = unsafe { std::ptr::read(self) };
        let new = match old {
            SplitTree::HSplit { left, right, ratio, root, custom_ratio } => {
                SplitTree::VSplit { top: left, bottom: right, ratio, root, custom_ratio }
            }
            SplitTree::VSplit { top, bottom, ratio, root, custom_ratio } => {
                SplitTree::HSplit { left: top, right: bottom, ratio, root, custom_ratio }
            }
            leaf => leaf,
        };
        unsafe { std::ptr::write(self, new) };
    }

    /// Return a raw mutable pointer to the Pane inside a Leaf node.
    fn pane_leaf_mut(&mut self, id: PaneId) -> Option<*mut Pane> {
        match self {
            SplitTree::Leaf(p) => {
                if p.id == id { Some(p as *mut Pane) } else { None }
            }
            SplitTree::HSplit { left, right, .. }
            | SplitTree::VSplit { top: left, bottom: right, .. } => {
                left.pane_leaf_mut(id).or_else(|| right.pane_leaf_mut(id))
            }
        }
    }

    /// Compute the viewport for a specific pane by id.
    pub fn viewport_for_pane(&self, id: PaneId, vp: PaneViewport) -> Option<PaneViewport> {
        match self {
            SplitTree::Leaf(p) => {
                if p.id == id { Some(vp) } else { None }
            }
            SplitTree::HSplit { left, right, ratio, .. } => {
                let (left_w, right_w) = split_sizes(vp.width, *ratio, left.is_fully_minimized(), right.is_fully_minimized());
                let left_vp = PaneViewport { x: vp.x, y: vp.y, width: left_w, height: vp.height };
                let right_vp = PaneViewport { x: vp.x + left_w, y: vp.y, width: right_w, height: vp.height };
                left.viewport_for_pane(id, left_vp)
                    .or_else(|| right.viewport_for_pane(id, right_vp))
            }
            SplitTree::VSplit { top, bottom, ratio, .. } => {
                let (top_h, bot_h) = split_sizes(vp.height, *ratio, top.is_fully_minimized(), bottom.is_fully_minimized());
                let top_vp = PaneViewport { x: vp.x, y: vp.y, width: vp.width, height: top_h };
                let bot_vp = PaneViewport { x: vp.x, y: vp.y + top_h, width: vp.width, height: bot_h };
                top.viewport_for_pane(id, top_vp)
                    .or_else(|| bottom.viewport_for_pane(id, bot_vp))
            }
        }
    }
}
