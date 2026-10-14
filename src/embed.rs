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
    NSAutoresizingMaskOptions, NSEvent, NSPasteboard, NSTextInputClient, NSTrackingArea,
    NSTrackingAreaOptions, NSView,
};
use objc2_core_foundation::{CGRect, CGSize};
use objc2_foundation::NSObjectProtocol;
use objc2_quartz_core::CAMetalLayer;

use crate::appkit_input::{self, copy_to_pasteboard, AppKitInput, PaneOrigin, ScrollAxisLock};
use crate::config::Config;
use crate::input::{self, KeyEvent, KeyResult, MouseAction, MouseEvent, MouseResult, ScrollEvent};
use crate::ipc::host::{self as ipc_host, Disposition, IpcCloseTabResult, IpcHost, PaneSignal, PendingWait, TabInfo};
use crate::ipc::{IpcCommand, IpcRequest, IpcResponse};
use crate::keybindings::Keybindings;
use crate::renderer::layer::{self, LayerRenderer};
use crate::renderer::{PaneRenderData, PaneViewport};
use crate::surface::{pane_attention, PaneId, Surface};
use crate::terminal::pty::SpawnEnv;
use crate::terminal::DumpMode;

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

thread_local! {
    static HOST: OnceCell<Rc<Host>> = const { OnceCell::new() };
    static PANES: RefCell<Vec<Rc<LibPane>>> = const { RefCell::new(Vec::new()) };
    static IPC: RefCell<Option<IpcState>> = const { RefCell::new(None) };
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

    /// Resize the terminal grid and tell the PTY. A no-op at the same size.
    pub fn resize(&self, cols: u16, rows: u16) {
        let (cols, rows) = (cols.max(1), rows.max(1));
        let mut term = self.surface.terminal.write();
        if term.cols != cols || term.rows != rows {
            term.resize(cols, rows);
            term.dirty.store(true, Ordering::Relaxed);
            drop(term);
            self.surface.pty.resize(cols, rows);
        }
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
        renderer.wants_frame(&self.render_data())
    }

    /// Draw the pane when it needs it (or always, with `force`). Returns
    /// whether a frame was presented.
    pub fn draw(&self, force: bool) -> bool {
        let Some(layer) = self.layer.borrow().clone() else { return false };
        if self.view().is_some_and(|v| v.bounds().size.width < 1.0 || v.bounds().size.height < 1.0) {
            return false; // collapsed: nothing to see
        }
        let data = self.render_data();
        let mut renderer = self.renderer.borrow_mut();
        let Some(renderer) = renderer.as_mut() else { return false };
        let drawn = renderer.draw(&layer, &data, force);
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
            shield("firstRectForCharacterRange:", || AppKitInput::new().first_rect_for_character_range(self))
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
            shield("becomeFirstResponder", || self.mark_dirty());
            accepted
        }

        #[unsafe(method(resignFirstResponder))]
        fn resign_first_responder(&self) -> bool {
            let accepted: bool = unsafe { msg_send![super(self), resignFirstResponder] };
            shield("resignFirstResponder", || self.mark_dirty());
            accepted
        }

        #[unsafe(method(keyDown:))]
        fn key_down(&self, event: &NSEvent) {
            shield("keyDown:", || {
                let Some(pane) = self.pane() else { return };
                let key = appkit_input::key_event(event);
                // Kova.app's window keys (Ctrl+F filter, Ctrl+Option+arrows)
                // have no window here: they do nothing, as they never reach
                // the PTY.
                if input::host_key(&key).is_some() {
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
            shield("mouseDown:", || self.mouse(event, MouseAction::Press));
        }

        #[unsafe(method(mouseDragged:))]
        fn mouse_dragged(&self, event: &NSEvent) {
            shield("mouseDragged:", || self.mouse(event, MouseAction::Drag));
        }

        #[unsafe(method(mouseUp:))]
        fn mouse_up(&self, event: &NSEvent) {
            shield("mouseUp:", || {
                let Some(pane) = self.pane() else { return };
                pane.input.end_drag();
                if self.mouse(event, MouseAction::Release) == Some(MouseResult::Reported) {
                    return;
                }
                pane.input.finish_selection(&pane.surface);
            });
        }

        #[unsafe(method(mouseMoved:))]
        fn mouse_moved(&self, event: &NSEvent) {
            shield("mouseMoved:", || self.mouse(event, MouseAction::Move));
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
                if let Some(text) = self.pane().and_then(|p| p.take_selection()) {
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

    /// Redraw on the next `draw`: focus changes the cursor and the dimming.
    fn mark_dirty(&self) {
        if let Some(pane) = self.pane() {
            pane.surface.terminal.read().dirty.store(true, Ordering::Relaxed);
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
        *ipc = Some(IpcState { rx, path, waits: Vec::new() });
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
/// resolved. Returns how many requests were received in this call (answered,
/// or parked as a pending wait); resolved waits are not counted.
pub fn ipc_poll() -> usize {
    IPC.with(|ipc| {
        let mut ipc = ipc.borrow_mut();
        let Some(state) = ipc.as_mut() else { return 0 };
        ipc_host::poll_pending_waits(&LibHost, &mut state.waits);
        let mut answered = 0;
        while let Ok((cmd, responder)) = state.rx.try_recv() {
            answered += 1;
            if let IpcCommand::Subscribe { .. } = cmd {
                let _ = responder.send(IpcResponse::Error {
                    message: "subscribe is not supported by this host".to_string(),
                });
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
        answered
    })
}

/// The socket's view of this thread's panes: one tab per pane, the tab id
/// being the pane id, all in window 0. Closing and focusing are requests the
/// host carries out (`EVENT_CLOSE_REQUESTED`, `EVENT_FOCUS_REQUESTED`): the
/// host owns the pane's place in its window.
struct LibHost;

impl IpcHost for LibHost {
    fn collect_panes(&self, out: &mut Vec<serde_json::Value>) {
        for (idx, pane) in all().iter().enumerate() {
            out.push(ipc_host::surface_json(&pane.surface, &pane.title(), false, 0, idx, pane.is_focused()));
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

    fn notify(&self, _pane_id: Option<PaneId>, _title: &str, _message: &str, _sound: bool) -> Result<(), String> {
        // Kova.app's notifications route their click through its own
        // delegate (`crate::notification`); a host posts its own.
        Err("notify is not supported by this host".to_string())
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
    use super::shield;

    /// A panic inside a view method's body stops at `shield`: AppKit, the
    /// caller, never sees it unwind.
    #[test]
    fn shield_stops_a_panic() {
        assert_eq!(shield("test", || 7), Some(7));
        assert_eq!(shield("test", || -> u8 { panic!("boom") }), None);
    }
}
