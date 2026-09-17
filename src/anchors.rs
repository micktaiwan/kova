//! The anchors — the few conversations today is for, kept at the top of Cmd+P
//! and one keystroke away.
//!
//! A bookmark answers "this conversation matters, give it back when I ask"; an
//! anchor answers a narrower question — "this is what I am supposed to be doing
//! right now" — so it is a short list, it sits above the bookmarks, and Cmd+A
//! always goes to its first entry without asking which one.
//!
//! An anchor is stored exactly like a bookmark (same fields, same key, same
//! resume line): the two lists differ by what they mean, not by what they hold,
//! and a conversation can be in both. When it is, it shows up only under the
//! anchors — repeating it in the bookmarks below would spend a row saying the
//! same thing twice.
//!
//! The list is also Kova's own: nothing outside has to exist for it to work.
//! An external tool that knows what the day is for (track, here) pushes its
//! anchor in through the `set-anchor` IPC command, which is the whole of the
//! coupling — Kova never calls out.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

use crate::bookmarks::Bookmark;

/// One anchored conversation. Same shape as a bookmark, deliberately.
pub type Anchor = Bookmark;

/// Cap on the list. Anchors are "what today is for": past a handful they stop
/// being a commitment and become another bookmark list, which already exists.
pub const MAX_ANCHORS: usize = 8;

/// The list, first entry first — the one Cmd+A goes to.
#[derive(Default, Serialize, Deserialize)]
pub struct Anchors {
    #[serde(default)]
    pub items: Vec<Anchor>,
}

fn path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    PathBuf::from(home).join(".config/kova/anchors.json")
}

/// Read the saved anchors. Same reading as the bookmarks, one file apart.
pub fn load() -> Anchors {
    crate::session::load_owner_only(&path(), "anchor list")
}

/// Write the list back, owner-only like the bookmarks and the session file.
pub fn save(anchors: &Anchors) {
    crate::session::save_owner_only(&path(), "anchors", anchors);
}

/// The anchor an outside tool describes: a directory, plus the conversation to
/// reopen there when it named one. A conversation id with no agent is dropped —
/// nothing could resume it, and a row Enter does nothing with is worse than no
/// row at all.
pub fn from_named(
    cwd: String,
    session_id: Option<String>,
    agent: Option<crate::agent_session::Agent>,
    label: Option<String>,
) -> Anchor {
    Anchor {
        session_id: session_id.filter(|_| agent.is_some()),
        agent,
        label: label.unwrap_or_else(|| {
            std::path::Path::new(&cwd)
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or(&cwd)
                .to_string()
        }),
        cwd,
    }
}

/// The keys of every anchor, for the frame-by-frame "is this pane anchored".
pub fn keys(items: &[Anchor]) -> std::collections::HashSet<String> {
    items.iter().map(|a| a.key().to_string()).collect()
}

/// What a toggle did, so the status bar can say it. A full list is its own
/// answer: silently dropping the anchor that was just asked for would read as
/// the key doing nothing.
#[derive(Debug, PartialEq, Eq)]
pub enum Toggled {
    Added,
    Removed,
    Full,
}

/// Add the conversation, or drop it if it is already anchored. A new anchor
/// goes to the **end**: Cmd+A has one target and it should not move under you
/// because you anchored something else.
pub fn toggle(items: &mut Vec<Anchor>, candidate: Anchor) -> Toggled {
    if remove(items, candidate.key()) {
        return Toggled::Removed;
    }
    if items.len() >= MAX_ANCHORS {
        return Toggled::Full;
    }
    items.push(candidate);
    Toggled::Added
}

/// Put the conversation at the head of the list — what Cmd+A goes to. This is
/// the shape `set-anchor` takes: an outside tool saying "today is this one".
/// Already anchored, it moves to the head rather than appearing twice, since
/// two rows for one conversation is exactly what the key is there to prevent.
/// Returns the anchor the cap pushed out, if any. The two ways in differ on
/// purpose: Cmd+Shift+A refuses a full list, because you are standing there and
/// can drop one; a push from outside always lands, because nobody is there to
/// answer, and the caller is told what fell off instead.
pub fn promote(items: &mut Vec<Anchor>, candidate: Anchor) -> Option<Anchor> {
    remove(items, candidate.key());
    items.insert(0, candidate);
    (items.len() > MAX_ANCHORS).then(|| items.remove(items.len() - 1))
}

/// Drop the anchor with this key. Returns whether there was one.
pub fn remove(items: &mut Vec<Anchor>, key: &str) -> bool {
    let before = items.len();
    items.retain(|a| a.key() != key);
    items.len() != before
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_session::Agent;

    fn claude(id: &str, cwd: &str) -> Anchor {
        Anchor {
            agent: Some(Agent::Claude),
            session_id: Some(id.to_string()),
            cwd: cwd.to_string(),
            label: id.to_string(),
        }
    }

    #[test]
    fn a_new_anchor_never_steals_the_first_place() {
        let mut items = vec![claude("first", "/a")];
        toggle(&mut items, claude("second", "/b"));
        assert_eq!(items[0].session_id.as_deref(), Some("first"), "Cmd+A keeps its target");
        assert_eq!(items.len(), 2);
    }

    #[test]
    fn toggling_the_same_conversation_twice_leaves_the_list_empty() {
        let mut items = Vec::new();
        assert_eq!(toggle(&mut items, claude("abc", "/a")), Toggled::Added);
        assert_eq!(toggle(&mut items, claude("abc", "/a")), Toggled::Removed);
        assert!(items.is_empty());
    }

    #[test]
    fn promoting_an_anchor_already_there_moves_it_without_duplicating_it() {
        let mut items = vec![claude("a", "/a"), claude("b", "/b")];
        promote(&mut items, claude("b", "/b"));
        assert_eq!(items.len(), 2, "no second row for the same conversation");
        assert_eq!(items[0].session_id.as_deref(), Some("b"));
        assert_eq!(items[1].session_id.as_deref(), Some("a"));
    }

    #[test]
    fn promoting_a_shell_dedupes_on_its_directory() {
        let shell = |cwd: &str| Anchor {
            agent: None,
            session_id: None,
            cwd: cwd.to_string(),
            label: cwd.to_string(),
        };
        let mut items = vec![shell("/work"), claude("a", "/a")];
        promote(&mut items, shell("/work"));
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].cwd, "/work");
    }

    #[test]
    fn a_full_list_says_so_instead_of_dropping_the_new_anchor_in_silence() {
        let mut items = Vec::new();
        for i in 0..MAX_ANCHORS {
            assert_eq!(toggle(&mut items, claude(&format!("id{i}"), "/a")), Toggled::Added);
        }
        assert_eq!(toggle(&mut items, claude("one-too-many", "/a")), Toggled::Full);
        assert_eq!(items.len(), MAX_ANCHORS);
        assert!(!items.iter().any(|a| a.session_id.as_deref() == Some("one-too-many")));
    }

    #[test]
    fn promoting_past_the_cap_drops_the_last_anchor_and_names_it() {
        let mut items: Vec<Anchor> =
            (0..MAX_ANCHORS).map(|i| claude(&format!("id{i}"), "/a")).collect();
        let dropped = promote(&mut items, claude("fresh", "/b"));
        assert_eq!(items.len(), MAX_ANCHORS);
        assert_eq!(items[0].session_id.as_deref(), Some("fresh"));
        assert_eq!(
            dropped.and_then(|a| a.session_id).as_deref(),
            Some(format!("id{}", MAX_ANCHORS - 1).as_str()),
            "the caller is told which anchor the cap pushed out"
        );
    }

    #[test]
    fn promoting_inside_the_cap_drops_nothing() {
        let mut items = vec![claude("a", "/a")];
        assert!(promote(&mut items, claude("b", "/b")).is_none());
    }

    #[test]
    fn a_conversation_id_with_no_agent_is_dropped_rather_than_saved_unreopenable() {
        let a = from_named("/w".into(), Some("abc".into()), None, None);
        assert_eq!(a.session_id, None, "nothing could resume it");
        assert_eq!(a.label, "w", "and the row is named after its directory");
        let b = from_named("/w".into(), Some("abc".into()), Some(Agent::Codex), Some("prez".into()));
        assert_eq!(b.session_id.as_deref(), Some("abc"));
        assert_eq!(b.label, "prez");
    }

    #[test]
    fn removing_by_key_drops_only_that_anchor() {
        let mut items = vec![claude("a", "/a"), claude("b", "/b")];
        assert!(remove(&mut items, "a"));
        assert!(!remove(&mut items, "a"));
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].session_id.as_deref(), Some("b"));
    }
}
