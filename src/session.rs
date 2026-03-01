use serde::{Deserialize, Serialize};
use std::path::PathBuf;

use crate::config::Config;
use crate::pane::{alloc_tab_id, Pane, PaneId, SplitTree, Tab};

const SESSION_VERSION: u32 = 1;

#[derive(Serialize, Deserialize)]
pub struct Session {
    pub version: u32,
    pub active_tab: usize,
    pub tabs: Vec<SavedTab>,
}

#[derive(Serialize, Deserialize)]
pub struct SavedTab {
    pub tree: SavedTree,
    pub focused_leaf_index: usize,
    pub custom_title: Option<String>,
    pub color: Option<usize>,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum SavedTree {
    Leaf {
        cwd: Option<String>,
        #[serde(default)]
        last_command: Option<String>,
    },
    HSplit { left: Box<SavedTree>, right: Box<SavedTree>, ratio: f32 },
    VSplit { top: Box<SavedTree>, bottom: Box<SavedTree>, ratio: f32 },
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
        SplitTree::HSplit { left, right, ratio } => SavedTree::HSplit {
            left: Box::new(snapshot_tree(left)),
            right: Box::new(snapshot_tree(right)),
            ratio: *ratio,
        },
        SplitTree::VSplit { top, bottom, ratio } => SavedTree::VSplit {
            top: Box::new(snapshot_tree(top)),
            bottom: Box::new(snapshot_tree(bottom)),
            ratio: *ratio,
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

pub fn save(tabs: &[Tab], active_tab: usize) {
    let session = Session {
        version: SESSION_VERSION,
        active_tab,
        tabs: tabs.iter().map(|tab| {
            let focused_leaf_index = leaf_index_of(&tab.tree, tab.focused_pane).unwrap_or(0);
            SavedTab {
                tree: snapshot_tree(&tab.tree),
                focused_leaf_index,
                custom_title: tab.custom_title.clone(),
                color: tab.color,
            }
        }).collect(),
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
                log::info!("Session saved to {}", path.display());
            }
        }
        Err(e) => log::warn!("Failed to serialize session: {}", e),
    }
}

pub fn load() -> Option<Session> {
    let path = session_path();
    let data = std::fs::read_to_string(&path).ok()?;
    let session: Session = match serde_json::from_str(&data) {
        Ok(s) => s,
        Err(e) => {
            log::warn!("Failed to parse session file: {}", e);
            return None;
        }
    };
    if session.version != SESSION_VERSION {
        log::warn!("Unknown session version {}, ignoring", session.version);
        return None;
    }
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
        SavedTree::HSplit { left, right, ratio } => {
            let (left_tree, mut left_ids) = restore_tree(left, cols, rows, config)?;
            let (right_tree, right_ids) = restore_tree(right, cols, rows, config)?;
            left_ids.extend(right_ids);
            Some((SplitTree::HSplit {
                left: Box::new(left_tree),
                right: Box::new(right_tree),
                ratio: *ratio,
            }, left_ids))
        }
        SavedTree::VSplit { top, bottom, ratio } => {
            let (top_tree, mut top_ids) = restore_tree(top, cols, rows, config)?;
            let (bottom_tree, bottom_ids) = restore_tree(bottom, cols, rows, config)?;
            top_ids.extend(bottom_ids);
            Some((SplitTree::VSplit {
                top: Box::new(top_tree),
                bottom: Box::new(bottom_tree),
                ratio: *ratio,
            }, top_ids))
        }
    }
}

pub fn restore_session(session: Session, config: &Config) -> Option<(Vec<Tab>, usize)> {
    let cols = config.terminal.columns;
    let rows = config.terminal.rows;
    let mut tabs = Vec::new();

    for saved_tab in &session.tabs {
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

    let active_tab = if session.active_tab < tabs.len() {
        session.active_tab
    } else {
        tabs.len() - 1
    };

    Some((tabs, active_tab))
}
