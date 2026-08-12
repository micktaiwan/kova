//! What Kova pushes to its IPC subscribers, and when.
//!
//! There is no single callback to hang these off. Focus moves from a dozen
//! places (keyboard navigation, a click, the pane switcher, `focus-pane`,
//! closing a pane, switching tab or window), and `working` / `awaiting` are
//! *derived* from the pane's title and a pushed flag rather than set in one
//! spot. Instrumenting every mutation site would mean missing one. So this
//! module keeps the last published state and diffs it on the render tick:
//! one place to get right, and nothing can change behind its back.
//!
//! Cost discipline, because this runs inside the render loop:
//! - nobody subscribed → one relaxed atomic load and we are done;
//! - focus is compared on every tick, but that is two `RefCell` reads;
//! - the full pane sweep is throttled to ~4 Hz (`pane_poll_interval`);
//! - the expensive JSON (a `proc_pidinfo` for the CWD, a process-table walk)
//!   is built only when an edge actually fired.

use objc2::rc::Retained;
use objc2_app_kit::NSWindow;
use std::cell::RefCell;
use std::collections::HashMap;

use crate::ipc::topic;
use crate::pane::PaneId;

/// The per-pane state the diff watches. Everything here is cheap to read.
pub struct PaneFlags {
    pub window: usize,
    pub tab: usize,
    pub working: bool,
    pub awaiting: bool,
    pub awaiting_since: Option<u64>,
}

/// Which pane holds the user's attention, and what is running in it.
///
/// The Claude conversation is part of the identity on purpose. Launching `claude`
/// in the pane you are already in changes what you are doing without moving the
/// focus anywhere, and a client that only hears about pane changes would learn of
/// it only when you happen to leave and come back.
#[derive(Clone, PartialEq, Eq, Debug)]
struct FocusKey {
    window: usize,
    tab: usize,
    pane: PaneId,
    session: Option<String>,
    /// The name `/rename` gave it. Part of the identity for the same reason as the id: renaming a
    /// conversation is how you declare it is about something else now.
    session_name: Option<String>,
}

/// The last state published to subscribers.
pub struct EventState {
    /// `None` = nothing holds the user's attention: Kova is not the active app,
    /// or it has no key window.
    focus: Option<FocusKey>,
    panes: HashMap<PaneId, PaneFlags>,
    /// Ticks left before the next full pane sweep.
    countdown: u32,
    /// False until the first pass has filled the state. That first pass must not
    /// publish: it would announce every existing pane as freshly opened.
    seeded: bool,
}

impl Default for EventState {
    fn default() -> Self {
        Self::new()
    }
}

impl EventState {
    pub fn new() -> Self {
        Self {
            focus: None,
            panes: HashMap::new(),
            countdown: 0,
            seeded: false,
        }
    }

    /// Diff the world against the last published state and push what changed.
    ///
    /// `force` skips the pane-sweep throttle — used when a client subscribes, so
    /// anything already pending goes out before the snapshot it is about to read.
    pub fn poll(
        &mut self,
        windows: &RefCell<Vec<Retained<NSWindow>>>,
        app_active: bool,
        fps: u32,
        force: bool,
    ) {
        if !crate::ipc::has_subscribers(topic::ALL) {
            // Nobody is listening. Drop the state so the next subscriber is
            // seeded from the truth of its own moment rather than diffed against
            // a world that may have moved on for hours without us watching.
            if self.seeded {
                *self = Self::new();
            }
            return;
        }

        let silent = !self.seeded;
        self.seeded = true;

        self.poll_focus(windows, app_active, silent);

        if force || self.countdown == 0 {
            self.countdown = pane_poll_interval(fps);
            self.poll_panes(windows, silent);
        } else {
            self.countdown -= 1;
        }
    }

    fn poll_focus(
        &mut self,
        windows: &RefCell<Vec<Retained<NSWindow>>>,
        app_active: bool,
        silent: bool,
    ) {
        // Kova not being frontmost is not "the same pane, still focused": the
        // user's attention has left the terminal entirely. Collapsing both facts
        // into one stream is the whole point — a client should not have to join
        // our focus with the system's active app to know whether you are here.
        let next = if app_active { current_focus(windows) } else { None };
        if next == self.focus {
            return;
        }
        let previous = self.focus.take();
        self.focus = next.clone();
        if silent || !crate::ipc::has_subscribers(topic::FOCUS) {
            return;
        }
        let pane = next
            .as_ref()
            .and_then(|key| pane_json(windows, key.window, key.pane));
        crate::ipc::publish(
            topic::FOCUS,
            serde_json::json!({
                "event": "focus",
                "app_active": app_active,
                "reason": focus_reason(previous.as_ref(), next.as_ref(), app_active),
                "pane": pane,
            }),
        );
    }

    fn poll_panes(&mut self, windows: &RefCell<Vec<Retained<NSWindow>>>, silent: bool) {
        let mut current: HashMap<PaneId, PaneFlags> = HashMap::new();
        {
            let wins = windows.borrow();
            for (idx, win) in wins.iter().enumerate() {
                if let Some(view) = crate::app::kova_view(win) {
                    view.events_collect_flags(idx, &mut current);
                }
            }
        }

        if !silent {
            for (id, flags) in &current {
                match self.panes.get(id) {
                    None => {
                        if crate::ipc::has_subscribers(topic::PANE_OPEN) {
                            let pane = pane_json(windows, flags.window, *id);
                            crate::ipc::publish(
                                topic::PANE_OPEN,
                                serde_json::json!({ "event": "pane-open", "pane": pane }),
                            );
                        }
                    }
                    Some(previous) => {
                        if previous.working != flags.working {
                            crate::ipc::publish(
                                topic::PANE_WORKING,
                                serde_json::json!({
                                    "event": "pane-working",
                                    "pane_id": id,
                                    "working": flags.working,
                                }),
                            );
                        }
                        if previous.awaiting != flags.awaiting {
                            crate::ipc::publish(
                                topic::PANE_STATUS,
                                serde_json::json!({
                                    "event": "pane-status",
                                    "pane_id": id,
                                    "awaiting": flags.awaiting,
                                    "awaiting_since": flags.awaiting_since,
                                }),
                            );
                        }
                    }
                }
            }
            for (id, previous) in &self.panes {
                if !current.contains_key(id) {
                    // No payload beyond the id: the pane is gone, there is
                    // nothing left to introspect.
                    crate::ipc::publish(
                        topic::PANE_CLOSE,
                        serde_json::json!({
                            "event": "pane-close",
                            "pane_id": id,
                            "window": previous.window,
                            "tab": previous.tab,
                        }),
                    );
                }
            }
        }

        self.panes = current;
    }
}

/// How many ticks between two full pane sweeps — about 250 ms, whatever the
/// frame rate. Fast enough that a subscriber sees a session go idle as it
/// happens, slow enough that 30 panes never cost anything measurable.
fn pane_poll_interval(fps: u32) -> u32 {
    (fps / 4).max(1)
}

/// Why the focus event fired, derived from the two states rather than from the
/// call site — there is no call site, the tick found it.
fn focus_reason(
    previous: Option<&FocusKey>,
    next: Option<&FocusKey>,
    app_active: bool,
) -> &'static str {
    match (previous, next) {
        (_, None) if !app_active => "app-inactive",
        (_, None) => "no-key-window",
        (None, Some(_)) => "app-active",
        (Some(p), Some(n)) if p.window != n.window => "window",
        (Some(p), Some(n)) if p.tab != n.tab => "tab",
        (Some(p), Some(n)) if p.pane != n.pane => "pane",
        // Same pane, different conversation: `claude` was just launched (or ended)
        // right where you already were.
        _ => "session",
    }
}

/// The pane the keyboard would reach right now, or `None` if no Kova window is key.
fn current_focus(windows: &RefCell<Vec<Retained<NSWindow>>>) -> Option<FocusKey> {
    let wins = windows.borrow();
    for (idx, win) in wins.iter().enumerate() {
        if !win.isKeyWindow() {
            continue;
        }
        if let Some(view) = crate::app::kova_view(win) {
            if let Some((tab, pane, session, session_name)) = view.events_focus_key() {
                return Some(FocusKey { window: idx, tab, pane, session, session_name });
            }
        }
    }
    None
}

/// Full JSON for a pane addressed by (window index, pane id).
fn pane_json(
    windows: &RefCell<Vec<Retained<NSWindow>>>,
    win_idx: usize,
    pane_id: PaneId,
) -> Option<serde_json::Value> {
    let wins = windows.borrow();
    let win = wins.get(win_idx)?;
    let is_key = win.isKeyWindow();
    crate::app::kova_view(win)?.events_pane_json(win_idx, pane_id, is_key)
}

/// Snapshot handed to a client the moment it subscribes: everything it would
/// otherwise have to reconstruct from `list-panes` plus a guess about focus.
pub fn snapshot(
    windows: &RefCell<Vec<Retained<NSWindow>>>,
    app_active: bool,
    topics: u32,
) -> serde_json::Value {
    let mut panes = Vec::new();
    let mut focus = serde_json::Value::Null;
    {
        let wins = windows.borrow();
        for (idx, win) in wins.iter().enumerate() {
            if let Some(view) = crate::app::kova_view(win) {
                view.ipc_collect_panes(idx, win.isKeyWindow(), &mut panes);
            }
        }
    }
    if app_active {
        if let Some(key) = current_focus(windows) {
            if let Some(pane) = pane_json(windows, key.window, key.pane) {
                focus = pane;
            }
        }
    }
    serde_json::json!({
        "events": topic::names(topics),
        "app_active": app_active,
        "focus": focus,
        "panes": panes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(window: usize, tab: usize, pane: PaneId) -> FocusKey {
        FocusKey { window, tab, pane, session: None, session_name: None }
    }

    fn key_with_session(window: usize, tab: usize, pane: PaneId, session: &str) -> FocusKey {
        FocusKey { window, tab, pane, session: Some(session.to_string()), session_name: None }
    }

    #[test]
    fn pane_poll_interval_is_about_four_hertz() {
        assert_eq!(pane_poll_interval(60), 15);
        assert_eq!(pane_poll_interval(120), 30);
        // A pathological fps must still sweep, never divide down to "every 0 ticks".
        assert_eq!(pane_poll_interval(1), 1);
        assert_eq!(pane_poll_interval(0), 1);
    }

    #[test]
    fn losing_the_app_is_reported_as_such() {
        // The distinction matters to a client that credits time: "you left Kova"
        // is not the same fact as "Kova has no window right now".
        assert_eq!(focus_reason(Some(&key(0, 0, 1)), None, false), "app-inactive");
        assert_eq!(focus_reason(Some(&key(0, 0, 1)), None, true), "no-key-window");
    }

    #[test]
    fn coming_back_to_the_same_pane_reads_as_app_active() {
        assert_eq!(focus_reason(None, Some(&key(0, 0, 1)), true), "app-active");
    }

    #[test]
    fn launching_claude_where_you_already_are_is_an_event() {
        // The whole reason the conversation is part of the identity: nothing moved,
        // but the pane is now about something, and a client keyed on conversations
        // would otherwise hear about it only when you next left and came back.
        let before = key(0, 0, 1);
        let after = key_with_session(0, 0, 1, "abc");
        assert_ne!(before, after);
        assert_eq!(focus_reason(Some(&before), Some(&after), true), "session");
    }

    #[test]
    fn moves_are_named_by_their_widest_hop() {
        // A pane change that is also a tab change is a tab move, and a tab change
        // that is also a window change is a window move — the coarsest true label.
        assert_eq!(focus_reason(Some(&key(0, 0, 1)), Some(&key(1, 0, 9)), true), "window");
        assert_eq!(focus_reason(Some(&key(0, 0, 1)), Some(&key(0, 2, 9)), true), "tab");
        assert_eq!(focus_reason(Some(&key(0, 0, 1)), Some(&key(0, 0, 9)), true), "pane");
    }
}
