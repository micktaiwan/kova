//! Kova's terminal in another app (docs/embedding.md, step 3): the panes a
//! host other than Kova.app creates, the view the library gives each of them,
//! and the IPC socket the host can serve over them. `crate::capi` is the C
//! face of this module; Kova.app never goes through it.
//!
//! A library pane is a `Surface` (the same terminal, PTY and agent tracking as
//! a Kova.app pane) and, when the host gives it a parent `NSView`, a
//! `KovaPaneView`: the shared input component (`AppKitInput`) plus a layer the
//! pane is drawn into alone (`renderer::layer`). The host owns the clock: it
//! calls `tick` and `draw` from its own timer, and nothing here runs between
//! two of its calls.
//!
//! Everything lives on the thread that created it — the main thread for any
//! pane with a view: AppKit objects and `Surface` are not `Send`. The pane
//! table and the socket state are therefore per thread.

use std::cell::{Cell, OnceCell, RefCell};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{define_class, msg_send, DefinedClass, MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{
    NSAutoresizingMaskOptions, NSCursor, NSEvent, NSEventModifierFlags, NSPasteboard, NSRunningApplication,
    NSTextInputClient, NSTrackingArea, NSTrackingAreaOptions, NSView, NSWorkspace,
};
use objc2_core_foundation::{CGRect, CGSize};
use objc2_foundation::{NSObjectProtocol, NSString, NSURL};
use objc2_quartz_core::CAMetalLayer;

use crate::appkit_input::{self, copy_to_pasteboard, AppKitInput, PaneOrigin, ScrollAxisLock};
use crate::config::Config;
use crate::input::{self, KeyEvent, KeyResult, MouseAction, MouseEvent, MouseResult, ScrollEvent};
use crate::ipc::host::{self as ipc_host, Disposition, IpcCloseTabResult, IpcHost, PaneSignal, PendingWait, TabInfo};
use crate::ipc::{IpcCommand, IpcRequest, IpcResponse};
use crate::keybindings::Keybindings;
use crate::events::{EventSink, EventSource, EventState, FocusKey, IpcSink, PaneFlags};
use crate::renderer::layer::{self, LayerRenderer, PaneExtras};
use crate::renderer::{PaneRenderData, PaneViewport};
use crate::resize_repair::{self, FrameClock, ResizeRepair};
use crate::surface::{pane_attention, PaneId, Surface};
use crate::terminal::pty::SpawnEnv;
use crate::terminal::DumpMode;
use crate::window::overlays::{self as filter, FilterKey, FilterKeyOutcome, FilterState};

/// What `tick` reports, as bits. A host polls these instead of being called
/// back: every one of them is decided on the thread that owns the pane.
pub const EVENT_TITLE_CHANGED: u32 = 1 << 0;
/// The shell (and whatever ran in it) has exited. Reported once.
pub const EVENT_EXITED: u32 = 1 << 1;
/// The "waiting for the user" flag went up or down.
pub const EVENT_AWAITING_CHANGED: u32 = 1 << 2;
/// An IPC client asked to close this pane (`close-pane`, `close-tab`).
pub const EVENT_CLOSE_REQUESTED: u32 = 1 << 3;
/// An IPC client asked to bring this pane to the front (`focus-pane`).
pub const EVENT_FOCUS_REQUESTED: u32 = 1 << 4;
/// The pane wants a frame: `draw` would present one.
pub const EVENT_NEEDS_DRAW: u32 = 1 << 5;

/// How often `tick` re-probes the foreground process, the agent session and
/// the waiting flag — Kova.app's throttle for the same probe.
const PROBE_INTERVAL: Duration = Duration::from_millis(500);
/// How often `tick` re-resolves the git branch shown in the status bar
/// (Kova.app: every `fps * 2` ticks, about two seconds).
const GIT_INTERVAL: Duration = Duration::from_secs(2);

/// The config every library pane of this thread is made with, read once.
struct Host {
    config: Config,
    keybindings: Keybindings,
}

/// What a host registers to post the notifications the socket's `notify`
/// asks for: `(pane, title, message, sound)`, called on this thread from
/// `ipc_poll`, after the request is answered.
pub type NotifyHandler = Rc<dyn Fn(Option<PaneId>, &str, &str, bool)>;

/// A `notify` received, waiting for `ipc_poll` to hand it to the handler.
struct PendingNotify {
    pane: Option<PaneId>,
    title: String,
    message: String,
    sound: bool,
}

thread_local! {
    static HOST: OnceCell<Rc<Host>> = const { OnceCell::new() };
    static PANES: RefCell<Vec<Rc<LibPane>>> = const { RefCell::new(Vec::new()) };
    static IPC: RefCell<Option<IpcState>> = const { RefCell::new(None) };
    static NOTIFY_HANDLER: RefCell<Option<NotifyHandler>> = const { RefCell::new(None) };
    static PENDING_NOTIFY: RefCell<Vec<PendingNotify>> = const { RefCell::new(Vec::new()) };
}

/// Register (or, with `None`, remove) the handler the socket's `notify`
/// requests go to on this thread. Without one, `notify` answers an error.
pub fn set_notify_handler(handler: Option<NotifyHandler>) {
    NOTIFY_HANDLER.with(|h| *h.borrow_mut() = handler);
}

fn has_notify_handler() -> bool {
    NOTIFY_HANDLER.with(|h| h.borrow().is_some())
}

/// Hand every `notify` received to the handler, outside any borrow of the
/// socket or pane state: the handler may call back into the library. Returns
/// how many were handed over.
fn deliver_notifications() -> usize {
    let pending = PENDING_NOTIFY.with(|q| std::mem::take(&mut *q.borrow_mut()));
    if pending.is_empty() {
        return 0;
    }
    // Cloned out: the handler may replace itself.
    let Some(handler) = NOTIFY_HANDLER.with(|h| h.borrow().clone()) else {
        log::warn!("Library: {} notification(s) dropped, the handler was removed", pending.len());
        return 0;
    };
    for n in &pending {
        handler(n.pane, &n.title, &n.message, n.sound);
    }
    pending.len()
}

fn host() -> Rc<Host> {
    HOST.with(|h| {
        h.get_or_init(|| {
            // Kova's own config file (`~/.config/kova/config.toml`): a pane in
            // another app looks and types like a Kova pane. Tests run on the
            // defaults, whatever the machine's config says.
            #[cfg(not(test))]
            let config = Config::load();
            #[cfg(test)]
            let config = Config::default();
            let keybindings = Keybindings::from_config(&config.keys);
            Rc::new(Host { config, keybindings })
        })
        .clone()
    })
}

/// The pane with this id, if this thread holds it. The table is never kept
/// borrowed while a pane runs code, so AppKit re-entering a view (text input
/// calling back into it) finds it again.
pub fn find(id: PaneId) -> Option<Rc<LibPane>> {
    PANES.with(|p| p.borrow().iter().find(|pane| pane.surface.id == id).cloned())
}

/// Every pane of this thread, in creation order.
fn all() -> Vec<Rc<LibPane>> {
    PANES.with(|p| p.borrow().clone())
}

/// How many panes this thread holds.
#[cfg(test)]
pub fn count() -> usize {
    PANES.with(|p| p.borrow().len())
}

/// What a host asks for when it creates a pane.
#[derive(Default)]
pub struct PaneOptions {
    /// Working directory of the shell; `$HOME` when `None`.
    pub cwd: Option<String>,
    /// A line typed into the shell once it is ready, then run (Enter). The
    /// shell stays when it exits, as in a Kova.app pane.
    pub command: Option<String>,
    /// Extra environment variables. The five Kova sets itself win over these:
    /// `TERM`, `TERM_PROGRAM`, `KOVA_SHELL_INTEGRATION`, `KOVA_SOCKET` and
    /// `KOVA_PANE_ID`. Any other variable, `KOVA_*` or not, passes as given.
    pub env: Vec<(String, String)>,
    /// Initial grid of a pane without a view; 0 = the config's default. A pane
    /// with a view takes the size of its view.
    pub cols: u16,
    pub rows: u16,
    /// The shell to run; `None` = `$SHELL`. Not in the C API: tests name
    /// `/bin/sh` so the user's shell setup plays no part.
    pub shell: Option<String>,
}

/// A terminal pane of a library host.
pub struct LibPane {
    pub surface: Surface,
    /// Composition, drag auto-scroll and scroll axis lock — the same component
    /// Kova.app's window uses.
    input: AppKitInput,
    view: RefCell<Option<Retained<KovaPaneView>>>,
    layer: RefCell<Option<Retained<CAMetalLayer>>>,
    renderer: RefCell<Option<LayerRenderer>>,
    /// Events raised between two ticks (by IPC commands), handed out by `tick`.
    pending_events: Cell<u32>,
    last_title: RefCell<String>,
    last_awaiting: Cell<bool>,
    exit_reported: Cell<bool>,
    last_probe: Cell<Option<Instant>>,
    last_git_poll: Cell<Option<Instant>>,
    /// `(unread completion, bell)` as of the last tick (`pane_attention`).
    attention: Cell<(bool, bool)>,
    first_paint_logged: Cell<bool>,
    /// The repairs Kova.app runs after a resize (round-trip nudge, settle
    /// repaint, deferred winsize restore, hole check), for this pane alone.
    repair: RefCell<ResizeRepair>,
    /// Frames of the config's fps elapsed between two ticks: the host's
    /// clock is not Kova's, the repair countdowns are counted in frames.
    repair_clock: RefCell<FrameClock>,
    /// Whether the view is the first responder, as last reported to the
    /// program (DEC mode 1004). Also what the socket's `focus` events follow.
    has_focus: Cell<bool>,
    /// The URL under the pointer while Cmd is held: its segments and text
    /// (Kova.app's Cmd+hover, `input::url_hover_update`).
    hovered_url: RefCell<Option<(Vec<(usize, u16, u16)>, String)>>,
    /// The pointer position and content the hover was computed against:
    /// `tick` re-runs the hover when output or a scroll moved the text under
    /// a still pointer (`refresh_stale_hover`).
    hover_tracker: RefCell<input::HoverTracker>,
    /// The Ctrl+F filter, while it is open: Kova.app's in-pane filter
    /// (`window::overlays`), drawn over this pane alone.
    filter: RefCell<Option<FilterState>>,
}

/// Where new panes point their `KOVA_SOCKET`: the socket this thread serves,
/// or the process's default path (`/tmp/kova-<pid>.sock`) when it serves none.
fn socket_for_new_panes() -> PathBuf {
    IPC.with(|ipc| ipc.borrow().as_ref().map(|s| s.path.clone())).unwrap_or_else(crate::ipc::socket_path)
}

/// Create a pane: spawn its shell, and give it a view inside `parent` when
/// there is one (main thread only). Returns its id, which is also the
/// `KOVA_PANE_ID` its shell sees.
pub fn create(options: PaneOptions, parent: Option<&NSView>) -> Result<PaneId, String> {
    let mtm = match parent {
        Some(_) => Some(MainThreadMarker::new().ok_or("a pane with a view must be created on the main thread")?),
        None => None,
    };
    let host = host();
    let cols = if options.cols > 0 { options.cols } else { host.config.terminal.columns };
    let rows = if options.rows > 0 { options.rows } else { host.config.terminal.rows };
    let env = SpawnEnv { socket: socket_for_new_panes(), vars: options.env, shell: options.shell };
    let surface = Surface::spawn_with(cols, rows, &host.config, options.cwd.as_deref(), &env)
        .map_err(|e| format!("spawn failed: {e}"))?;
    if let Some(command) = options.command.filter(|c| !c.is_empty()) {
        surface.pending_command.set(Some(command));
        surface.run_pending_command();
    }
    let pane = Rc::new(LibPane::new(surface));
    let id = pane.surface.id;
    PANES.with(|p| p.borrow_mut().push(pane.clone()));
    if let (Some(parent), Some(mtm)) = (parent, mtm) {
        pane.attach(mtm, parent);
    }
    log::info!("Library pane {} created", id);
    Ok(id)
}

/// Test-only: a pane without a shell whose PTY input is readable on the
/// returned fd (`Surface::capture`), registered like any other.
#[cfg(test)]
pub fn create_capture(cols: u16, rows: u16) -> (PaneId, std::os::fd::OwnedFd) {
    let (surface, reader) = Surface::capture(cols, rows);
    let pane = Rc::new(LibPane::new(surface));
    let id = pane.surface.id;
    PANES.with(|p| p.borrow_mut().push(pane));
    (id, reader)
}

/// Close a pane: its view leaves its parent, its shell is hung up and reaped
/// (`Pty`'s drop, on a reaper thread: the shell and its foreground job may
/// still run when this returns — `shutdown` waits for them). False when this
/// thread holds no such pane.
pub fn close(id: PaneId) -> bool {
    let pane = PANES.with(|p| {
        let mut panes = p.borrow_mut();
        let idx = panes.iter().position(|pane| pane.surface.id == id)?;
        Some(panes.remove(idx))
    });
    let Some(pane) = pane else { return false };
    if let Some(view) = pane.view.borrow_mut().take() {
        view.removeFromSuperview();
    }
    pane.layer.borrow_mut().take();
    pane.renderer.borrow_mut().take();
    log::info!("Library pane {} closed", id);
    true
}

/// What a host calls when it quits: close every pane of this thread, stop the
/// socket, then wait until the shell and foreground job of every pane closed
/// so far — before this call too — are gone (`pty::wait_for_reapers`). The
/// equivalent of Kova.app's `pty::shutdown_all` on `applicationWillTerminate`,
/// for the panes of a library host. Returns how many panes were open.
pub fn shutdown() -> usize {
    let ids: Vec<PaneId> = all().iter().map(|p| p.surface.id).collect();
    for &id in &ids {
        close(id);
    }
    ipc_stop();
    let waited = crate::terminal::pty::wait_for_reapers();
    log::info!("Library shutdown: {} pane(s) closed, waited for {} reaper(s)", ids.len(), waited);
    ids.len()
}

impl LibPane {
    fn new(surface: Surface) -> Self {
        LibPane {
            surface,
            input: AppKitInput::new(),
            view: RefCell::new(None),
            layer: RefCell::new(None),
            renderer: RefCell::new(None),
            pending_events: Cell::new(0),
            last_title: RefCell::new(String::new()),
            last_awaiting: Cell::new(false),
            exit_reported: Cell::new(false),
            last_probe: Cell::new(None),
            last_git_poll: Cell::new(None),
            attention: Cell::new((false, false)),
            first_paint_logged: Cell::new(false),
            repair: RefCell::new(ResizeRepair::default()),
            repair_clock: RefCell::new(FrameClock::new(Instant::now())),
            has_focus: Cell::new(false),
            hovered_url: RefCell::new(None),
            hover_tracker: RefCell::new(input::HoverTracker::default()),
            filter: RefCell::new(None),
        }
    }

    fn raise(&self, event: u32) {
        self.pending_events.set(self.pending_events.get() | event);
    }

    /// The view of this pane, if it has one.
    pub fn view(&self) -> Option<Retained<KovaPaneView>> {
        self.view.borrow().clone()
    }

    /// Give the pane its view, filling `parent` (resized with it) until the
    /// host sets another frame.
    fn attach(&self, mtm: MainThreadMarker, parent: &NSView) {
        let view = KovaPaneView::new(mtm, parent.bounds(), self.surface.id);
        view.setAutoresizingMask(
            NSAutoresizingMaskOptions::ViewWidthSizable | NSAutoresizingMaskOptions::ViewHeightSizable,
        );
        let layer = CAMetalLayer::new();
        layer.setPixelFormat(layer::PIXEL_FORMAT);
        layer.setFramebufferOnly(true);
        // Same order as Kova.app's window (`setup_metal`).
        view.setWantsLayer(true);
        view.setLayer(Some(&layer));
        *self.layer.borrow_mut() = Some(layer);
        *self.view.borrow_mut() = Some(view.clone());
        parent.addSubview(&view);
        self.relayout();
    }

    /// Bring the layer and the grid in line with the view: its scale (a new
    /// context when the display scale changed), its drawable size, and the
    /// columns and rows that fit. Called on every frame or backing change.
    fn relayout(&self) {
        let (Some(view), Some(layer)) = (self.view.borrow().clone(), self.layer.borrow().clone()) else { return };
        let scale = view.window().map_or(2.0, |w| w.backingScaleFactor());
        let grid = {
            let mut slot = self.renderer.borrow_mut();
            if slot.as_ref().is_none_or(|r| r.scale() != scale) {
                *slot = LayerRenderer::new(scale, &host().config);
            }
            let Some(renderer) = slot.as_ref() else {
                log::error!("Library pane {}: no Metal device, nothing will be drawn", self.surface.id);
                return;
            };
            layer.setDevice(Some(&renderer.device()));
            layer.setContentsScale(scale);
            let bounds = view.bounds();
            let (w, h) = (bounds.size.width * scale, bounds.size.height * scale);
            // A collapsed view keeps its last drawable: Metal refuses a 0×0 one.
            if w >= 1.0 && h >= 1.0 {
                layer.setDrawableSize(CGSize { width: w, height: h });
            }
            renderer.grid_for(w as f32, h as f32)
        };
        self.resize(grid.0, grid.1);
    }

    /// Resize the terminal grid and tell the PTY, then arm Kova.app's
    /// post-resize repairs (`resize_repair`), which `tick` runs. A no-op at
    /// the same size.
    pub fn resize(&self, cols: u16, rows: u16) {
        let (cols, rows) = (cols.max(1), rows.max(1));
        let mut term = self.surface.terminal.write();
        if term.cols != cols || term.rows != rows {
            let old = (term.cols, term.rows);
            term.resize(cols, rows);
            term.dirty.store(true, Ordering::Relaxed);
            drop(term);
            self.surface.pty.resize(cols, rows);
            let now = Instant::now();
            let mut repair = self.repair.borrow_mut();
            repair.prune_history(now);
            let settle = resize_repair::settle_frames(host().config.terminal.fps as u32);
            repair.on_resized(&self.surface, old, (cols, rows), now, settle);
        }
    }

    /// The frames elapsed since the last tick, at the config's fps, run
    /// through the resize repairs.
    fn tick_resize_repairs(&self, now: Instant) {
        let advance = self.repair_clock.borrow_mut().advance(now, host().config.terminal.fps as u32);
        let id = self.surface.id;
        let mut lookup = |pane_id: PaneId, f: &mut dyn FnMut(&Surface, bool)| {
            if pane_id == id {
                f(&self.surface, false);
            }
        };
        self.repair.borrow_mut().tick(advance, &mut lookup);
    }

    /// The view became (`true`) or stopped being (`false`) the first
    /// responder: redraw (the cursor and the dimming follow focus), and tell
    /// the program when it asked for focus events (DEC mode 1004) and the
    /// focus really changed.
    fn set_focus(&self, focused: bool) {
        let was = self.has_focus.replace(focused);
        self.surface.report_focus(was, focused);
        self.surface.terminal.read().dirty.store(true, Ordering::Relaxed);
    }

    /// Test-only: the pending resize repairs of this pane.
    #[cfg(test)]
    pub fn pending_repairs(&self) -> (Option<u32>, Option<u32>, Option<u32>) {
        self.repair.borrow().pending(self.surface.id)
    }

    /// Test-only: drive the focus as the view's responder methods do.
    #[cfg(test)]
    pub fn set_focus_for_test(&self, focused: bool) {
        self.set_focus(focused);
    }

    /// Set the view's frame in its parent's coordinates (points). False
    /// without a view.
    pub fn set_frame(&self, frame: CGRect) -> bool {
        let Some(view) = self.view() else { return false };
        // From now on the host places the view: it no longer follows its
        // parent's size on its own.
        view.setAutoresizingMask(NSAutoresizingMaskOptions::empty());
        // `setFrame:` relays out through `setFrameSize:` when the size
        // changes; a move alone does not need it.
        view.setFrame(frame);
        true
    }

    /// The pane is under the eye: its view is the first responder of the key
    /// window. Always false without a view.
    pub fn is_focused(&self) -> bool {
        let Some(view) = self.view() else { return false };
        let Some(window) = view.window() else { return false };
        if !window.isKeyWindow() {
            return false;
        }
        window.firstResponder().is_some_and(|r| {
            std::ptr::eq(Retained::as_ptr(&r) as *const (), Retained::as_ptr(&view) as *const ())
        })
    }

    /// The pane holds the keyboard for the socket's `focus` events: its view
    /// is the first responder (`set_focus`) of its window, and that window is
    /// key. A pane without view only holds it when a test says so.
    fn holds_focus(&self) -> bool {
        if !self.has_focus.get() {
            return false;
        }
        match self.view() {
            Some(view) => view.window().is_some_and(|w| w.isKeyWindow()),
            None => true,
        }
    }

    /// Display title: agent session name, OSC title, foreground process, cwd.
    pub fn title(&self) -> String {
        self.surface.display_title(None, "shell")
    }

    /// The per-frame work Kova.app's tick does for a pane, then what changed
    /// since the last call (`EVENT_*` bits).
    pub fn tick(&self) -> u32 {
        let s = &self.surface;
        let mut events = self.pending_events.replace(0);
        s.inject_pending_command();
        let speed = self.input.autoscroll_speed();
        if speed != 0 {
            s.autoscroll_selection(speed);
        }
        let now = Instant::now();
        self.tick_resize_repairs(now);
        let due = |slot: &Cell<Option<Instant>>, every: Duration| {
            let due = slot.get().is_none_or(|t| now.duration_since(t) >= every);
            if due {
                slot.set(Some(now));
            }
            due
        };
        s.check_running(due(&self.last_probe, PROBE_INTERVAL));
        if due(&self.last_git_poll, GIT_INTERVAL) {
            s.refresh_git_branch();
        }
        let focused = self.is_focused();
        if focused {
            s.mark_awaiting_seen();
        }
        self.attention.set(pane_attention(&s.terminal.read(), focused));
        // The open filter follows the output, at most every 150 ms, as in
        // Kova.app (`FilterState::refresh`).
        if let Some(state) = self.filter.borrow_mut().as_mut() {
            state.refresh(&s.terminal.read(), now);
        }
        // The band a filter jump left on its line is cleared once it has run
        // its course.
        let showing = s.terminal.read().highlight_line.is_some();
        if showing {
            s.terminal.write().tick_highlight_line();
        }
        // Output or a scroll may have slid other text under a still pointer.
        self.refresh_stale_hover();

        let title = self.title();
        if *self.last_title.borrow() != title {
            *self.last_title.borrow_mut() = title;
            events |= EVENT_TITLE_CHANGED;
        }
        let awaiting = s.is_awaiting();
        if awaiting != self.last_awaiting.replace(awaiting) {
            events |= EVENT_AWAITING_CHANGED;
        }
        if !s.is_alive() && !self.exit_reported.replace(true) {
            events |= EVENT_EXITED;
        }
        if self.needs_draw() {
            events |= EVENT_NEEDS_DRAW;
        }
        events
    }

    fn render_data(&self) -> PaneRenderData {
        let s = &self.surface;
        let (has_completion, has_bell) = self.attention.get();
        PaneRenderData {
            terminal: s.terminal.clone(),
            // The layer renderer fills it in from the drawable.
            viewport: PaneViewport { x: 0.0, y: 0.0, width: 0.0, height: 0.0 },
            shell_ready: s.is_ready(),
            is_focused: self.is_focused(),
            // The host marks which of its views has the keyboard: the
            // library cannot know whether this pane is the only one shown.
            outlined: false,
            pane_id: s.id,
            custom_title: None,
            has_completion,
            has_bell,
            minimized: false,
            input_chars: s.pty.input_chars.clone(),
            fg_process: s.fg_process().map(|p| p.name),
            bookmarked: false,
        }
    }

    /// Whether `draw` would present a frame. Always false without a view.
    pub fn needs_draw(&self) -> bool {
        let renderer = self.renderer.borrow();
        let Some(renderer) = renderer.as_ref() else { return false };
        let hover = self.hovered_url.borrow();
        let filter_open = self.filter.borrow().is_some();
        // The filter draws on every frame: no need to build its data to know.
        let filter_data = filter_open.then(|| crate::renderer::FilterRenderData {
            query: String::new(),
            matches: Vec::new(),
            selected: 0,
            hint: None,
        });
        let extras = PaneExtras {
            hovered_url: hover.as_ref().map(|(segments, url)| (segments.as_slice(), url.as_str())),
            filter: filter_data.as_ref(),
        };
        renderer.wants_frame(&self.render_data(), &extras)
    }

    /// Draw the pane when it needs it (or always, with `force`). Returns
    /// whether a frame was presented.
    pub fn draw(&self, force: bool) -> bool {
        let Some(layer) = self.layer.borrow().clone() else { return false };
        if self.view().is_some_and(|v| v.bounds().size.width < 1.0 || v.bounds().size.height < 1.0) {
            return false; // collapsed: nothing to see
        }
        let data = self.render_data();
        let hover = self.hovered_url.borrow();
        let filter_data = self.filter.borrow().as_ref().map(FilterState::render_data);
        let extras = PaneExtras {
            hovered_url: hover.as_ref().map(|(segments, url)| (segments.as_slice(), url.as_str())),
            filter: filter_data.as_ref(),
        };
        let mut renderer = self.renderer.borrow_mut();
        let Some(renderer) = renderer.as_mut() else { return false };
        let drawn = renderer.draw(&layer, &data, &extras, force);
        if drawn && !self.first_paint_logged.replace(true) {
            self.surface.open_timer.mark_first_paint(self.surface.id);
        }
        drawn
    }

    /// Cell size in pixels and the text origin's padding, from the renderer.
    fn geometry(&self) -> Option<(f64, (f32, f32), f32)> {
        let renderer = self.renderer.borrow();
        let r = renderer.as_ref()?;
        Some((r.scale(), r.cell_size(), r.h_padding()))
    }

    fn mark_dirty(&self) {
        self.surface.terminal.read().dirty.store(true, Ordering::Relaxed);
    }

    // --- The Ctrl+F filter (Kova.app's, `window::overlays`) ---

    /// Whether the filter is open.
    pub fn filter_open(&self) -> bool {
        self.filter.borrow().is_some()
    }

    /// Open the filter, or close it when it is open (Ctrl+F).
    fn toggle_filter(&self) {
        if self.filter_open() {
            self.close_filter();
            return;
        }
        *self.filter.borrow_mut() = Some(FilterState::new());
        self.mark_dirty();
    }

    /// Close the filter, its query joining the recall list (↑). The one exit
    /// point, as Kova.app's `close_filter`. Returns the closed state.
    fn close_filter(&self) -> Option<FilterState> {
        let state = self.filter.borrow_mut().take();
        if let Some(state) = state.as_ref() {
            state.record_query();
        }
        self.mark_dirty();
        state
    }

    /// Re-run the open filter's search now (its query changed).
    fn rerun_filter(&self) {
        if let Some(state) = self.filter.borrow_mut().as_mut() {
            state.rerun(&self.surface.terminal.read());
        }
        self.mark_dirty();
    }

    /// A key, before anything else sees it: while the filter is open every
    /// key goes to it (typing filters, ↑/↓ pick a match or recall a query,
    /// Enter jumps to the picked match, Escape or Ctrl+F closes); otherwise
    /// Ctrl+F opens it, and
    /// Kova.app's other window keys (Ctrl+Option+arrows, its virtual width)
    /// do nothing here. True when the key was taken: it must not reach the
    /// PTY. Shared by the view and `kova_pane_key`.
    pub fn filter_key(&self, key: &KeyEvent) -> bool {
        if !self.filter_open() {
            return match input::host_key(key) {
                // Also without a view: the filter then takes keys unseen until
                // Escape or Ctrl+F (documented on kova_pane_key in kova.h).
                Some(input::HostKey::ToggleFilter) => {
                    self.toggle_filter();
                    true
                }
                Some(input::HostKey::AdjustVirtualWidth(_)) => true,
                None => false,
            };
        }
        let text = filter::filter_typed_text(key);
        let fkey = FilterKey::from_key_event(key, &text);
        let outcome = match self.filter.borrow_mut().as_mut() {
            Some(state) => filter::filter_key(state, &fkey, || {
                crate::search_history::list(crate::search_history::Scope::Filter)
            }),
            None => return false,
        };
        match outcome {
            FilterKeyOutcome::Toggle | FilterKeyOutcome::Close => {
                self.close_filter();
            }
            FilterKeyOutcome::Jump => {
                if let Some(mut state) = self.close_filter() {
                    let picked = state.selected;
                    state.jump_to_pick(picked, &mut self.surface.terminal.write());
                }
            }
            FilterKeyOutcome::Rerun => self.rerun_filter(),
            FilterKeyOutcome::Navigate => self.mark_dirty(),
            FilterKeyOutcome::Ignored => {}
        }
        true
    }

    /// A paste while the filter is open goes into its query (first line).
    /// False when the filter is closed: the paste is the pane's.
    pub fn paste_into_filter(&self, text: &str) -> bool {
        let changed = match self.filter.borrow_mut().as_mut() {
            Some(state) => filter::paste_into_filter_state(state, text),
            None => return false,
        };
        if changed {
            self.rerun_filter();
        }
        true
    }

    /// Copy while the filter is open: the matched lines, then the filter
    /// closes (Kova.app's Cmd+C). `None` when it is closed or nothing matches.
    fn take_filter_copy(&self) -> Option<String> {
        let text = filter::filter_copy_text(self.filter.borrow().as_ref()?)?;
        self.close_filter();
        Some(text)
    }

    /// A click at `y` pixels from the top of the pane while the filter is
    /// open: on a listed line, close the filter and scroll to it; below the
    /// list, close it; on the query bar, nothing.
    fn filter_click(&self, py: f32) {
        let Some((scale, (_, cell_h), _)) = self.geometry() else {
            return;
        };
        let Some(row) = filter::filter_click_row(py, 0.0, cell_h) else {
            return;
        };
        // `py` and `cell_h` are drawable pixels, `bounds()` is in points: one
        // scale apart, and mixing the two halves the clickable window on a
        // Retina screen, so a click below its middle lands on the wrong row.
        let height_px = self
            .view()
            .map_or(0.0, |v| v.bounds().size.height as f32 * scale as f32);
        let Some(mut state) = self.close_filter() else {
            return;
        };
        // The rows are drawn from the scroll that keeps the pick in view, so a
        // click lands on that window's row, not on the list's.
        let max_visible = ((height_px / cell_h).floor() as usize).saturating_sub(1);
        if let Some(idx) =
            filter::filter_click_pick(state.selected, max_visible, row, state.matches.len())
        {
            state.jump_to_pick(idx, &mut self.surface.terminal.write());
        }
    }

    // --- Cmd+hover and Cmd+click on a URL (Kova.app's, `input::url_at_point`) ---

    /// The URL under `(px, py)` pixels from the view's top-left corner.
    fn url_at_pixel(&self, px: f32, py: f32) -> Option<(Vec<(usize, u16, u16)>, String)> {
        let (_, cell, pad) = self.geometry()?;
        input::url_at_point(&self.surface.terminal.read(), px - pad, py, cell)
    }

    /// Follow the pointer: with Cmd held, underline the URL under it and show
    /// the pointing hand; otherwise drop the hover. Returns the URL hovered
    /// after the update.
    fn update_hover(&self, cmd: bool, px: f32, py: f32) -> Option<String> {
        let current = self.hovered_url.borrow().clone();
        let current = current.as_ref().map(|(segments, url)| (segments.as_slice(), url.as_str()));
        match input::url_hover_update(cmd, || self.url_at_pixel(px, py), current) {
            input::HoverUpdate::Keep => {}
            input::HoverUpdate::Set(segments, url) => {
                *self.hovered_url.borrow_mut() = Some((segments, url));
                if MainThreadMarker::new().is_some() {
                    NSCursor::pointingHandCursor().set();
                }
                self.mark_dirty();
            }
            input::HoverUpdate::Clear => self.clear_hover(),
        }
        let url = self.hovered_url.borrow().as_ref().map(|(_, url)| url.clone());
        let stamp = if url.is_some() { self.hover_stamp() } else { None };
        self.hover_tracker.borrow_mut().seen((px, py), stamp);
        url
    }

    /// What the cells under the pointer depend on (`input::HoverStamp`): the
    /// content generation, the scroll, and the cell geometry.
    fn hover_stamp(&self) -> Option<input::HoverStamp> {
        let (_, (cell_w, cell_h), pad) = self.geometry()?;
        Some(input::HoverStamp::of(&self.surface.terminal.read(), [pad, cell_w, cell_h, 0.0]))
    }

    /// Once per tick: when output, a scroll or a resize moved the text under
    /// a still pointer, compute the hover again at the last pointer position
    /// (Kova.app's `refresh_stale_url_hover`). Free when nothing is hovered
    /// or nothing changed.
    fn refresh_stale_hover(&self) {
        let hovering = self.hovered_url.borrow().is_some();
        let point = self.hover_tracker.borrow_mut().recheck(hovering, |_| self.hover_stamp());
        if let Some((px, py)) = point {
            // A hover only exists while Cmd is held (`flagsChanged:` drops it).
            self.update_hover(true, px, py);
        }
    }

    /// Drop the hover (Cmd released): no underline, the arrow cursor back.
    fn clear_hover(&self) {
        if self.hovered_url.borrow_mut().take().is_some() {
            if MainThreadMarker::new().is_some() {
                NSCursor::arrowCursor().set();
            }
            self.mark_dirty();
        }
    }

    // --- Typed input, for hosts that do not use the library's view ---

    /// A key press (`KeyResult::Compose`: run it through the platform's text
    /// input, which ends in `text` or `send_key`).
    pub fn key(&self, key: &KeyEvent) -> KeyResult {
        self.surface.key(key, &host().keybindings)
    }

    /// A key the platform's text input did not turn into text.
    pub fn send_key(&self, key: &KeyEvent) {
        self.surface.send_key(key, &host().keybindings);
    }

    /// Text being composed (dead key, IME); empty ends the composition.
    pub fn preedit(&self, text: &str) {
        self.input.set_preedit(text);
    }

    /// The text being composed, if any.
    #[cfg(test)]
    pub fn has_preedit(&self) -> bool {
        self.input.has_marked_text()
    }

    /// A mouse event at `(x, y)` points from the view's top-left corner.
    /// `None` without a view (no cell size to map the point with).
    pub fn mouse(&self, action: MouseAction, button: u8, mods: input::Mods, x: f64, y: f64, click_count: isize) -> Option<MouseResult> {
        let (scale, cell, pad) = self.geometry()?;
        let ev = MouseEvent {
            action,
            button,
            mods,
            x: (x * scale) as f32 - pad,
            y: (y * scale) as f32,
            click_count,
        };
        let result = self.input.typed_mouse(&ev, &self.surface, cell);
        if action == MouseAction::Release {
            // Same end of gesture as the view's `mouseUp:`.
            self.input.end_drag();
            if result != MouseResult::Reported {
                self.input.finish_selection(&self.surface);
            }
        }
        Some(result)
    }

    /// A vertical scroll at `(x, y)` points from the view's top-left corner.
    /// False without a view.
    pub fn scroll(&self, dy: f64, precise: bool, mods: input::Mods, x: f64, y: f64) -> bool {
        let Some((scale, cell, pad)) = self.geometry() else { return false };
        let ev = ScrollEvent { dy, precise, mods, x: (x * scale) as f32 - pad, y: (y * scale) as f32 };
        self.surface.scroll(&ev, cell, host().config.terminal.scroll_sensitivity, 0);
        true
    }

    /// The selection as text (consecutive non-empty lines joined with a
    /// space), then cleared — what Kova.app's Cmd+C copies. `None` when
    /// nothing is selected.
    pub fn take_selection(&self) -> Option<String> {
        let text = self.surface.terminal.read().selected_text_joined();
        if text.is_empty() {
            return None;
        }
        self.surface.terminal.write().clear_selection();
        Some(text)
    }

    /// The text `get-pane-content` returns for this pane.
    pub fn text(&self, mode: DumpMode, trim: bool) -> String {
        self.surface.terminal.read().dump_text(mode, trim).text
    }
}

// ─── The view ────────────────────────────────────────────────────────────────

pub struct KovaPaneViewIvars {
    pane_id: Cell<PaneId>,
}

define_class!(
    /// The view a library pane draws into and takes its input from. Every
    /// method is a delegation to the pane's `Surface` through the shared input
    /// component, as in Kova.app's `KovaView`, minus everything that belongs
    /// to Kova.app's window (overlays, tabs, separators, key equivalents).
    /// Each body that runs Rust code goes through `shield`: AppKit is the
    /// caller, and a panic must not unwind into it.
    #[unsafe(super(objc2_app_kit::NSView))]
    #[thread_kind = MainThreadOnly]
    #[name = "KovaPaneView"]
    #[ivars = KovaPaneViewIvars]
    pub struct KovaPaneView;

    unsafe impl NSObjectProtocol for KovaPaneView {}

    unsafe impl NSTextInputClient for KovaPaneView {
        #[unsafe(method(insertText:replacementRange:))]
        unsafe fn insert_text_replacement_range(&self, string: &AnyObject, _replacement_range: objc2_foundation::NSRange) {
            shield("insertText:", || {
                if let Some(pane) = self.pane() {
                    unsafe { pane.input.insert_text(string, Some(&pane.surface)) }
                }
            });
        }

        #[unsafe(method(doCommandBySelector:))]
        unsafe fn do_command_by_selector(&self, _selector: objc2::runtime::Sel) {
            shield("doCommandBySelector:", || {
                if let Some(pane) = self.pane() {
                    let host = host();
                    pane.input.do_command_by_selector(|| Some(&pane.surface), Some(&host.keybindings));
                }
            });
        }

        #[unsafe(method(setMarkedText:selectedRange:replacementRange:))]
        unsafe fn set_marked_text_selected_range_replacement_range(
            &self,
            string: &AnyObject,
            _selected_range: objc2_foundation::NSRange,
            _replacement_range: objc2_foundation::NSRange,
        ) {
            shield("setMarkedText:", || {
                if let Some(pane) = self.pane() {
                    unsafe { pane.input.set_marked_text(string) }
                }
            });
        }

        #[unsafe(method(unmarkText))]
        fn unmark_text(&self) {
            shield("unmarkText", || {
                if let Some(pane) = self.pane() {
                    pane.input.unmark_text();
                }
            });
        }

        #[unsafe(method(hasMarkedText))]
        fn has_marked_text(&self) -> bool {
            shield("hasMarkedText", || self.pane().is_some_and(|p| p.input.has_marked_text())).unwrap_or(false)
        }

        #[unsafe(method(markedRange))]
        fn marked_range(&self) -> objc2_foundation::NSRange {
            shield("markedRange", || match self.pane() {
                Some(pane) => pane.input.marked_range(),
                None => AppKitInput::new().marked_range(),
            })
            .unwrap_or(objc2_foundation::NSRange::new(objc2_foundation::NSNotFound as usize, 0))
        }

        #[unsafe(method(selectedRange))]
        fn selected_range(&self) -> objc2_foundation::NSRange {
            shield("selectedRange", || AppKitInput::new().selected_range())
                .unwrap_or(objc2_foundation::NSRange::new(objc2_foundation::NSNotFound as usize, 0))
        }

        #[unsafe(method_id(attributedSubstringForProposedRange:actualRange:))]
        #[unsafe(method_family = none)]
        unsafe fn attributed_substring_for_proposed_range(
            &self,
            _range: objc2_foundation::NSRange,
            _actual_range: objc2_foundation::NSRangePointer,
        ) -> Option<Retained<objc2_foundation::NSAttributedString>> {
            None
        }

        #[unsafe(method_id(validAttributesForMarkedText))]
        #[unsafe(method_family = none)]
        fn valid_attributes_for_marked_text(&self) -> Retained<objc2_foundation::NSArray<objc2_foundation::NSAttributedStringKey>> {
            objc2_foundation::NSArray::new()
        }

        #[unsafe(method(firstRectForCharacterRange:actualRange:))]
        unsafe fn first_rect_for_character_range(
            &self,
            _range: objc2_foundation::NSRange,
            _actual_range: objc2_foundation::NSRangePointer,
        ) -> CGRect {
            shield("firstRectForCharacterRange:", || {
                let pane = self.pane()?;
                let (_, cell, pad) = pane.geometry()?;
                Some(pane.input.cursor_rect_on_screen(self, &pane.surface, cell, pad))
            })
            .flatten()
            .unwrap_or(CGRect::ZERO)
        }

        #[unsafe(method(characterIndexForPoint:))]
        fn character_index_for_point(&self, _point: objc2_core_foundation::CGPoint) -> usize {
            shield("characterIndexForPoint:", || AppKitInput::new().character_index_for_point())
                .unwrap_or(objc2_foundation::NSNotFound as usize)
        }
    }

    impl KovaPaneView {
        #[unsafe(method(acceptsFirstResponder))]
        fn accepts_first_responder(&self) -> bool {
            true
        }

        #[unsafe(method(becomeFirstResponder))]
        fn become_first_responder(&self) -> bool {
            let accepted: bool = unsafe { msg_send![super(self), becomeFirstResponder] };
            if accepted {
                shield("becomeFirstResponder", || self.set_focus(true));
            }
            accepted
        }

        #[unsafe(method(resignFirstResponder))]
        fn resign_first_responder(&self) -> bool {
            let accepted: bool = unsafe { msg_send![super(self), resignFirstResponder] };
            if accepted {
                shield("resignFirstResponder", || self.set_focus(false));
            }
            accepted
        }

        #[unsafe(method(keyDown:))]
        fn key_down(&self, event: &NSEvent) {
            shield("keyDown:", || {
                let Some(pane) = self.pane() else { return };
                let key = appkit_input::key_event(event);
                // The filter first: open, it takes every key; closed, Ctrl+F
                // opens it, and Kova.app's other window keys
                // (Ctrl+Option+arrows) do nothing. None of them reaches the PTY.
                if pane.filter_key(&key) {
                    return;
                }
                let host = host();
                pane.input.key_down(self, event, &key, &pane.surface, &host.keybindings);
            });
        }

        #[unsafe(method(setFrameSize:))]
        fn set_frame_size(&self, new_size: CGSize) {
            let _: () = unsafe { msg_send![super(self), setFrameSize: new_size] };
            shield("setFrameSize:", || {
                if let Some(pane) = self.pane() {
                    pane.relayout();
                }
            });
        }

        #[unsafe(method(viewDidChangeBackingProperties))]
        fn view_did_change_backing_properties(&self) {
            shield("viewDidChangeBackingProperties", || {
                if let Some(pane) = self.pane() {
                    pane.relayout();
                }
            });
        }

        #[unsafe(method(viewDidMoveToWindow))]
        fn view_did_move_to_window(&self) {
            shield("viewDidMoveToWindow", || {
                if let Some(pane) = self.pane() {
                    pane.relayout();
                }
            });
        }

        #[unsafe(method(scrollWheel:))]
        fn scroll_wheel(&self, event: &NSEvent) {
            shield("scrollWheel:", || {
                let Some(pane) = self.pane() else { return };
                let lock = pane.input.scroll_axis_lock(event);
                if lock == ScrollAxisLock::Horizontal {
                    return;
                }
                if let Some((_, cell, pad)) = pane.geometry() {
                    let sensitivity = host().config.terminal.scroll_sensitivity;
                    pane.input.scroll(self, event, &pane.surface, origin(pad), cell, sensitivity, 0);
                }
            });
        }

        #[unsafe(method(mouseDown:))]
        fn mouse_down(&self, event: &NSEvent) {
            if let Some(window) = self.window() {
                window.makeFirstResponder(Some(self));
            }
            shield("mouseDown:", || {
                let Some(pane) = self.pane() else { return };
                let (px, py) = appkit_input::event_to_pixel(self, event);
                // A click while the filter is open picks a line (Kova.app's
                // `handle_filter_click`); the pane under it sees nothing.
                if pane.filter_open() {
                    pane.filter_click(py);
                    return;
                }
                // Cmd+click opens the URL under the pointer. Re-validated at
                // the click: output may have moved the text since the last
                // mouse move.
                let cmd = event.modifierFlags().contains(NSEventModifierFlags::Command);
                if cmd {
                    if let Some(url) = pane.update_hover(true, px, py) {
                        open_url(&url);
                        return;
                    }
                }
                self.mouse(event, MouseAction::Press);
            });
        }

        #[unsafe(method(mouseDragged:))]
        fn mouse_dragged(&self, event: &NSEvent) {
            shield("mouseDragged:", || {
                if self.pane().is_some_and(|p| p.filter_open()) {
                    return;
                }
                self.mouse(event, MouseAction::Drag);
            });
        }

        #[unsafe(method(mouseUp:))]
        fn mouse_up(&self, event: &NSEvent) {
            shield("mouseUp:", || {
                let Some(pane) = self.pane() else { return };
                if pane.filter_open() {
                    return;
                }
                pane.input.end_drag();
                if self.mouse(event, MouseAction::Release) == Some(MouseResult::Reported) {
                    return;
                }
                pane.input.finish_selection(&pane.surface);
            });
        }

        #[unsafe(method(mouseMoved:))]
        fn mouse_moved(&self, event: &NSEvent) {
            shield("mouseMoved:", || {
                let Some(pane) = self.pane() else { return };
                if pane.filter_open() {
                    return;
                }
                // Reported to the app if it tracks all motion (mode 1003);
                // otherwise Cmd+hover underlines a URL, as in Kova.app.
                if self.mouse(event, MouseAction::Move) == Some(MouseResult::Reported) {
                    return;
                }
                let (px, py) = appkit_input::event_to_pixel(self, event);
                let cmd = event.modifierFlags().contains(NSEventModifierFlags::Command);
                pane.update_hover(cmd, px, py);
            });
        }

        /// Releasing Cmd drops the URL hover (Kova.app's `flagsChanged:`).
        #[unsafe(method(flagsChanged:))]
        fn flags_changed(&self, event: &NSEvent) {
            shield("flagsChanged:", || {
                if !event.modifierFlags().contains(NSEventModifierFlags::Command) {
                    if let Some(pane) = self.pane() {
                        pane.clear_hover();
                    }
                }
            });
            let _: () = unsafe { msg_send![super(self), flagsChanged: event] };
        }

        #[unsafe(method(updateTrackingAreas))]
        fn update_tracking_areas(&self) {
            shield("updateTrackingAreas", || {
                for area in self.trackingAreas().to_vec() {
                    self.removeTrackingArea(&area);
                }
                let options = NSTrackingAreaOptions::MouseMoved
                    | NSTrackingAreaOptions::ActiveInKeyWindow
                    | NSTrackingAreaOptions::InVisibleRect;
                let area = unsafe {
                    let alloc: objc2::rc::Allocated<NSTrackingArea> = msg_send![objc2::class!(NSTrackingArea), alloc];
                    NSTrackingArea::initWithRect_options_owner_userInfo(alloc, self.bounds(), options, Some(self.as_ref()), None)
                };
                self.addTrackingArea(&area);
            });
        }

        /// Edit > Copy (Cmd+C in the host's menu): the selection.
        #[unsafe(method(copy:))]
        fn copy(&self, _sender: Option<&AnyObject>) {
            shield("copy:", || {
                let Some(pane) = self.pane() else { return };
                // While the filter is open: the matched lines, as Kova.app.
                let text = if pane.filter_open() { pane.take_filter_copy() } else { pane.take_selection() };
                if let Some(text) = text {
                    copy_to_pasteboard(&text);
                }
            });
        }

        /// Edit > Paste (Cmd+V in the host's menu): the pasteboard's text, in
        /// bracketed-paste markers when the app asked for them. An image is
        /// written to /tmp and its path pasted, as Kova.app does: that is how
        /// Claude Code takes an image from a terminal.
        #[unsafe(method(paste:))]
        fn paste(&self, _sender: Option<&AnyObject>) {
            shield("paste:", || {
                let Some(pane) = self.pane() else { return };
                let pasteboard = NSPasteboard::generalPasteboard();
                // The open filter owns the paste, as in Kova.app: never the
                // PTY hidden behind it.
                if pane.filter_open() {
                    if let Some(text) = unsafe { pasteboard.stringForType(objc2_app_kit::NSPasteboardTypeString) } {
                        pane.paste_into_filter(&text.to_string());
                    }
                    return;
                }
                if let Some(path) = appkit_input::pasted_image_path(&pasteboard) {
                    // The path Kova just built holds no paste terminator.
                    pane.surface.paste(&path);
                } else if let Some(text) = unsafe { pasteboard.stringForType(objc2_app_kit::NSPasteboardTypeString) } {
                    pane.surface.paste(&text.to_string());
                }
            });
        }
    }
);

/// Run the body of a `KovaPaneView` method, which AppKit calls: a panic is
/// logged and stops there (`None`) instead of unwinding into AppKit's frames,
/// as `capi`'s `guard` does for the C entry points.
fn shield<R>(method: &str, f: impl FnOnce() -> R) -> Option<R> {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)) {
        Ok(r) => Some(r),
        Err(_) => {
            log::error!("KovaPaneView {} panicked", method);
            None
        }
    }
}

/// Open a URL found under a Cmd+click with the user's default handler.
/// Through `NSWorkspace` rather than Kova.app's `open` child: a host process
/// would have to reap it.
fn open_url(url: &str) {
    let Some(nsurl) = NSURL::URLWithString(&NSString::from_str(url)) else {
        log::warn!("Library pane: not a URL, not opened: {}", url);
        return;
    };
    if !NSWorkspace::sharedWorkspace().openURL(&nsurl) {
        log::warn!("Library pane: NSWorkspace could not open {}", url);
    }
}

/// The text origin of a pane that fills its view.
fn origin(pad_x: f32) -> PaneOrigin {
    PaneOrigin { x: 0.0, y: 0.0, pad_x }
}

impl KovaPaneView {
    fn new(mtm: MainThreadMarker, frame: CGRect, pane_id: PaneId) -> Retained<Self> {
        let this = mtm.alloc::<Self>().set_ivars(KovaPaneViewIvars { pane_id: Cell::new(pane_id) });
        unsafe { msg_send![super(this), initWithFrame: frame] }
    }

    fn pane(&self) -> Option<Rc<LibPane>> {
        find(self.ivars().pane_id.get())
    }

    /// The view's first-responder state changed: the pane redraws and, in
    /// mode 1004, tells its program.
    fn set_focus(&self, focused: bool) {
        if let Some(pane) = self.pane() {
            pane.set_focus(focused);
        }
    }

    fn mouse(&self, event: &NSEvent, action: MouseAction) -> Option<MouseResult> {
        let pane = self.pane()?;
        let (_, cell, pad) = pane.geometry()?;
        Some(pane.input.mouse(self, event, action, &pane.surface, origin(pad), cell))
    }
}

// ─── The socket ──────────────────────────────────────────────────────────────

struct IpcState {
    rx: std::sync::mpsc::Receiver<IpcRequest>,
    path: PathBuf,
    waits: Vec<PendingWait>,
    /// The last state published to `subscribe` clients (Kova.app's diff).
    events: EventState,
}

/// Serve the socket protocol at `path`, over this thread's panes ("one tab per
/// pane"). Panes created afterwards get `KOVA_SOCKET=path`. Requests are
/// answered by `ipc_poll`, never in between. Fails when a socket is served
/// already, or when the socket cannot be bound (bound before returning).
pub fn ipc_start(path: PathBuf) -> Result<(), String> {
    IPC.with(|ipc| {
        let mut ipc = ipc.borrow_mut();
        if ipc.is_some() {
            return Err("a socket is already served".to_string());
        }
        let rx = crate::ipc::start_bound(path.clone()).map_err(|e| format!("bind {}: {e}", path.display()))?;
        *ipc = Some(IpcState { rx, path, waits: Vec::new(), events: EventState::new() });
        Ok(())
    })
}

/// Stop answering and remove the socket file. Clients still connected get
/// "app shutting down"; the listener thread stays parked on `accept`.
pub fn ipc_stop() -> bool {
    let state = IPC.with(|ipc| ipc.borrow_mut().take());
    match state {
        Some(state) => {
            let _ = std::fs::remove_file(&state.path);
            true
        }
        None => false,
    }
}

/// Answer every request received since the last call, and the waits that
/// resolved; publish what changed to `subscribe` clients; then hand the
/// `notify` requests received to the host's handler. Returns how many
/// requests were received in this call (answered, or parked as a pending
/// wait); resolved waits are not counted.
pub fn ipc_poll() -> usize {
    let answered = ipc_poll_with(host_app_active(), &mut IpcSink);
    deliver_notifications();
    answered
}

/// Whether the host app is the active app: what `focus` events call
/// `app_active`. Read from `NSRunningApplication`, which needs no
/// `NSApplication` (a host without one is never active).
fn host_app_active() -> bool {
    NSRunningApplication::currentApplication().isActive()
}

/// `ipc_poll` with the activation and the event sink given: tests drive both.
fn ipc_poll_with(app_active: bool, sink: &mut impl EventSink) -> usize {
    let fps = host().config.terminal.fps as u32;
    IPC.with(|ipc| {
        let mut ipc = ipc.borrow_mut();
        let Some(state) = ipc.as_mut() else { return 0 };
        ipc_host::poll_pending_waits(&LibHost, &mut state.waits);
        let mut answered = 0;
        while let Ok((cmd, responder)) = state.rx.try_recv() {
            answered += 1;
            if let IpcCommand::Subscribe { topics } = cmd {
                // As Kova.app: flush what is pending first, so the clients
                // already subscribed never learn of a change after the new
                // one was handed it as settled state; then the snapshot. The
                // connection thread registered the client before asking.
                state.events.poll_with(&LibEvents, sink, app_active, fps, true);
                let data = crate::events::snapshot_with(&LibEvents, app_active, topics);
                let _ = responder.send(IpcResponse::Ok { data: Some(data) });
                continue;
            }
            match ipc_host::dispatch(&LibHost, cmd) {
                Ok(Disposition::Reply(response)) => {
                    let _ = responder.send(response);
                }
                Ok(Disposition::Pending(wait)) => state.waits.push(PendingWait {
                    pane_id: wait.pane_id,
                    response_tx: responder,
                    deadline: wait.deadline,
                }),
                Err(_) => {
                    let _ = responder.send(IpcResponse::Error {
                        message: "command not supported by this host".to_string(),
                    });
                }
            }
        }
        // What changed this tick, to the subscribers' queues. One atomic load
        // when nobody is subscribed.
        state.events.poll_with(&LibEvents, sink, app_active, fps, false);
        answered
    })
}

/// The `subscribe` events' view of this thread's panes, in the socket's model:
/// one tab per pane, window 0. The focused pane is the one whose view is the
/// first responder of the key window (`LibPane::holds_focus`); none while the
/// user is elsewhere in the host (`no-key-window`) or in another app
/// (`app-inactive`).
struct LibEvents;

impl EventSource for LibEvents {
    fn focus(&self) -> Option<FocusKey> {
        all().iter().enumerate().find(|(_, p)| p.holds_focus()).map(|(idx, p)| FocusKey {
            window: 0,
            tab: idx,
            pane: p.surface.id,
            session: p.surface.agent_session_id(),
            session_name: p.surface.agent_session_name(),
        })
    }

    fn collect_flags(&self, out: &mut std::collections::HashMap<PaneId, PaneFlags>) {
        for (idx, pane) in all().iter().enumerate() {
            let s = &pane.surface;
            out.insert(
                s.id,
                PaneFlags {
                    window: 0,
                    tab: idx,
                    working: s.is_working(),
                    awaiting: s.is_awaiting(),
                    awaiting_since: s.awaiting_since(),
                },
            );
        }
    }

    fn pane_json(&self, _window: usize, pane_id: PaneId) -> Option<serde_json::Value> {
        let panes = all();
        let (idx, pane) = panes.iter().enumerate().find(|(_, p)| p.surface.id == pane_id)?;
        Some(lib_pane_json(pane, idx))
    }

    fn collect_panes(&self, out: &mut Vec<serde_json::Value>) {
        LibHost.collect_panes(out);
    }
}

/// One library pane in the socket's JSON shape (`list-panes`, the snapshot,
/// the events). `focused` is the same fact the `focus` events follow.
fn lib_pane_json(pane: &LibPane, idx: usize) -> serde_json::Value {
    ipc_host::surface_json(&pane.surface, &pane.title(), false, 0, idx, pane.holds_focus())
}

/// The socket's view of this thread's panes: one tab per pane, the tab id
/// being the pane id, all in window 0. Closing and focusing are requests the
/// host carries out (`EVENT_CLOSE_REQUESTED`, `EVENT_FOCUS_REQUESTED`): the
/// host owns the pane's place in its window.
struct LibHost;

impl IpcHost for LibHost {
    fn collect_panes(&self, out: &mut Vec<serde_json::Value>) {
        for (idx, pane) in all().iter().enumerate() {
            out.push(lib_pane_json(pane, idx));
        }
    }

    fn collect_pane_ids(&self, out: &mut Vec<PaneId>) {
        out.extend(all().iter().map(|p| p.surface.id));
    }

    fn send_keys(&self, pane_id: PaneId, text: &str) -> bool {
        find(pane_id).map(|p| ipc_host::send_keys_to(&p.surface, text)).is_some()
    }

    fn dump_pane_text(&self, pane_id: PaneId, mode: DumpMode, trim: bool) -> Option<serde_json::Value> {
        find(pane_id).map(|p| ipc_host::pane_text_entry(pane_id, &p.surface, mode, trim))
    }

    fn measure_pane_text(&self, pane_id: PaneId, mode: DumpMode, trim: bool) -> Option<(usize, usize)> {
        find(pane_id).map(|p| ipc_host::measure_pane_text(&p.surface, mode, trim))
    }

    fn check_completion(&self, pane_id: PaneId) -> Option<bool> {
        find(pane_id).map(|p| ipc_host::completion_flag(&p.surface))
    }

    fn focus_pane(&self, pane_id: PaneId) -> bool {
        find(pane_id).map(|p| p.raise(EVENT_FOCUS_REQUESTED)).is_some()
    }

    fn close_pane(&self, pane_id: PaneId) -> Option<bool> {
        // Never refused: a host has no "last pane" to keep.
        find(pane_id).map(|p| {
            p.raise(EVENT_CLOSE_REQUESTED);
            true
        })
    }

    fn set_pane_status(&self, pane_id: PaneId, waiting: bool) -> bool {
        find(pane_id)
            .map(|p| {
                if waiting {
                    p.surface.set_awaiting();
                } else {
                    p.surface.clear_awaiting();
                }
                p.surface.terminal.read().dirty.store(true, Ordering::Relaxed);
            })
            .is_some()
    }

    fn pane_signal(&self, pane_id: PaneId, signal: PaneSignal) -> bool {
        find(pane_id)
            .map(|p| {
                signal.apply(&p.surface);
                p.surface.terminal.read().dirty.store(true, Ordering::Relaxed);
            })
            .is_some()
    }

    fn notify(&self, pane_id: Option<PaneId>, title: &str, message: &str, sound: bool) -> Result<(), String> {
        // Kova.app posts its own notifications and routes their click through
        // its delegate (`crate::notification`); a host knows which of its
        // documents holds the pane, so it posts them itself, through the
        // handler it registered. Queued: `ipc_poll` calls the handler once
        // the socket state is released.
        if !has_notify_handler() {
            return Err("notify is not supported by this host".to_string());
        }
        PENDING_NOTIFY.with(|q| {
            q.borrow_mut().push(PendingNotify {
                pane: pane_id,
                title: title.to_string(),
                message: message.to_string(),
                sound,
            })
        });
        Ok(())
    }

    fn collect_tabs(&self, out: &mut Vec<TabInfo>) {
        for (idx, pane) in all().iter().enumerate() {
            let s = &pane.surface;
            let (has_completion, has_bell) = pane.attention.get();
            let command_running = s.terminal.read().command_running.load(Ordering::Relaxed);
            out.push(TabInfo {
                id: s.id,
                window: 0,
                tab_index: idx,
                title: pane.title(),
                pane_count: 1,
                focused_pane_id: s.id,
                active: pane.is_focused(),
                has_bell,
                has_completion,
                has_running: command_running || s.fg_process().is_some(),
            });
        }
    }

    fn close_tab(&self, tab_id: u32) -> IpcCloseTabResult {
        match self.close_pane(tab_id) {
            Some(_) => IpcCloseTabResult::Closed,
            None => IpcCloseTabResult::NotFound,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::input::tests::drain;

    /// A panic inside a view method's body stops at `shield`: AppKit, the
    /// caller, never sees it unwind.
    #[test]
    fn shield_stops_a_panic() {
        assert_eq!(shield("test", || 7), Some(7));
        assert_eq!(shield("test", || -> u8 { panic!("boom") }), None);
    }

    /// A resize of a library pane arms the same repairs as a Kova.app pane,
    /// and the pane's tick runs them on the real clock: the settle repaint
    /// after ~150ms of quiet, then the winsize restore ~50ms later, which
    /// arms the hole check.
    #[test]
    fn a_library_resize_settles_through_kovas_repairs() {
        let (id, _reader) = create_capture(80, 24);
        let pane = find(id).unwrap();
        let settle = resize_repair::settle_frames(host().config.terminal.fps as u32);
        pane.resize(100, 30);
        assert_eq!(pane.pending_repairs(), (None, Some(settle), None));
        pane.resize(100, 30);
        assert_eq!(pane.pending_repairs(), (None, Some(settle), None), "same size: nothing re-armed");
        let frame = Duration::from_secs_f64(1.0 / host().config.terminal.fps as f64);
        std::thread::sleep(frame * (settle + 2));
        pane.tick();
        let (restore, settled, _) = pane.pending_repairs();
        assert_eq!(settled, None, "the settle repaint fired");
        assert_eq!(restore, Some(resize_repair::RESTORE_FRAMES), "its nudge waits for a restore");
        std::thread::sleep(frame * (resize_repair::RESTORE_FRAMES + 2));
        pane.tick();
        let (restore, _, check) = pane.pending_repairs();
        assert_eq!(restore, None, "the winsize was restored");
        assert!(check.is_some(), "the hole check is armed");
        assert!(close(id));
    }

    /// Going back within 500ms to a size the pane just had nudges the PTY at
    /// once and schedules the restore, as in Kova.app.
    #[test]
    fn a_library_round_trip_nudges_at_once() {
        let (id, _reader) = create_capture(80, 24);
        let pane = find(id).unwrap();
        pane.resize(100, 30);
        assert_eq!(pane.pending_repairs().0, None);
        pane.resize(80, 24);
        assert_eq!(pane.pending_repairs().0, Some(resize_repair::RESTORE_FRAMES));
        assert!(close(id));
    }

    /// Focus events (DEC mode 1004) follow the view's first-responder state:
    /// sent only when the program asked, and only on a real change.
    #[test]
    fn focus_changes_reach_the_program_in_mode_1004() {
        let (id, reader) = create_capture(80, 24);
        let pane = find(id).unwrap();
        pane.set_focus_for_test(true);
        assert_eq!(drain(&reader), b"", "mode off: nothing");
        pane.set_focus_for_test(false);
        pane.surface.terminal.write().focus_reporting = true;
        pane.set_focus_for_test(true);
        assert_eq!(drain(&reader), b"\x1b[I");
        pane.set_focus_for_test(true);
        assert_eq!(drain(&reader), b"", "already focused: nothing");
        pane.set_focus_for_test(false);
        assert_eq!(drain(&reader), b"\x1b[O");
        pane.set_focus_for_test(false);
        assert_eq!(drain(&reader), b"");
        assert!(close(id));
    }

    fn ctrl_f() -> KeyEvent {
        crate::input::tests::typed("\u{6}", "f", 0x03, "ctrl")
    }

    fn type_lines(pane: &LibPane, lines: &[&str]) {
        let mut t = pane.surface.terminal.write();
        for l in lines {
            t.carriage_return();
            for c in l.chars() {
                t.put_char(c);
            }
            t.newline();
        }
    }

    /// Ctrl+F opens Kova.app's filter in a library pane: typing filters the
    /// pane's lines, ↑/↓ pick a match (starting on the first) and Enter closes
    /// it on that one, Escape and Ctrl+F close it; Ctrl+Option+arrows do
    /// nothing; no key reaches the PTY.
    #[test]
    fn the_filter_takes_the_keys_of_a_library_pane() {
        use crate::input::tests::typed;
        let (id, reader) = create_capture(20, 3);
        let pane = find(id).unwrap();
        type_lines(&pane, &["alpha", "beta 1", "gamma", "beta 2", "delta", "eps"]);
        assert!(pane.filter_key(&typed("\u{F703}", "\u{F703}", 0x7C, "ctrl alt")), "virtual width: taken, nothing done");
        assert!(!pane.filter_open());
        assert!(!pane.filter_key(&typed("b", "b", 0x0B, "")), "closed: the pane's key");

        assert!(pane.filter_key(&ctrl_f()));
        assert!(pane.filter_open());
        for (chars, code) in [("b", 0x0B), ("e", 0x0E), ("t", 0x11)] {
            assert!(pane.filter_key(&typed(chars, chars, code, "")));
        }
        {
            let filter = pane.filter.borrow();
            let state = filter.as_ref().unwrap();
            assert_eq!(state.query, "bet");
            let texts: Vec<&str> = state.matches.iter().map(|m| m.text.as_str()).collect();
            assert_eq!(texts, ["beta 1", "beta 2"]);
        }
        assert!(pane.filter_key(&typed("\u{7f}", "\u{7f}", 0x33, "")));
        assert_eq!(pane.filter.borrow().as_ref().unwrap().query, "be");
        assert!(pane.paste_into_filter("ta 1\nignored"));
        assert_eq!(pane.filter.borrow().as_ref().unwrap().matches.len(), 1);

        // Enter: closed, scrolled back to the picked match (the first one).
        assert!(pane.filter_key(&typed("\r", "\r", 0x24, "")));
        assert!(!pane.filter_open());
        assert!(pane.surface.terminal.read().scroll_offset() > 0, "scrolled to `beta 1` in the scrollback");

        assert!(pane.filter_key(&ctrl_f()));
        assert!(pane.filter_key(&typed("\u{1b}", "\u{1b}", 0x35, "")), "Escape");
        assert!(!pane.filter_open());
        assert!(pane.filter_key(&ctrl_f()));
        assert!(pane.filter_key(&ctrl_f()), "Ctrl+F again closes it");
        assert!(!pane.filter_open());
        assert!(!pane.paste_into_filter("x"), "closed: the paste is the pane's");
        assert_eq!(drain(&reader), b"", "nothing reached the PTY");
        assert!(close(id));
    }

    /// Enter jumps to the match ↑/↓ picked, not to the first one: the line the
    /// pane lands on follows the pick.
    #[test]
    fn enter_jumps_to_the_picked_match() {
        use crate::input::tests::typed;
        let (id, reader) = create_capture(20, 3);
        let pane = find(id).unwrap();
        type_lines(
            &pane,
            &["alpha", "beta 1", "gamma", "beta 2", "delta", "eps"],
        );
        // Type "bet", press ↓ `downs` times, Enter, and report where the
        // scrollback landed. The filter closes, so each run reopens it.
        let jump = |downs: usize| {
            assert!(pane.filter_key(&ctrl_f()));
            for (chars, code) in [("b", 0x0B), ("e", 0x0E), ("t", 0x11)] {
                assert!(pane.filter_key(&typed(chars, chars, code, "")));
            }
            for _ in 0..downs {
                assert!(pane.filter_key(&typed("\u{F701}", "\u{F701}", 0x7D, "")));
            }
            assert!(pane.filter_key(&typed("\r", "\r", 0x24, "")));
            pane.surface.terminal.read().scroll_offset()
        };
        let first = jump(0);
        let second = jump(1);
        // The second match sits further down the buffer, so centring on it
        // leaves a smaller offset into the scrollback.
        assert!(
            second < first,
            "↓ then Enter landed on the second match: {second} vs {first}"
        );
        assert_eq!(drain(&reader), b"", "nothing reached the PTY");
        assert!(close(id));
    }

    /// The open filter follows the output on the pane's tick, throttled as
    /// in Kova.app; Cmd+C while it is open copies the matches and closes it.
    #[test]
    fn the_open_filter_follows_the_output() {
        let (id, _reader) = create_capture(20, 3);
        let pane = find(id).unwrap();
        type_lines(&pane, &["hit 1", "miss"]);
        assert!(pane.filter_key(&ctrl_f()));
        assert!(pane.filter_key(&crate::input::tests::typed("h", "h", 0x04, "")));
        assert_eq!(pane.filter.borrow().as_ref().unwrap().matches.len(), 1);
        type_lines(&pane, &["hit 2"]);
        std::thread::sleep(crate::window::overlays::FILTER_REFRESH_INTERVAL);
        pane.tick();
        assert_eq!(pane.filter.borrow().as_ref().unwrap().matches.len(), 2, "the new line is listed");
        assert_eq!(pane.take_filter_copy().as_deref(), Some("hit 1\nhit 2"));
        assert!(!pane.filter_open(), "copying closed it");
        assert_eq!(pane.take_filter_copy(), None);
        assert!(close(id));
    }

    struct Recorder(Vec<serde_json::Value>);

    impl EventSink for Recorder {
        fn wants(&self, _topics: u32) -> bool {
            true
        }
        fn publish(&mut self, _topic: u32, event: serde_json::Value) {
            self.0.push(event);
        }
    }

    impl Recorder {
        fn take(&mut self) -> Vec<serde_json::Value> {
            std::mem::take(&mut self.0)
        }
    }

    /// The `subscribe` view of a library host: one tab per pane in window 0,
    /// focus from the view that is first responder, the same edges as
    /// Kova.app.
    #[test]
    fn subscribe_sees_the_library_panes() {
        let (a, _ra) = create_capture(20, 3);
        let (b, _rb) = create_capture(20, 3);
        let (pa, pb) = (find(a).unwrap(), find(b).unwrap());
        let mut state = EventState::new();
        let mut sink = Recorder(Vec::new());
        state.poll_with(&LibEvents, &mut sink, true, 60, true);
        assert!(sink.take().is_empty(), "seeded silently");

        let snap = crate::events::snapshot_with(&LibEvents, true, crate::ipc::topic::ALL);
        let ids: Vec<u64> = snap["panes"].as_array().unwrap().iter().map(|p| p["id"].as_u64().unwrap()).collect();
        assert_eq!(ids, [a as u64, b as u64], "this thread's panes, in order");
        assert_eq!(snap["focus"], serde_json::Value::Null, "no view is first responder");

        pb.set_focus_for_test(true);
        state.poll_with(&LibEvents, &mut sink, true, 60, false);
        let ev = sink.take();
        assert_eq!(ev.len(), 1);
        assert_eq!(ev[0]["event"], "focus");
        assert_eq!(ev[0]["reason"], "app-active");
        assert_eq!(ev[0]["pane"]["id"].as_u64(), Some(b as u64));
        assert_eq!(ev[0]["pane"]["tab"], 1);
        assert_eq!(ev[0]["pane"]["focused"], true);
        let snap = crate::events::snapshot_with(&LibEvents, true, crate::ipc::topic::ALL);
        assert_eq!(snap["focus"]["id"].as_u64(), Some(b as u64));

        pb.set_focus_for_test(false);
        pa.set_focus_for_test(true);
        state.poll_with(&LibEvents, &mut sink, true, 60, false);
        assert_eq!(sink.take()[0]["reason"], "tab", "one tab per pane");
        pa.set_focus_for_test(false);
        state.poll_with(&LibEvents, &mut sink, true, 60, false);
        assert_eq!(sink.take()[0]["reason"], "no-key-window", "elsewhere in the host");
        pa.set_focus_for_test(true);
        state.poll_with(&LibEvents, &mut sink, true, 60, false);
        assert_eq!(sink.take()[0]["reason"], "app-active", "back on a pane");
        state.poll_with(&LibEvents, &mut sink, false, 60, false);
        assert_eq!(sink.take()[0]["reason"], "app-inactive", "the host is in the background");
        state.poll_with(&LibEvents, &mut sink, true, 60, false);
        assert_eq!(sink.take()[0]["reason"], "app-active");

        pa.surface.set_awaiting();
        pb.surface.terminal.write().title = Some("\u{2810} Claude Code".into());
        let (c, _rc) = create_capture(20, 3);
        state.poll_with(&LibEvents, &mut sink, true, 60, true);
        let ev = sink.take();
        let find_ev = |name: &str| ev.iter().find(|e| e["event"] == name).cloned();
        assert_eq!(find_ev("pane-status").unwrap()["pane_id"].as_u64(), Some(a as u64));
        assert_eq!(find_ev("pane-working").unwrap()["working"], true);
        assert_eq!(find_ev("pane-open").unwrap()["pane"]["id"].as_u64(), Some(c as u64));

        assert!(close(b));
        state.poll_with(&LibEvents, &mut sink, true, 60, true);
        assert_eq!(sink.take(), [serde_json::json!({"event": "pane-close", "pane_id": b, "window": 0, "tab": 1})]);
        assert!(close(a));
        assert!(close(c));
    }

    /// `notify` goes to the handler the host registered, on the next poll,
    /// after the request was answered; without one it is refused.
    #[test]
    fn notify_reaches_the_host_handler() {
        set_notify_handler(None);
        assert_eq!(
            LibHost.notify(Some(3), "t", "m", false),
            Err("notify is not supported by this host".to_string())
        );
        let seen: Rc<RefCell<Vec<(Option<PaneId>, String, String, bool)>>> = Rc::new(RefCell::new(Vec::new()));
        let log = seen.clone();
        set_notify_handler(Some(Rc::new(move |pane, title: &str, message: &str, sound| {
            log.borrow_mut().push((pane, title.to_string(), message.to_string(), sound));
        })));
        assert_eq!(LibHost.notify(Some(3), "Claude Code", "done", true), Ok(()));
        assert_eq!(LibHost.notify(None, "Kova", "bare", false), Ok(()));
        assert!(seen.borrow().is_empty(), "not called while the request is served");
        assert_eq!(deliver_notifications(), 2);
        assert_eq!(
            *seen.borrow(),
            [(Some(3), "Claude Code".to_string(), "done".to_string(), true), (None, "Kova".to_string(), "bare".to_string(), false)]
        );
        assert_eq!(deliver_notifications(), 0, "delivered once");
        // A handler that removes itself while called does not break the poll.
        set_notify_handler(Some(Rc::new(|_, _: &str, _: &str, _| set_notify_handler(None))));
        assert_eq!(LibHost.notify(None, "a", "b", false), Ok(()));
        assert_eq!(deliver_notifications(), 1);
        assert!(!has_notify_handler());
    }
}
