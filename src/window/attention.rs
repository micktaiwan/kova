//! Where `Cmd+J` sends the eye next: the two tiers it walks (unread output
//! first, an idle Claude session last), the ring it walks them in, and the
//! banner naming the tier it landed in. The visit history walked by
//! `Cmd+Shift+Option+arrows` shares the same jump.

use super::*;

/// How far a candidate pane sits from the pane in focus. Ordering matters:
/// the variants are ranked nearest-first, and `Ord` is what sorts them.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub(super) enum PaneLocality {
    /// Same tab as the focused pane — already on screen, no jump at all.
    CurrentTab,
    /// Another tab of the same window.
    CurrentWindow,
    /// Another window entirely.
    OtherWindow,
}

/// Next pane to land on among `candidates`, all of the same attention tier.
///
/// The nearest locality present wins outright: the tab under the eye is walked
/// dry before the key crosses a tab boundary, and that window before it hops to
/// another one. Nothing is stranded by this — a pane leaves the candidate set
/// the moment it has been read, so the near group drains and the far ones come
/// up next. Within the chosen locality the walk is by ascending pane id,
/// wrapping around to the lowest.
///
/// `candidates` arrives in any order and is sorted, deduped and stripped of
/// `current` in place first, so the pane being looked at is never the answer.
fn next_pane_in_cycle(
    candidates: &mut Vec<(PaneLocality, PaneId)>,
    current: Option<PaneId>,
) -> Option<PaneId> {
    candidates.sort_unstable();
    candidates.dedup();
    if let Some(c) = current {
        candidates.retain(|&(_, id)| id != c);
    }
    let nearest = candidates.first()?.0;
    let ids: Vec<PaneId> =
        candidates.iter().filter(|(loc, _)| *loc == nearest).map(|&(_, id)| id).collect();
    next_id_after(&ids, current)
}

/// Next pane in the non-draining loop over every open idle session.
///
/// Locality is deliberately ignored here, unlike in the draining tiers: nothing
/// leaves this set by being looked at, so favouring the nearest group would pin
/// the walk to it for good — two idle sessions in the focused tab would hand
/// each other back and forth, and the sessions sitting in other tabs or other
/// windows would never come up at all. One global ring by ascending pane id.
///
/// `candidates` arrives in any order and is sorted, deduped and stripped of
/// `current` in place first, so the pane being looked at is never the answer.
fn next_pane_in_loop(candidates: &mut Vec<PaneId>, current: Option<PaneId>) -> Option<PaneId> {
    candidates.sort_unstable();
    candidates.dedup();
    if let Some(c) = current {
        candidates.retain(|&id| id != c);
    }
    next_id_after(candidates, current)
}

/// Walk `ids` — ascending, `current` already removed — to the first id above
/// `current`, wrapping around to the lowest. With no focused pane the walk
/// starts at the lowest id. `None` when the set is empty.
fn next_id_after(ids: &[PaneId], current: Option<PaneId>) -> Option<PaneId> {
    let after = current.unwrap_or(0);
    ids.iter().find(|&&id| current.is_none() || id > after).or_else(|| ids.first()).copied()
}

/// Which of Cmd+J's tiers a jump landed in — what the banner across the focused
/// pane's status bar names, and what colours it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum AttentionTier {
    /// A bell, or a command that finished while the eye was elsewhere.
    Unread,
    /// A Claude session sitting open and idle, first time round.
    IdleClaude,
    /// Every open session, walked round and round once the draining tiers are
    /// empty — working ones included, since a session left chewing is an open
    /// loop too. This tier no longer drains: the key keeps handing them back
    /// until they are closed or picked up.
    ClaudeLoop,
}

impl AttentionTier {
    /// Label painted across the focused pane's status bar.
    fn label(self) -> &'static str {
        match self {
            Self::Unread => "Tier 1 — unread output",
            Self::IdleClaude => "Tier 2 — idle Claude session",
            Self::ClaudeLoop => "Tier 2 — open Claude session (loop)",
        }
    }

    /// Banner background. Orange is what waits on an answer, green the idle-session
    /// walk; unread output sits between the two and takes blue so the three never
    /// read as the same event.
    fn color(self) -> [f32; 3] {
        match self {
            Self::Unread => [0.15, 0.33, 0.68],
            Self::IdleClaude | Self::ClaudeLoop => [0.15, 0.50, 0.24],
        }
    }
}

/// Pick the pane to jump to, in two tiers: unread panes first (a bell, or a
/// command that finished while you were elsewhere), then Claude sessions left
/// open and idle, which nobody is waiting on but which still have to be closed
/// or picked back up. Locality never crosses that line — an idle session sitting
/// right next door still loses to unread output in another window — it only
/// orders the walk *within* a tier. Returns the tier alongside the pane, since
/// the banner has to name it.
fn next_attention_pane(
    unread: &mut Vec<(PaneLocality, PaneId)>,
    idle_claude: &mut Vec<(PaneLocality, PaneId)>,
    current: Option<PaneId>,
) -> Option<(AttentionTier, PaneId)> {
    next_pane_in_cycle(unread, current)
        .map(|id| (AttentionTier::Unread, id))
        .or_else(|| {
            next_pane_in_cycle(idle_claude, current).map(|id| (AttentionTier::IdleClaude, id))
        })
}

/// Status line for a Cmd+J press that has nowhere to send the eye. Reaching it
/// means not one open session is left to walk, the pane under the eye aside, so
/// the count of sessions still working can only come from that pane itself or
/// from a minimized one — the two places the ring never goes. It says whether
/// the quiet means "everything is dealt with" or "wait, something is still
/// chewing behind a collapsed pane".
fn nothing_to_show_status(thinking: usize) -> String {
    if thinking == 0 {
        "Nothing to show, no thinking".to_string()
    } else {
        format!("Nothing to show ({thinking} thinking)")
    }
}

/// Focus a pane wherever it lives: find the window holding it, bring that
/// window front and let it switch to the right tab. Also flashes the pane
/// border so the jump is visible when it lands far from where the eye was.
fn focus_pane_in_any_window(pane_id: PaneId) {
    let mtm = unsafe { MainThreadMarker::new_unchecked() };
    let app = NSApplication::sharedApplication(mtm);
    let ns_windows = app.windows();
    for i in 0..ns_windows.count() {
        let win = ns_windows.objectAtIndex(i);
        let view = match crate::app::kova_view(&win) {
            Some(v) => v,
            None => continue,
        };
        if view.ipc_focus_pane(pane_id) {
            win.makeKeyAndOrderFront(None);
            // The jump crosses tabs and windows, so a border pulse alone does
            // not say where it landed: name the directory in big over the pane.
            // ~54 frames ≈ 0.9s @ 60fps, held opaque until the last 30 fade.
            let label = view.pane_cwd(pane_id).map(|cwd| {
                let home = std::env::var("HOME").unwrap_or_default();
                let (name, parent) = flash_label_parts(&cwd, &home);
                PaneFlashLabel { name, parent }
            });
            view.set_pane_flash(pane_id, 54, label);
            return;
        }
    }
    log::debug!("focus_pane_in_any_window: pane {} not found", pane_id);
}

/// Show the tier banner in whichever window owns `pane_id` — the jump may have
/// crossed windows, and the banner belongs on the pane it landed on.
fn show_attention_banner(pane_id: PaneId, tier: AttentionTier) {
    let mtm = unsafe { MainThreadMarker::new_unchecked() };
    let app = NSApplication::sharedApplication(mtm);
    let ns_windows = app.windows();
    for i in 0..ns_windows.count() {
        let win = ns_windows.objectAtIndex(i);
        let Some(view) = crate::app::kova_view(&win) else { continue };
        let owns = view.ivars().tabs.borrow().iter().any(|t| t.contains(pane_id));
        if owns {
            view.set_attention_banner(tier);
            return;
        }
    }
}

/// Walk the pane visit history one step and focus what it lands on. `forward`
/// replays the trail toward the most recent pane; otherwise it goes back
/// toward the older ones. Does nothing at either end of the trail.
pub(super) fn do_history_step(forward: bool) {
    let target = if forward {
        crate::pane_history::forward(&pane_history_state)
    } else {
        crate::pane_history::back(&pane_history_state)
    };
    match target {
        Some(id) => focus_pane_in_any_window(id),
        None => log::debug!(
            "pane history: nothing {} of here",
            if forward { "ahead" } else { "behind" }
        ),
    }
}

/// Where a pane recorded in the visit history stands now: still a valid
/// landing spot, minimized (walked over, never focused), or closed.
pub(super) fn pane_history_state(pane_id: PaneId) -> crate::pane_history::PaneState {
    use crate::pane_history::PaneState;
    let mtm = unsafe { MainThreadMarker::new_unchecked() };
    let app = NSApplication::sharedApplication(mtm);
    let ns_windows = app.windows();
    for i in 0..ns_windows.count() {
        let win = ns_windows.objectAtIndex(i);
        let view = match crate::app::kova_view(&win) {
            Some(v) => v,
            None => continue,
        };
        let tabs = view.ivars().tabs.borrow();
        for tab in tabs.iter() {
            if let Some(pane) = tab.pane(pane_id) {
                return if pane.minimized { PaneState::Hidden } else { PaneState::Focusable };
            }
        }
    }
    PaneState::Gone
}

impl KovaView {
    /// Paint the attention banner across this window's focused pane status bar
    /// for ~2 seconds. Same lifetime as `set_transient_status`, different slot:
    /// this one sits on the pane the jump landed on, not in the global bar.
    fn set_attention_banner(&self, tier: AttentionTier) {
        let fps = self.ivars().config.get().map(|c| c.terminal.fps).unwrap_or(60) as u32;
        *self.ivars().attention_banner.borrow_mut() =
            Some((tier.label().to_string(), tier.color(), fps * 2));
        self.mark_dirty();
    }

    /// Jump to the next pane asking for attention, across every tab and every
    /// window, in two tiers: first a pane left unread — a bell, or a command that
    /// finished while the eye was elsewhere (the same signal the switcher's Tab
    /// key and the status-bar counter use); then a Claude session left open and
    /// idle (`Pane::is_idle_claude_unseen`), which asks for nothing but is still
    /// an open loop: the tour ends on it so it gets closed or resumed rather than
    /// piling up unnoticed. A session that is actually working is never announced
    /// by one of those tiers — it has nothing to hand over yet.
    ///
    /// Both tiers hold *unread* panes only: a pane drops out of the set the
    /// moment it has been looked at (bell and completion are acked on the
    /// focused pane; an idle session is marked seen there too, and re-armed by
    /// `Tab::check_running` when it works again). Within a tier the scan is
    /// ordered by locality — the focused tab first, then the rest of its window,
    /// then other windows — and by ascending pane id inside that.
    ///
    /// Once the two drain, the key stops draining and walks the open sessions
    /// again, round and round, so an open session is either closed or picked
    /// back up rather than forgotten — no dead-end message in between, since a
    /// session left to walk is a better answer than a status line. This last
    /// ring takes the working sessions too: one still chewing is an open loop
    /// like any other, and passing back through it is how the eye returns to the
    /// answer it is about to print. It ignores locality and walks by ascending
    /// pane id — nothing drains out of it, so preferring the nearest group would
    /// pin the key to the focused tab and strand the sessions living in other
    /// tabs and windows. "Nothing to show" is left for the one case where the
    /// key really has nowhere to go: not one open session besides the pane under
    /// the eye. It carries how many sessions are still working, which by then
    /// can only be that pane itself or one behind a minimized split — the two
    /// spots the ring never visits.
    ///
    /// Each jump paints a banner across the focused pane's status bar naming the
    /// tier it came from, so the key never moves the eye without saying why.
    pub(super) fn do_focus_next_attention(&self) {
        let active_tab = self.ivars().active_tab.get();
        let current = {
            let tabs = self.ivars().tabs.borrow();
            tabs.get(active_tab).map(|t| t.focused_pane)
        };

        let mut unread: Vec<(PaneLocality, PaneId)> = Vec::new();
        let mut idle_claude: Vec<(PaneLocality, PaneId)> = Vec::new();
        // Every open session — idle or working, looked at or not: what the
        // post-message loop walks once the draining tiers are empty. No locality
        // here, that ring never drains and a nearest-first rule would trap it in
        // one tab.
        let mut session_ring: Vec<PaneId> = Vec::new();
        // Claude sessions actively working: what the dead-end message counts,
        // for the two spots the ring never reaches (the focused pane and the
        // minimized ones).
        let mut thinking = 0usize;
        let mtm = unsafe { MainThreadMarker::new_unchecked() };
        let app = NSApplication::sharedApplication(mtm);
        let ns_windows = app.windows();
        for i in 0..ns_windows.count() {
            let win = ns_windows.objectAtIndex(i);
            let view = match crate::app::kova_view(&win) {
                Some(v) => v,
                None => continue,
            };
            let is_current_window = std::ptr::eq(view as *const KovaView, self as *const KovaView);
            let tabs = view.ivars().tabs.borrow();
            for (tab_idx, tab) in tabs.iter().enumerate() {
                let locality = match (is_current_window, tab_idx == active_tab) {
                    (true, true) => PaneLocality::CurrentTab,
                    (true, false) => PaneLocality::CurrentWindow,
                    (false, _) => PaneLocality::OtherWindow,
                };
                tab.for_each_pane(&mut |pane| {
                    // Counted before every early return, minimized included: a
                    // session chewing away behind a collapsed pane is exactly
                    // what the user wants to hear about when nothing else waits.
                    if pane.is_working_claude() {
                        thinking += 1;
                    }
                    // A minimized pane is never a landing spot: jumping to it
                    // would have to give it its space back, and the user
                    // collapsed it on purpose. It keeps running and keeps its
                    // marker — Cmd+J simply walks past it. Restoring one is
                    // `restore-minimized`, or the IPC `focus-pane` command.
                    if pane.minimized {
                        return;
                    }
                    // Scoped: `is_idle_claude_unseen` reads the terminal too,
                    // and holding two read guards on the same lock deadlocks
                    // the moment a writer queues between them.
                    let has_unread = {
                        let term = pane.terminal.read();
                        term.bell.load(std::sync::atomic::Ordering::Relaxed)
                            || term.unread_completion()
                    };
                    if has_unread {
                        unread.push((locality, pane.id));
                        return;
                    }
                    if pane.has_claude_session() {
                        // Working sessions ride the ring too: one still chewing
                        // is as much an open loop as an idle one, and landing on
                        // it is how the eye gets back to the answer it will
                        // print. Only the draining tier stays idle-only, so a
                        // working session is never announced as something to
                        // deal with now.
                        session_ring.push(pane.id);
                        if pane.is_idle_claude_unseen() {
                            idle_claude.push((locality, pane.id));
                        }
                    }
                });
            }
        }

        let hit = next_attention_pane(&mut unread, &mut idle_claude, current);
        let (tier, target) = match hit {
            Some(hit) => hit,
            None => match next_pane_in_loop(&mut session_ring, current) {
                Some(id) => (AttentionTier::ClaudeLoop, id),
                // Not one open session left: nothing to hand over at all.
                None => {
                    self.set_transient_status(&nothing_to_show_status(thinking));
                    return;
                }
            },
        };
        focus_pane_in_any_window(target);
        show_attention_banner(target, tier);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nothing_to_show_names_the_sessions_still_working() {
        // Nothing left to jump to and nothing running: say both.
        assert_eq!(nothing_to_show_status(0), "Nothing to show, no thinking");
        // Sessions still chewing are the reason the screen looks quiet.
        assert_eq!(nothing_to_show_status(1), "Nothing to show (1 thinking)");
        assert_eq!(nothing_to_show_status(4), "Nothing to show (4 thinking)");
    }

    /// Candidate panes all sitting in the tab under the eye — the shape most of
    /// these cases care about, where only the id walk is under test.
    fn here(ids: &[PaneId]) -> Vec<(PaneLocality, PaneId)> {
        ids.iter().map(|&id| (PaneLocality::CurrentTab, id)).collect()
    }

    /// An empty tier. A fresh Vec per call, since each one is borrowed mutably.
    fn nothing() -> Vec<(PaneLocality, PaneId)> {
        Vec::new()
    }

    /// The landing pane alone: the walk-order cases below do not care which
    /// tier answered, only where the key lands.
    fn jump_to(
        unread: &mut Vec<(PaneLocality, PaneId)>,
        idle_claude: &mut Vec<(PaneLocality, PaneId)>,
        current: Option<PaneId>,
    ) -> Option<PaneId> {
        next_attention_pane(unread, idle_claude, current).map(|(_, id)| id)
    }

    #[test]
    fn next_attention_walks_unread_panes_in_id_order() {
        let unread = || here(&[7, 2, 5]);
        assert_eq!(jump_to(&mut unread(), &mut nothing(), Some(2)), Some(5));
        assert_eq!(jump_to(&mut unread(), &mut nothing(), Some(5)), Some(7));
        // Past the highest id, wrap back to the lowest.
        assert_eq!(jump_to(&mut unread(), &mut nothing(), Some(7)), Some(2));
        // From a pane that is not itself unread, take the next id above it.
        assert_eq!(jump_to(&mut unread(), &mut nothing(), Some(3)), Some(5));
        // Unfocused window: start at the lowest unread id.
        assert_eq!(jump_to(&mut unread(), &mut nothing(), None), Some(2));
    }

    #[test]
    fn next_attention_visits_idle_claude_sessions_last() {
        // An idle session loses to unread output, whatever the ids...
        assert_eq!(jump_to(&mut here(&[9]), &mut here(&[3]), Some(1)), Some(9));
        // ...and comes up only once the unread tier is dry, in id order.
        assert_eq!(jump_to(&mut nothing(), &mut here(&[3, 8]), Some(4)), Some(8));
        assert_eq!(jump_to(&mut nothing(), &mut here(&[3, 8]), Some(8)), Some(3));
        // The focused pane being the only unread one: fall through to the idle
        // tier rather than re-focusing where the cursor already is.
        assert_eq!(jump_to(&mut here(&[4]), &mut here(&[6]), Some(4)), Some(6));
        // The last idle session being the focused one: the tour is over.
        assert_eq!(jump_to(&mut nothing(), &mut here(&[4]), Some(4)), None);
    }

    #[test]
    fn the_session_loop_walks_every_session_wherever_it_lives() {
        // Two idle sessions in the focused tab used to hand each other back for
        // ever, since the loop drains nothing and the nearest locality won: the
        // sessions in the other tab and the other window were never reached.
        let ring = || vec![4, 6, 8, 2];
        assert_eq!(next_pane_in_loop(&mut ring(), Some(4)), Some(6));
        assert_eq!(next_pane_in_loop(&mut ring(), Some(6)), Some(8));
        // Past the highest id, wrap back to the lowest.
        assert_eq!(next_pane_in_loop(&mut ring(), Some(8)), Some(2));
        // From a pane that is not itself an idle session, take the next id above.
        assert_eq!(next_pane_in_loop(&mut ring(), Some(5)), Some(6));
        // Unfocused window: start at the lowest id.
        assert_eq!(next_pane_in_loop(&mut ring(), None), Some(2));
        // The only idle session is the one under the eye: nothing to hand over.
        assert_eq!(next_pane_in_loop(&mut vec![4], Some(4)), None);
        assert_eq!(next_pane_in_loop(&mut Vec::new(), Some(4)), None);
    }

    #[test]
    fn next_attention_names_the_tier_it_answered_from() {
        use AttentionTier::{IdleClaude, Unread};
        assert_eq!(
            next_attention_pane(&mut here(&[3]), &mut here(&[4]), Some(1)),
            Some((Unread, 3))
        );
        assert_eq!(
            next_attention_pane(&mut nothing(), &mut here(&[4]), Some(1)),
            Some((IdleClaude, 4))
        );
    }

    #[test]
    fn next_attention_walks_the_current_tab_before_leaving_it() {
        use PaneLocality::{CurrentTab, CurrentWindow, OtherWindow};
        // A lower id in another window loses to the tab under the eye...
        let mut spread = vec![(OtherWindow, 2), (CurrentWindow, 3), (CurrentTab, 9)];
        assert_eq!(jump_to(&mut spread, &mut Vec::new(), Some(1)), Some(9));
        // ...and the current tab wraps onto itself rather than crossing over,
        // since a pane leaves the set once read and the group drains.
        let mut two_here = vec![(CurrentWindow, 8), (CurrentTab, 4), (CurrentTab, 6)];
        assert_eq!(jump_to(&mut two_here, &mut Vec::new(), Some(6)), Some(4));
        // Nothing left in the current tab: the rest of the window comes next,
        // and only then another window.
        let mut away = vec![(OtherWindow, 2), (CurrentWindow, 8)];
        assert_eq!(jump_to(&mut away, &mut Vec::new(), Some(6)), Some(8));
        // Locality never outranks the tier: an idle session in the current tab
        // still waits behind unread output in another window.
        let mut far_unread = vec![(OtherWindow, 2)];
        assert_eq!(jump_to(&mut far_unread, &mut here(&[7]), Some(1)), Some(2));
    }

    #[test]
    fn next_attention_handles_empty_and_lone_candidates() {
        assert_eq!(jump_to(&mut vec![], &mut nothing(), Some(4)), None);
        // Nothing else unread or idle: no jump at all.
        assert_eq!(jump_to(&mut here(&[4]), &mut nothing(), Some(4)), None);
        // Duplicates (same pane seen twice) collapse instead of stalling.
        assert_eq!(jump_to(&mut here(&[4, 4, 9]), &mut nothing(), Some(4)), Some(9));
    }
}
