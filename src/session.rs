use serde::{Deserialize, Serialize};
use std::path::PathBuf;

use crate::config::Config;
use crate::pane::{alloc_tab_id, ColumnTree, Pane, PaneId, Tab};

const SESSION_VERSION: u32 = 3;

/// Multi-window session format (v3 — flat columns).
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

// ---------------------------------------------------------------
// New column-based save format
// ---------------------------------------------------------------

#[derive(Clone, Serialize, Deserialize)]
pub struct SavedTab {
    /// New format: columns + weights (v3).
    #[serde(default)]
    pub columns: Option<Vec<SavedColumn>>,
    #[serde(default)]
    pub column_weights: Option<Vec<f32>>,
    /// Legacy format: single tree (v2).
    #[serde(default)]
    pub tree: Option<SavedTree>,
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
pub enum SavedColumn {
    Leaf {
        cwd: Option<String>,
        #[serde(default)]
        last_command: Option<String>,
        #[serde(default)]
        custom_title: Option<String>,
        #[serde(default)]
        minimized: bool,
    },
    VSplit {
        top: Box<SavedColumn>,
        bottom: Box<SavedColumn>,
        ratio: f32,
        #[serde(default)]
        custom_ratio: bool,
    },
}

/// Legacy tree format — kept for backward compat reading.
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
    HSplit { left: Box<SavedTree>, right: Box<SavedTree>, ratio: f32, #[serde(default)] root: bool, #[serde(default)] custom_ratio: bool },
    VSplit { top: Box<SavedTree>, bottom: Box<SavedTree>, ratio: f32, #[serde(default)] root: bool, #[serde(default)] custom_ratio: bool },
}

fn session_path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    PathBuf::from(home).join(".config/kova/session.json")
}

// ---------------------------------------------------------------
// Snapshot (save)
// ---------------------------------------------------------------

fn snapshot_column(col: &ColumnTree) -> SavedColumn {
    match col {
        ColumnTree::Leaf(pane) => SavedColumn::Leaf {
            cwd: pane.cwd(),
            last_command: pane.last_command(),
            custom_title: pane.custom_title.clone(),
            minimized: pane.minimized,
        },
        ColumnTree::VSplit { top, bottom, ratio, custom_ratio } => SavedColumn::VSplit {
            top: Box::new(snapshot_column(top)),
            bottom: Box::new(snapshot_column(bottom)),
            ratio: *ratio,
            custom_ratio: *custom_ratio,
        },
    }
}

/// Find the depth-first leaf index of a pane by id across all columns.
fn leaf_index_of_tab(tab: &Tab) -> usize {
    let mut idx = 0;
    let target = tab.focused_pane;
    let mut found = None;
    for col in &tab.columns {
        leaf_index_walk(col, target, &mut idx, &mut found);
        if found.is_some() { break; }
    }
    found.unwrap_or(0)
}

fn leaf_index_walk(col: &ColumnTree, target: PaneId, idx: &mut usize, found: &mut Option<usize>) {
    match col {
        ColumnTree::Leaf(p) => {
            if p.id == target {
                *found = Some(*idx);
            }
            *idx += 1;
        }
        ColumnTree::VSplit { top, bottom, .. } => {
            leaf_index_walk(top, target, idx, found);
            if found.is_none() {
                leaf_index_walk(bottom, target, idx, found);
            }
        }
    }
}

/// Snapshot a single tab into a SavedTab (public for recent_projects).
pub fn snapshot_tab(tab: &Tab) -> SavedTab {
    let focused_leaf_index = leaf_index_of_tab(tab);
    SavedTab {
        columns: Some(tab.columns.iter().map(snapshot_column).collect()),
        column_weights: Some(tab.column_weights.clone()),
        tree: None,
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

/// Rotate session.json -> session.1.json -> session.2.json -> ...
fn rotate_session_backups(path: &std::path::Path, new_content: &[u8]) {
    if !path.exists() {
        return;
    }
    let parent = path.parent().unwrap();
    let backup = |n: usize| {
        if n == 0 { path.to_path_buf() }
        else { parent.join(format!("session.{}.json", n)) }
    };

    if let Ok(latest) = std::fs::read(&backup(1)) {
        if new_content == latest {
            return;
        }
    }

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

    // Try v3 first, then v2, then v1
    let session: Session = if let Ok(s) = serde_json::from_str::<Session>(&data) {
        if s.version == SESSION_VERSION || s.version == 2 {
            s
        } else if s.version == 1 {
            log::warn!("Session v1 with windows field, ignoring");
            return None;
        } else {
            log::warn!("Unknown session version {}, ignoring", s.version);
            return None;
        }
    } else if let Ok(v1) = serde_json::from_str::<SessionV1>(&data) {
        if v1.version == 1 {
            log::info!("Migrating v1 session to v3");
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

    let _ = std::fs::remove_file(&path);
    Some(session)
}

// ---------------------------------------------------------------
// Restore
// ---------------------------------------------------------------

/// Restore a saved column tree.
fn restore_column(saved: &SavedColumn, cols: u16, rows: u16, config: &Config) -> Option<(ColumnTree, Vec<PaneId>)> {
    match saved {
        SavedColumn::Leaf { cwd, last_command, custom_title, minimized } => {
            let mut pane = Pane::spawn(cols, rows, config, cwd.as_deref()).ok()?;
            let id = pane.id;
            if let Some(cmd) = last_command {
                pane.pending_command.set(Some(cmd.clone()));
                pane.terminal.write().last_command = Some(cmd.clone());
            }
            pane.custom_title = custom_title.clone();
            pane.minimized = *minimized;
            Some((ColumnTree::Leaf(pane), vec![id]))
        }
        SavedColumn::VSplit { top, bottom, ratio, custom_ratio } => {
            let (top_tree, mut top_ids) = restore_column(top, cols, rows, config)?;
            let (bot_tree, bot_ids) = restore_column(bottom, cols, rows, config)?;
            top_ids.extend(bot_ids);
            Some((ColumnTree::VSplit {
                top: Box::new(top_tree),
                bottom: Box::new(bot_tree),
                ratio: *ratio,
                custom_ratio: *custom_ratio,
            }, top_ids))
        }
    }
}

/// Restore from legacy SavedTree format, flattening HSplits into columns.
fn restore_legacy_tree(saved: &SavedTree, cols: u16, rows: u16, config: &Config) -> Option<(Vec<ColumnTree>, Vec<f32>, Vec<PaneId>)> {
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
            Some((vec![ColumnTree::Leaf(pane)], vec![1.0], vec![id]))
        }
        SavedTree::HSplit { left, right, ratio, .. } => {
            let (mut left_cols, mut left_weights, mut left_ids) = restore_legacy_tree(left, cols, rows, config)?;
            let (right_cols, right_weights, right_ids) = restore_legacy_tree(right, cols, rows, config)?;
            // Scale weights: left side gets ratio, right side gets 1-ratio
            let left_sum: f32 = left_weights.iter().sum();
            let right_sum: f32 = right_weights.iter().sum();
            if left_sum > 0.0 {
                for w in &mut left_weights {
                    *w = *w / left_sum * ratio;
                }
            }
            let right_ratio = 1.0 - ratio;
            let scaled_right: Vec<f32> = if right_sum > 0.0 {
                right_weights.iter().map(|w| w / right_sum * right_ratio).collect()
            } else {
                right_weights
            };
            left_cols.extend(right_cols);
            left_weights.extend(scaled_right);
            left_ids.extend(right_ids);
            Some((left_cols, left_weights, left_ids))
        }
        SavedTree::VSplit { top, bottom, ratio, custom_ratio, .. } => {
            // VSplit within a column — restore as a single column with VSplit
            let saved_col = SavedColumn::VSplit {
                top: Box::new(saved_tree_to_saved_column(top)),
                bottom: Box::new(saved_tree_to_saved_column(bottom)),
                ratio: *ratio,
                custom_ratio: *custom_ratio,
            };
            let (col, ids) = restore_column(&saved_col, cols, rows, config)?;
            Some((vec![col], vec![1.0], ids))
        }
    }
}

/// Convert a legacy SavedTree to SavedColumn (for VSplit branches).
fn saved_tree_to_saved_column(tree: &SavedTree) -> SavedColumn {
    match tree {
        SavedTree::Leaf { cwd, last_command, custom_title, minimized } => {
            SavedColumn::Leaf {
                cwd: cwd.clone(),
                last_command: last_command.clone(),
                custom_title: custom_title.clone(),
                minimized: *minimized,
            }
        }
        SavedTree::HSplit { left, .. } => {
            // HSplit inside a VSplit: flatten by taking the left side as top
            // This is a lossy conversion but should be rare
            log::warn!("HSplit nested inside VSplit during legacy migration — flattening");
            saved_tree_to_saved_column(left)
        }
        SavedTree::VSplit { top, bottom, ratio, custom_ratio, .. } => {
            SavedColumn::VSplit {
                top: Box::new(saved_tree_to_saved_column(top)),
                bottom: Box::new(saved_tree_to_saved_column(bottom)),
                ratio: *ratio,
                custom_ratio: *custom_ratio,
            }
        }
    }
}

/// Restore a single saved tab. Used by recent projects and session restore.
pub fn restore_saved_tab(saved: &SavedTab, cols: u16, rows: u16, config: &Config) -> Option<Tab> {
    let (columns, column_weights, pane_ids) = if let Some(ref saved_cols) = saved.columns {
        // New format (v3)
        let weights = saved.column_weights.clone().unwrap_or_else(|| vec![1.0; saved_cols.len()]);
        let mut all_ids = Vec::new();
        let mut cols_vec = Vec::new();
        for sc in saved_cols {
            let (col, ids) = restore_column(sc, cols, rows, config)?;
            cols_vec.push(col);
            all_ids.extend(ids);
        }
        (cols_vec, weights, all_ids)
    } else if let Some(ref tree) = saved.tree {
        // Legacy format (v2) — flatten HSplits
        restore_legacy_tree(tree, cols, rows, config)?
    } else {
        log::warn!("SavedTab has neither columns nor tree");
        return None;
    };

    let focused_pane = if saved.focused_leaf_index < pane_ids.len() {
        pane_ids[saved.focused_leaf_index]
    } else {
        *pane_ids.first()?
    };
    let mut tab = Tab {
        id: alloc_tab_id(),
        columns,
        column_weights,
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
