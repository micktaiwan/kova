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
pub enum NavDirection {
    Left,
    Right,
    Up,
    Down,
}

static NEXT_PANE_ID: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(1);

fn alloc_pane_id() -> PaneId {
    NEXT_PANE_ID.fetch_add(1, Ordering::Relaxed)
}

/// A single terminal pane: owns its PTY, terminal state, and per-pane flags.
pub struct Pane {
    pub id: PaneId,
    pub terminal: Arc<RwLock<TerminalState>>,
    pub pty: Pty,
    pub shell_exited: Arc<AtomicBool>,
    pub shell_ready: Arc<AtomicBool>,
    pub scroll_accumulator: Cell<f64>,
}

impl Pane {
    pub fn spawn(cols: u16, rows: u16, config: &Config) -> Result<Self, Box<dyn std::error::Error>> {
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
        )?;

        Ok(Pane {
            id: alloc_pane_id(),
            terminal,
            pty,
            shell_exited,
            shell_ready,
            scroll_accumulator: Cell::new(0.0),
        })
    }

    pub fn is_alive(&self) -> bool {
        !self.shell_exited.load(Ordering::Relaxed)
    }

    pub fn is_ready(&self) -> bool {
        self.shell_ready.load(Ordering::Relaxed)
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
