use serde::{Deserialize, Serialize};
use std::path::PathBuf;

use crate::config::Config;
use crate::pane::{alloc_tab_id, Column, Pane, PaneId, Tab};

const SESSION_VERSION: u32 = 4;

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
    /// New format (v4): flat columns with pane lists.
    #[serde(default)]
    pub flat_columns: Option<Vec<SavedFlatColumn>>,
    /// Legacy format: columns + weights (v3).
    #[serde(default)]
    pub columns: Option<Vec<SavedColumn>>,
    #[serde(default)]
    pub column_weights: Option<Vec<f32>>,
    #[serde(default)]
    pub custom_weights: Option<Vec<bool>>,
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

/// Flat column format (v4): a column is a list of panes with row weights.
#[derive(Clone, Serialize, Deserialize)]
pub struct SavedFlatColumn {
    pub panes: Vec<SavedPane>,
    pub row_weights: Vec<f32>,
    #[serde(default)]
    pub custom_row_weights: Option<Vec<bool>>,
}

/// A single pane in the flat format.
#[derive(Clone, Serialize, Deserialize)]
pub struct SavedPane {
    pub cwd: Option<String>,
    #[serde(default)]
    pub last_command: Option<String>,
    #[serde(default)]
    pub custom_title: Option<String>,
    #[serde(default)]
    pub minimized: bool,
}

/// Legacy column format (v3) — kept for backward compat reading.
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

fn snapshot_flat_column(col: &Column) -> SavedFlatColumn {
    SavedFlatColumn {
        panes: col.panes.iter().map(|p| SavedPane {
            cwd: p.cwd(),
            last_command: p.last_command(),
            custom_title: p.custom_title.clone(),
            minimized: p.minimized,
        }).collect(),
        row_weights: col.row_weights.clone(),
        custom_row_weights: if col.custom_row_weights.iter().any(|&cw| cw) {
            Some(col.custom_row_weights.clone())
        } else {
            None
        },
    }
}

/// Find the flat leaf index of a pane by id across all columns.
fn leaf_index_of_tab(tab: &Tab) -> usize {
    let mut idx = 0;
    let target = tab.focused_pane;
    for col in &tab.columns {
        for pane in &col.panes {
            if pane.id == target {
                return idx;
            }
            idx += 1;
        }
    }
    0
}

/// Snapshot a single tab into a SavedTab (public for recent_projects).
pub fn snapshot_tab(tab: &Tab) -> SavedTab {
    let focused_leaf_index = leaf_index_of_tab(tab);
    SavedTab {
        flat_columns: Some(tab.columns.iter().map(snapshot_flat_column).collect()),
        columns: None,
        column_weights: Some(tab.column_weights.clone()),
        custom_weights: if tab.custom_weights.iter().any(|&cw| cw) { Some(tab.custom_weights.clone()) } else { None },
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
    /// Tabs not yet spawned (deferred for progressive loading).
    pub deferred_tabs: Vec<(usize, SavedTab)>,
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

    // Try v4/v3 first, then v2, then v1
    let session: Session = if let Ok(s) = serde_json::from_str::<Session>(&data) {
        if s.version == SESSION_VERSION || s.version == 3 || s.version == 2 {
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

/// Restore a flat column (v4 format).
fn restore_flat_column(saved: &SavedFlatColumn, cols: u16, rows: u16, config: &Config) -> Option<(Column, Vec<PaneId>)> {
    let mut panes = Vec::new();
    let mut ids = Vec::new();
    for sp in &saved.panes {
        let t = std::time::Instant::now();
        let mut pane = Pane::spawn(cols, rows, config, sp.cwd.as_deref()).ok()?;
        log::info!("[STARTUP] Pane::spawn id={} cwd={:?} in {:?}", pane.id, sp.cwd, t.elapsed());
        let id = pane.id;
        if let Some(ref cmd) = sp.last_command {
            pane.pending_command.set(Some(cmd.clone()));
            pane.terminal.write().last_command = Some(cmd.clone());
        }
        pane.custom_title = sp.custom_title.clone();
        pane.minimized = sp.minimized;
        panes.push(pane);
        ids.push(id);
    }
    if panes.is_empty() { return None; }
    let n = panes.len();
    let row_weights = if saved.row_weights.len() == n {
        saved.row_weights.clone()
    } else {
        log::warn!("Session: row_weights len {} != panes len {}, using equal weights", saved.row_weights.len(), n);
        vec![1.0; n]
    };
    let custom_row_weights = saved.custom_row_weights.clone()
        .filter(|v| v.len() == n)
        .unwrap_or_else(|| vec![false; n]);
    Some((Column { panes, row_weights, custom_row_weights }, ids))
}

/// Restore a saved column tree (v3 legacy format) into a flat Column.
fn restore_column(saved: &SavedColumn, cols: u16, rows: u16, config: &Config) -> Option<(Column, Vec<PaneId>)> {
    // Flatten the tree into a flat column by collecting all leaves depth-first,
    // converting ratios to weights.
    let mut panes = Vec::new();
    let mut weights = Vec::new();
    let mut ids = Vec::new();
    flatten_saved_column(saved, 1.0, cols, rows, config, &mut panes, &mut weights, &mut ids)?;
    if panes.is_empty() { return None; }
    let custom_row_weights = vec![false; panes.len()];
    Some((Column { panes, row_weights: weights, custom_row_weights }, ids))
}

/// Recursively flatten a SavedColumn tree into panes + weights.
/// `weight_share` is this node's share of the parent's height.
fn flatten_saved_column(
    saved: &SavedColumn,
    weight_share: f32,
    cols: u16,
    rows: u16,
    config: &Config,
    panes: &mut Vec<Pane>,
    weights: &mut Vec<f32>,
    ids: &mut Vec<PaneId>,
) -> Option<()> {
    match saved {
        SavedColumn::Leaf { cwd, last_command, custom_title, minimized } => {
            let t = std::time::Instant::now();
            let mut pane = Pane::spawn(cols, rows, config, cwd.as_deref()).ok()?;
            log::info!("[STARTUP] Pane::spawn id={} cwd={:?} in {:?}", pane.id, cwd, t.elapsed());
            let id = pane.id;
            if let Some(cmd) = last_command {
                pane.pending_command.set(Some(cmd.clone()));
                pane.terminal.write().last_command = Some(cmd.clone());
            }
            pane.custom_title = custom_title.clone();
            pane.minimized = *minimized;
            panes.push(pane);
            weights.push(weight_share);
            ids.push(id);
            Some(())
        }
        SavedColumn::VSplit { top, bottom, ratio, .. } => {
            flatten_saved_column(top, weight_share * ratio, cols, rows, config, panes, weights, ids)?;
            flatten_saved_column(bottom, weight_share * (1.0 - ratio), cols, rows, config, panes, weights, ids)?;
            Some(())
        }
    }
}

/// Restore from legacy SavedTree format, flattening HSplits into columns.
fn restore_legacy_tree(saved: &SavedTree, cols: u16, rows: u16, config: &Config) -> Option<(Vec<Column>, Vec<f32>, Vec<PaneId>)> {
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
            Some((vec![Column::new(pane)], vec![1.0], vec![id]))
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
            // VSplit within a column — restore as a single flat column
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

fn count_panes_in_saved_column(col: &SavedColumn) -> usize {
    match col {
        SavedColumn::Leaf { .. } => 1,
        SavedColumn::VSplit { top, bottom, .. } => {
            count_panes_in_saved_column(top) + count_panes_in_saved_column(bottom)
        }
    }
}

pub fn count_panes_in_saved_tab(tab: &SavedTab) -> usize {
    if let Some(ref flat_cols) = tab.flat_columns {
        flat_cols.iter().map(|c| c.panes.len()).sum()
    } else if let Some(ref cols) = tab.columns {
        cols.iter().map(count_panes_in_saved_column).sum()
    } else {
        1 // legacy format, approximate
    }
}

/// Restore a single saved tab. Used by recent projects and session restore.
pub fn restore_saved_tab(saved: &SavedTab, cols: u16, rows: u16, config: &Config) -> Option<Tab> {
    let (columns, column_weights, custom_weights, pane_ids) = if let Some(ref flat_cols) = saved.flat_columns {
        // New flat format (v4)
        let weights = saved.column_weights.clone().unwrap_or_else(|| vec![1.0; flat_cols.len()]);
        let cweights = saved.custom_weights.clone().unwrap_or_else(|| vec![false; flat_cols.len()]);
        let mut all_ids = Vec::new();
        let mut cols_vec = Vec::new();
        for fc in flat_cols {
            let (col, ids) = restore_flat_column(fc, cols, rows, config)?;
            cols_vec.push(col);
            all_ids.extend(ids);
        }
        (cols_vec, weights, cweights, all_ids)
    } else if let Some(ref saved_cols) = saved.columns {
        // Legacy format (v3) — flatten SavedColumn trees into flat Columns
        let weights = saved.column_weights.clone().unwrap_or_else(|| vec![1.0; saved_cols.len()]);
        let cweights = saved.custom_weights.clone().unwrap_or_else(|| vec![false; saved_cols.len()]);
        let mut all_ids = Vec::new();
        let mut cols_vec = Vec::new();
        for sc in saved_cols {
            let (col, ids) = restore_column(sc, cols, rows, config)?;
            cols_vec.push(col);
            all_ids.extend(ids);
        }
        (cols_vec, weights, cweights, all_ids)
    } else if let Some(ref tree) = saved.tree {
        // Legacy format (v2) — flatten HSplits
        let (cols_vec, weights, ids) = restore_legacy_tree(tree, cols, rows, config)?;
        let cweights = vec![false; cols_vec.len()];
        (cols_vec, weights, cweights, ids)
    } else {
        log::warn!("SavedTab has neither flat_columns, columns, nor tree");
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
        custom_weights,
        focused_pane,
        custom_title: saved.custom_title.clone(),
        color: saved.color,
        has_bell: false,
        has_completion: false,
        minimized_stack: Vec::new(),
        scroll_offset_x: saved.scroll_offset_x.unwrap_or(0.0),
        virtual_width_override: saved.virtual_width_override.unwrap_or(0.0),
        cell_h: std::cell::Cell::new(0.0),
    };
    tab.rebuild_minimized_stack();
    Some(tab)
}

fn restore_window_tabs(ws: &WindowSession, config: &Config) -> Option<(Vec<Tab>, usize, Vec<(usize, SavedTab)>)> {
    let cols = config.terminal.columns;
    let rows = config.terminal.rows;
    let total = ws.tabs.len();
    let active_idx = ws.active_tab.min(total.saturating_sub(1));

    // Restore the active tab first (priority)
    let t = std::time::Instant::now();
    let pane_count = count_panes_in_saved_tab(&ws.tabs[active_idx]);
    let active_tab = match restore_saved_tab(&ws.tabs[active_idx], cols, rows, config) {
        Some(tab) => {
            log::info!("[STARTUP] active tab {}/{} restored ({} panes) in {:?}", active_idx + 1, total, pane_count, t.elapsed());
            tab
        }
        None => {
            log::warn!("Failed to restore active tab, falling back to sequential restore");
            // Fallback: try all tabs sequentially
            let mut tabs = Vec::new();
            for saved_tab in &ws.tabs {
                if let Some(tab) = restore_saved_tab(saved_tab, cols, rows, config) {
                    tabs.push(tab);
                }
            }
            if tabs.is_empty() { return None; }
            return Some((tabs, 0, Vec::new()));
        }
    };

    // Build tab list: placeholders at deferred positions, real tab at active position
    let mut tabs = Vec::with_capacity(total);
    let mut deferred = Vec::new();
    let mut active_tab = Some(active_tab);
    for (i, saved_tab) in ws.tabs.iter().enumerate() {
        if i == active_idx {
            tabs.push(active_tab.take().unwrap());
        } else {
            // Create a placeholder tab (single empty pane) so tab indices are stable
            match crate::pane::Tab::new(config) {
                Ok(mut placeholder) => {
                    // Copy visual metadata so the tab bar looks right
                    placeholder.custom_title = saved_tab.custom_title.clone();
                    placeholder.color = saved_tab.color;
                    tabs.push(placeholder);
                    deferred.push((i, saved_tab.clone()));
                }
                Err(e) => log::warn!("Failed to create placeholder tab {}: {}", i, e),
            }
        }
    }

    if tabs.is_empty() { return None; }
    log::info!("[STARTUP] {} tab(s) deferred for progressive restore", deferred.len());
    Some((tabs, active_idx, deferred))
}

/// Restore a multi-window session. Returns a list of windows to create.
pub fn restore_session(session: Session, config: &Config) -> Option<Vec<RestoredWindow>> {
    let mut windows = Vec::new();

    for ws in &session.windows {
        if let Some((tabs, active_tab, deferred_tabs)) = restore_window_tabs(ws, config) {
            windows.push(RestoredWindow {
                tabs,
                active_tab,
                frame: ws.frame,
                deferred_tabs,
            });
        }
    }

    if windows.is_empty() {
        return None;
    }

    Some(windows)
}
