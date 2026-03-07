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
    #[serde(default)]
    pub virtual_width_override: Option<f32>,
    #[serde(default)]
    pub scroll_offset_x: Option<f32>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum SavedTree {
    Leaf {
        cwd: Option<String>,
        #[serde(default)]
        last_command: Option<String>,
        #[serde(default)]
        custom_title: Option<String>,
        #[serde(default)]
        minimized: bool,
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
            custom_title: pane.custom_title.clone(),
            minimized: pane.minimized,
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

/// Snapshot a single tab into a SavedTab (public for recent_projects).
pub fn snapshot_tab(tab: &Tab) -> SavedTab {
    let focused_leaf_index = leaf_index_of(&tab.tree, tab.focused_pane).unwrap_or(0);
    SavedTab {
        tree: snapshot_tree(&tab.tree),
        focused_leaf_index,
        custom_title: tab.custom_title.clone(),
        color: tab.color,
        virtual_width_override: if tab.virtual_width_override > 0.0 { Some(tab.virtual_width_override) } else { None },
        scroll_offset_x: if tab.scroll_offset_x != 0.0 { Some(tab.scroll_offset_x) } else { None },
    }
}

fn snapshot_tabs(tabs: &[Tab]) -> Vec<SavedTab> {
    tabs.iter().map(snapshot_tab).collect()
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

/// Maximum number of session backups to keep.
const SESSION_HISTORY_COUNT: usize = 10;

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
            rotate_session_backups(&path, json.as_bytes());
            if let Err(e) = std::fs::write(&path, json) {
                log::warn!("Failed to write session file: {}", e);
            } else {
                log::info!("Session saved to {} ({} window(s))", path.display(), windows.len());
            }
        }
        Err(e) => log::warn!("Failed to serialize session: {}", e),
    }
}

/// Rotate session.json → session.1.json → session.2.json → ... → session.N.json
/// Only rotates if the current file differs from the latest backup (avoids
/// filling history with identical periodic saves).
fn rotate_session_backups(path: &std::path::Path, new_content: &[u8]) {
    if !path.exists() {
        return;
    }
    let parent = path.parent().unwrap();
    let backup = |n: usize| {
        if n == 0 { path.to_path_buf() }
        else { parent.join(format!("session.{}.json", n)) }
    };

    // Skip rotation if new content is identical to most recent backup
    if let Ok(latest) = std::fs::read(&backup(1)) {
        if new_content == latest {
            return;
        }
    }

    // Drop oldest, shift everything down
    let _ = std::fs::remove_file(backup(SESSION_HISTORY_COUNT));
    for i in (1..SESSION_HISTORY_COUNT).rev() {
        let _ = std::fs::rename(backup(i), backup(i + 1));
    }
    let _ = std::fs::rename(backup(0), backup(1));
}

/// Loaded window data ready for restoration.
pub struct RestoredWindow {
    pub tabs: Vec<Tab>,
    pub active_tab: usize,
    pub frame: Option<(f64, f64, f64, f64)>,
}

/// Print available session backups to stdout.
pub fn list_session_backups() {
    let main_path = session_path();
    let dir = main_path.parent().unwrap();

    print_session_entry(&main_path, "current");
    for i in 1..=SESSION_HISTORY_COUNT {
        let path = dir.join(format!("session.{}.json", i));
        print_session_entry(&path, &format!("{:>7}", i));
    }
}

fn print_session_entry(path: &std::path::Path, label: &str) {
    let Ok(data) = std::fs::read_to_string(path) else { return };
    let modified = std::fs::metadata(path).ok()
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| {
            let secs = d.as_secs();
            format!("{}h{:02} UTC", (secs % 86400) / 3600, (secs % 3600) / 60)
        })
        .unwrap_or_default();
    let summary = match serde_json::from_str::<Session>(&data) {
        Ok(s) => {
            let tabs: usize = s.windows.iter().map(|w| w.tabs.len()).sum();
            format!("{} window(s), {} tab(s)", s.windows.len(), tabs)
        }
        Err(_) => "(invalid)".into(),
    };
    println!("  {}  {} {}", label, modified, summary);
}

pub fn load(backup: Option<usize>) -> Option<Session> {
    let path = match backup {
        Some(n) => {
            let dir = session_path().parent().unwrap().to_path_buf();
            let p = dir.join(format!("session.{}.json", n));
            log::info!("Restoring session backup #{}", n);
            p
        }
        None => session_path(),
    };
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
        SavedTree::Leaf { cwd, last_command, custom_title, minimized } => {
            let mut pane = Pane::spawn(cols, rows, config, cwd.as_deref()).ok()?;
            let id = pane.id;
            if let Some(cmd) = last_command {
                pane.pending_command.set(Some(cmd.clone()));
                pane.terminal.write().last_command = Some(cmd.clone());
            }
            pane.custom_title = custom_title.clone();
            pane.minimized = *minimized;
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

/// Restore a single saved tab. Used by recent projects and session restore.
pub fn restore_saved_tab(saved: &SavedTab, cols: u16, rows: u16, config: &Config) -> Option<Tab> {
    let (tree, pane_ids) = restore_tree(&saved.tree, cols, rows, config)?;
    let focused_pane = if saved.focused_leaf_index < pane_ids.len() {
        pane_ids[saved.focused_leaf_index]
    } else {
        pane_ids[0]
    };
    let mut tab = Tab {
        id: alloc_tab_id(),
        tree,
        focused_pane,
        custom_title: saved.custom_title.clone(),
        color: saved.color,
        has_bell: false,
        has_completion: false,
        minimized_stack: Vec::new(),
        scroll_offset_x: saved.scroll_offset_x.unwrap_or(0.0),
        virtual_width_override: saved.virtual_width_override.unwrap_or(0.0),
    };
    tab.rebuild_minimized_stack();
    Some(tab)
}

fn restore_window_tabs(ws: &WindowSession, config: &Config) -> Option<(Vec<Tab>, usize)> {
    let cols = config.terminal.columns;
    let rows = config.terminal.rows;
    let mut tabs = Vec::new();

    for saved_tab in &ws.tabs {
        match restore_saved_tab(saved_tab, cols, rows, config) {
            Some(tab) => tabs.push(tab),
            None => log::warn!("Failed to restore a tab, skipping"),
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
