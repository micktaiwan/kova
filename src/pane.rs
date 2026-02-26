use parking_lot::RwLock;
use std::cell::Cell;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use crate::config::Config;
use crate::renderer::PaneViewport;
use crate::terminal::pty::Pty;
use crate::terminal::TerminalState;

pub type PaneId = u32;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum SplitDirection {
    Horizontal, // side by side (left | right)
    Vertical,   // stacked (top / bottom)
}

#[derive(Clone, Copy, PartialEq, Eq)]
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

#[derive(Clone, Copy, PartialEq, Eq)]
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
        })
    }

    /// Title for this tab: custom title if set, then OSC title of focused pane, or CWD basename, or "shell".
    pub fn title(&self) -> String {
        if let Some(ref custom) = self.custom_title {
            return custom.clone();
        }
        if let Some(pane) = self.tree.pane(self.focused_pane) {
            let term = pane.terminal.read();
            if let Some(ref title) = term.title {
                return title.clone();
            }
            drop(term);
            if let Some(cwd) = pane.cwd() {
                if let Some(base) = std::path::Path::new(&cwd).file_name() {
                    return base.to_string_lossy().to_string();
                }
            }
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
}

impl Pane {
    pub fn spawn(cols: u16, rows: u16, config: &Config, working_dir: Option<&str>) -> Result<Self, Box<dyn std::error::Error>> {
        let terminal = Arc::new(RwLock::new(TerminalState::new(
            cols,
            rows,
            config.terminal.scrollback,
            config.colors.foreground,
            config.colors.background,
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
        })
    }

    pub fn cwd(&self) -> Option<String> {
        self.pty.cwd()
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
    },
    VSplit {
        top: Box<SplitTree>,
        bottom: Box<SplitTree>,
        /// Fraction of height allocated to the top child (0.0–1.0).
        ratio: f32,
    },
}

impl SplitTree {
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
            SplitTree::HSplit { left, right, ratio } => {
                if left.contains(id) {
                    match left.remove_pane(id) {
                        Some(new_left) => Some(SplitTree::HSplit { left: Box::new(new_left), right, ratio }),
                        None => Some(*right), // left was a leaf that got removed, promote right
                    }
                } else {
                    match right.remove_pane(id) {
                        Some(new_right) => Some(SplitTree::HSplit { left, right: Box::new(new_right), ratio }),
                        None => Some(*left),
                    }
                }
            }
            SplitTree::VSplit { top, bottom, ratio } => {
                if top.contains(id) {
                    match top.remove_pane(id) {
                        Some(new_top) => Some(SplitTree::VSplit { top: Box::new(new_top), bottom, ratio }),
                        None => Some(*bottom),
                    }
                } else {
                    match bottom.remove_pane(id) {
                        Some(new_bottom) => Some(SplitTree::VSplit { top, bottom: Box::new(new_bottom), ratio }),
                        None => Some(*top),
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
                    SplitDirection::Horizontal => SplitTree::HSplit { left: old, right: new, ratio: 0.5 },
                    SplitDirection::Vertical => SplitTree::VSplit { top: old, bottom: new, ratio: 0.5 },
                }
            }
            SplitTree::Leaf(_) => self,
            SplitTree::HSplit { left, right, ratio } => {
                if left.contains(id) {
                    SplitTree::HSplit { left: Box::new(left.with_split(id, new_pane, direction)), right, ratio }
                } else {
                    SplitTree::HSplit { left, right: Box::new(right.with_split(id, new_pane, direction)), ratio }
                }
            }
            SplitTree::VSplit { top, bottom, ratio } => {
                if top.contains(id) {
                    SplitTree::VSplit { top: Box::new(top.with_split(id, new_pane, direction)), bottom, ratio }
                } else {
                    SplitTree::VSplit { top, bottom: Box::new(bottom.with_split(id, new_pane, direction)), ratio }
                }
            }
        }
    }

    /// Count leaves in a chain of same-direction splits.
    /// A split node of a different direction counts as a single unit.
    fn chain_leaf_count(&self, horizontal: bool) -> usize {
        match self {
            SplitTree::Leaf(_) => 1,
            SplitTree::HSplit { left, right, .. } if horizontal => {
                left.chain_leaf_count(true) + right.chain_leaf_count(true)
            }
            SplitTree::VSplit { top, bottom, .. } if !horizontal => {
                top.chain_leaf_count(false) + bottom.chain_leaf_count(false)
            }
            _ => 1,
        }
    }

    /// Equalize ratios so that all panes along a same-direction chain get equal space.
    /// For example, HSplit(A, HSplit(B, C)) gets ratios 1/3 and 1/2, giving each pane 1/3.
    pub fn equalize(&mut self) {
        match self {
            SplitTree::Leaf(_) => {}
            SplitTree::HSplit { left, right, ratio } => {
                left.equalize();
                right.equalize();
                let left_count = left.chain_leaf_count(true);
                let total = left_count + right.chain_leaf_count(true);
                *ratio = left_count as f32 / total as f32;
            }
            SplitTree::VSplit { top, bottom, ratio } => {
                top.equalize();
                bottom.equalize();
                let top_count = top.chain_leaf_count(false);
                let total = top_count + bottom.chain_leaf_count(false);
                *ratio = top_count as f32 / total as f32;
            }
        }
    }

    /// Walk the tree, computing viewports by splitting according to ratios.
    /// Calls `f` for each leaf with its pane and computed viewport.
    pub fn for_each_pane_with_viewport<F: FnMut(&Pane, PaneViewport)>(&self, vp: PaneViewport, f: &mut F) {
        match self {
            SplitTree::Leaf(p) => f(p, vp),
            SplitTree::HSplit { left, right, ratio } => {
                let left_w = vp.width * ratio;
                let left_vp = PaneViewport { x: vp.x, y: vp.y, width: left_w, height: vp.height };
                let right_vp = PaneViewport { x: vp.x + left_w, y: vp.y, width: vp.width - left_w, height: vp.height };
                left.for_each_pane_with_viewport(left_vp, f);
                right.for_each_pane_with_viewport(right_vp, f);
            }
            SplitTree::VSplit { top, bottom, ratio } => {
                let top_h = vp.height * ratio;
                let top_vp = PaneViewport { x: vp.x, y: vp.y, width: vp.width, height: top_h };
                let bot_vp = PaneViewport { x: vp.x, y: vp.y + top_h, width: vp.width, height: vp.height - top_h };
                top.for_each_pane_with_viewport(top_vp, f);
                bottom.for_each_pane_with_viewport(bot_vp, f);
            }
        }
    }

    /// Collect separator lines between splits as (x1, y1, x2, y2) segments.
    pub fn collect_separators(&self, vp: PaneViewport, out: &mut Vec<(f32, f32, f32, f32)>) {
        match self {
            SplitTree::Leaf(_) => {}
            SplitTree::HSplit { left, right, ratio } => {
                let split_x = vp.x + vp.width * ratio;
                out.push((split_x, vp.y, split_x, vp.y + vp.height));
                let left_vp = PaneViewport { x: vp.x, y: vp.y, width: vp.width * ratio, height: vp.height };
                let right_vp = PaneViewport { x: split_x, y: vp.y, width: vp.width * (1.0 - ratio), height: vp.height };
                left.collect_separators(left_vp, out);
                right.collect_separators(right_vp, out);
            }
            SplitTree::VSplit { top, bottom, ratio } => {
                let split_y = vp.y + vp.height * ratio;
                out.push((vp.x, split_y, vp.x + vp.width, split_y));
                let top_vp = PaneViewport { x: vp.x, y: vp.y, width: vp.width, height: vp.height * ratio };
                let bot_vp = PaneViewport { x: vp.x, y: split_y, width: vp.width, height: vp.height * (1.0 - ratio) };
                top.collect_separators(top_vp, out);
                bottom.collect_separators(bot_vp, out);
            }
        }
    }

    /// Find the neighbor pane in the given direction from the pane with `id`.
    /// Uses viewport geometry: finds the pane whose center is closest in the given direction.
    pub fn neighbor(&self, id: PaneId, dir: NavDirection, total_vp: PaneViewport) -> Option<PaneId> {
        // Collect all panes with their viewports
        let mut panes: Vec<(PaneId, PaneViewport)> = Vec::new();
        self.for_each_pane_with_viewport(total_vp, &mut |p, vp| {
            panes.push((p.id, vp));
        });

        let source = panes.iter().find(|(pid, _)| *pid == id)?;
        let (_, src_vp) = source;
        let src_cx = src_vp.x + src_vp.width / 2.0;
        let src_cy = src_vp.y + src_vp.height / 2.0;

        let mut best: Option<(PaneId, f32)> = None;
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

            let dist = (cx - src_cx).abs() + (cy - src_cy).abs();
            if best.map_or(true, |(_, d)| dist < d) {
                best = Some((pid, dist));
            }
        }
        best.map(|(pid, _)| pid)
    }

    /// Check if this tree contains a pane with the given id.
    pub fn contains(&self, id: PaneId) -> bool {
        self.pane(id).is_some()
    }

    /// Move the nearest separator in the arrow direction.
    /// `delta > 0` (Right/Down): separator moves right/down (ratio increases).
    /// `delta < 0` (Left/Up): separator moves left/up (ratio decreases).
    /// Finds the nearest ancestor of the matching axis and applies delta to its ratio.
    pub fn adjust_ratio_for_pane(&mut self, id: PaneId, delta: f32, axis: SplitAxis) -> bool {
        match self {
            SplitTree::Leaf(_) => false,
            SplitTree::HSplit { left, right, ratio } if axis == SplitAxis::Horizontal => {
                if left.contains(id) {
                    if left.adjust_ratio_for_pane(id, delta, axis) {
                        return true;
                    }
                    *ratio = (*ratio + delta).clamp(0.1, 0.9);
                    true
                } else if right.contains(id) {
                    if right.adjust_ratio_for_pane(id, delta, axis) {
                        return true;
                    }
                    *ratio = (*ratio + delta).clamp(0.1, 0.9);
                    true
                } else {
                    false
                }
            }
            SplitTree::VSplit { top, bottom, ratio } if axis == SplitAxis::Vertical => {
                if top.contains(id) {
                    if top.adjust_ratio_for_pane(id, delta, axis) {
                        return true;
                    }
                    *ratio = (*ratio + delta).clamp(0.1, 0.9);
                    true
                } else if bottom.contains(id) {
                    if bottom.adjust_ratio_for_pane(id, delta, axis) {
                        return true;
                    }
                    *ratio = (*ratio + delta).clamp(0.1, 0.9);
                    true
                } else {
                    false
                }
            }
            // Wrong axis — recurse through children
            SplitTree::HSplit { left, right, .. } | SplitTree::VSplit { top: left, bottom: right, .. } => {
                left.adjust_ratio_for_pane(id, delta, axis)
                    || right.adjust_ratio_for_pane(id, delta, axis)
            }
        }
    }

    /// Collect separator info for mouse hit-testing and dragging.
    pub fn collect_separator_info(&self, vp: PaneViewport, out: &mut Vec<SeparatorInfo>) {
        match self {
            SplitTree::Leaf(_) => {}
            SplitTree::HSplit { left, right, ratio } => {
                let split_x = vp.x + vp.width * ratio;
                out.push(SeparatorInfo {
                    pos: split_x,
                    cross_start: vp.y,
                    cross_end: vp.y + vp.height,
                    is_hsplit: true,
                    origin_ratio: *ratio,
                    parent_dim: vp.width,
                    node_ptr: self as *const SplitTree as usize,
                });
                let left_vp = PaneViewport { x: vp.x, y: vp.y, width: vp.width * ratio, height: vp.height };
                let right_vp = PaneViewport { x: split_x, y: vp.y, width: vp.width * (1.0 - ratio), height: vp.height };
                left.collect_separator_info(left_vp, out);
                right.collect_separator_info(right_vp, out);
            }
            SplitTree::VSplit { top, bottom, ratio } => {
                let split_y = vp.y + vp.height * ratio;
                out.push(SeparatorInfo {
                    pos: split_y,
                    cross_start: vp.x,
                    cross_end: vp.x + vp.width,
                    is_hsplit: false,
                    origin_ratio: *ratio,
                    parent_dim: vp.height,
                    node_ptr: self as *const SplitTree as usize,
                });
                let top_vp = PaneViewport { x: vp.x, y: vp.y, width: vp.width, height: vp.height * ratio };
                let bot_vp = PaneViewport { x: vp.x, y: split_y, width: vp.width, height: vp.height * (1.0 - ratio) };
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
            SplitTree::HSplit { left, right, ratio } => {
                if self_ptr == ptr {
                    *ratio = new_ratio.clamp(0.1, 0.9);
                    true
                } else {
                    left.set_ratio_by_ptr(ptr, new_ratio)
                        || right.set_ratio_by_ptr(ptr, new_ratio)
                }
            }
            SplitTree::VSplit { top, bottom, ratio } => {
                if self_ptr == ptr {
                    *ratio = new_ratio.clamp(0.1, 0.9);
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
            SplitTree::HSplit { left, right, ratio } => {
                let left_w = vp.width * ratio;
                let left_vp = PaneViewport { x: vp.x, y: vp.y, width: left_w, height: vp.height };
                let right_vp = PaneViewport { x: vp.x + left_w, y: vp.y, width: vp.width - left_w, height: vp.height };
                left.hit_test(x, y, left_vp)
                    .or_else(|| right.hit_test(x, y, right_vp))
            }
            SplitTree::VSplit { top, bottom, ratio } => {
                let top_h = vp.height * ratio;
                let top_vp = PaneViewport { x: vp.x, y: vp.y, width: vp.width, height: top_h };
                let bot_vp = PaneViewport { x: vp.x, y: vp.y + top_h, width: vp.width, height: vp.height - top_h };
                top.hit_test(x, y, top_vp)
                    .or_else(|| bottom.hit_test(x, y, bot_vp))
            }
        }
    }

    /// Compute the viewport for a specific pane by id.
    pub fn viewport_for_pane(&self, id: PaneId, vp: PaneViewport) -> Option<PaneViewport> {
        match self {
            SplitTree::Leaf(p) => {
                if p.id == id { Some(vp) } else { None }
            }
            SplitTree::HSplit { left, right, ratio } => {
                let left_w = vp.width * ratio;
                let left_vp = PaneViewport { x: vp.x, y: vp.y, width: left_w, height: vp.height };
                let right_vp = PaneViewport { x: vp.x + left_w, y: vp.y, width: vp.width - left_w, height: vp.height };
                left.viewport_for_pane(id, left_vp)
                    .or_else(|| right.viewport_for_pane(id, right_vp))
            }
            SplitTree::VSplit { top, bottom, ratio } => {
                let top_h = vp.height * ratio;
                let top_vp = PaneViewport { x: vp.x, y: vp.y, width: vp.width, height: top_h };
                let bot_vp = PaneViewport { x: vp.x, y: vp.y + top_h, width: vp.width, height: vp.height - top_h };
                top.viewport_for_pane(id, top_vp)
                    .or_else(|| bottom.viewport_for_pane(id, bot_vp))
            }
        }
    }
}
