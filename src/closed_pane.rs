//! Undo for a closed pane: the record of what it takes to put one back, and
//! the stack of the most recent closes.
//!
//! Closing a pane kills its PTY, so none of the terminal itself survives — the
//! scrollback lives in RAM and goes with it. What comes back is a fresh shell
//! in the same directory, with the resume line of the agent conversation the
//! closed pane was holding pre-typed (`claude --resume <id>`, `omp --resume
//! <id>`, …), so the conversation is picked up exactly where it stopped.
//!
//! The record is taken while the pane is still alive: its conversation id is
//! read off the running process, and an agent forgets it on exit. The stack
//! lives in memory only, like the visit trail next door — after a relaunch, the
//! session file is what brings panes back.

use crate::pane::TabId;
use crate::session::{SavedFlatColumn, SavedPane, SavedTab};
use std::sync::Mutex;

/// How many closed panes the undo keeps. Deeper than any accident: `Cmd+W` is
/// pressed once, not ten times, and each entry costs a pane's worth of text.
pub const MAX_CLOSED_PANES: usize = 10;

/// A closed pane, and the slot it left.
pub struct ClosedPane {
    /// The pane as a session snapshot saves it: cwd, last command, titles,
    /// subject, and the agent conversation it was holding.
    pub saved: SavedPane,
    /// Tab it was closed from. The undo looks the tab up by this id so the
    /// pane goes back into the same column and row; when no tab of this window
    /// carries it any more — closed, or moved to another window since — the
    /// pane comes back as a tab of its own.
    pub tab_id: TabId,
    /// Column and pane index inside it — the slot the pane occupied.
    pub column: usize,
    pub index: usize,
    /// The tab's own look, used only when the tab is gone too (the closed pane
    /// was its last one): the tab that comes back still wears its name and
    /// color.
    pub tab_title: Option<String>,
    pub tab_color: Option<usize>,
}

impl ClosedPane {
    /// The single-pane tab this record becomes when the original tab is gone.
    pub fn sole_pane_tab(&self) -> SavedTab {
        let mut saved = self.saved.clone();
        // A pane the user just undid must be on screen: the record may come
        // from a pane that was parked minimized when it was closed, and coming
        // back invisible would look like the undo did nothing.
        saved.minimized = false;
        SavedTab {
            flat_columns: Some(vec![SavedFlatColumn {
                panes: vec![saved],
                row_weights: vec![1.0],
                custom_row_weights: None,
            }]),
            columns: None,
            column_weights: Some(vec![1.0]),
            custom_weights: None,
            tree: None,
            focused_leaf_index: 0,
            custom_title: self.tab_title.clone(),
            color: self.tab_color,
            virtual_width_override: None,
            scroll_offset_x: None,
            geometry_scale: None,
            subjects: Vec::new(),
            subject_colors: Default::default(),
        }
    }
}

/// The most recently closed panes, newest last.
pub struct ClosedPanes {
    entries: Vec<ClosedPane>,
}

impl ClosedPanes {
    pub const fn new() -> Self {
        ClosedPanes { entries: Vec::new() }
    }

    /// Record a pane that is about to close. Past the cap, the oldest entry
    /// falls off — an undo nobody reached for in ten closes is not wanted.
    pub fn push(&mut self, entry: ClosedPane) {
        self.entries.push(entry);
        if self.entries.len() > MAX_CLOSED_PANES {
            let excess = self.entries.len() - MAX_CLOSED_PANES;
            self.entries.drain(..excess);
        }
    }

    /// Take back the most recently closed pane. `None` when nothing was closed
    /// since the app started, or since the last undo emptied the stack.
    pub fn pop(&mut self) -> Option<ClosedPane> {
        self.entries.pop()
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.entries.len()
    }
}

/// The app-wide stack: closing is a pane gesture, and the undo crosses tabs and
/// windows the way the visit trail does.
static STACK: Mutex<ClosedPanes> = Mutex::new(ClosedPanes::new());

/// Record the pane `entry` describes, just before it is closed.
pub fn push(entry: ClosedPane) {
    STACK.lock().unwrap_or_else(|e| e.into_inner()).push(entry);
}

/// Take back the most recently closed pane, if any.
pub fn pop() -> Option<ClosedPane> {
    STACK.lock().unwrap_or_else(|e| e.into_inner()).pop()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pane(cwd: &str) -> SavedPane {
        SavedPane {
            cwd: Some(cwd.into()),
            last_command: Some("omp".into()),
            custom_title: None,
            minimized: false,
            title: None,
            claude_session: None,
            agent_session: None,
            subject: None,
        }
    }

    fn entry(cwd: &str, tab_id: TabId, index: usize) -> ClosedPane {
        ClosedPane {
            saved: pane(cwd),
            tab_id,
            column: 0,
            index,
            tab_title: Some("Kova".into()),
            tab_color: Some(2),
        }
    }

    #[test]
    fn the_undo_hands_panes_back_newest_first() {
        let mut stack = ClosedPanes::new();
        stack.push(entry("/a", 1, 0));
        stack.push(entry("/b", 1, 1));
        assert_eq!(stack.len(), 2);
        assert_eq!(stack.pop().unwrap().saved.cwd.as_deref(), Some("/b"));
        assert_eq!(stack.pop().unwrap().saved.cwd.as_deref(), Some("/a"));
        assert!(stack.pop().is_none(), "an empty stack reopens nothing");
    }

    #[test]
    fn only_the_most_recent_closes_are_kept() {
        let mut stack = ClosedPanes::new();
        for i in 0..MAX_CLOSED_PANES + 3 {
            stack.push(entry(&format!("/{i}"), 1, i));
        }
        assert_eq!(stack.len(), MAX_CLOSED_PANES);
        // The newest are still there, the three oldest have fallen off.
        assert_eq!(stack.pop().unwrap().saved.cwd.as_deref(), Some("/12"));
        let mut oldest = String::new();
        while let Some(e) = stack.pop() {
            oldest = e.saved.cwd.unwrap();
        }
        assert_eq!(oldest, "/3");
    }

    #[test]
    fn a_record_whose_tab_is_gone_becomes_that_tab_again() {
        let mut record = entry("/work", 7, 2);
        record.saved.minimized = true;
        let saved = record.sole_pane_tab();
        assert_eq!(saved.custom_title.as_deref(), Some("Kova"));
        assert_eq!(saved.color, Some(2));
        assert_eq!(saved.flat_columns.as_ref().unwrap().len(), 1);
        let panes = &saved.flat_columns.as_ref().unwrap()[0].panes;
        assert_eq!(panes.len(), 1);
        assert_eq!(panes[0].cwd.as_deref(), Some("/work"));
        assert!(!panes[0].minimized, "an undone pane comes back on screen");
    }
}
