//! One conversation, two panes: how a duplicate is spotted, and the question
//! the window asks about it.
//!
//! A session ends up in two panes easily — a restore that pre-types
//! `claude --resume` in the pane it came from while another pane already holds
//! that conversation, or a `--resume` typed by hand. Both panes then write to
//! the same transcript, which is not something either of them can share.

use super::*;

/// The duplicated conversation to ask about, newest pane first:
/// `(duplicate, original)`. Pane ids are handed out in order, so the highest id
/// of a group is the pane that joined last — the one the question is about.
///
/// `accepted` holds the duplicates already kept on purpose; they are skipped,
/// which is what stops the poll asking again every half second. Panes with no
/// session are ignored: a bare shell duplicates nothing.
pub(super) fn duplicate_session(
    panes: &[(PaneId, Option<String>)],
    accepted: &std::collections::HashSet<PaneId>,
) -> Option<(PaneId, PaneId)> {
    let mut by_session: std::collections::HashMap<&str, Vec<PaneId>> = std::collections::HashMap::new();
    for (id, session) in panes {
        if let Some(s) = session {
            by_session.entry(s.as_str()).or_default().push(*id);
        }
    }
    by_session
        .values()
        .filter_map(|ids| {
            let newest = *ids.iter().max()?;
            let oldest = *ids.iter().min()?;
            (newest != oldest && !accepted.contains(&newest)).then_some((newest, oldest))
        })
        // Several duplicated conversations at once: ask about the oldest
        // offender first, so the questions come in the order the panes appeared.
        .min_by_key(|(newest, _)| *newest)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn panes(spec: &[(PaneId, Option<&str>)]) -> Vec<(PaneId, Option<String>)> {
        spec.iter().map(|(id, s)| (*id, s.map(str::to_string))).collect()
    }

    #[test]
    fn a_conversation_in_two_panes_is_reported_newest_first() {
        let p = panes(&[(3, Some("abc")), (7, Some("abc")), (5, None)]);
        assert_eq!(duplicate_session(&p, &Default::default()), Some((7, 3)));
    }

    #[test]
    fn one_pane_per_conversation_is_not_a_duplicate() {
        let p = panes(&[(1, Some("a")), (2, Some("b")), (3, None), (4, None)]);
        assert_eq!(duplicate_session(&p, &Default::default()), None);
    }

    #[test]
    fn a_duplicate_kept_on_purpose_is_never_asked_about_again() {
        let p = panes(&[(1, Some("a")), (2, Some("a"))]);
        let accepted: std::collections::HashSet<PaneId> = [2].into_iter().collect();
        assert_eq!(duplicate_session(&p, &accepted), None);
    }

    #[test]
    fn the_first_question_is_about_the_oldest_offender() {
        let p = panes(&[(1, Some("a")), (9, Some("a")), (2, Some("b")), (4, Some("b"))]);
        assert_eq!(duplicate_session(&p, &Default::default()), Some((4, 2)));
    }
}

impl KovaView {
    /// Ask about the duplicate the session poll raised, if there is one.
    ///
    /// Called at the top of a tick, before anything borrows the tabs: the alert
    /// runs a modal loop that dispatches events, and those events read the tabs.
    pub(super) fn ask_about_duplicate_session(&self) {
        let Some((duplicate, original)) = self.ivars().duplicate_session.take() else { return };
        // The panes may be gone by now: the question was raised a frame ago and
        // a modal about a pane that closed meanwhile would be a lie.
        let label = {
            let tabs = self.ivars().tabs.borrow();
            let alive = |id: PaneId| tabs.iter().any(|t| t.contains(id));
            if !alive(duplicate) || !alive(original) {
                return;
            }
            tabs.iter()
                .find_map(|t| t.pane(original))
                .map(|p| p.display_title("this conversation"))
                .unwrap_or_else(|| "this conversation".to_string())
        };
        let mtm = unsafe { MainThreadMarker::new_unchecked() };
        let informative = format!(
            "\u{ab}{}\u{bb} is already open in another pane. Two panes on one conversation write to the same transcript.",
            label
        );
        let focus_original = confirm_choice(
            mtm,
            "This conversation is already open",
            &informative,
            "Focus the original",
            "Keep both",
        );
        if !focus_original {
            self.ivars().accepted_duplicates.borrow_mut().insert(duplicate);
            return;
        }
        self.close_pane_by_id(duplicate);
        self.ipc_focus_pane(original);
        self.set_pane_flash(original, 30, None);
    }
}
