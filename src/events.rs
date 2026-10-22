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
//! The diff reads the panes through an `EventSource` and publishes through an
//! `EventSink`: Kova.app's windows (`WindowsSource`) and a library host's
//! panes (`crate::embed`) feed the same state machine, and the subscribers'
//! queues (`IpcSink`) are the one sink outside tests.
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
#[derive(Clone, Debug)]
pub struct PaneFlags {
    pub window: usize,
    pub tab: usize,
    pub working: bool,
    pub awaiting: bool,
    pub awaiting_since: Option<u64>,
}

/// Which pane holds the user's attention, and what is running in it.
///
/// The agent conversation is part of the identity on purpose. Launching an agent
/// in the pane you are already in changes what you are doing without moving the
/// focus anywhere, and a client that only hears about pane changes would learn of
/// it only when you happen to leave and come back.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct FocusKey {
    pub window: usize,
    pub tab: usize,
    pub pane: PaneId,
    pub session: Option<String>,
    /// The name `/rename` gave it. Part of the identity for the same reason as the id: renaming a
    /// conversation is how you declare it is about something else now.
    pub session_name: Option<String>,
}

/// What the diff reads from the app that holds the panes.
pub trait EventSource {
    /// The pane the keyboard would reach right now, the app being active;
    /// `None` when no pane holds it (no key window, nothing focused).
    fn focus(&self) -> Option<FocusKey>;
    /// The flags of every pane. Must not cost a syscall: it runs on a timer.
    fn collect_flags(&self, out: &mut HashMap<PaneId, PaneFlags>);
    /// The full JSON of one pane (`list-panes`' shape), built on an edge only.
    fn pane_json(&self, window: usize, pane: PaneId) -> Option<serde_json::Value>;
    /// Every pane's JSON, in `list-panes` order, for the snapshot.
    fn collect_panes(&self, out: &mut Vec<serde_json::Value>);
    /// Every subject, in `list-subjects`' shape and order. Read on the pane
    /// sweep, so it must not cost a syscall either. A host without subjects
    /// keeps the default: none.
    fn collect_subjects(&self, _out: &mut Vec<serde_json::Value>) {}
}

/// Where the events go.
pub trait EventSink {
    /// Whether any subscriber wants one of `topics`.
    fn wants(&self, topics: u32) -> bool;
    /// Push one event to the subscribers of `topic`. Never blocks.
    fn publish(&mut self, topic: u32, event: serde_json::Value);
}

/// The subscribers' bounded queues (`crate::ipc::publish`), drained by their
/// connection threads: the main thread never writes to a socket.
pub struct IpcSink;

impl EventSink for IpcSink {
    fn wants(&self, topics: u32) -> bool {
        crate::ipc::has_subscribers(topics)
    }

    fn publish(&mut self, topic: u32, event: serde_json::Value) {
        crate::ipc::publish(topic, event);
    }
}

/// Kova.app's windows, as the diff reads them.
pub struct WindowsSource<'a>(pub &'a RefCell<Vec<Retained<NSWindow>>>);

impl EventSource for WindowsSource<'_> {
    fn focus(&self) -> Option<FocusKey> {
        current_focus(self.0)
    }

    fn collect_flags(&self, out: &mut HashMap<PaneId, PaneFlags>) {
        let wins = self.0.borrow();
        for (idx, win) in wins.iter().enumerate() {
            if let Some(view) = crate::app::kova_view(win) {
                view.events_collect_flags(idx, out);
            }
        }
    }

    fn pane_json(&self, window: usize, pane: PaneId) -> Option<serde_json::Value> {
        pane_json(self.0, window, pane)
    }

    fn collect_panes(&self, out: &mut Vec<serde_json::Value>) {
        let wins = self.0.borrow();
        for (idx, win) in wins.iter().enumerate() {
            if let Some(view) = crate::app::kova_view(win) {
                view.ipc_collect_panes(idx, win.isKeyWindow(), out);
            }
        }
    }

    fn collect_subjects(&self, out: &mut Vec<serde_json::Value>) {
        let wins = self.0.borrow();
        for (idx, win) in wins.iter().enumerate() {
            if let Some(view) = crate::app::kova_view(win) {
                view.ipc_collect_subjects(idx, out);
            }
        }
    }
}

/// The last state published to subscribers.
pub struct EventState {
    /// `None` = nothing holds the user's attention: Kova is not the active app,
    /// or it has no key window.
    focus: Option<FocusKey>,
    panes: HashMap<PaneId, PaneFlags>,
    /// The subjects last published, `None` while nobody wants the topic: a
    /// list kept up to date for nobody would be diffed, once someone asks,
    /// against a world that moved on unwatched.
    subjects: Option<Vec<serde_json::Value>>,
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
            subjects: None,
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
        self.poll_with(&WindowsSource(windows), &mut IpcSink, app_active, fps, force);
    }

    /// `poll` against any source and sink.
    pub fn poll_with(
        &mut self,
        source: &impl EventSource,
        sink: &mut impl EventSink,
        app_active: bool,
        fps: u32,
        force: bool,
    ) {
        if !sink.wants(topic::ALL) {
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

        self.poll_focus(source, sink, app_active, silent);

        if force || self.countdown == 0 {
            self.countdown = pane_poll_interval(fps);
            self.poll_panes(source, sink, silent);
            self.poll_subjects(source, sink, silent);
        } else {
            self.countdown -= 1;
        }
    }

    fn poll_focus(
        &mut self,
        source: &impl EventSource,
        sink: &mut impl EventSink,
        app_active: bool,
        silent: bool,
    ) {
        // Kova not being frontmost is not "the same pane, still focused": the
        // user's attention has left the terminal entirely. Collapsing both facts
        // into one stream is the whole point — a client should not have to join
        // our focus with the system's active app to know whether you are here.
        let next = if app_active { source.focus() } else { None };
        if next == self.focus {
            return;
        }
        let previous = self.focus.take();
        self.focus = next.clone();
        if silent || !sink.wants(topic::FOCUS) {
            return;
        }
        let pane = next
            .as_ref()
            .and_then(|key| source.pane_json(key.window, key.pane));
        sink.publish(
            topic::FOCUS,
            serde_json::json!({
                "event": "focus",
                "app_active": app_active,
                "reason": focus_reason(previous.as_ref(), next.as_ref(), app_active),
                "pane": pane,
            }),
        );
    }

    fn poll_panes(&mut self, source: &impl EventSource, sink: &mut impl EventSink, silent: bool) {
        let mut current: HashMap<PaneId, PaneFlags> = HashMap::new();
        source.collect_flags(&mut current);

        if !silent {
            for (id, flags) in &current {
                match self.panes.get(id) {
                    None => {
                        if sink.wants(topic::PANE_OPEN) {
                            let pane = source.pane_json(flags.window, *id);
                            sink.publish(
                                topic::PANE_OPEN,
                                serde_json::json!({ "event": "pane-open", "pane": pane }),
                            );
                        }
                    }
                    Some(previous) => {
                        if previous.working != flags.working {
                            sink.publish(
                                topic::PANE_WORKING,
                                serde_json::json!({
                                    "event": "pane-working",
                                    "pane_id": id,
                                    "working": flags.working,
                                }),
                            );
                        }
                        if previous.awaiting != flags.awaiting {
                            sink.publish(
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
                    sink.publish(
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

    /// The whole list goes out when anything in it changed: a client replaces
    /// its copy instead of replaying edits, and a rename never reads as a
    /// deletion followed by a creation.
    fn poll_subjects(&mut self, source: &impl EventSource, sink: &mut impl EventSink, silent: bool) {
        if !sink.wants(topic::SUBJECTS) {
            self.subjects = None;
            return;
        }
        let mut current = Vec::new();
        source.collect_subjects(&mut current);
        let Some(previous) = self.subjects.replace(current) else { return };
        let current = self.subjects.as_ref().expect("just set");
        if silent || previous == *current {
            return;
        }
        sink.publish(topic::SUBJECTS, serde_json::json!({ "event": "subjects", "subjects": current }));
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
    snapshot_with(&WindowsSource(windows), app_active, topics)
}

/// `snapshot` against any source.
pub fn snapshot_with(source: &impl EventSource, app_active: bool, topics: u32) -> serde_json::Value {
    let mut panes = Vec::new();
    source.collect_panes(&mut panes);
    let mut subjects = Vec::new();
    source.collect_subjects(&mut subjects);
    let mut focus = serde_json::Value::Null;
    if app_active {
        if let Some(key) = source.focus() {
            if let Some(pane) = source.pane_json(key.window, key.pane) {
                focus = pane;
            }
        }
    }
    serde_json::json!({
        "events": topic::names(topics),
        "app_active": app_active,
        "focus": focus,
        "panes": panes,
        "subjects": subjects,
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

    /// A world the test sets by hand.
    #[derive(Default)]
    struct FakeSource {
        focus: Option<FocusKey>,
        panes: Vec<(PaneId, PaneFlags)>,
        subjects: Vec<serde_json::Value>,
    }

    impl EventSource for FakeSource {
        fn focus(&self) -> Option<FocusKey> {
            self.focus.clone()
        }
        fn collect_flags(&self, out: &mut HashMap<PaneId, PaneFlags>) {
            out.extend(self.panes.iter().cloned());
        }
        fn pane_json(&self, window: usize, pane: PaneId) -> Option<serde_json::Value> {
            Some(serde_json::json!({ "id": pane, "window": window }))
        }
        fn collect_panes(&self, out: &mut Vec<serde_json::Value>) {
            out.extend(self.panes.iter().map(|(id, f)| serde_json::json!({ "id": id, "window": f.window })));
        }
        fn collect_subjects(&self, out: &mut Vec<serde_json::Value>) {
            out.extend(self.subjects.iter().cloned());
        }
    }

    /// Records what would go to the subscribers of `mask`.
    struct Recorder {
        mask: u32,
        events: Vec<serde_json::Value>,
    }

    impl EventSink for Recorder {
        fn wants(&self, topics: u32) -> bool {
            self.mask & topics != 0
        }
        fn publish(&mut self, topic: u32, event: serde_json::Value) {
            if self.wants(topic) {
                self.events.push(event);
            }
        }
    }

    fn flags(working: bool, awaiting: bool) -> PaneFlags {
        PaneFlags { window: 0, tab: 0, working, awaiting, awaiting_since: awaiting.then_some(7) }
    }

    /// The diff, run against a hand-set world: the first pass is silent, then
    /// every edge goes out once, the sweep throttled to `pane_poll_interval`
    /// ticks unless forced.
    #[test]
    fn the_diff_publishes_each_edge_once() {
        let mut state = EventState::new();
        let mut sink = Recorder { mask: topic::ALL, events: Vec::new() };
        let mut world = FakeSource { focus: Some(key(0, 0, 1)), panes: vec![(1, flags(false, false))], ..Default::default() };
        state.poll_with(&world, &mut sink, true, 4, false);
        assert!(sink.events.is_empty(), "the seeding pass announces nothing");

        world.panes.push((2, flags(false, false)));
        world.panes[0].1 = flags(true, true);
        state.poll_with(&world, &mut sink, true, 4, false);
        assert!(sink.events.is_empty(), "throttled: the sweep waits");
        state.poll_with(&world, &mut sink, true, 4, true);
        let mut names: Vec<String> = sink.events.iter().map(|e| e["event"].as_str().unwrap().to_string()).collect();
        names.sort();
        assert_eq!(names, ["pane-open", "pane-status", "pane-working"]);
        let status = sink.events.iter().find(|e| e["event"] == "pane-status").unwrap();
        assert_eq!(status["awaiting_since"], 7);
        sink.events.clear();

        state.poll_with(&world, &mut sink, true, 4, true);
        assert!(sink.events.is_empty(), "no change, nothing");

        world.focus = Some(key(0, 1, 2));
        world.panes.remove(0);
        state.poll_with(&world, &mut sink, true, 4, true);
        assert_eq!(sink.events[0]["event"], "focus", "focus first");
        assert_eq!(sink.events[0]["reason"], "tab");
        assert_eq!(sink.events[0]["pane"]["id"], 2);
        assert_eq!(sink.events[1], serde_json::json!({"event": "pane-close", "pane_id": 1, "window": 0, "tab": 0}));
        sink.events.clear();

        state.poll_with(&world, &mut sink, false, 4, false);
        assert_eq!(sink.events[0]["reason"], "app-inactive");
        assert_eq!(sink.events[0]["pane"], serde_json::Value::Null);
    }

    #[test]
    fn a_subject_change_publishes_the_whole_list_once() {
        let subject = |name: &str, panes: &[PaneId]| serde_json::json!({ "tab_id": 1, "name": name, "pane_ids": panes });
        let mut state = EventState::new();
        let mut sink = Recorder { mask: topic::SUBJECTS, events: Vec::new() };
        let mut world = FakeSource { subjects: vec![subject("auth", &[1])], ..Default::default() };
        state.poll_with(&world, &mut sink, true, 4, true);
        assert!(sink.events.is_empty(), "the seeding pass announces nothing");

        world.subjects = vec![subject("login", &[1]), subject("idle", &[])];
        state.poll_with(&world, &mut sink, true, 4, true);
        assert_eq!(
            sink.events,
            vec![serde_json::json!({ "event": "subjects", "subjects": world.subjects })]
        );
        state.poll_with(&world, &mut sink, true, 4, true);
        assert_eq!(sink.events.len(), 1, "no change, nothing");

        // Nobody wants the topic: its state goes, and a later subscriber is
        // seeded again rather than told of what changed while unwatched.
        let mut other = Recorder { mask: topic::FOCUS, events: Vec::new() };
        state.poll_with(&world, &mut other, true, 4, true);
        world.subjects.clear();
        state.poll_with(&world, &mut sink, true, 4, true);
        assert_eq!(sink.events.len(), 1);
        world.subjects = vec![subject("auth", &[2])];
        state.poll_with(&world, &mut sink, true, 4, true);
        assert_eq!(sink.events.len(), 2);
        assert_eq!(world.subjects, sink.events[1]["subjects"].as_array().unwrap().clone());
    }

    #[test]
    fn nobody_listening_drops_the_state() {
        let mut state = EventState::new();
        let world = FakeSource { focus: Some(key(0, 0, 1)), panes: vec![(1, flags(false, false))], ..Default::default() };
        let mut deaf = Recorder { mask: 0, events: Vec::new() };
        state.poll_with(&world, &mut deaf, true, 60, true);
        assert!(!state.seeded);
        let mut sink = Recorder { mask: topic::PANE_OPEN, events: Vec::new() };
        state.poll_with(&world, &mut sink, true, 60, true);
        assert!(sink.events.is_empty(), "a new subscriber is seeded, not told of every pane");
    }

    #[test]
    fn the_snapshot_reads_the_source() {
        let world = FakeSource { focus: Some(key(0, 0, 2)), panes: vec![(2, flags(false, false))], ..Default::default() };
        let snap = snapshot_with(&world, true, topic::FOCUS | topic::PANE_CLOSE);
        assert_eq!(snap["events"], serde_json::json!(["focus", "pane-close"]));
        assert_eq!(snap["focus"]["id"], 2);
        assert_eq!(snap["panes"].as_array().unwrap().len(), 1);
        assert_eq!(snap["subjects"], serde_json::json!([]));
        let away = snapshot_with(&world, false, topic::ALL);
        assert_eq!(away["app_active"], false);
        assert_eq!(away["focus"], serde_json::Value::Null);
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
    fn renaming_the_focused_conversation_is_a_session_event() {
        let mut before = key_with_session(0, 0, 1, "codex-id");
        before.session_name = Some("before".into());
        let mut after = before.clone();
        after.session_name = Some("after".into());
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
