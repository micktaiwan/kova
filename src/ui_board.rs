//! The main thread's side of pane UI views (`docs/pane-ui-spec.md`): the
//! socket commands (`ui-show`, `ui-update`, `ui-update-card`, `ui-close`,
//! `ui-wait`), clicks from the views, and the routing of each view's events
//! to a waiting `kova ui wait` or into the agent's conversation.
//!
//! The views themselves live in the records of the panes that show them
//! (`Pane::ui_view`); this board reaches the panes through [`UiHost`], which
//! Kova.app implements over its windows and the tests implement over a fake.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::agent_session::Agent;
use crate::ask::{Asker, Trigger};
use crate::ipc::IpcResponse;
use crate::pane::PaneId;
use crate::pane_ui::{self, Placement, Route, Stored, UiEvent, View};

/// One `ui-*` request, with the pid of the process at the other end of the
/// socket (`LOCAL_PEERPID`): permissions go by pane, through that process.
#[derive(Clone, Debug, PartialEq)]
pub struct UiRequest {
    pub peer_pid: i32,
    /// Who asks (the CLI fills it from its environment, like `kova ask`).
    pub from: Asker,
    pub op: UiOp,
}

#[derive(Clone, Debug, PartialEq)]
pub enum UiOp {
    Show { pane_id: PaneId, placement: Placement, size: Option<f32>, view: Value },
    Update { view_id: String, view: Value },
    UpdateCard { view_id: String, card_id: String, card: Option<Value> },
    Close { view_id: String },
    Wait { view_id: String, timeout_ms: u64 },
    /// Internal, never parsed from the socket: the `ui-wait` client wrote
    /// `ack` (or not) for the event it was handed.
    Ack { view_id: String, ok: bool },
    /// Internal: the `ui-wait` timed out on its side; drop the waiter.
    Unwait { view_id: String },
}

/// What the board needs to know of a pane.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PaneInfo {
    pub id: PaneId,
    pub shell_pid: i32,
    pub ui_pane: bool,
    pub uid: String,
    /// The agent conversation Kova detects in it.
    pub agent: Option<(Agent, String)>,
    pub cwd: Option<String>,
    /// Characters typed since the user's last Return (`Pty::typed_pending`).
    pub typed_pending: u32,
}

/// The panes, as the board sees them. Main thread.
pub trait UiHost {
    fn pane(&self, id: PaneId) -> Option<PaneInfo>;
    fn pane_by_uid(&self, uid: &str) -> Option<PaneInfo>;
    /// The pane showing view `view_id`, and the view.
    fn find_view(&self, view_id: &str) -> Option<(PaneInfo, View)>;
    /// The view pane `id` shows, if any.
    fn view_in(&self, id: PaneId) -> Option<View>;
    /// Every view shown, with the pane showing it.
    fn views(&self) -> Vec<(PaneInfo, View)>;
    /// Put `view` on pane `id` (docked unless it is a UI pane), or take it off.
    fn set_view(&self, id: PaneId, view: Option<View>);
    /// Open a UI pane next to `opener`; its id.
    fn open_ui_pane(&self, opener: PaneId, placement: Placement, size: f32) -> Option<PaneId>;
    fn close_pane(&self, id: PaneId);
    /// UI panes whose view was opened by the pane with this uid.
    fn ui_panes_of(&self, opener_uid: &str) -> usize;
    /// Whether `pid` descends from `ancestor` (`crate::ask::descends`).
    fn descends(&self, pid: i32, ancestor: i32) -> bool;
    fn own_socket(&self) -> String;
}

/// The board. Main thread only, except the set of views with a late delivery
/// on its way, which the delivery threads clear.
pub struct UiBoard {
    root: PathBuf,
    queues: pane_ui::Queues,
    waiters: HashMap<String, std::sync::mpsc::Sender<IpcResponse>>,
    wait_returned: HashMap<String, std::time::Instant>,
    delivering: Arc<Mutex<HashSet<String>>>,
    /// Views already closed (by their agent or the ✕): their pane closing
    /// afterwards sends no second `closed`.
    closed: HashSet<String>,
}

fn now_secs() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn ok(data: Option<Value>) -> IpcResponse {
    IpcResponse::Ok { data }
}

fn err(message: impl Into<String>) -> IpcResponse {
    IpcResponse::Error { message: message.into() }
}

/// What the owner pane says about the conversation running in it, merged
/// over the last known owner: the detected session wins (§ 12 "Pane versus
/// session_id").
fn owner_asker(last: &Asker, owner: Option<&PaneInfo>, own_socket: &str) -> Asker {
    let mut asker = last.clone();
    if let Some(p) = owner {
        if let Some((agent, session)) = &p.agent {
            asker.agent = *agent;
            asker.session_id = Some(session.clone());
        }
        asker.pane_id = Some(p.id);
        asker.kova_socket = Some(own_socket.to_string());
        if p.cwd.is_some() {
            asker.cwd = p.cwd.clone();
        }
    }
    asker
}

#[derive(Serialize, Deserialize)]
struct ShowReply {
    view_id: String,
    pane_id: PaneId,
}

impl UiBoard {
    pub fn new(root: PathBuf) -> UiBoard {
        let stored = pane_ui::load_stored(&root, now_secs());
        let start = stored.iter().map(|s| s.seq).max().unwrap_or(0).max(now_secs() * 1000);
        let mut queues = pane_ui::Queues::new(start);
        for s in stored {
            log::info!("ui {}: undelivered event restored from disk", s.event.view_id());
            queues.push(s);
        }
        UiBoard {
            root,
            queues,
            waiters: HashMap::new(),
            wait_returned: HashMap::new(),
            delivering: Arc::new(Mutex::new(HashSet::new())),
            closed: HashSet::new(),
        }
    }

    /// The pane that owns view `view` shown in `host`: the host itself for a
    /// docked view, the opener for a UI pane (`None` once it is closed).
    fn owner_of(&self, host: &dyn UiHost, shown_in: &PaneInfo, view: &View) -> Option<PaneInfo> {
        if !shown_in.ui_pane {
            return Some(shown_in.clone());
        }
        view.opener_uid.as_deref().and_then(|uid| host.pane_by_uid(uid))
    }

    /// The view `view_id`, if the caller runs in its owner pane.
    fn owned(&self, host: &dyn UiHost, peer_pid: i32, view_id: &str) -> Result<(PaneInfo, View, PaneInfo), String> {
        let (shown_in, view) = host.find_view(view_id).ok_or("no such view")?;
        let owner = self.owner_of(host, &shown_in, &view).ok_or("not the owner of this view")?;
        if !pane_ui::runs_in_pane(peer_pid, owner.shell_pid, |p, a| host.descends(p, a)) {
            return Err("not the owner of this view".into());
        }
        Ok((shown_in, view, owner))
    }

    fn facts(owner: &PaneInfo, from: &Asker) -> (Option<String>, bool) {
        match &owner.agent {
            Some((agent, session)) => (Some(session.clone()), *agent == Agent::Claude),
            None => (from.session_id.clone(), from.agent == Agent::Claude && from.session_id.is_some()),
        }
    }

    fn validate_view(owner: &PaneInfo, from: &Asker, view: &Value) -> Result<crate::decision_card::ViewDoc, String> {
        let (session, claude) = Self::facts(owner, from);
        let facts = crate::decision_card::AskerFacts { session_id: session.as_deref(), claude };
        crate::decision_card::validate_view(view, &facts)
    }

    fn drop_grants(&self, view_id: &str, cards: &[String]) {
        for card in cards {
            crate::decision_card::remove_grant(&self.root, &pane_ui::grant_id(view_id, card));
        }
    }

    /// A socket request. `responder` is kept for a `ui-wait`; every other
    /// request is answered at once.
    pub fn handle(&mut self, host: &dyn UiHost, req: UiRequest, responder: std::sync::mpsc::Sender<IpcResponse>) {
        let reply = match req.op {
            UiOp::Wait { view_id, .. } => {
                match self.owned(host, req.peer_pid, &view_id) {
                    Err(e) => Some(err(e)),
                    Ok(_) if self.waiters.contains_key(&view_id) => Some(err("already waiting")),
                    Ok(_) => {
                        self.waiters.insert(view_id, responder.clone());
                        None
                    }
                }
            }
            UiOp::Ack { view_id, ok: acked } => {
                match self.queues.acked(&view_id, acked) {
                    Some(s) => {
                        pane_ui::remove_stored(&self.root, &s);
                        pane_ui::write_last(&self.root, &s.event);
                        log::info!("ui {}: event handed to its waiting agent", view_id);
                    }
                    None if !acked => log::info!("ui {}: no ack from the waiting agent, event kept", view_id),
                    None => {}
                }
                self.wait_returned.insert(view_id, std::time::Instant::now());
                Some(ok(None))
            }
            UiOp::Unwait { view_id } => {
                self.waiters.remove(&view_id);
                Some(ok(None))
            }
            op => Some(self.command(host, req.peer_pid, &req.from, op)),
        };
        if let Some(reply) = reply {
            let _ = responder.send(reply);
        }
        self.pump(host);
    }

    fn command(&mut self, host: &dyn UiHost, peer_pid: i32, from: &Asker, op: UiOp) -> IpcResponse {
        match op {
            UiOp::Show { pane_id, placement, size, view } => self.show(host, peer_pid, from, pane_id, placement, size, &view),
            UiOp::Update { view_id, view } => {
                let (shown_in, mut current, owner) = match self.owned(host, peer_pid, &view_id) {
                    Ok(v) => v,
                    Err(e) => return err(e),
                };
                let doc = match Self::validate_view(&owner, from, &view) {
                    Ok(d) => d,
                    Err(e) => return err(e),
                };
                let stale = current.replace(doc);
                self.drop_grants(&view_id, &stale);
                current.owner = owner_asker(&current.owner, Some(&owner), &host.own_socket());
                host.set_view(shown_in.id, Some(current));
                ok(None)
            }
            UiOp::UpdateCard { view_id, card_id, card } => {
                let (shown_in, mut current, owner) = match self.owned(host, peer_pid, &view_id) {
                    Ok(v) => v,
                    Err(e) => return err(e),
                };
                let card = match card {
                    None => None,
                    Some(raw) => {
                        let (session, claude) = Self::facts(&owner, from);
                        let facts = crate::decision_card::AskerFacts { session_id: session.as_deref(), claude };
                        match crate::decision_card::validate(&raw, None, &facts) {
                            Ok(c) => Some(c),
                            Err(e) => return err(format!("card: {}", e)),
                        }
                    }
                };
                if !crate::decision_card::is_action_id(&card_id) {
                    return err(format!("bad card id {}", card_id));
                }
                match current.set_card(&card_id, card) {
                    Err(e) => return err(e),
                    Ok(true) => self.drop_grants(&view_id, &[card_id.clone()]),
                    Ok(false) => {}
                }
                let version = current.card(&card_id).map(|c| c.version);
                host.set_view(shown_in.id, Some(current));
                ok(Some(serde_json::json!({ "card_version": version })))
            }
            UiOp::Close { view_id } => {
                let (shown_in, current, _) = match self.owned(host, peer_pid, &view_id) {
                    Ok(v) => v,
                    Err(e) => return err(e),
                };
                let ids: Vec<String> = current.cards.iter().map(|c| c.id.clone()).collect();
                self.drop_grants(&view_id, &ids);
                self.closed.insert(view_id.clone());
                host.set_view(shown_in.id, None);
                if shown_in.ui_pane {
                    host.close_pane(shown_in.id);
                }
                if let Some(tx) = self.waiters.remove(&view_id) {
                    let _ = tx.send(ok(None));
                }
                ok(None)
            }
            UiOp::Wait { .. } | UiOp::Ack { .. } | UiOp::Unwait { .. } => err("internal: not a command"),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn show(&mut self, host: &dyn UiHost, peer_pid: i32, from: &Asker, pane_id: PaneId, placement: Placement, size: Option<f32>, raw: &Value) -> IpcResponse {
        let Some(pane) = host.pane(pane_id) else { return err(format!("pane {} not found", pane_id)) };
        if pane.ui_pane {
            return err("use ui-update");
        }
        if !pane_ui::runs_in_pane(peer_pid, pane.shell_pid, |p, a| host.descends(p, a)) {
            return err("not your pane");
        }
        let doc = match Self::validate_view(&pane, from, raw) {
            Ok(d) => d,
            Err(e) => return err(e),
        };
        let owner = owner_asker(from, Some(&pane), &host.own_socket());
        let size = size.unwrap_or(placement.default_size());
        if placement.docked() {
            let existing = host.view_in(pane_id);
            let view = match existing {
                Some(mut v) => {
                    let stale = v.replace(doc);
                    self.drop_grants(&v.view_id, &stale);
                    if !v.dragged {
                        v.size = size;
                    }
                    v.placement = placement;
                    v.owner = owner;
                    v
                }
                None => View::new(crate::ask::new_question_id(), doc, placement, size, owner),
            };
            let view_id = view.view_id.clone();
            log::info!("ui {}: docked in pane {} ({:?}, {} cards)", view_id, pane_id, placement, view.cards.len());
            host.set_view(pane_id, Some(view));
            return ok(serde_json::to_value(ShowReply { view_id, pane_id }).ok());
        }
        let uid = pane.uid.clone();
        if host.ui_panes_of(&uid) >= pane_ui::MAX_UI_PANES {
            return err("too many ui panes");
        }
        let Some(new_id) = host.open_ui_pane(pane_id, placement, size) else { return err("could not open a ui pane") };
        let mut view = View::new(crate::ask::new_question_id(), doc, placement, size, owner);
        view.opener_uid = Some(uid);
        let view_id = view.view_id.clone();
        log::info!("ui {}: in its own pane {} opened by pane {}", view_id, new_id, pane_id);
        host.set_view(new_id, Some(view));
        ok(serde_json::to_value(ShowReply { view_id, pane_id: new_id }).ok())
    }

    /// A click on a card of a view, from the window. Locked cards and plain
    /// Return on a grant answer nothing. `edits` are the user's versions of
    /// the card's `code` blocks (block index, text), made on the version
    /// shown: every code block's final text is written to a file the event
    /// names, then a grant action writes its grant over those bytes, as a
    /// `kova ask` card does. When a write fails the click is dropped and the
    /// reason returned.
    #[allow(clippy::too_many_arguments)]
    pub fn click(&mut self, host: &dyn UiHost, view_id: &str, card_id: &str, action: &str, text: &str, edits: &[(usize, String)], trigger: Trigger) -> Result<(), String> {
        let (shown_in, mut view) = host.find_view(view_id).ok_or("no such view")?;
        let now = now_secs();
        let slot = view.card(card_id).cloned().ok_or("no such card")?;
        if slot.locked(now) {
            return Ok(());
        }
        let chosen = slot.card.action(action).ok_or_else(|| format!("no action {} in this card", action))?.clone();
        let text = text.trim().to_string();
        if chosen.needs_input && text.is_empty() {
            return Err(format!("{} needs text", action));
        }
        let owner = self.owner_of(host, &shown_in, &view);
        let asker = owner_asker(&view.owner, owner.as_ref(), &host.own_socket());
        let grant_id = pane_ui::grant_id(view_id, card_id);
        let session = match &chosen.grant {
            Some(_) if trigger == Trigger::Return || trigger == Trigger::Remote => return Ok(()),
            Some(_) => Some(asker.session_id.clone().ok_or("grant requires an asker session id")?),
            None => None,
        };
        let finals = crate::ask::final_blocks(&slot.card, edits);
        // The texts first: a grant must never exist for a payload whose file is missing.
        let blocks = crate::ask::write_block_files(&self.root, &grant_id, &finals)
            .map_err(|e| format!("Could not save the edited text: {}", e))?;
        if let (Some(decl), Some(session)) = (&chosen.grant, session) {
            let grant = crate::ask::grant_for(&grant_id, session, decl, &finals, slot.version, now);
            crate::decision_card::write_grant(&self.root, &grant).map_err(|e| format!("Could not record the authorization: {}", e))?;
            log::info!("ui {}: grant written for card {} ({})", view_id, card_id, grant.target);
        }
        let event = UiEvent::Click {
            view_id: view_id.to_string(),
            card_id: card_id.to_string(),
            action: action.to_string(),
            text,
            card_version: slot.version,
            blocks,
        };
        if let Some(s) = view.cards.iter_mut().find(|c| c.id == card_id) {
            s.clicked_at = Some(now);
        }
        let card_title = slot.card.title.clone();
        view.owner = asker.clone();
        host.set_view(shown_in.id, Some(view.clone()));
        self.enqueue(event, asker, &view.title, &card_title);
        self.pump(host);
        Ok(())
    }

    /// Every view shown, for a remote client (`list-ui-views`): who owns it
    /// (pane and session), and each card with its version and lock.
    pub fn list(&self, host: &dyn UiHost) -> Value {
        let now = now_secs();
        let views: Vec<Value> = host
            .views()
            .into_iter()
            .map(|(shown_in, view)| {
                let owner = self.owner_of(host, &shown_in, &view);
                let asker = owner_asker(&view.owner, owner.as_ref(), &host.own_socket());
                let cards: Vec<Value> = view
                    .cards
                    .iter()
                    .map(|s| {
                        serde_json::json!({
                            "id": s.id,
                            "card_version": s.version,
                            "locked": s.locked(now),
                            "card": serde_json::to_value(&s.card).unwrap_or_default(),
                        })
                    })
                    .collect();
                serde_json::json!({
                    "view_id": view.view_id,
                    "title": view.title,
                    "pane_id": shown_in.id,
                    "owner_pane_id": owner.as_ref().map(|p| p.id),
                    "session_id": asker.session_id,
                    "cards": cards,
                })
            })
            .collect();
        serde_json::json!({ "views": views })
    }

    /// A click from a remote client (`answer-ui`): the window's own path, but
    /// refused out loud where the window stays silent. A grant action is never
    /// granted from here, whoever calls: any local process can reach the socket,
    /// the agent that asked included (`docs/decision-cards-spec.md` § 8.3).
    pub fn answer_remote(&mut self, host: &dyn UiHost, view_id: &str, card_id: &str, action: &str, text: &str, card_version: Option<u32>) -> Result<Value, String> {
        let (_, view) = host.find_view(view_id).ok_or("no such view")?;
        let slot = view.card(card_id).ok_or("no such card")?;
        if card_version.is_some_and(|v| v != slot.version) {
            return Err(format!("stale card: version {} is shown now", slot.version));
        }
        if slot.locked(now_secs()) {
            return Err("card locked: its last click is not handled yet".into());
        }
        let chosen = slot.card.action(action).ok_or_else(|| format!("no action {} in this card", action))?;
        if chosen.grant.is_some() {
            return Err("a grant action can only be clicked".into());
        }
        let version = slot.version;
        self.click(host, view_id, card_id, action, text, &[], Trigger::Remote)?;
        Ok(serde_json::json!({ "view_id": view_id, "card_id": card_id, "action": action, "card_version": version }))
    }

    /// The user closed a view (its ✕, or its pane): `closed` goes to its agent.
    pub fn closed_by_user(&mut self, host: &dyn UiHost, view: View, owner_now: Option<PaneInfo>) {
        let asker = owner_asker(&view.owner, owner_now.as_ref(), &host.own_socket());
        // A card the user just clicked keeps its grant: clicking Post then closing the view is one
        // gesture, and the agent's send may start after the close. The grant stays single-use and
        // expires with GRANT_TTL_SECS.
        let ids: Vec<String> = view.cards.iter().filter(|c| c.clicked_at.is_none()).map(|c| c.id.clone()).collect();
        self.drop_grants(&view.view_id, &ids);
        log::info!("ui {}: closed by the user", view.view_id);
        self.enqueue(UiEvent::closed(&view.view_id), asker, &view.title, "");
        self.pump(host);
    }

    fn enqueue(&mut self, event: UiEvent, owner: Asker, view_title: &str, card_title: &str) {
        let s = Stored {
            seq: self.queues.seq(),
            at: now_secs(),
            event,
            owner,
            view_title: view_title.to_string(),
            card_title: card_title.to_string(),
        };
        pane_ui::save_stored(&self.root, &s);
        self.queues.push(s);
    }

    /// Route every view's head event: to its waiting `kova ui wait`, or typed
    /// into its conversation when nothing holds it. Called after each change
    /// and from the tick (holds expire).
    pub fn pump(&mut self, host: &dyn UiHost) {
        for view_id in self.queues.ready_views() {
            let Some(head) = self.queues.head(&view_id).cloned() else { continue };
            let waiting = self.waiters.contains_key(&view_id);
            let since = self.wait_returned.get(&view_id).map(|t| t.elapsed().as_secs_f64());
            let delivering = self.delivering.lock().map(|d| d.contains(&view_id)).unwrap_or(false);
            let half_typed = head
                .owner
                .pane_id
                .and_then(|id| host.pane(id))
                .is_some_and(|p| p.typed_pending > 0);
            match pane_ui::route(waiting, since, half_typed, delivering) {
                Route::Hold => {}
                Route::Waiter => {
                    let Some(tx) = self.waiters.remove(&view_id) else { continue };
                    let Some(s) = self.queues.hand_to_waiter(&view_id) else { continue };
                    let data = serde_json::to_value(&s.event).ok();
                    if tx.send(ok(data)).is_err() {
                        // The client left: back at the head.
                        self.queues.acked(&view_id, false);
                    }
                }
                Route::Type => {
                    let Some(s) = self.queues.take_head(&view_id) else { continue };
                    // Tests never type into a real conversation (nor read ~/.claude).
                    if cfg!(test) {
                        continue;
                    }
                    if let Ok(mut d) = self.delivering.lock() {
                        d.insert(view_id.clone());
                    }
                    let root = self.root.clone();
                    let delivering = self.delivering.clone();
                    let own = host.own_socket();
                    let spawned = std::thread::Builder::new().name("ui-delivery".into()).spawn(move || {
                        let last = pane_ui::last_event_path(&root, s.event.view_id());
                        let line = pane_ui::typed_line(&s.event, &s.view_title, &s.card_title, &last);
                        let id = format!("ui-{}", s.event.view_id());
                        crate::ask::deliver_line(&id, &s.owner, &line, &own, &|| {
                            pane_ui::write_last(&root, &s.event);
                            pane_ui::remove_stored(&root, &s);
                        });
                        if let Ok(mut d) = delivering.lock() {
                            d.remove(s.event.view_id());
                        }
                    });
                    if let Err(e) = spawned {
                        log::error!("ui: cannot start a delivery thread ({}); the event stays on disk", e);
                    }
                }
            }
        }
    }

    /// The window's clicks and closes since the last tick, then the events
    /// waiting. Called from the tick.
    pub fn drain_views(&mut self, host: &dyn UiHost) {
        for c in crate::window::dock_view::take_ui_clicks() {
            // Edits made on another version of the card than the one now shown are dropped.
            let current = host.find_view(&c.view_id).and_then(|(_, v)| v.card(&c.card_id).map(|s| s.version));
            let edits: &[(usize, String)] = if current == Some(c.edits_version) { &c.edits } else { &[] };
            if let Err(e) = self.click(host, &c.view_id, &c.card_id, &c.action, &c.text, edits, c.trigger) {
                log::info!("ui {}: click on card {} refused: {}", c.view_id, c.card_id, e);
                let version = host.find_view(&c.view_id).and_then(|(_, v)| v.card(&c.card_id).map(|s| s.version)).unwrap_or(0);
                crate::window::dock_view::set_ui_error(&c.view_id, &c.card_id, version, &e);
            }
        }
        for view in crate::window::dock_view::take_ui_closes() {
            if !self.closed.insert(view.view_id.clone()) {
                continue;
            }
            // ✕ on a view still shown: take it off, and close its UI pane.
            let shown = host.find_view(&view.view_id);
            let owner = match view.opener_uid.as_deref() {
                Some(uid) => host.pane_by_uid(uid),
                None => shown.as_ref().map(|(p, _)| p.clone()),
            };
            if let Some((pane, _)) = &shown {
                host.set_view(pane.id, None);
                if pane.ui_pane {
                    host.close_pane(pane.id);
                }
            }
            self.closed_by_user(host, view, owner);
        }
        self.pump(host);
    }

    #[cfg(test)]
    pub fn pending_events(&self, view_id: &str) -> usize {
        self.queues.len(view_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::cell::RefCell;

    #[derive(Default)]
    struct Fake {
        panes: RefCell<HashMap<PaneId, (PaneInfo, Option<View>)>>,
        next: RefCell<PaneId>,
        closed: RefCell<Vec<PaneId>>,
    }

    impl Fake {
        fn with_pane(id: PaneId, shell: i32, session: &str) -> Fake {
            let f = Fake { next: RefCell::new(100), ..Default::default() };
            f.add(id, shell, session);
            f
        }
        fn add(&self, id: PaneId, shell: i32, session: &str) {
            let info = PaneInfo {
                id,
                shell_pid: shell,
                uid: format!("uid-{}", id),
                agent: Some((Agent::Claude, session.to_string())),
                ..Default::default()
            };
            self.panes.borrow_mut().insert(id, (info, None));
        }
        fn type_in(&self, id: PaneId, n: u32) {
            self.panes.borrow_mut().get_mut(&id).unwrap().0.typed_pending = n;
        }
    }

    impl UiHost for Fake {
        fn pane(&self, id: PaneId) -> Option<PaneInfo> {
            self.panes.borrow().get(&id).map(|p| p.0.clone())
        }
        fn pane_by_uid(&self, uid: &str) -> Option<PaneInfo> {
            self.panes.borrow().values().find(|p| p.0.uid == uid).map(|p| p.0.clone())
        }
        fn find_view(&self, view_id: &str) -> Option<(PaneInfo, View)> {
            self.panes
                .borrow()
                .values()
                .find(|p| p.1.as_ref().is_some_and(|v| v.view_id == view_id))
                .map(|p| (p.0.clone(), p.1.clone().unwrap()))
        }
        fn view_in(&self, id: PaneId) -> Option<View> {
            self.panes.borrow().get(&id).and_then(|p| p.1.clone())
        }
        fn views(&self) -> Vec<(PaneInfo, View)> {
            self.panes.borrow().values().filter_map(|p| p.1.clone().map(|v| (p.0.clone(), v))).collect()
        }
        fn set_view(&self, id: PaneId, view: Option<View>) {
            if let Some(p) = self.panes.borrow_mut().get_mut(&id) {
                p.1 = view;
            }
        }
        fn open_ui_pane(&self, _opener: PaneId, _p: Placement, _s: f32) -> Option<PaneId> {
            let id = {
                let mut n = self.next.borrow_mut();
                *n += 1;
                *n
            };
            let info = PaneInfo { id, shell_pid: 0, ui_pane: true, uid: format!("uid-{}", id), ..Default::default() };
            self.panes.borrow_mut().insert(id, (info, None));
            Some(id)
        }
        fn close_pane(&self, id: PaneId) {
            self.closed.borrow_mut().push(id);
            self.panes.borrow_mut().remove(&id);
        }
        fn ui_panes_of(&self, opener_uid: &str) -> usize {
            self.panes
                .borrow()
                .values()
                .filter(|p| p.0.ui_pane && p.1.as_ref().and_then(|v| v.opener_uid.as_deref()) == Some(opener_uid))
                .count()
        }
        fn descends(&self, pid: i32, ancestor: i32) -> bool {
            // Agents run as shell pid + 1.
            pid == ancestor + 1
        }
        fn own_socket(&self) -> String {
            "/tmp/kova-test.sock".into()
        }
    }

    fn card(title: &str) -> Value {
        json!({"version": 1, "title": title, "blocks": [{"type": "text", "text": "x"}],
               "actions": [{"id": "ok", "label": "OK"}, {"id": "edit", "label": "Edit", "needs_input": true}],
               "input": {}})
    }

    fn grant_card(payload: &str) -> Value {
        json!({"version": 1, "title": "Post?", "blocks": [{"type": "code", "text": payload}],
               "actions": [{"id": "post", "label": "Post", "style": "primary",
                            "grant": {"kind": "outbound", "target": "slack:C1/-",
                                      "payload_sha256": crate::decision_card::sha256_hex(payload.as_bytes()), "payload_block": 0}},
                           {"id": "later", "label": "Later"}]})
    }

    fn view(cards: Vec<(&str, Value)>) -> Value {
        json!({"version": 1, "title": "Drafts", "cards": cards.into_iter().map(|(id, c)| json!({"id": id, "card": c})).collect::<Vec<_>>()})
    }

    fn board(name: &str) -> UiBoard {
        let root = std::env::temp_dir().join(format!("kova-uiboard-{}-{}", name, std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        UiBoard::new(root)
    }

    fn call(b: &mut UiBoard, host: &Fake, peer: i32, op: UiOp) -> IpcResponse {
        let (tx, rx) = std::sync::mpsc::channel();
        b.handle(host, UiRequest { peer_pid: peer, from: Asker::default(), op }, tx);
        rx.try_recv().unwrap_or(IpcResponse::Error { message: "no reply yet".into() })
    }

    fn show(b: &mut UiBoard, host: &Fake, peer: i32, placement: Placement, v: Value) -> Result<(String, PaneId), String> {
        match call(b, host, peer, UiOp::Show { pane_id: 1, placement, size: None, view: v }) {
            IpcResponse::Ok { data: Some(d) } => Ok((d["view_id"].as_str().unwrap().to_string(), d["pane_id"].as_u64().unwrap() as PaneId)),
            IpcResponse::Error { message } => Err(message),
            other => panic!("{:?}", other.to_json()),
        }
    }

    #[test]
    fn only_the_agent_in_the_pane_shows_updates_and_closes() {
        let host = Fake::with_pane(1, 500, "s1");
        let mut b = board("own");
        assert_eq!(show(&mut b, &host, 777, Placement::DockBottom, view(vec![])), Err("not your pane".into()));
        let (id, pane) = show(&mut b, &host, 501, Placement::DockBottom, view(vec![("a", card("A"))])).unwrap();
        assert_eq!(pane, 1);
        let again = show(&mut b, &host, 501, Placement::DockBottom, view(vec![("a", card("A2"))])).unwrap();
        assert_eq!(again.0, id, "one docked view per pane: replaced, same id");
        assert_eq!(host.view_in(1).unwrap().cards[0].version, 2);
        let refused = call(&mut b, &host, 777, UiOp::Update { view_id: id.clone(), view: view(vec![]) });
        assert!(matches!(refused, IpcResponse::Error { message } if message == "not the owner of this view"));
        let refused = call(&mut b, &host, 777, UiOp::Close { view_id: id.clone() });
        assert!(matches!(refused, IpcResponse::Error { .. }));
        assert!(matches!(call(&mut b, &host, 501, UiOp::Close { view_id: id.clone() }), IpcResponse::Ok { .. }));
        assert!(host.view_in(1).is_none());
        assert!(matches!(call(&mut b, &host, 501, UiOp::Close { view_id: id }), IpcResponse::Error { message } if message == "no such view"));
    }

    #[test]
    fn ui_panes_belong_to_their_opener_and_are_capped() {
        let host = Fake::with_pane(1, 500, "s1");
        let mut b = board("panes");
        let (id, pane) = show(&mut b, &host, 501, Placement::PaneRight, view(vec![])).unwrap();
        assert_ne!(pane, 1);
        let waiting = call(&mut b, &host, 501, UiOp::Wait { view_id: id.clone(), timeout_ms: 1000 });
        assert!(matches!(waiting, IpcResponse::Error { message } if message == "no reply yet"), "the opener may wait on it");
        let stranger = call(&mut b, &host, 777, UiOp::Wait { view_id: id.clone(), timeout_ms: 1000 });
        assert!(matches!(stranger, IpcResponse::Error { message } if message == "not the owner of this view"));
        for _ in 0..3 {
            show(&mut b, &host, 501, Placement::PaneDown, view(vec![])).unwrap();
        }
        assert_eq!(show(&mut b, &host, 501, Placement::PaneDown, view(vec![])), Err("too many ui panes".into()));
        let ui_shown = match call(&mut b, &host, 501, UiOp::Show { pane_id: pane, placement: Placement::DockBottom, size: None, view: view(vec![]) }) {
            IpcResponse::Error { message } => message,
            _ => String::new(),
        };
        assert_eq!(ui_shown, "use ui-update");
    }

    #[test]
    fn a_click_reaches_a_waiting_agent_once_and_locks_the_card() {
        let host = Fake::with_pane(1, 500, "s1");
        let mut b = board("wait");
        let (id, _) = show(&mut b, &host, 501, Placement::DockBottom, view(vec![("a", card("A"))])).unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        b.handle(&host, UiRequest { peer_pid: 501, from: Asker::default(), op: UiOp::Wait { view_id: id.clone(), timeout_ms: 1000 } }, tx);
        assert!(rx.try_recv().is_err(), "waits");
        assert!(matches!(call(&mut b, &host, 501, UiOp::Wait { view_id: id.clone(), timeout_ms: 1000 }), IpcResponse::Error { message } if message == "already waiting"));
        assert_eq!(b.click(&host, &id, "a", "edit", " ", &[], Trigger::Click), Err("edit needs text".into()));
        b.click(&host, &id, "a", "ok", "", &[], Trigger::Click).unwrap();
        match rx.try_recv() {
            Ok(IpcResponse::Ok { data: Some(d) }) => assert_eq!(d, json!({"view_id": id, "card_id": "a", "action": "ok", "text": "", "card_version": 1})),
            _ => panic!("handed to the waiter"),
        }
        assert!(host.view_in(1).unwrap().cards[0].clicked_at.is_some(), "locked");
        b.click(&host, &id, "a", "ok", "", &[], Trigger::Click).unwrap();
        assert_eq!(b.pending_events(&id), 1, "a locked card answers nothing: only the in-flight one");
        call(&mut b, &host, 501, UiOp::Ack { view_id: id.clone(), ok: true });
        assert_eq!(b.pending_events(&id), 0);
        // The agent updates the card: lock released, version bumped.
        call(&mut b, &host, 501, UiOp::UpdateCard { view_id: id.clone(), card_id: "a".into(), card: Some(card("A")) });
        let slot = host.view_in(1).unwrap().cards[0].clone();
        assert_eq!((slot.version, slot.clicked_at), (2, None));
    }

    #[test]
    fn without_ack_the_event_comes_back_and_a_half_typed_line_holds_it() {
        let host = Fake::with_pane(1, 500, "s1");
        let mut b = board("ack");
        let (id, _) = show(&mut b, &host, 501, Placement::DockBottom, view(vec![("a", card("A"))])).unwrap();
        let (tx, _rx) = std::sync::mpsc::channel();
        b.handle(&host, UiRequest { peer_pid: 501, from: Asker::default(), op: UiOp::Wait { view_id: id.clone(), timeout_ms: 1000 } }, tx);
        host.type_in(1, 3);
        b.click(&host, &id, "a", "ok", "", &[], Trigger::Click).unwrap();
        call(&mut b, &host, 501, UiOp::Ack { view_id: id.clone(), ok: false });
        assert_eq!(b.pending_events(&id), 1, "back in the queue");
        // No waiter now, a wait just returned and the user has a half-typed line: held.
        b.pump(&host);
        assert_eq!(b.pending_events(&id), 1);
    }

    #[test]
    fn a_grant_click_writes_the_pane_form_id_and_return_never_grants() {
        let host = Fake::with_pane(1, 500, "s1");
        let mut b = board("grant");
        let (id, _) = show(&mut b, &host, 501, Placement::DockBottom, view(vec![("d", grant_card("hi\n"))])).unwrap();
        b.click(&host, &id, "d", "post", "", &[], Trigger::Return).unwrap();
        let dir = crate::decision_card::grants_dir(&b.root);
        assert!(!dir.exists(), "plain Return");
        b.click(&host, &id, "d", "post", "", &[], Trigger::Click).unwrap();
        let file = dir.join(format!("{}_d.json", id));
        let grant: crate::decision_card::Grant = serde_json::from_slice(&std::fs::read(&file).unwrap()).unwrap();
        assert_eq!((grant.question_id, grant.session_id, grant.card_version), (format!("{}:d", id), "s1".to_string(), 1));
        // Replacing the card deletes its unused grant.
        call(&mut b, &host, 501, UiOp::UpdateCard { view_id: id.clone(), card_id: "d".into(), card: Some(card("Posted ✓")) });
        assert!(!file.exists());
        let _ = std::fs::remove_dir_all(&b.root);
    }

    #[test]
    fn an_edited_code_block_is_what_the_click_reports_and_the_grant_authorizes() {
        let host = Fake::with_pane(1, 500, "s1");
        let mut b = board("edited");
        let (id, _) = show(&mut b, &host, 501, Placement::DockBottom, view(vec![("d", grant_card("Draft"))])).unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        b.handle(&host, UiRequest { peer_pid: 501, from: Asker::default(), op: UiOp::Wait { view_id: id.clone(), timeout_ms: 1000 } }, tx);
        b.click(&host, &id, "d", "post", "", &[(0, "My draft".into())], Trigger::Click).unwrap();
        let event = match rx.try_recv() {
            Ok(IpcResponse::Ok { data: Some(d) }) => d,
            other => panic!("{:?}", other.map(|r| r.to_json())),
        };
        let block = &event["blocks"][0];
        assert_eq!((block["block"].as_u64(), block["edited"].as_bool()), (Some(0), Some(true)));
        let file = block["file"].as_str().unwrap().to_string();
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "My draft");
        assert_eq!(block["sha256"].as_str().unwrap(), crate::decision_card::sha256_hex(b"My draft"));
        let grant_file = crate::decision_card::grants_dir(&b.root).join(format!("{}_d.json", id));
        let grant: crate::decision_card::Grant = serde_json::from_slice(&std::fs::read(&grant_file).unwrap()).unwrap();
        assert_eq!(grant.payload_sha256, crate::decision_card::sha256_hex(b"My draft"), "the gate checks the file Kova wrote");
        // The line typed when nobody waits names the same file.
        let parsed: UiEvent = serde_json::from_value(event.clone()).unwrap();
        let line = pane_ui::typed_line(&parsed, "Drafts", "Post?", std::path::Path::new("/x"));
        assert!(line.starts_with(&format!("[Kova] UI \"Drafts\", card \"Post?\" (v1): post [block 0 edited by the user: {} sha256 ", file)), "{}", line);
        let _ = std::fs::remove_dir_all(&b.root);
    }

    #[test]
    fn an_unedited_click_reports_every_code_block_unchanged() {
        let host = Fake::with_pane(1, 500, "s1");
        let mut b = board("unedited");
        let (id, _) = show(&mut b, &host, 501, Placement::DockBottom, view(vec![("d", grant_card("hi\n"))])).unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        b.handle(&host, UiRequest { peer_pid: 501, from: Asker::default(), op: UiOp::Wait { view_id: id.clone(), timeout_ms: 1000 } }, tx);
        b.click(&host, &id, "d", "later", "", &[], Trigger::Click).unwrap();
        let Ok(IpcResponse::Ok { data: Some(event) }) = rx.try_recv() else { panic!("no event") };
        assert_eq!(event["blocks"][0]["edited"], false);
        assert_eq!(std::fs::read_to_string(event["blocks"][0]["file"].as_str().unwrap()).unwrap(), "hi\n");
        assert!(!crate::decision_card::grants_dir(&b.root).exists(), "no grant for a plain action");
        let _ = std::fs::remove_dir_all(&b.root);
    }

    #[test]
    fn a_remote_client_lists_views_and_clicks_but_never_grants() {
        let host = Fake::with_pane(1, 500, "s1");
        let mut b = board("remote");
        let (id, _) = show(&mut b, &host, 501, Placement::DockBottom, view(vec![("a", card("A")), ("d", grant_card("hi\n"))])).unwrap();
        let list = b.list(&host);
        let v = &list["views"][0];
        assert_eq!((v["view_id"].as_str(), v["session_id"].as_str(), v["owner_pane_id"].as_u64()), (Some(id.as_str()), Some("s1"), Some(1)));
        assert_eq!(v["cards"][1]["card"]["actions"][0]["grant"]["target"], "slack:C1/-");
        assert_eq!(b.answer_remote(&host, &id, "d", "post", "", Some(1)), Err("a grant action can only be clicked".into()));
        assert!(!crate::decision_card::grants_dir(&b.root).exists());
        assert_eq!(b.answer_remote(&host, &id, "a", "ok", "", Some(7)), Err("stale card: version 1 is shown now".into()));
        assert_eq!(b.answer_remote(&host, &id, "a", "edit", "", None), Err("edit needs text".into()));
        b.answer_remote(&host, &id, "a", "edit", "shorter", Some(1)).unwrap();
        // Nobody waits: the event went straight to the conversation (not typed under test).
        assert_eq!(b.pending_events(&id), 0);
        assert_eq!(b.list(&host)["views"][0]["cards"][0]["locked"], true);
        assert_eq!(b.answer_remote(&host, &id, "a", "ok", "", None), Err("card locked: its last click is not handled yet".into()));
        let _ = std::fs::remove_dir_all(&b.root);
    }

    #[test]
    fn a_ui_pane_whose_opener_is_gone_has_no_owner() {
        let host = Fake::with_pane(1, 500, "s1");
        let mut b = board("orphan");
        let (id, _) = show(&mut b, &host, 501, Placement::PaneRight, view(vec![])).unwrap();
        host.panes.borrow_mut().remove(&1);
        assert!(matches!(call(&mut b, &host, 501, UiOp::Close { view_id: id }), IpcResponse::Error { message } if message == "not the owner of this view"));
    }
}
