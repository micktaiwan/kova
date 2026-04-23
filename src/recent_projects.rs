use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;

use crate::session::SavedTab;

/// Maximum number of recent projects to keep.
const MAX_RECENT_PROJECTS: usize = 50;

#[derive(Clone, Serialize, Deserialize)]
pub struct RecentProject {
    pub path: String,
    pub last_opened: u64, // seconds since UNIX epoch
    pub tab: SavedTab,
}

#[derive(Default, Serialize, Deserialize)]
pub struct RecentProjects {
    pub projects: Vec<RecentProject>,
}

fn recent_projects_path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    PathBuf::from(home).join(".config/kova/recent_projects.json")
}

pub fn load() -> RecentProjects {
    let path = recent_projects_path();
    let data = match std::fs::read_to_string(&path) {
        Ok(d) => d,
        Err(_) => return RecentProjects::default(),
    };
    serde_json::from_str(&data).unwrap_or_default()
}

fn save(projects: &RecentProjects) {
    let path = recent_projects_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    match serde_json::to_string_pretty(projects) {
        Ok(json) => {
            if let Err(e) = std::fs::write(&path, json) {
                log::warn!("Failed to write recent projects: {}", e);
            }
        }
        Err(e) => log::warn!("Failed to serialize recent projects: {}", e),
    }
}

/// Add a tab snapshot to recent projects, keyed by its primary CWD.
/// Replaces any existing entry with the same path.
pub fn add(tab: &crate::pane::Tab) {
    add_batch(std::slice::from_ref(tab));
}

/// Add multiple tabs at once (single load/save cycle).
pub fn add_batch(tabs: &[crate::pane::Tab]) {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    let mut new_entries: Vec<RecentProject> = Vec::new();
    for tab in tabs {
        let path = primary_cwd(tab);
        if path.is_empty() {
            continue;
        }
        // If we already have this path in this batch, replace it
        new_entries.retain(|e| e.path != path);
        new_entries.push(RecentProject {
            path,
            last_opened: now,
            tab: crate::session::snapshot_tab(tab),
        });
    }

    if new_entries.is_empty() {
        return;
    }

    let mut projects = load();
    // Remove existing entries with matching paths
    for entry in &new_entries {
        projects.projects.retain(|p| p.path != entry.path);
    }
    // Prepend new entries (most recent first)
    new_entries.extend(projects.projects);
    projects.projects = new_entries;
    // Cap at max
    projects.projects.truncate(MAX_RECENT_PROJECTS);
    save(&projects);
}

/// Remove a recent project by path.
pub fn remove(path: &str) {
    let mut projects = load();
    projects.projects.retain(|p| p.path != path);
    save(&projects);
}

/// Determine the primary CWD of a tab: the CWD of the focused pane,
/// or the most common CWD among all panes.
fn primary_cwd(tab: &crate::pane::Tab) -> String {
    // Try focused pane first
    if let Some(pane) = tab.pane(tab.focused_pane) {
        if let Some(cwd) = pane.cwd() {
            return cwd;
        }
    }

    // Fallback: most common CWD
    let mut counts: HashMap<String, usize> = HashMap::new();
    tab.for_each_pane(&mut |p| {
        if let Some(cwd) = p.cwd() {
            *counts.entry(cwd).or_insert(0) += 1;
        }
    });
    counts.into_iter()
        .max_by_key(|(_, count)| *count)
        .map(|(path, _)| path)
        .unwrap_or_default()
}

/// Tildify a path for display: /Users/foo/bar → ~/bar
pub fn tildify(path: &str) -> String {
    if let Ok(home) = std::env::var("HOME") {
        if let Some(rest) = path.strip_prefix(&home) {
            return format!("~{}", rest);
        }
    }
    path.to_string()
}

/// Format a duration as relative time: "2s", "3m", "1h", "2d", "1w", "3mo"
pub fn time_ago(epoch_secs: u64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let delta = now.saturating_sub(epoch_secs);
    if delta < 60 {
        format!("{}s", delta)
    } else if delta < 3600 {
        format!("{}m", delta / 60)
    } else if delta < 86400 {
        format!("{}h", delta / 3600)
    } else if delta < 604800 {
        format!("{}d", delta / 86400)
    } else if delta < 2592000 {
        format!("{}w", delta / 604800)
    } else {
        format!("{}mo", delta / 2592000)
    }
}

/// Count the number of panes (leaves) in a saved tab.
pub fn pane_count_tab(tab: &crate::session::SavedTab) -> usize {
    if let Some(ref flat) = tab.flat_columns {
        flat.iter().map(|c| c.panes.len()).sum()
    } else if let Some(ref cols) = tab.columns {
        cols.iter().map(pane_count_column).sum()
    } else if let Some(ref tree) = tab.tree {
        pane_count_tree(tree)
    } else {
        0
    }
}

fn pane_count_column(col: &crate::session::SavedColumn) -> usize {
    match col {
        crate::session::SavedColumn::Leaf { .. } => 1,
        crate::session::SavedColumn::VSplit { top, bottom, .. } => {
            pane_count_column(top) + pane_count_column(bottom)
        }
    }
}

fn pane_count_tree(tree: &crate::session::SavedTree) -> usize {
    match tree {
        crate::session::SavedTree::Leaf { .. } => 1,
        crate::session::SavedTree::HSplit { left, right, .. }
        | crate::session::SavedTree::VSplit { top: left, bottom: right, .. } => {
            pane_count_tree(left) + pane_count_tree(right)
        }
    }
}
