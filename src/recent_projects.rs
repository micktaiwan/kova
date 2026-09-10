use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;

use crate::session::SavedTab;

/// Maximum number of recent projects to keep.
const MAX_RECENT_PROJECTS: usize = 50;

/// A closed tab, kept so `Cmd+O` can put it back. The file and type keep their
/// "project" name for compatibility; what is stored has always been a whole tab.
#[derive(Clone, Serialize, Deserialize)]
pub struct RecentProject {
    /// Primary CWD of the tab when it was closed — shown next to the name, and
    /// the identity of a tab the user never named.
    pub path: String,
    pub last_opened: u64, // seconds since UNIX epoch
    pub tab: SavedTab,
}

impl RecentProject {
    /// Name the user gave the tab, if any.
    pub fn title(&self) -> Option<&str> {
        named(self.tab.custom_title.as_deref())
    }

    /// What makes two closed tabs the same one. See [`key_of`].
    pub fn key(&self) -> String {
        key_of(self.tab.custom_title.as_deref(), &self.path)
    }

    /// Whether any pane of the saved tab was in `cwd` — the focused pane is
    /// only one of them.
    pub fn has_cwd(&self, cwd: &str) -> bool {
        let mut found = self.path == cwd;
        if let Some(ref flat) = self.tab.flat_columns {
            found |= flat.iter().flat_map(|c| &c.panes).any(|p| p.cwd.as_deref() == Some(cwd));
        } else if let Some(ref cols) = self.tab.columns {
            found |= cols.iter().any(|c| column_has_cwd(c, cwd));
        } else if let Some(ref tree) = self.tab.tree {
            found |= tree_has_cwd(tree, cwd);
        }
        found
    }
}

fn named(title: Option<&str>) -> Option<&str> {
    title.map(str::trim).filter(|t| !t.is_empty())
}

/// Identity of a tab: its name, case-insensitive, or its primary CWD when it
/// has none. Keying on the CWD alone filed a tab under whatever directory its
/// focused pane happened to be in, and let two tabs sharing it overwrite each
/// other.
fn key_of(custom_title: Option<&str>, path: &str) -> String {
    match named(custom_title) {
        Some(t) => format!("title:{}", t.to_lowercase()),
        None => format!("path:{}", path),
    }
}

/// Key of a live tab, comparable with [`RecentProject::key`].
pub fn tab_key(tab: &crate::pane::Tab) -> String {
    key_of(tab.custom_title.as_deref(), &primary_cwd(tab))
}

fn column_has_cwd(col: &crate::session::SavedColumn, cwd: &str) -> bool {
    match col {
        crate::session::SavedColumn::Leaf { cwd: c, .. } => c.as_deref() == Some(cwd),
        crate::session::SavedColumn::VSplit { top, bottom, .. } => {
            column_has_cwd(top, cwd) || column_has_cwd(bottom, cwd)
        }
    }
}

fn tree_has_cwd(tree: &crate::session::SavedTree, cwd: &str) -> bool {
    match tree {
        crate::session::SavedTree::Leaf { cwd: c, .. } => c.as_deref() == Some(cwd),
        crate::session::SavedTree::HSplit { left, right, .. }
        | crate::session::SavedTree::VSplit { top: left, bottom: right, .. } => {
            tree_has_cwd(left, cwd) || tree_has_cwd(right, cwd)
        }
    }
}

/// Keep the first entry of each key. Entries are most recent first, so the
/// latest closing of a tab wins over older ones.
pub fn dedup(entries: Vec<RecentProject>) -> Vec<RecentProject> {
    let mut seen = std::collections::HashSet::new();
    entries.into_iter().filter(|e| seen.insert(e.key())).collect()
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
    match serde_json::from_str(&data) {
        Ok(p) => p,
        Err(e) => {
            // A corrupt file silently became an empty list before; the next
            // save then wiped the history. At least leave a trace.
            log::warn!(
                "Failed to parse {} ({}); starting with an empty list (history will be overwritten on next save)",
                path.display(), e
            );
            RecentProjects::default()
        }
    }
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

/// Add a closed tab, replacing any earlier closing of the same tab (same name,
/// or same primary CWD for an unnamed tab).
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
    projects.projects = merge(new_entries, projects.projects);
    save(&projects);
}

/// Put freshly closed tabs in front of the stored ones, in their tab order, one
/// entry per tab, capped. Within `fresh`, a later tab wins over an earlier one
/// of the same key.
fn merge(fresh: Vec<RecentProject>, stored: Vec<RecentProject>) -> Vec<RecentProject> {
    let mut all = dedup(fresh.into_iter().rev().collect());
    all.reverse();
    all.extend(stored);
    let mut all = dedup(all);
    all.truncate(MAX_RECENT_PROJECTS);
    all
}

/// Forget a closed tab, by its key.
pub fn remove(key: &str) {
    let mut projects = load();
    projects.projects.retain(|p| p.key() != key);
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A closed tab named `title`, focused in `path`, whose panes sit in `cwds`.
    fn closed(title: Option<&str>, path: &str, cwds: &[&str], at: u64) -> RecentProject {
        let panes: Vec<serde_json::Value> = cwds.iter().map(|c| serde_json::json!({ "cwd": c })).collect();
        let tab = serde_json::from_value(serde_json::json!({
            "flat_columns": [{ "panes": panes, "row_weights": vec![1.0; cwds.len()] }],
            "focused_leaf_index": 0,
            "custom_title": title,
            "color": null,
        }))
        .expect("a SavedTab");
        RecentProject { path: path.into(), last_opened: at, tab }
    }

    #[test]
    fn a_named_tab_is_the_same_tab_whatever_its_focused_directory() {
        let a = closed(Some("Kova"), "/p/kova", &["/p/kova"], 1);
        let b = closed(Some(" kova "), "/p/mickaelfm", &["/p/kova", "/p/mickaelfm"], 2);
        assert_eq!(a.key(), b.key());
    }

    #[test]
    fn an_unnamed_tab_is_identified_by_its_directory() {
        let a = closed(None, "/p/nfc", &["/p/nfc"], 1);
        let blank = closed(Some("  "), "/p/nfc", &["/p/nfc"], 2);
        let other = closed(None, "/p/music", &["/p/music"], 3);
        assert_eq!(a.key(), blank.key(), "a blank name is no name");
        assert_ne!(a.key(), other.key());
        // Two different named tabs may share a focused directory.
        assert_ne!(
            closed(Some("ASL"), "/p/asl", &["/p/asl"], 1).key(),
            closed(Some("Docs"), "/p/asl", &["/p/asl"], 1).key(),
        );
    }

    #[test]
    fn closing_a_tab_again_replaces_its_older_entry() {
        let stored = vec![
            closed(Some("ASL"), "/p/asl", &["/p/asl"], 5),
            closed(Some("Kova"), "/p/kova", &["/p/kova"], 4),
        ];
        let fresh = vec![closed(Some("kova"), "/p/mickaelfm", &["/p/mickaelfm"], 9)];
        let merged = merge(fresh, stored);
        let paths: Vec<&str> = merged.iter().map(|e| e.path.as_str()).collect();
        assert_eq!(paths, ["/p/mickaelfm", "/p/asl"]);
    }

    #[test]
    fn closing_a_window_keeps_its_tab_order_and_the_last_duplicate() {
        let fresh = vec![
            closed(Some("A"), "/a1", &["/a1"], 9),
            closed(Some("B"), "/b", &["/b"], 9),
            closed(Some("A"), "/a2", &["/a2"], 9),
            closed(Some("C"), "/c", &["/c"], 9),
        ];
        let merged = merge(fresh, Vec::new());
        let paths: Vec<&str> = merged.iter().map(|e| e.path.as_str()).collect();
        assert_eq!(paths, ["/b", "/a2", "/c"]);
    }

    #[test]
    fn dedup_keeps_the_most_recent_of_legacy_duplicates() {
        // Files written before tabs were keyed by name hold the same tab twice.
        let entries = vec![
            closed(Some("ASL"), "/p/asl", &["/p/asl"], 5),
            closed(Some("ASL"), "/p/asl/server", &["/p/asl/server"], 3),
        ];
        let kept = dedup(entries);
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].path, "/p/asl");
    }

    #[test]
    fn has_cwd_looks_at_every_pane_not_only_the_focused_one() {
        let e = closed(Some("Kova"), "/p/kova", &["/p/kova", "/p/mickaelfm"], 1);
        assert!(e.has_cwd("/p/kova"));
        assert!(e.has_cwd("/p/mickaelfm"));
        assert!(!e.has_cwd("/p/mira"));
    }
}
