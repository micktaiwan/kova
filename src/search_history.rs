//! What has been searched during this run of Kova.
//!
//! Deliberately in memory only: a query is a scratch thought ("that pane where
//! the deploy failed"), useful for the next few minutes and stale by tomorrow.
//! Nothing is written to disk, so nothing has to be pruned, and a crash loses
//! nothing anyone would miss. The list dies with the process.
//!
//! Two independent lists, because the two searches answer different questions:
//! the palette (Cmd+Shift+F) looks across every pane and every closed Claude
//! session, the filter (Cmd+F) looks inside one pane's scrollback.

use parking_lot::Mutex;

/// Queries kept per scope. Small on purpose: the palette shows them as rows on
/// an empty query, and a list longer than a screenful stops being a shortcut.
pub const MAX_ENTRIES: usize = 10;

/// Which search a query came from.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// The global search palette (Cmd+Shift+F).
    Palette,
    /// The in-pane line filter (Cmd+F).
    Filter,
}

static PALETTE: Mutex<Vec<String>> = Mutex::new(Vec::new());
static FILTER: Mutex<Vec<String>> = Mutex::new(Vec::new());

fn slot(scope: Scope) -> &'static Mutex<Vec<String>> {
    match scope {
        Scope::Palette => &PALETTE,
        Scope::Filter => &FILTER,
    }
}

/// Remember `query` as the most recent search of this scope.
///
/// Called when a search *ends* (the palette closes, the filter closes), not on
/// every keystroke: the live search re-runs on each character, and recording
/// those would fill the list with the prefixes of one query.
pub fn record(scope: Scope, query: &str) {
    let mut list = slot(scope).lock();
    push_entry(&mut list, query);
}

/// The queries of this scope, most recent first.
pub fn list(scope: Scope) -> Vec<String> {
    slot(scope).lock().clone()
}

/// Whether this scope has anything to recall. Cheaper than `list` for the
/// render path, which asks on every frame the filter is open.
pub fn is_empty(scope: Scope) -> bool {
    slot(scope).lock().is_empty()
}

/// Insert `query` at the front of `list`, deduped and capped.
///
/// Split out of `record` so the ordering rules are testable without touching
/// the process-wide state.
fn push_entry(list: &mut Vec<String>, query: &str) {
    let query = query.trim();
    if query.is_empty() {
        return;
    }
    // Re-searching something already in the list moves it to the front rather
    // than adding a second copy: the list is "what I looked for", not a log.
    // Case-insensitive, because "Deploy" and "deploy" are the same search.
    list.retain(|q| !q.eq_ignore_ascii_case(query));
    list.insert(0, query.to_string());
    list.truncate(MAX_ENTRIES);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn most_recent_comes_first() {
        let mut list = Vec::new();
        push_entry(&mut list, "alpha");
        push_entry(&mut list, "beta");
        assert_eq!(list, vec!["beta".to_string(), "alpha".to_string()]);
    }

    #[test]
    fn a_repeat_moves_up_instead_of_duplicating() {
        let mut list = Vec::new();
        push_entry(&mut list, "alpha");
        push_entry(&mut list, "beta");
        push_entry(&mut list, "ALPHA");
        assert_eq!(list, vec!["ALPHA".to_string(), "beta".to_string()]);
    }

    #[test]
    fn blank_queries_are_not_recorded() {
        let mut list = Vec::new();
        push_entry(&mut list, "");
        push_entry(&mut list, "   ");
        assert!(list.is_empty());
        // Surrounding spaces are not part of the query.
        push_entry(&mut list, "  deploy  ");
        assert_eq!(list, vec!["deploy".to_string()]);
    }

    #[test]
    fn the_list_is_capped_and_drops_the_oldest() {
        let mut list = Vec::new();
        for i in 0..MAX_ENTRIES + 5 {
            push_entry(&mut list, &format!("q{i}"));
        }
        assert_eq!(list.len(), MAX_ENTRIES);
        assert_eq!(list[0], format!("q{}", MAX_ENTRIES + 4));
        // The oldest entries fell off the end.
        assert!(!list.contains(&"q0".to_string()));
    }
}
