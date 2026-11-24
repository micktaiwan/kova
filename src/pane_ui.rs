//! Native UI views agents show in panes (`docs/pane-ui-spec.md`).
//!
//! A view is a list of cards (`crate::decision_card` documents) docked in the
//! agent's own pane or held by a pane of its own. This module is the state and
//! its rules, without AppKit: the views, each card's version and click lock,
//! the ordered event queue of each view, `ui-wait` hand-offs and their
//! acknowledgement, the hold rules before an event is typed into the
//! conversation, and the line typed. The window draws the views
//! (`crate::window::dock_view`); the main thread owns this state.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::decision_card::{Card, ViewDoc};

/// How long a click locks its card's buttons, unless the agent answers first.
pub const LOCK_SECS: u64 = 600;
/// An event arriving this soon after a `ui-wait` returned waits for the next one.
pub const WAIT_GAP_SECS: f64 = 5.0;
/// Undelivered events older than this are dropped at launch.
pub const EVENT_TTL_SECS: u64 = 7 * 24 * 3600;
/// The user's text in a typed line is cut here.
pub const MAX_TYPED_TEXT: usize = 500;
/// UI panes one pane may open.
pub const MAX_UI_PANES: usize = 4;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Placement {
    DockBottom,
    DockRight,
    PaneRight,
    PaneDown,
}

impl Placement {
    pub fn parse(s: &str) -> Option<Placement> {
        serde_json::from_value(serde_json::Value::String(s.to_string())).ok()
    }

    pub fn docked(self) -> bool {
        matches!(self, Placement::DockBottom | Placement::DockRight)
    }

    pub fn default_size(self) -> f32 {
        if self.docked() { 0.4 } else { 0.5 }
    }
}

/// One card of a view, as Kova keeps it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CardSlot {
    pub id: String,
    pub card: Card,
    /// 1 when shown, bumped on every replacement (`ui-update-card` always,
    /// `ui-update` when the card's JSON changed).
    pub version: u32,
    /// Epoch seconds of the last click: the buttons stay locked until the
    /// agent updates the card, or for [`LOCK_SECS`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub clicked_at: Option<u64>,
}

impl CardSlot {
    pub fn locked(&self, now: u64) -> bool {
        self.clicked_at.is_some_and(|t| now.saturating_sub(t) < LOCK_SECS)
    }
}

/// A view: its document and everything that must survive a relaunch (it is
/// saved in the record of the pane that shows it).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct View {
    pub view_id: String,
    pub title: String,
    pub cards: Vec<CardSlot>,
    pub placement: Placement,
    /// Share of the pane (docked) or of the split (UI pane), as asked or dragged.
    pub size: f32,
    /// The user dragged the separator: a replacing `ui-show` keeps this size.
    #[serde(default)]
    pub dragged: bool,
    /// For a UI pane: the `uid` of the pane that opened it (its owner).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub opener_uid: Option<String>,
    /// The last session that owned the view: grants and late delivery go there
    /// when Kova cannot tell who runs in the owner pane now.
    pub owner: crate::ask::Asker,
}

impl View {
    pub fn new(view_id: String, doc: ViewDoc, placement: Placement, size: f32, owner: crate::ask::Asker) -> View {
        View {
            view_id,
            title: doc.title,
            cards: doc.cards.into_iter().map(|c| CardSlot { id: c.id, card: c.card, version: 1, clicked_at: None }).collect(),
            placement,
            size,
            dragged: false,
            opener_uid: None,
            owner,
        }
    }

    pub fn card(&self, id: &str) -> Option<&CardSlot> {
        self.cards.iter().find(|c| c.id == id)
    }

    /// `ui-update`: the new document replaces the view. A card whose JSON is
    /// unchanged keeps its version and grants; any other gets a new version.
    /// Every lock is released. Returns the ids of the cards whose unused
    /// grants must go (changed or removed).
    pub fn replace(&mut self, doc: ViewDoc) -> Vec<String> {
        let mut stale: Vec<String> = Vec::new();
        let mut cards = Vec::new();
        for new in doc.cards {
            match self.card(&new.id) {
                Some(old) if old.card == new.card => cards.push(CardSlot { clicked_at: None, ..old.clone() }),
                Some(old) => {
                    stale.push(new.id.clone());
                    cards.push(CardSlot { id: new.id, card: new.card, version: old.version + 1, clicked_at: None });
                }
                None => cards.push(CardSlot { id: new.id, card: new.card, version: 1, clicked_at: None }),
            }
        }
        for old in &self.cards {
            if !cards.iter().any(|c| c.id == old.id) {
                stale.push(old.id.clone());
            }
        }
        self.title = doc.title;
        self.cards = cards;
        stale
    }

    /// `ui-update-card`: replace one card (always a new version, lock
    /// released), append it when unknown, or remove it (`None`). Returns
    /// whether its grants must go, or an error when a 51st card is added.
    pub fn set_card(&mut self, id: &str, card: Option<Card>) -> Result<bool, String> {
        let index = self.cards.iter().position(|c| c.id == id);
        match (index, card) {
            (Some(i), Some(card)) => {
                let slot = &mut self.cards[i];
                slot.card = card;
                slot.version += 1;
                slot.clicked_at = None;
                Ok(true)
            }
            (Some(i), None) => {
                self.cards.remove(i);
                Ok(true)
            }
            (None, Some(card)) => {
                if self.cards.len() >= crate::decision_card::MAX_CARDS {
                    return Err("too many cards".into());
                }
                self.cards.push(CardSlot { id: id.to_string(), card, version: 1, clicked_at: None });
                Ok(false)
            }
            (None, None) => Ok(false),
        }
    }
}

/// What a view tells its agent.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum UiEvent {
    Click {
        view_id: String,
        card_id: String,
        action: String,
        text: String,
        card_version: u32,
        /// Every `code` block of the card as the user left it, in a file Kova
        /// wrote: the agent sends that file, never its own copy.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        blocks: Vec<crate::ask::AnswerBlock>,
    },
    Closed { view_id: String, event: String },
}

impl UiEvent {
    pub fn closed(view_id: &str) -> UiEvent {
        UiEvent::Closed { view_id: view_id.to_string(), event: "closed".to_string() }
    }

    pub fn view_id(&self) -> &str {
        match self {
            UiEvent::Click { view_id, .. } | UiEvent::Closed { view_id, .. } => view_id,
        }
    }
}

/// Any control character becomes a space: the line is typed into a terminal.
fn one_line(t: &str) -> String {
    t.replace("\r\n", " ").chars().map(|c| if c.is_control() { ' ' } else { c }).collect()
}

/// The line typed into the agent's conversation for `event`. The user's text
/// is cut at [`MAX_TYPED_TEXT`] chars; the full event is then in `last_path`,
/// named at the end of the line.
pub fn typed_line(event: &UiEvent, view_title: &str, card_title: &str, last_path: &Path) -> String {
    match event {
        UiEvent::Closed { .. } => format!("[Kova] UI \"{}\" was closed by the user", one_line(view_title)),
        UiEvent::Click { action, text, card_version, blocks, .. } => {
            let text = one_line(text);
            let cut = text.chars().count() > MAX_TYPED_TEXT;
            let shown: String = if cut { text.chars().take(MAX_TYPED_TEXT).collect::<String>() + "…" } else { text };
            let answer = if shown.is_empty() { action.clone() } else { format!("{}: {}", action, shown) };
            let mut line = format!(
                "[Kova] UI \"{}\", card \"{}\" (v{}): {}",
                one_line(view_title),
                one_line(card_title),
                card_version,
                answer
            );
            line.push_str(&one_line(&crate::ask::block_reports(blocks)));
            if cut {
                line.push_str(&format!(" (full event: {})", last_path.display()));
            }
            line
        }
    }
}

/// Where undelivered events and each view's last event live: `<ask dir>/ui/`.
pub fn ui_dir(ask_root: &Path) -> PathBuf {
    ask_root.join("ui")
}

pub fn last_event_path(ask_root: &Path, view_id: &str) -> PathBuf {
    ui_dir(ask_root).join(format!("{}.last.json", view_id))
}

/// An event kept on disk until delivered.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Stored {
    pub seq: u64,
    pub at: u64,
    pub event: UiEvent,
    /// Where it goes when nobody waits: the view's owner, its titles.
    pub owner: crate::ask::Asker,
    pub view_title: String,
    pub card_title: String,
}

fn stored_path(ask_root: &Path, s: &Stored) -> PathBuf {
    ui_dir(ask_root).join(format!("{}.{:020}.json", s.event.view_id(), s.seq))
}

pub fn save_stored(ask_root: &Path, s: &Stored) {
    let dir = ui_dir(ask_root);
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    if let Ok(data) = serde_json::to_vec(s) {
        let path = stored_path(ask_root, s);
        let tmp = path.with_extension("tmp");
        let _ = std::fs::write(&tmp, data).and_then(|_| std::fs::rename(&tmp, &path));
    }
}

pub fn remove_stored(ask_root: &Path, s: &Stored) {
    let _ = std::fs::remove_file(stored_path(ask_root, s));
}

pub fn write_last(ask_root: &Path, event: &UiEvent) {
    let dir = ui_dir(ask_root);
    if std::fs::create_dir_all(&dir).is_ok() {
        if let Ok(data) = serde_json::to_vec(event) {
            let _ = std::fs::write(last_event_path(ask_root, event.view_id()), data);
        }
    }
}

/// Undelivered events on disk, oldest first; those older than [`EVENT_TTL_SECS`]
/// are deleted on the way.
pub fn load_stored(ask_root: &Path, now: u64) -> Vec<Stored> {
    let Ok(entries) = std::fs::read_dir(ui_dir(ask_root)) else { return Vec::new() };
    let mut out: Vec<Stored> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or_default().to_string();
        if !name.ends_with(".json") || name.ends_with(".last.json") {
            continue;
        }
        match std::fs::read(&path).ok().and_then(|d| serde_json::from_slice::<Stored>(&d).ok()) {
            Some(s) if now.saturating_sub(s.at) < EVENT_TTL_SECS => out.push(s),
            _ => {
                let _ = std::fs::remove_file(&path);
            }
        }
    }
    out.sort_by_key(|s| s.seq);
    out
}

/// What to do with the head event of a view's queue now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Route {
    /// A `ui-wait` is open: hand it over.
    Waiter,
    /// Keep it: a `ui-wait` just returned, the user has a half-typed line,
    /// or a late delivery of this view is still on its way.
    Hold,
    /// Type it into the conversation.
    Type,
}

/// The routing rule of § 6 / § 12, for the head event of one view.
pub fn route(waiting: bool, since_wait_returned: Option<f64>, half_typed: bool, delivering: bool) -> Route {
    if waiting {
        return Route::Waiter;
    }
    if since_wait_returned.is_some_and(|s| s < WAIT_GAP_SECS) || half_typed || delivering {
        return Route::Hold;
    }
    Route::Type
}

/// The ordered event queue of every view, with the hand-off in flight to a
/// `ui-wait` (it goes back to the head without an `ack`). Main thread only.
#[derive(Default)]
pub struct Queues {
    pending: HashMap<String, VecDeque<Stored>>,
    in_flight: HashMap<String, Stored>,
    next_seq: u64,
}

impl Queues {
    pub fn new(start_seq: u64) -> Queues {
        Queues { next_seq: start_seq, ..Default::default() }
    }

    pub fn seq(&mut self) -> u64 {
        self.next_seq += 1;
        self.next_seq
    }

    pub fn push(&mut self, s: Stored) {
        self.pending.entry(s.event.view_id().to_string()).or_default().push_back(s);
    }

    pub fn head(&self, view_id: &str) -> Option<&Stored> {
        if self.in_flight.contains_key(view_id) {
            return None;
        }
        self.pending.get(view_id).and_then(|q| q.front())
    }

    /// Views with an event ready to route (none in flight).
    pub fn ready_views(&self) -> Vec<String> {
        self.pending
            .iter()
            .filter(|(id, q)| !q.is_empty() && !self.in_flight.contains_key(*id))
            .map(|(id, _)| id.clone())
            .collect()
    }

    /// Take the head event out for a `ui-wait`: in flight until acknowledged.
    pub fn hand_to_waiter(&mut self, view_id: &str) -> Option<Stored> {
        let s = self.pending.get_mut(view_id)?.pop_front()?;
        self.in_flight.insert(view_id.to_string(), s.clone());
        Some(s)
    }

    /// The `ui-wait` client acknowledged (`true`): the event is consumed.
    /// Without it, the event goes back to the head of its queue.
    pub fn acked(&mut self, view_id: &str, ok: bool) -> Option<Stored> {
        let s = self.in_flight.remove(view_id)?;
        if ok {
            return Some(s);
        }
        self.pending.entry(view_id.to_string()).or_default().push_front(s);
        None
    }

    /// Take the head event out to type it into the conversation.
    pub fn take_head(&mut self, view_id: &str) -> Option<Stored> {
        self.pending.get_mut(view_id)?.pop_front()
    }

    #[cfg(test)]
    pub fn len(&self, view_id: &str) -> usize {
        self.pending.get(view_id).map_or(0, |q| q.len()) + usize::from(self.in_flight.contains_key(view_id))
    }
}

/// Whether `pid` runs in the pane whose shell is `shell`: the process
/// descends from it (`ps`-free, through `crate::ask::descends`).
pub fn runs_in_pane(pid: i32, shell: i32, descends: impl Fn(i32, i32) -> bool) -> bool {
    pid > 0 && shell > 0 && (pid == shell || descends(pid, shell))
}

/// The grant id of a card of a view (`decision-cards-spec` § 8, pane form).
pub fn grant_id(view_id: &str, card_id: &str) -> String {
    format!("{}:{}", view_id, card_id)
}


#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn card(title: &str) -> Card {
        serde_json::from_value(json!({
            "version": 1, "title": title,
            "blocks": [{"type": "text", "text": "x"}],
            "actions": [{"id": "ok", "label": "OK"}]
        }))
        .unwrap()
    }

    fn doc(cards: &[(&str, &str)]) -> ViewDoc {
        ViewDoc {
            version: 1,
            title: "Drafts".into(),
            cards: cards.iter().map(|(id, t)| crate::decision_card::ViewCard { id: id.to_string(), card: card(t) }).collect(),
        }
    }

    fn view(cards: &[(&str, &str)]) -> View {
        View::new("v1".into(), doc(cards), Placement::DockBottom, 0.4, crate::ask::Asker::default())
    }

    #[test]
    fn placement_names_and_defaults() {
        assert_eq!(Placement::parse("dock-right"), Some(Placement::DockRight));
        assert_eq!(Placement::parse("pane-down"), Some(Placement::PaneDown));
        assert_eq!(Placement::parse("side"), None);
        assert_eq!(Placement::DockBottom.default_size(), 0.4);
        assert_eq!(Placement::PaneRight.default_size(), 0.5);
    }

    #[test]
    fn update_bumps_only_changed_cards_and_releases_locks() {
        let mut v = view(&[("a", "A"), ("b", "B"), ("c", "C")]);
        v.cards[0].clicked_at = Some(10);
        let stale = v.replace(doc(&[("a", "A"), ("b", "B2"), ("d", "D")]));
        assert_eq!(stale, vec!["b", "c"]);
        let versions: Vec<(&str, u32)> = v.cards.iter().map(|c| (c.id.as_str(), c.version)).collect();
        assert_eq!(versions, vec![("a", 1), ("b", 2), ("d", 1)]);
        assert!(v.cards.iter().all(|c| c.clicked_at.is_none()));
    }

    #[test]
    fn update_card_replaces_appends_and_removes() {
        let mut v = view(&[("a", "A")]);
        v.cards[0].clicked_at = Some(10);
        assert_eq!(v.set_card("a", Some(card("A"))), Ok(true), "identical still bumps");
        assert_eq!((v.cards[0].version, v.cards[0].clicked_at), (2, None));
        assert_eq!(v.set_card("z", Some(card("Z"))), Ok(false));
        assert_eq!(v.cards.last().unwrap().id, "z");
        assert_eq!(v.set_card("a", None), Ok(true));
        assert_eq!(v.set_card("nope", None), Ok(false), "removing an unknown card is a no-op");
        let mut full = view(&[]);
        for i in 0..50 {
            full.set_card(&format!("c{}", i), Some(card("x"))).unwrap();
        }
        assert_eq!(full.set_card("c50", Some(card("x"))), Err("too many cards".into()));
    }

    #[test]
    fn a_click_locks_the_card_for_ten_minutes() {
        let mut v = view(&[("a", "A")]);
        assert!(!v.cards[0].locked(100));
        v.cards[0].clicked_at = Some(100);
        assert!(v.cards[0].locked(100 + LOCK_SECS - 1));
        assert!(!v.cards[0].locked(100 + LOCK_SECS));
    }

    #[test]
    fn typed_lines() {
        let p = Path::new("/x/v1.last.json");
        let click = UiEvent::Click { view_id: "v1".into(), card_id: "a".into(), action: "edit".into(), text: "shorter\nplease".into(), card_version: 3, blocks: vec![] };
        assert_eq!(typed_line(&click, "Slack drafts", "Reply to Alex", p), "[Kova] UI \"Slack drafts\", card \"Reply to Alex\" (v3): edit: shorter please");
        let bare = UiEvent::Click { view_id: "v1".into(), card_id: "a".into(), action: "post".into(), text: String::new(), card_version: 1, blocks: vec![] };
        assert!(typed_line(&bare, "V", "C", p).ends_with("(v1): post"));
        let long = UiEvent::Click { view_id: "v1".into(), card_id: "a".into(), action: "edit".into(), text: "y".repeat(600), card_version: 1, blocks: vec![] };
        let line = typed_line(&long, "V", "C", p);
        assert!(line.contains(&format!("{}…", "y".repeat(500))) && line.ends_with("(full event: /x/v1.last.json)"));
        assert_eq!(typed_line(&UiEvent::closed("v1"), "V\u{1b}", "", p), "[Kova] UI \"V \" was closed by the user");
    }

    #[test]
    fn events_serialize_as_the_spec_shows() {
        let click = UiEvent::Click { view_id: "v".into(), card_id: "a".into(), action: "ok".into(), text: "".into(), card_version: 2, blocks: vec![] };
        assert_eq!(serde_json::to_value(&click).unwrap(), json!({"view_id": "v", "card_id": "a", "action": "ok", "text": "", "card_version": 2}));
        assert_eq!(serde_json::to_value(UiEvent::closed("v")).unwrap(), json!({"view_id": "v", "event": "closed"}));
        let back: UiEvent = serde_json::from_value(json!({"view_id": "v", "event": "closed"})).unwrap();
        assert_eq!(back, UiEvent::closed("v"));
    }

    #[test]
    fn routing_rules() {
        assert_eq!(route(true, None, true, true), Route::Waiter, "a waiting client always gets it");
        assert_eq!(route(false, Some(2.0), false, false), Route::Hold, "between two waits of a loop");
        assert_eq!(route(false, Some(6.0), false, false), Route::Type);
        assert_eq!(route(false, None, true, false), Route::Hold, "half-typed line");
        assert_eq!(route(false, None, false, true), Route::Hold, "one delivery at a time");
        assert_eq!(route(false, None, false, false), Route::Type, "a view never waited on");
    }

    fn stored(q: &mut Queues, view: &str, n: u64) -> Stored {
        Stored {
            seq: q.seq(),
            at: n,
            event: UiEvent::Click { view_id: view.into(), card_id: format!("c{}", n), action: "ok".into(), text: "".into(), card_version: 1, blocks: vec![] },
            owner: crate::ask::Asker::default(),
            view_title: "V".into(),
            card_title: "C".into(),
        }
    }

    #[test]
    fn events_leave_in_order_once_and_come_back_without_ack() {
        let mut q = Queues::new(0);
        let a = stored(&mut q, "v", 1);
        let b = stored(&mut q, "v", 2);
        q.push(a.clone());
        q.push(b.clone());
        assert_eq!(q.hand_to_waiter("v"), Some(a.clone()));
        assert_eq!(q.head("v"), None, "nothing else leaves while one is in flight");
        assert_eq!(q.acked("v", false), None);
        assert_eq!(q.head("v"), Some(&a), "no ack: back at the head");
        assert_eq!(q.hand_to_waiter("v"), Some(a.clone()));
        assert_eq!(q.acked("v", true), Some(a));
        assert_eq!(q.take_head("v"), Some(b));
        assert_eq!(q.len("v"), 0);
    }

    #[test]
    fn events_survive_on_disk_and_expire_after_a_week() {
        let root = std::env::temp_dir().join(format!("kova-ui-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let mut q = Queues::new(0);
        let old = Stored { at: 0, ..stored(&mut q, "v", 1) };
        let fresh = Stored { at: EVENT_TTL_SECS, ..stored(&mut q, "v", 2) };
        save_stored(&root, &old);
        save_stored(&root, &fresh);
        write_last(&root, &fresh.event);
        let back = load_stored(&root, EVENT_TTL_SECS + 10);
        assert_eq!(back, vec![fresh.clone()]);
        assert_eq!(load_stored(&root, EVENT_TTL_SECS + 10).len(), 1, "the old one was deleted");
        remove_stored(&root, &fresh);
        assert!(load_stored(&root, EVENT_TTL_SECS + 10).is_empty());
        assert!(last_event_path(&root, "v").exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_process_runs_in_a_pane_when_it_descends_from_its_shell() {
        let parent = |pid: i32, anc: i32| pid == 30 && anc == 10;
        assert!(runs_in_pane(30, 10, parent));
        assert!(runs_in_pane(10, 10, parent));
        assert!(!runs_in_pane(31, 10, parent));
        assert!(!runs_in_pane(0, 10, parent));
    }

    #[test]
    fn grant_ids() {
        assert_eq!(grant_id("v1", "draft-a"), "v1:draft-a");
        assert_eq!(crate::decision_card::grant_file_stem("0f8fad5b-d9cb-469f-a165-70867728950e:draft-a").as_deref(), Some("0f8fad5b-d9cb-469f-a165-70867728950e_draft-a"));
    }
}
