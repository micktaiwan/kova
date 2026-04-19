use block2::RcBlock;
use objc2::rc::Retained;
use objc2::{define_class, msg_send, DefinedClass, MainThreadOnly, MainThreadMarker};
use objc2_app_kit::{NSApplication, NSApplicationDelegate, NSApplicationTerminateReply, NSMenu, NSMenuItem, NSWindow};
use objc2_foundation::{NSNotification, NSObject, NSObjectProtocol, NSRunLoop, NSRunLoopCommonModes, NSString, NSTimer};
use std::cell::{Cell, OnceCell, RefCell};
use std::ptr::NonNull;

use crate::config::Config;
use crate::window;

pub struct AppDelegateIvars {
    pub windows: RefCell<Vec<Retained<NSWindow>>>,
    /// Windows pending dealloc — kept alive one extra timer tick so AppKit
    /// finishes its run-loop work before the Retained is dropped.
    pending_close: RefCell<Vec<Retained<NSWindow>>>,
    /// Session data collected from windows as they close, so we don't lose
    /// their state when they're deallocated before app termination.
    closed_sessions: RefCell<Vec<crate::session::WindowSession>>,
    config: OnceCell<Config>,
    /// Frame counter for periodic session save (every 30s).
    tick_count: Cell<u64>,
    /// Optional session backup number to restore (--session N).
    session_backup: Option<usize>,
    /// IPC command receiver — polled in the timer tick on the main thread.
    ipc_rx: RefCell<Option<std::sync::mpsc::Receiver<crate::ipc::IpcRequest>>>,
}

define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "KovaAppDelegate"]
    #[ivars = AppDelegateIvars]
    pub struct AppDelegate;

    unsafe impl NSObjectProtocol for AppDelegate {}

    unsafe impl NSApplicationDelegate for AppDelegate {
        #[unsafe(method(applicationDidFinishLaunching:))]
        fn did_finish_launching(&self, _notification: &NSNotification) {
            let mtm = MainThreadMarker::from(self);
            setup_menu(mtm);

            let config = self.ivars().config.get().unwrap();
            log::debug!("Config loaded: {}x{} cols/rows, {} scrollback", config.terminal.columns, config.terminal.rows, config.terminal.scrollback);

            // Restore session (all windows) or create a single fresh window
            let restored = crate::session::load(self.ivars().session_backup)
                .and_then(|s| crate::session::restore_session(s, config));

            match restored {
                Some(windows) => {
                    log::info!("Restoring {} window(s) from session", windows.len());
                    let mut win_vec = self.ivars().windows.borrow_mut();
                    for (i, rw) in windows.into_iter().enumerate() {
                        let win = window::create_window(mtm, config, rw.tabs, rw.active_tab, rw.deferred_tabs);
                        // Restore saved window position if available
                        if let Some((x, y, w, h)) = rw.frame {
                            let frame = objc2_core_foundation::CGRect {
                                origin: objc2_core_foundation::CGPoint { x, y },
                                size: objc2_core_foundation::CGSize { width: w, height: h },
                            };
                            win.setFrame_display(frame, i == 0);
                        }
                        win.makeKeyAndOrderFront(None);
                        win_vec.push(win);
                    }
                }
                None => {
                    let tab = crate::pane::Tab::new(config).expect("failed to create initial tab");
                    let win = window::create_window(mtm, config, vec![tab], 0, Vec::new());
                    win.makeKeyAndOrderFront(None);
                    self.ivars().windows.borrow_mut().push(win);
                }
            }

            let app = NSApplication::sharedApplication(mtm);
            app.activate();

            // Start IPC server (Unix socket for external process control)
            let ipc_rx = crate::ipc::start();
            *self.ivars().ipc_rx.borrow_mut() = Some(ipc_rx);

            // Start global render timer — single timer for all windows
            self.start_global_timer(config.terminal.fps);
        }

        #[unsafe(method(applicationShouldTerminate:))]
        fn should_terminate(&self, _sender: &NSApplication) -> NSApplicationTerminateReply {
            let mtm = MainThreadMarker::from(self);
            let mut all_procs = Vec::new();
            let windows = self.ivars().windows.borrow();
            for win in windows.iter() {
                if let Some(view) = kova_view(win) {
                    all_procs.extend(view.running_processes());
                }
            }
            drop(windows);

            if !window::confirm_running_processes(mtm, &all_procs, "Do you want to quit Kova?", "Quit") {
                return NSApplicationTerminateReply::TerminateCancel;
            }
            NSApplicationTerminateReply::TerminateNow
        }

        #[unsafe(method(applicationShouldTerminateAfterLastWindowClosed:))]
        fn should_terminate_after_last_window_closed(
            &self,
            _sender: &NSApplication,
        ) -> bool {
            true
        }

        #[unsafe(method(applicationWillTerminate:))]
        fn will_terminate(&self, _notification: &NSNotification) {
            log::info!("Kova shutting down");
            // Save session BEFORE shutting down PTYs (we need them alive for CWD detection)
            // Start with sessions saved when windows were closed during this run
            // (pending_close windows are already in closed_sessions — no need to re-collect)
            let mut window_sessions = self.ivars().closed_sessions.borrow().clone();
            window_sessions.extend(collect_window_sessions(&self.ivars().windows.borrow()));
            crate::session::save(&window_sessions);
            crate::terminal::pty::shutdown_all();
            crate::ipc::cleanup();
            log::logger().flush();
        }
    }
);

impl AppDelegate {
    pub fn new(mtm: MainThreadMarker, config: Config, session_backup: Option<usize>) -> Retained<Self> {
        let this = mtm.alloc::<Self>().set_ivars(AppDelegateIvars {
            windows: RefCell::new(Vec::new()),
            pending_close: RefCell::new(Vec::new()),
            tick_count: Cell::new(0),
            closed_sessions: RefCell::new(Vec::new()),
            config: OnceCell::new(),
            session_backup,
            ipc_rx: RefCell::new(None),
        });
        let retained: Retained<Self> = unsafe { msg_send![super(this), init] };
        retained.ivars().config.set(config).ok();
        retained
    }

    /// Start a single global NSTimer that ticks all windows.
    fn start_global_timer(&self, fps: u32) {
        let ivars = self.ivars() as *const AppDelegateIvars;
        let timer = unsafe {
            NSTimer::scheduledTimerWithTimeInterval_repeats_block(
                1.0 / fps as f64,
                true,
                &RcBlock::new(move |_timer: NonNull<NSTimer>| {
                    let ivars = &*ivars;

                    // Drop windows pending close from the previous tick.
                    // Deferring by one tick lets AppKit finish run-loop work
                    // before the NSWindow/KovaView is deallocated.
                    ivars.pending_close.borrow_mut().clear();

                    // Tick all windows, collect indices of dead ones
                    let mut dead_indices: Vec<usize> = Vec::new();
                    {
                        let windows = ivars.windows.borrow();
                        for (i, win) in windows.iter().enumerate() {
                            if let Some(view) = kova_view(win) {
                                if !view.tick() {
                                    dead_indices.push(i);
                                }
                            }
                        }
                    }

                    // Process IPC commands from external processes
                    {
                        let rx_borrow = ivars.ipc_rx.borrow();
                        if let Some(ref rx) = *rx_borrow {
                            while let Ok((cmd, responder)) = rx.try_recv() {
                                let response = handle_ipc_command(cmd, &ivars.windows, &ivars.config);
                                let _ = responder.send(response);
                            }
                        }
                    }

                    // Periodic session save (every ~30s) to survive crashes.
                    // Serialization + I/O is offloaded to a thread to avoid frame drops.
                    let count = ivars.tick_count.get() + 1;
                    ivars.tick_count.set(count);
                    if count % (fps as u64 * 30) == 0 {
                        let sessions = collect_window_sessions(&ivars.windows.borrow());
                        if !sessions.is_empty() {
                            std::thread::spawn(move || {
                                crate::session::save(&sessions);
                            });
                        }
                    }

                    if !dead_indices.is_empty() {
                        // Move dead windows to pending_close — they'll be deallocated
                        // at the start of the next tick.
                        let mut windows = ivars.windows.borrow_mut();
                        let mut pending = ivars.pending_close.borrow_mut();
                        let mut closed = ivars.closed_sessions.borrow_mut();
                        for &idx in dead_indices.iter().rev() {
                            let win = windows.remove(idx);
                            // Save session data before the window is deallocated
                            // (skip if killed with Cmd+Shift+Q)
                            if let Some(view) = kova_view(&win) {
                                if !view.skip_session_save() {
                                    view.append_session_data(&mut closed);
                                }
                            }
                            win.orderOut(None);
                            pending.push(win);
                        }
                        drop(closed);
                        let is_empty = windows.is_empty();
                        drop(pending);
                        drop(windows);

                        if is_empty {
                            let mtm = MainThreadMarker::new_unchecked();
                            let app = NSApplication::sharedApplication(mtm);
                            app.terminate(None);
                        }
                    }
                }),
            )
        };
        let run_loop = NSRunLoop::currentRunLoop();
        unsafe { run_loop.addTimer_forMode(&timer, NSRunLoopCommonModes) };
    }
}

/// Collect session data from all live windows.
fn collect_window_sessions(windows: &[Retained<NSWindow>]) -> Vec<crate::session::WindowSession> {
    let mut sessions = Vec::new();
    for win in windows.iter() {
        if let Some(view) = kova_view(win) {
            view.append_session_data(&mut sessions);
        }
    }
    sessions
}

/// Get a reference to our AppDelegate from the shared NSApplication.
/// SAFETY: The app delegate is always our AppDelegate (set in main.rs).
pub fn app_delegate(mtm: MainThreadMarker) -> &'static AppDelegate {
    let app = NSApplication::sharedApplication(mtm);
    let delegate = app.delegate().expect("no app delegate");
    unsafe {
        let raw: *const AppDelegate = msg_send![&*delegate, self];
        &*raw
    }
}

/// Create a new empty window and register it.
/// Called from KovaView on Cmd+N.
pub fn create_new_window(mtm: MainThreadMarker) {
    let ad = app_delegate(mtm);
    let config = ad.ivars().config.get().unwrap();
    let tab = crate::pane::Tab::new(config).expect("failed to create tab");
    let win = window::create_window(mtm, config, vec![tab], 0, Vec::new());
    win.makeKeyAndOrderFront(None);
    ad.ivars().windows.borrow_mut().push(win);
}

/// Detach a tab into a new window, offset from the source window.
pub fn detach_tab_to_new_window(
    mtm: MainThreadMarker,
    tab: crate::pane::Tab,
    source_frame: Option<objc2_core_foundation::CGRect>,
) {
    let ad = app_delegate(mtm);
    let config = ad.ivars().config.get().unwrap();
    let win = window::create_window(mtm, config, vec![tab], 0, Vec::new());

    // Offset new window from source (+20x, -20y cascade)
    if let Some(sf) = source_frame {
        use objc2_core_foundation::{CGPoint, CGRect, CGSize};
        let new_frame = CGRect {
            origin: CGPoint {
                x: sf.origin.x + 20.0,
                y: sf.origin.y - 20.0,
            },
            size: CGSize {
                width: sf.size.width,
                height: sf.size.height,
            },
        };
        win.setFrame_display(new_frame, true);
    }

    win.makeKeyAndOrderFront(None);
    ad.ivars().windows.borrow_mut().push(win);
}

/// Info about a window for the "Send Tab to Window" overlay.
pub struct WindowInfo {
    pub label: String,
    /// Index in the app delegate's windows list.
    pub index: usize,
}

/// List other windows (excluding `source`) with their tab summaries.
pub fn list_other_windows(mtm: MainThreadMarker, source: &NSWindow) -> Vec<WindowInfo> {
    let ad = app_delegate(mtm);
    let windows = ad.ivars().windows.borrow();
    let mut result = Vec::new();
    for (i, win) in windows.iter().enumerate() {
        if win.isEqual(Some(source)) {
            continue;
        }
        let label = if let Some(view) = kova_view(win) {
            let names = view.tab_titles();
            if names.len() == 1 {
                names[0].clone()
            } else {
                format!("{} tabs: {}", names.len(), names.join(", "))
            }
        } else {
            format!("Window {}", i + 1)
        };
        result.push(WindowInfo { label, index: i });
    }
    result
}

/// Send a tab to an existing window (by index in the app delegate's window list).
pub fn send_tab_to_window(mtm: MainThreadMarker, tab: crate::pane::Tab, window_index: usize) {
    let ad = app_delegate(mtm);
    let windows = ad.ivars().windows.borrow();
    if let Some(target) = windows.get(window_index) {
        if let Some(view) = kova_view(target) {
            view.append_tabs(vec![tab]);
        }
        target.makeKeyAndOrderFront(None);
    }
}

/// Cast the window's contentView to our KovaView.
/// SAFETY: contentView is always a KovaView (set in `create_window`).
pub fn kova_view(window: &NSWindow) -> Option<&crate::window::KovaView> {
    window.contentView().map(|cv| {
        let ptr: *const objc2_app_kit::NSView = &*cv;
        unsafe { &*(ptr as *const crate::window::KovaView) }
    })
}

/// Handle a single IPC command on the main thread.
/// All window/pane operations happen here (AppKit requirement).
fn handle_ipc_command(
    cmd: crate::ipc::IpcCommand,
    windows: &RefCell<Vec<Retained<NSWindow>>>,
    config_cell: &OnceCell<Config>,
) -> crate::ipc::IpcResponse {
    use crate::ipc::IpcCommand;

    match cmd {
        IpcCommand::Split { direction, cmd: command, cwd } => {
            handle_ipc_split(windows, config_cell, &direction, command, cwd)
        }
        IpcCommand::ListPanes => {
            handle_ipc_list_panes(windows)
        }
        IpcCommand::ClosePaneById(pane_id) => {
            handle_ipc_close_pane(windows, pane_id)
        }
        IpcCommand::SendKeys { pane_id, text } => {
            handle_ipc_send_keys(windows, pane_id, &text)
        }
        IpcCommand::FocusPane(pane_id) => {
            handle_ipc_focus_pane(windows, pane_id)
        }
        IpcCommand::NewTab { cwd, cmd } => {
            handle_ipc_new_tab(windows, config_cell, cwd, cmd)
        }
        IpcCommand::SetTabTitle { pane_id, title } => {
            handle_ipc_set_tab_title(windows, pane_id, title)
        }
    }
}

/// IPC: set the custom title of the tab containing `pane_id`.
fn handle_ipc_set_tab_title(
    windows: &RefCell<Vec<Retained<NSWindow>>>,
    pane_id: u32,
    title: Option<String>,
) -> crate::ipc::IpcResponse {
    use crate::ipc::IpcResponse;

    let wins = windows.borrow();
    for win in wins.iter() {
        let view = match kova_view(win) {
            Some(v) => v,
            None => continue,
        };
        if view.ipc_set_tab_title(pane_id, title.clone()) {
            return IpcResponse::Ok { data: None };
        }
    }

    IpcResponse::Error { message: format!("pane {} not found", pane_id) }
}

/// IPC: split the focused pane in the key window.
fn handle_ipc_split(
    windows: &RefCell<Vec<Retained<NSWindow>>>,
    config_cell: &OnceCell<Config>,
    direction: &str,
    command: Option<String>,
    cwd: Option<String>,
) -> crate::ipc::IpcResponse {
    use crate::ipc::IpcResponse;

    let config = match config_cell.get() {
        Some(c) => c,
        None => return IpcResponse::Error { message: "config not loaded".to_string() },
    };

    let wins = windows.borrow();
    // Find the key window (first one, or the one that isKeyWindow)
    let win = wins.iter()
        .find(|w| w.isKeyWindow())
        .or_else(|| wins.first());
    let win = match win {
        Some(w) => w,
        None => return IpcResponse::Error { message: "no window".to_string() },
    };
    let view = match kova_view(win) {
        Some(v) => v,
        None => return IpcResponse::Error { message: "no view".to_string() },
    };

    let split_dir = match direction {
        "vertical" => crate::pane::SplitDirection::Vertical,
        _ => crate::pane::SplitDirection::Horizontal,
    };

    // Determine CWD: explicit param > focused pane's CWD
    let effective_cwd = cwd.or_else(|| view.ipc_focused_cwd());

    let new_pane_id = view.ipc_split(config, split_dir, effective_cwd.as_deref(), command);
    match new_pane_id {
        Some(id) => IpcResponse::Ok {
            data: Some(serde_json::json!({"pane_id": id})),
        },
        None => IpcResponse::Error { message: "split failed".to_string() },
    }
}

/// IPC: list all panes across all windows.
fn handle_ipc_list_panes(
    windows: &RefCell<Vec<Retained<NSWindow>>>,
) -> crate::ipc::IpcResponse {
    use crate::ipc::IpcResponse;

    let wins = windows.borrow();
    let mut panes = Vec::new();
    for (win_idx, win) in wins.iter().enumerate() {
        let view = match kova_view(win) {
            Some(v) => v,
            None => continue,
        };
        let is_key = win.isKeyWindow();
        view.ipc_collect_panes(win_idx, is_key, &mut panes);
    }

    IpcResponse::Ok {
        data: Some(serde_json::Value::Array(panes)),
    }
}

/// IPC: close a pane by ID.
fn handle_ipc_close_pane(
    windows: &RefCell<Vec<Retained<NSWindow>>>,
    pane_id: u32,
) -> crate::ipc::IpcResponse {
    use crate::ipc::IpcResponse;

    let wins = windows.borrow();
    for win in wins.iter() {
        let view = match kova_view(win) {
            Some(v) => v,
            None => continue,
        };
        match view.ipc_close_pane(pane_id) {
            Some(true) => return IpcResponse::Ok { data: None },
            Some(false) => return IpcResponse::Error { message: format!("pane {} is the last pane — cannot close", pane_id) },
            None => continue,
        }
    }

    IpcResponse::Error { message: format!("pane {} not found", pane_id) }
}

/// IPC: send keystrokes to a pane's PTY.
fn handle_ipc_send_keys(
    windows: &RefCell<Vec<Retained<NSWindow>>>,
    pane_id: u32,
    text: &str,
) -> crate::ipc::IpcResponse {
    use crate::ipc::IpcResponse;

    let wins = windows.borrow();
    for win in wins.iter() {
        let view = match kova_view(win) {
            Some(v) => v,
            None => continue,
        };
        if view.ipc_send_keys(pane_id, text) {
            return IpcResponse::Ok { data: None };
        }
    }

    IpcResponse::Error { message: format!("pane {} not found", pane_id) }
}

/// IPC: focus a pane by ID (switch tab/window if needed).
fn handle_ipc_focus_pane(
    windows: &RefCell<Vec<Retained<NSWindow>>>,
    pane_id: u32,
) -> crate::ipc::IpcResponse {
    use crate::ipc::IpcResponse;

    let wins = windows.borrow();
    for win in wins.iter() {
        let view = match kova_view(win) {
            Some(v) => v,
            None => continue,
        };
        if view.ipc_focus_pane(pane_id) {
            win.makeKeyAndOrderFront(None);
            let app = NSApplication::sharedApplication(MainThreadMarker::new().unwrap());
            app.activate();
            return IpcResponse::Ok { data: None };
        }
    }

    IpcResponse::Error { message: format!("pane {} not found", pane_id) }
}

/// IPC: create a new tab in the key window.
fn handle_ipc_new_tab(
    windows: &RefCell<Vec<Retained<NSWindow>>>,
    config_cell: &OnceCell<Config>,
    cwd: Option<String>,
    cmd: Option<String>,
) -> crate::ipc::IpcResponse {
    use crate::ipc::IpcResponse;

    let config = match config_cell.get() {
        Some(c) => c,
        None => return IpcResponse::Error { message: "config not loaded".to_string() },
    };

    let wins = windows.borrow();
    let win = wins.iter()
        .find(|w| w.isKeyWindow())
        .or_else(|| wins.first());
    let win = match win {
        Some(w) => w,
        None => return IpcResponse::Error { message: "no window".to_string() },
    };
    let view = match kova_view(win) {
        Some(v) => v,
        None => return IpcResponse::Error { message: "no view".to_string() },
    };

    match view.ipc_new_tab(config, cwd.as_deref(), cmd) {
        Some((tab_id, pane_id)) => IpcResponse::Ok {
            data: Some(serde_json::json!({"tab_id": tab_id, "pane_id": pane_id})),
        },
        None => IpcResponse::Error { message: "failed to create tab".to_string() },
    }
}

fn setup_menu(mtm: MainThreadMarker) {
    let menu_bar = NSMenu::new(mtm);
    let app_menu_item = NSMenuItem::new(mtm);
    let app_menu = NSMenu::new(mtm);

    // Cmd+Q is handled in KovaView::performKeyEquivalent (close window, not app).
    // Menu item with empty key so it doesn't compete with performKeyEquivalent.
    let quit_item = unsafe {
        NSMenuItem::initWithTitle_action_keyEquivalent(
            mtm.alloc(),
            &NSString::from_str("Close Window"),
            None,
            &NSString::from_str(""),
        )
    };
    app_menu.addItem(&quit_item);

    app_menu_item.setSubmenu(Some(&app_menu));
    menu_bar.addItem(&app_menu_item);

    let app = NSApplication::sharedApplication(mtm);
    app.setMainMenu(Some(&menu_bar));
}
