use serde::{Deserialize, Serialize};
use std::path::PathBuf;

use crate::config::Config;
use crate::pane::{alloc_tab_id, Pane, PaneId, SplitTree, Tab};

const SESSION_VERSION: u32 = 2;

/// Multi-window session format (v2).
#[derive(Serialize, Deserialize)]
pub struct Session {
    pub version: u32,
    pub windows: Vec<WindowSession>,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct WindowSession {
    pub tabs: Vec<SavedTab>,
    pub active_tab: usize,
    /// Window frame: (x, y, width, height) in screen points.
    #[serde(default)]
    pub frame: Option<(f64, f64, f64, f64)>,
}

/// Legacy single-window session format (v1) — kept for backward compat loading.
#[derive(Deserialize)]
struct SessionV1 {
    pub version: u32,
    pub active_tab: usize,
    pub tabs: Vec<SavedTab>,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct SavedTab {
    pub tree: SavedTree,
    pub focused_leaf_index: usize,
    pub custom_title: Option<String>,
    pub color: Option<usize>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum SavedTree {
    Leaf {
        cwd: Option<String>,
        #[serde(default)]
        last_command: Option<String>,
    },
    HSplit { left: Box<SavedTree>, right: Box<SavedTree>, ratio: f32, #[serde(default)] root: bool },
    VSplit { top: Box<SavedTree>, bottom: Box<SavedTree>, ratio: f32, #[serde(default)] root: bool },
}

fn session_path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    PathBuf::from(home).join(".config/kova/session.json")
}

fn snapshot_tree(tree: &SplitTree) -> SavedTree {
    match tree {
        SplitTree::Leaf(pane) => SavedTree::Leaf {
            cwd: pane.cwd(),
            last_command: pane.last_command(),
        },
        SplitTree::HSplit { left, right, ratio, root } => SavedTree::HSplit {
            left: Box::new(snapshot_tree(left)),
            right: Box::new(snapshot_tree(right)),
            ratio: *ratio,
            root: *root,
        },
        SplitTree::VSplit { top, bottom, ratio, root } => SavedTree::VSplit {
            top: Box::new(snapshot_tree(top)),
            bottom: Box::new(snapshot_tree(bottom)),
            ratio: *ratio,
            root: *root,
        },
    }
}

/// Find the depth-first leaf index of a pane by id. Returns None if not found.
fn leaf_index_of(tree: &SplitTree, target: PaneId) -> Option<usize> {
    fn walk(tree: &SplitTree, target: PaneId, idx: &mut usize) -> Option<usize> {
        match tree {
            SplitTree::Leaf(p) => {
                if p.id == target {
                    Some(*idx)
                } else {
                    *idx += 1;
                    None
                }
            }
            SplitTree::HSplit { left, right, .. }
            | SplitTree::VSplit { top: left, bottom: right, .. } => {
                walk(left, target, idx).or_else(|| walk(right, target, idx))
            }
        }
    }
    walk(tree, target, &mut 0)
}

fn snapshot_tabs(tabs: &[Tab]) -> Vec<SavedTab> {
    tabs.iter().map(|tab| {
        let focused_leaf_index = leaf_index_of(&tab.tree, tab.focused_pane).unwrap_or(0);
        SavedTab {
            tree: snapshot_tree(&tab.tree),
            focused_leaf_index,
            custom_title: tab.custom_title.clone(),
            color: tab.color,
        }
    }).collect()
}

impl WindowSession {
    /// Build a WindowSession from live tab data.
    pub fn from_tabs(tabs: &[Tab], active_tab: usize, frame: Option<(f64, f64, f64, f64)>) -> Self {
        Self {
            tabs: snapshot_tabs(tabs),
            active_tab,
            frame,
        }
    }
}

/// Save all windows to a single session file.
pub fn save(windows: &[WindowSession]) {
    let session = Session {
        version: SESSION_VERSION,
        windows: windows.to_vec(),
    };

    let path = session_path();
    if let Some(parent) = path.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            log::warn!("Failed to create session dir: {}", e);
            return;
        }
    }
    match serde_json::to_string_pretty(&session) {
        Ok(json) => {
            if let Err(e) = std::fs::write(&path, json) {
                log::warn!("Failed to write session file: {}", e);
            } else {
                log::info!("Session saved to {} ({} window(s))", path.display(), windows.len());
            }
        }
        Err(e) => log::warn!("Failed to serialize session: {}", e),
    }
}

/// Loaded window data ready for restoration.
pub struct RestoredWindow {
    pub tabs: Vec<Tab>,
    pub active_tab: usize,
    pub frame: Option<(f64, f64, f64, f64)>,
}

pub fn load() -> Option<Session> {
    let path = session_path();
    let data = std::fs::read_to_string(&path).ok()?;

    // Try v2 first, then fall back to v1
    let session: Session = if let Ok(s) = serde_json::from_str::<Session>(&data) {
        if s.version == SESSION_VERSION {
            s
        } else if s.version == 1 {
            // Shouldn't happen (v1 doesn't have `windows` field), but handle gracefully
            log::warn!("Session v1 with windows field, ignoring");
            return None;
        } else {
            log::warn!("Unknown session version {}, ignoring", s.version);
            return None;
        }
    } else if let Ok(v1) = serde_json::from_str::<SessionV1>(&data) {
        if v1.version == 1 {
            log::info!("Migrating v1 session to v2");
            Session {
                version: SESSION_VERSION,
                windows: vec![WindowSession {
                    tabs: v1.tabs,
                    active_tab: v1.active_tab,
                    frame: None,
                }],
            }
        } else {
            log::warn!("Unknown session version {}, ignoring", v1.version);
            return None;
        }
    } else {
        log::warn!("Failed to parse session file");
        return None;
    };

    // Remove session file after loading so a crash during restore doesn't loop
    let _ = std::fs::remove_file(&path);
    Some(session)
}

/// Restore a saved tree, spawning new panes. Returns the tree and a list of pane ids in depth-first order.
fn restore_tree(saved: &SavedTree, cols: u16, rows: u16, config: &Config) -> Option<(SplitTree, Vec<PaneId>)> {
    match saved {
        SavedTree::Leaf { cwd, last_command } => {
            let pane = Pane::spawn(cols, rows, config, cwd.as_deref()).ok()?;
            let id = pane.id;
            if let Some(cmd) = last_command {
                pane.pending_command.set(Some(cmd.clone()));
                pane.terminal.write().last_command = Some(cmd.clone());
            }
            Some((SplitTree::Leaf(pane), vec![id]))
        }
        SavedTree::HSplit { left, right, ratio, root } => {
            let (left_tree, mut left_ids) = restore_tree(left, cols, rows, config)?;
            let (right_tree, right_ids) = restore_tree(right, cols, rows, config)?;
            left_ids.extend(right_ids);
            Some((SplitTree::HSplit {
                left: Box::new(left_tree),
                right: Box::new(right_tree),
                ratio: *ratio,
                root: *root,
            }, left_ids))
        }
        SavedTree::VSplit { top, bottom, ratio, root } => {
            let (top_tree, mut top_ids) = restore_tree(top, cols, rows, config)?;
            let (bottom_tree, bottom_ids) = restore_tree(bottom, cols, rows, config)?;
            top_ids.extend(bottom_ids);
            Some((SplitTree::VSplit {
                top: Box::new(top_tree),
                bottom: Box::new(bottom_tree),
                ratio: *ratio,
                root: *root,
            }, top_ids))
        }
    }
}

fn restore_window_tabs(ws: &WindowSession, config: &Config) -> Option<(Vec<Tab>, usize)> {
    let cols = config.terminal.columns;
    let rows = config.terminal.rows;
    let mut tabs = Vec::new();

    for saved_tab in &ws.tabs {
        match restore_tree(&saved_tab.tree, cols, rows, config) {
            Some((tree, pane_ids)) => {
                let focused_pane = if saved_tab.focused_leaf_index < pane_ids.len() {
                    pane_ids[saved_tab.focused_leaf_index]
                } else {
                    pane_ids[0]
                };
                tabs.push(Tab {
                    id: alloc_tab_id(),
                    tree,
                    focused_pane,
                    custom_title: saved_tab.custom_title.clone(),
                    color: saved_tab.color,
                    has_bell: false,
                    scroll_offset_x: 0.0,
                    virtual_width_override: 0.0,
                });
            }
            None => {
                log::warn!("Failed to restore a tab, skipping");
            }
        }
    }

    if tabs.is_empty() {
        return None;
    }

    let active_tab = if ws.active_tab < tabs.len() {
        ws.active_tab
    } else {
        tabs.len() - 1
    };

    Some((tabs, active_tab))
}

/// Restore a multi-window session. Returns a list of windows to create.
pub fn restore_session(session: Session, config: &Config) -> Option<Vec<RestoredWindow>> {
    let mut windows = Vec::new();

    for ws in &session.windows {
        if let Some((tabs, active_tab)) = restore_window_tabs(ws, config) {
            windows.push(RestoredWindow {
                tabs,
                active_tab,
                frame: ws.frame,
            });
        }
    }

    if windows.is_empty() {
        return None;
    }

    Some(windows)
}
