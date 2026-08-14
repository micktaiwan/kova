//! Visit history of panes — the trail of panes the user focused, walked
//! backward and forward like a browser's back/forward buttons.
//!
//! The trail is global (it crosses tabs and windows) and is fed by sampling
//! the focused pane of the key window once per frame, so every way of
//! focusing a pane (keyboard, mouse, IPC, tab switch) lands in it without
//! each call site having to remember to record anything.

use crate::pane::PaneId;
use std::sync::Mutex;

/// Cap on the trail length. Older entries are dropped from the front.
const MAX_ENTRIES: usize = 128;

/// What the rest of the app says about a pane the trail refers to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaneState {
    /// Visible and focusable — a valid landing spot.
    Focusable,
    /// Still there but minimized. Walked over without being focused, and kept
    /// in the trail so it comes back if the user restores it.
    Hidden,
    /// Closed. Dropped from the trail on sight.
    Gone,
}

/// An ordered trail of visited panes plus a cursor into it.
///
/// Entries are ordered oldest → newest. `pos` is the index of the pane
/// currently focused; everything after it is the forward trail left behind by
/// [`PaneHistory::back`].
pub struct PaneHistory {
    entries: Vec<PaneId>,
    pos: usize,
}

impl PaneHistory {
    pub const fn new() -> Self {
        PaneHistory { entries: Vec::new(), pos: 0 }
    }

    /// The pane the cursor sits on, if any.
    pub fn current(&self) -> Option<PaneId> {
        self.entries.get(self.pos).copied()
    }

    /// Record a visit to `id`.
    ///
    /// A no-op when `id` is already the current entry — which is what makes
    /// back/forward navigation transparent: they move the cursor onto an
    /// existing entry, and the next sample records nothing.
    ///
    /// A deliberate jump drops the forward trail, like following a link in a
    /// browser after going back. But when the pane under the cursor is no
    /// longer focusable — closed or minimized — the focus change is fallout
    /// rather than a jump: `id` takes that slot and the forward trail lives on.
    ///
    /// A pane only ever holds one slot: revisiting it moves it to the new spot
    /// instead of adding a copy. Without that, going back and forth between two
    /// panes would fill the trail with `A B A B` and walking back would bounce
    /// between the same two instead of reaching what came before them.
    pub fn record(&mut self, id: PaneId, state: &dyn Fn(PaneId) -> PaneState) {
        if self.current() == Some(id) {
            return;
        }
        if let Some(cur) = self.current() {
            if state(cur) != PaneState::Focusable {
                self.entries[self.pos] = id;
                self.drop_repeats_of_current();
                return;
            }
        }
        self.entries.truncate(self.pos + 1);
        self.entries.push(id);
        self.pos = self.entries.len() - 1;
        self.drop_repeats_of_current();
        if self.entries.len() > MAX_ENTRIES {
            let excess = self.entries.len() - MAX_ENTRIES;
            self.entries.drain(..excess);
            self.pos -= excess;
        }
    }

    /// Remove every other slot holding the pane the cursor sits on, so that
    /// pane appears exactly once in the trail.
    fn drop_repeats_of_current(&mut self) {
        let id = self.entries[self.pos];
        let mut i = 0;
        while i < self.entries.len() {
            if i != self.pos && self.entries[i] == id {
                self.entries.remove(i);
                if i < self.pos {
                    self.pos -= 1;
                }
            } else {
                i += 1;
            }
        }
    }

    /// Move the cursor one step toward the older end and return the pane to
    /// focus. `None` when the trail is exhausted.
    pub fn back(&mut self, state: &dyn Fn(PaneId) -> PaneState) -> Option<PaneId> {
        self.step(false, state)
    }

    /// Move the cursor one step toward the newer end and return the pane to
    /// focus. `None` when there is nothing to redo.
    pub fn forward(&mut self, state: &dyn Fn(PaneId) -> PaneState) -> Option<PaneId> {
        self.step(true, state)
    }

    /// Walk the trail in one direction until a focusable pane turns up.
    ///
    /// Closed panes and duplicates of the pane already focused are dropped on
    /// the way — the trail self-heals. Minimized panes are stepped over but
    /// kept, since restoring one puts it back in reach.
    fn step(&mut self, forward: bool, state: &dyn Fn(PaneId) -> PaneState) -> Option<PaneId> {
        let current = self.current();
        // Cursor for the walk. `self.pos` only moves once we commit to a
        // landing spot, so a walk that finds nothing leaves the trail put.
        let mut i = self.pos;
        loop {
            let next = if forward {
                if i + 1 >= self.entries.len() {
                    return None;
                }
                i + 1
            } else {
                if i == 0 {
                    return None;
                }
                i - 1
            };

            let id = self.entries[next];
            let drop_entry = match state(id) {
                // A redundant repeat of where we already are, e.g. left behind
                // by a pane that died between the two.
                PaneState::Focusable if Some(id) == current => true,
                PaneState::Focusable => {
                    self.pos = next;
                    return Some(id);
                }
                PaneState::Hidden => false,
                PaneState::Gone => true,
            };

            if drop_entry {
                self.entries.remove(next);
                // Everything past the hole shifts down by one.
                if next < self.pos {
                    self.pos -= 1;
                }
                if next < i {
                    i -= 1;
                }
            } else {
                i = next;
            }
        }
    }
}

static HISTORY: Mutex<PaneHistory> = Mutex::new(PaneHistory::new());

/// Record a visit in the global trail.
pub fn record(id: PaneId, state: &dyn Fn(PaneId) -> PaneState) {
    if let Ok(mut h) = HISTORY.lock() {
        h.record(id, state);
    }
}

/// Step back in the global trail; returns the pane to focus.
pub fn back(state: &dyn Fn(PaneId) -> PaneState) -> Option<PaneId> {
    HISTORY.lock().ok().and_then(|mut h| h.back(state))
}

/// Step forward in the global trail; returns the pane to focus.
pub fn forward(state: &dyn Fn(PaneId) -> PaneState) -> Option<PaneId> {
    HISTORY.lock().ok().and_then(|mut h| h.forward(state))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn all_focusable(_: PaneId) -> PaneState {
        PaneState::Focusable
    }

    /// Every pane is focusable except the listed ones, which are `state`.
    fn except(dead: &[PaneId], state: PaneState) -> impl Fn(PaneId) -> PaneState + '_ {
        move |id| if dead.contains(&id) { state } else { PaneState::Focusable }
    }

    fn trail(ids: &[PaneId]) -> PaneHistory {
        let mut h = PaneHistory::new();
        for id in ids {
            h.record(*id, &all_focusable);
        }
        h
    }

    #[test]
    fn back_and_forward_walk_the_visit_order() {
        let mut h = trail(&[1, 2, 3]);

        assert_eq!(h.back(&all_focusable), Some(2));
        assert_eq!(h.back(&all_focusable), Some(1));
        assert_eq!(h.back(&all_focusable), None, "oldest entry is a wall");
        assert_eq!(h.forward(&all_focusable), Some(2));
        assert_eq!(h.forward(&all_focusable), Some(3));
        assert_eq!(h.forward(&all_focusable), None, "newest entry is a wall");
    }

    #[test]
    fn navigating_does_not_pollute_the_trail() {
        let mut h = trail(&[1, 2, 3]);

        // Going back focuses pane 2, which the sampler records right after.
        assert_eq!(h.back(&all_focusable), Some(2));
        h.record(2, &all_focusable);
        // The forward trail must have survived that record.
        assert_eq!(h.forward(&all_focusable), Some(3));
    }

    #[test]
    fn a_fresh_visit_drops_the_forward_trail() {
        let mut h = trail(&[1, 2, 3]);
        h.back(&all_focusable);
        h.record(9, &all_focusable);

        assert_eq!(h.forward(&all_focusable), None);
        assert_eq!(h.back(&all_focusable), Some(2));
        assert_eq!(h.back(&all_focusable), Some(1));
    }

    #[test]
    fn closing_the_current_pane_keeps_the_forward_trail() {
        let mut h = trail(&[1, 2, 3]);
        assert_eq!(h.back(&all_focusable), Some(2));

        // Pane 2 is closed; focus falls back on its neighbour 7. That is not
        // a jump, so 7 takes 2's slot instead of cutting the branch ahead.
        let state = except(&[2], PaneState::Gone);
        h.record(7, &state);
        assert_eq!(h.current(), Some(7));
        assert_eq!(h.forward(&state), Some(3));
        assert_eq!(h.back(&state), Some(7));
        assert_eq!(h.back(&state), Some(1));
    }

    #[test]
    fn minimizing_the_current_pane_keeps_the_forward_trail() {
        let mut h = trail(&[1, 2, 3]);
        assert_eq!(h.back(&all_focusable), Some(2));

        let state = except(&[2], PaneState::Hidden);
        h.record(7, &state);
        assert_eq!(h.forward(&state), Some(3));
    }

    #[test]
    fn repeated_visits_to_the_same_pane_are_ignored() {
        let mut h = trail(&[1, 1, 2, 2]);

        assert_eq!(h.back(&all_focusable), Some(1));
        assert_eq!(h.back(&all_focusable), None);
    }

    #[test]
    fn closed_panes_are_skipped_and_dropped() {
        let mut h = trail(&[1, 2, 3, 4]);

        let state = except(&[2, 3], PaneState::Gone);
        assert_eq!(h.back(&state), Some(1));
        // 2 and 3 are gone for good — coming forward lands straight on 4.
        assert_eq!(h.forward(&all_focusable), Some(4));
    }

    #[test]
    fn minimized_panes_are_stepped_over_but_kept() {
        let mut h = trail(&[1, 2, 3]);

        let hidden = except(&[2], PaneState::Hidden);
        assert_eq!(h.back(&hidden), Some(1), "2 is minimized, walk past it");
        assert_eq!(h.forward(&hidden), Some(3), "and past it on the way back");

        // Restoring 2 puts it back in reach — it was never dropped.
        assert_eq!(h.back(&all_focusable), Some(2));
    }

    #[test]
    fn a_walk_that_finds_nothing_leaves_the_cursor_alone() {
        let mut h = trail(&[1, 2]);

        let hidden = except(&[1], PaneState::Hidden);
        assert_eq!(h.back(&hidden), None, "only older entry is minimized");
        assert_eq!(h.current(), Some(2), "cursor stayed on the focused pane");
        assert_eq!(h.back(&all_focusable), Some(1), "and 1 is still reachable");
    }

    #[test]
    fn back_skips_a_duplicate_of_the_current_pane() {
        let mut h = trail(&[1, 2, 1]);

        // Entry 1 (pane 2) is dead, so the trail reads [1, 1] — going back
        // must not "move" onto the pane we are already on.
        let state = except(&[2], PaneState::Gone);
        assert_eq!(h.back(&state), None);
    }

    #[test]
    fn bouncing_between_two_panes_leaves_one_slot_each() {
        // A B A B: the user hopped back and forth between two panes after
        // visiting 1 and 2. Walking back must reach them, not bounce.
        let mut h = trail(&[1, 2, 3, 4, 3, 4]);
        assert_eq!(h.entries, vec![1, 2, 3, 4]);

        assert_eq!(h.back(&all_focusable), Some(3));
        assert_eq!(h.back(&all_focusable), Some(2));
        assert_eq!(h.back(&all_focusable), Some(1));
        assert_eq!(h.back(&all_focusable), None);
    }

    #[test]
    fn a_revisit_moves_the_pane_instead_of_copying_it() {
        let mut h = trail(&[1, 2, 3]);
        h.record(1, &all_focusable);

        assert_eq!(h.entries, vec![2, 3, 1], "1 left its old slot");
        assert_eq!(h.current(), Some(1));
        assert_eq!(h.back(&all_focusable), Some(3));
        assert_eq!(h.back(&all_focusable), Some(2));
        assert_eq!(h.back(&all_focusable), None);
    }

    #[test]
    fn a_fallout_focus_does_not_duplicate_a_pane_ahead() {
        let mut h = trail(&[1, 2, 3]);
        assert_eq!(h.back(&all_focusable), Some(2));

        // 2 is closed and focus falls on 3, which is already the forward trail.
        let state = except(&[2], PaneState::Gone);
        h.record(3, &state);
        assert_eq!(h.entries, vec![1, 3]);
        assert_eq!(h.current(), Some(3));
        assert_eq!(h.forward(&state), None);
        assert_eq!(h.back(&state), Some(1));
    }

    #[test]
    fn the_trail_is_capped() {
        let mut h = PaneHistory::new();
        for id in 0..(MAX_ENTRIES as PaneId + 50) {
            h.record(id, &all_focusable);
        }
        assert_eq!(h.entries.len(), MAX_ENTRIES);
        assert_eq!(h.current(), Some(MAX_ENTRIES as PaneId + 49));
        assert_eq!(h.entries[0], 50);
    }
}
