use block2::RcBlock;
use objc2::rc::Retained;
use objc2::{define_class, msg_send, DefinedClass, MainThreadOnly, MainThreadMarker};
use objc2_app_kit::{NSApplication, NSApplicationDelegate, NSApplicationTerminateReply, NSMenu, NSMenuItem, NSWindow};
use objc2_foundation::{NSNotification, NSObject, NSObjectProtocol, NSRunLoop, NSRunLoopCommonModes, NSString, NSTimer};
use std::cell::{OnceCell, RefCell};
use std::ptr::NonNull;

use crate::config::Config;
use crate::window;

pub struct AppDelegateIvars {
    windows: RefCell<Vec<Retained<NSWindow>>>,
    /// Windows pending dealloc — kept alive one extra timer tick so AppKit
    /// finishes its run-loop work before the Retained is dropped.
    pending_close: RefCell<Vec<Retained<NSWindow>>>,
    /// Session data collected from windows as they close, so we don't lose
    /// their state when they're deallocated before app termination.
    closed_sessions: RefCell<Vec<crate::session::WindowSession>>,
    config: OnceCell<Config>,
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
            log::info!("Application launched");
            let mtm = MainThreadMarker::from(self);
            setup_menu(mtm);

            let config = self.ivars().config.get().unwrap();
            log::debug!("Config loaded: {}x{} cols/rows, {} scrollback", config.terminal.columns, config.terminal.rows, config.terminal.scrollback);

            // Restore session (all windows) or create a single fresh window
            let restored = crate::session::load()
                .and_then(|s| crate::session::restore_session(s, config));

            match restored {
                Some(windows) => {
                    log::info!("Restoring {} window(s) from session", windows.len());
                    let mut win_vec = self.ivars().windows.borrow_mut();
                    for (i, rw) in windows.into_iter().enumerate() {
                        let win = window::create_window(mtm, config, rw.tabs, rw.active_tab);
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
                    let win = window::create_window(mtm, config, vec![tab], 0);
                    win.makeKeyAndOrderFront(None);
                    self.ivars().windows.borrow_mut().push(win);
                }
            }

            let app = NSApplication::sharedApplication(mtm);
            app.activate();

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
            // Add still-live windows only
            let wins = self.ivars().windows.borrow();
            for win in wins.iter() {
                if let Some(view) = kova_view(win) {
                    view.append_session_data(&mut window_sessions);
                }
            }
            drop(wins);
            crate::session::save(&window_sessions);
            crate::terminal::pty::shutdown_all();
        }
    }
);

impl AppDelegate {
    pub fn new(mtm: MainThreadMarker, config: Config) -> Retained<Self> {
        let this = mtm.alloc::<Self>().set_ivars(AppDelegateIvars {
            windows: RefCell::new(Vec::new()),
            pending_close: RefCell::new(Vec::new()),
            closed_sessions: RefCell::new(Vec::new()),
            config: OnceCell::new(),
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

/// Get a reference to our AppDelegate from the shared NSApplication.
/// SAFETY: The app delegate is always our AppDelegate (set in main.rs).
fn app_delegate(mtm: MainThreadMarker) -> &'static AppDelegate {
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
    let win = window::create_window(mtm, config, vec![tab], 0);
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
    let win = window::create_window(mtm, config, vec![tab], 0);

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

/// Drain all tabs from `source_tabs` and append them to the first other window.
/// Returns `false` (no-op) if there is no other window; tabs are untouched in that case.
pub fn merge_tabs_from(
    mtm: MainThreadMarker,
    source_tabs: &std::cell::RefCell<Vec<crate::pane::Tab>>,
    source: &NSWindow,
) -> bool {
    let ad = app_delegate(mtm);
    let windows = ad.ivars().windows.borrow();
    let Some(target) = windows.iter().find(|w| !w.isEqual(Some(source))) else {
        return false;
    };
    let tabs: Vec<crate::pane::Tab> = source_tabs.borrow_mut().drain(..).collect();
    if let Some(view) = kova_view(target) {
        view.append_tabs(tabs);
    }
    target.makeKeyAndOrderFront(None);
    true
}

/// Cast the window's contentView to our KovaView.
/// SAFETY: contentView is always a KovaView (set in `create_window`).
pub fn kova_view(window: &NSWindow) -> Option<&crate::window::KovaView> {
    window.contentView().map(|cv| {
        let ptr: *const objc2_app_kit::NSView = &*cv;
        unsafe { &*(ptr as *const crate::window::KovaView) }
    })
}

fn setup_menu(mtm: MainThreadMarker) {
    let menu_bar = NSMenu::new(mtm);
    let app_menu_item = NSMenuItem::new(mtm);
    let app_menu = NSMenu::new(mtm);

    // Cmd+Q is handled in KovaView::performKeyEquivalent (close window, not app).
    // Menu item with empty key so it doesn't compete with performKeyEquivalent.
    let quit_title = NSString::from_str("Close Window");
    let quit_key = NSString::from_str("q");
    let quit_item = unsafe {
        NSMenuItem::initWithTitle_action_keyEquivalent(
            mtm.alloc(),
            &quit_title,
            None,
            &quit_key,
        )
    };
    app_menu.addItem(&quit_item);
    app_menu_item.setSubmenu(Some(&app_menu));
    menu_bar.addItem(&app_menu_item);

    let app = NSApplication::sharedApplication(mtm);
    app.setMainMenu(Some(&menu_bar));
}
