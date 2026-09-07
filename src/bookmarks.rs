//! Conversations the user wants back — the list Cmd+P offers under
//! "Bookmarks", kept apart from the session file on purpose.
//!
//! The session file answers "what was open when Kova quit"; it is a safety net
//! and it forgets a tab as soon as it is closed. A bookmark answers a different
//! question — "this conversation matters, put it back in front of me whenever I
//! ask" — so it survives closing the pane, quitting, and a session file that
//! never came back.
//!
//! A bookmark points at a conversation by the id its agent's `resume` takes,
//! which is the only identifier that outlives a pane. Bookmarking a pane at a
//! bare shell prompt is allowed too: it then only remembers the directory.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

use crate::agent_session::Agent;

/// Cap on the list. Past this it stops being a shortlist and becomes a second
/// history, which the search palette already is.
pub const MAX_BOOKMARKS: usize = 32;

/// One saved conversation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Bookmark {
    /// Agent holding the conversation, absent for a bookmarked shell.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<Agent>,
    /// Conversation id, absent for a bookmarked shell.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// Directory the pane was in — where the conversation is reopened.
    pub cwd: String,
    /// What the pane was called when it was bookmarked. Kept as written: it is
    /// the name the user recognises the row by, and the live title of a
    /// reopened conversation may say something else entirely.
    pub label: String,
}

impl Bookmark {
    /// What makes two bookmarks the same thing: the conversation id when there
    /// is one, the directory otherwise.
    pub fn key(&self) -> &str {
        self.session_id.as_deref().unwrap_or(&self.cwd)
    }

    /// The command line that puts this conversation back, or `None` for a
    /// bookmarked shell (the pane just opens in `cwd`).
    pub fn resume_command(&self) -> Option<String> {
        let agent = self.agent?;
        let id = self.session_id.as_deref()?;
        crate::agent_session::resume_command(agent, id, None)
    }
}

/// The list, newest first.
#[derive(Default, Serialize, Deserialize)]
pub struct Bookmarks {
    #[serde(default)]
    pub items: Vec<Bookmark>,
}

fn path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    PathBuf::from(home).join(".config/kova/bookmarks.json")
}

/// Read the list from disk. A missing or unreadable file is an empty list —
/// never a reason to refuse to open the switcher.
pub fn load() -> Bookmarks {
    let path = path();
    let Ok(data) = std::fs::read_to_string(&path) else {
        return Bookmarks::default();
    };
    match serde_json::from_str(&data) {
        Ok(b) => b,
        Err(e) => {
            log::warn!("Failed to parse {} ({}); starting from an empty list", path.display(), e);
            Bookmarks::default()
        }
    }
}

/// Write the list back, owner-readable only: it holds working directories and
/// conversation ids, same as the session file.
pub fn save(bookmarks: &Bookmarks) {
    let path = path();
    if let Some(parent) = path.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            log::warn!("Failed to create {}: {}", parent.display(), e);
            return;
        }
    }
    match serde_json::to_string_pretty(bookmarks) {
        Ok(json) => {
            if let Err(e) = crate::session::write_owner_only(&path, &json) {
                log::warn!("Failed to write {}: {}", path.display(), e);
            }
        }
        Err(e) => log::warn!("Failed to serialize bookmarks: {}", e),
    }
}

/// The keys of every saved bookmark, for the per-frame "is this pane
/// bookmarked?" test: the status bar asks it for every pane of every frame, so
/// it reads a cached set instead of the file.
pub fn keys(items: &[Bookmark]) -> std::collections::HashSet<String> {
    items.iter().map(|b| b.key().to_string()).collect()
}

/// Whether `items` already holds this conversation.
pub fn contains(items: &[Bookmark], candidate: &Bookmark) -> bool {
    items.iter().any(|b| b.key() == candidate.key())
}

/// Add `candidate`, or drop it if it is already there. Returns true when the
/// list ends up holding it — what the caller says out loud.
///
/// A new bookmark goes on top: the list is read top-down and the freshest
/// decision is the one most likely to be wanted again.
pub fn toggle(items: &mut Vec<Bookmark>, candidate: Bookmark) -> bool {
    if contains(items, &candidate) {
        let key = candidate.key().to_string();
        items.retain(|b| b.key() != key);
        return false;
    }
    items.insert(0, candidate);
    items.truncate(MAX_BOOKMARKS);
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn claude(id: &str, cwd: &str) -> Bookmark {
        Bookmark {
            agent: Some(Agent::Claude),
            session_id: Some(id.to_string()),
            cwd: cwd.to_string(),
            label: format!("pane in {}", cwd),
        }
    }

    #[test]
    fn toggling_the_same_conversation_twice_leaves_the_list_empty() {
        let mut items = Vec::new();
        assert!(toggle(&mut items, claude("abc", "/a")));
        assert_eq!(items.len(), 1);
        // Same conversation, seen from another directory and with another
        // title: still the same bookmark.
        let mut moved = claude("abc", "/b");
        moved.label = "renamed".into();
        assert!(!toggle(&mut items, moved));
        assert!(items.is_empty());
    }

    #[test]
    fn keys_mix_conversation_ids_and_shell_directories() {
        let mut shell = claude("ignored", "/tmp/work");
        shell.agent = None;
        shell.session_id = None;
        let set = keys(&[claude("abc", "/a"), shell]);
        assert!(set.contains("abc"));
        assert!(set.contains("/tmp/work"));
        // The cwd of a bookmark that has a conversation id is not a key: two
        // panes in the same directory are not the same bookmark.
        assert!(!set.contains("/a"));
    }

    #[test]
    fn a_shell_bookmark_is_keyed_on_its_directory() {
        let shell = Bookmark {
            agent: None,
            session_id: None,
            cwd: "/a".into(),
            label: "a".into(),
        };
        let mut items = vec![shell.clone()];
        assert!(contains(&items, &shell));
        // A conversation that happens to run in the same directory is a
        // different thing, and does not collide with it.
        assert!(toggle(&mut items, claude("abc", "/a")));
        assert_eq!(items.len(), 2);
        assert!(items[0].session_id.is_some(), "the newest lands on top");
    }

    #[test]
    fn a_shell_bookmark_reopens_without_a_command() {
        let shell = Bookmark { agent: None, session_id: None, cwd: "/a".into(), label: "a".into() };
        assert_eq!(shell.resume_command(), None);
        assert_eq!(
            claude("abc", "/a").resume_command().as_deref(),
            Some("claude --resume abc")
        );
        let codex = Bookmark {
            agent: Some(Agent::Codex),
            session_id: Some("01a07651-015e-78a3-97f2-2eaf0f0cd663".into()),
            cwd: "/a".into(),
            label: "a".into(),
        };
        assert_eq!(
            codex.resume_command().as_deref(),
            Some("codex resume 01a07651-015e-78a3-97f2-2eaf0f0cd663")
        );
    }

    #[test]
    fn the_list_never_grows_past_its_cap() {
        let mut items = Vec::new();
        for i in 0..MAX_BOOKMARKS + 5 {
            toggle(&mut items, claude(&format!("id-{}", i), "/a"));
        }
        assert_eq!(items.len(), MAX_BOOKMARKS);
        // The cap drops the oldest, not the newest.
        assert_eq!(items[0].session_id.as_deref(), Some(format!("id-{}", MAX_BOOKMARKS + 4).as_str()));
    }
}
