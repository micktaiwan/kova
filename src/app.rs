use objc2::rc::Retained;
use objc2::{define_class, msg_send, DefinedClass, MainThreadOnly, MainThreadMarker};
use objc2_app_kit::{NSAlert, NSAlertStyle, NSApplication, NSApplicationDelegate, NSApplicationTerminateReply, NSMenu, NSMenuItem, NSWindow};
use objc2_foundation::{NSNotification, NSObject, NSObjectProtocol, NSString};
use std::cell::OnceCell;

use crate::config::Config;
use crate::window;

pub struct AppDelegateIvars {
    window: OnceCell<Retained<NSWindow>>,
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
            let win = window::create_window(mtm, config);
            win.makeKeyAndOrderFront(None);

            let app = NSApplication::sharedApplication(mtm);
            app.activate();

            self.ivars().window.set(win).ok();
        }

        #[unsafe(method(applicationShouldTerminate:))]
        fn should_terminate(&self, _sender: &NSApplication) -> NSApplicationTerminateReply {
            let mtm = MainThreadMarker::from(self);
            if let Some(window) = self.ivars().window.get() {
                if let Some(view) = kova_view(window) {
                    let procs = view.running_processes();
                    if !procs.is_empty() {
                        let alert = NSAlert::new(mtm);
                        alert.setAlertStyle(NSAlertStyle::Warning);
                        alert.setMessageText(&NSString::from_str("Do you want to quit Kova?"));

                        let mut lines = String::from("The following processes are running:");
                        for (tab, name) in &procs {
                            lines.push_str(&format!("\n\u{2022} Tab \u{ab}{}\u{bb}: {}", tab, name));
                        }
                        alert.setInformativeText(&NSString::from_str(&lines));

                        alert.addButtonWithTitle(&NSString::from_str("Quit"));
                        alert.addButtonWithTitle(&NSString::from_str("Cancel"));

                        let response = alert.runModal();
                        // NSAlertFirstButtonReturn = 1000
                        if response == 1000 {
                            return NSApplicationTerminateReply::TerminateNow;
                        } else {
                            return NSApplicationTerminateReply::TerminateCancel;
                        }
                    }
                }
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
            if let Some(window) = self.ivars().window.get() {
                if let Some(view) = kova_view(window) {
                    view.save_session();
                }
            }
            crate::terminal::pty::shutdown_all();
        }
    }
);

impl AppDelegate {
    pub fn new(mtm: MainThreadMarker, config: Config) -> Retained<Self> {
        let this = mtm.alloc::<Self>().set_ivars(AppDelegateIvars {
            window: OnceCell::new(),
            config: OnceCell::new(),
        });
        let retained: Retained<Self> = unsafe { msg_send![super(this), init] };
        retained.ivars().config.set(config).ok();
        retained
    }
}

/// Cast the window's contentView to our KovaView.
/// SAFETY: contentView is always a KovaView (set in `create_window`).
fn kova_view(window: &NSWindow) -> Option<&crate::window::KovaView> {
    window.contentView().map(|cv| {
        let ptr: *const objc2_app_kit::NSView = &*cv;
        unsafe { &*(ptr as *const crate::window::KovaView) }
    })
}

fn setup_menu(mtm: MainThreadMarker) {
    let menu_bar = NSMenu::new(mtm);
    let app_menu_item = NSMenuItem::new(mtm);
    let app_menu = NSMenu::new(mtm);

    let quit_title = NSString::from_str("Quit Kova");
    let quit_key = NSString::from_str("q");
    let quit_item = unsafe {
        NSMenuItem::initWithTitle_action_keyEquivalent(
            mtm.alloc(),
            &quit_title,
            Some(objc2::sel!(terminate:)),
            &quit_key,
        )
    };
    app_menu.addItem(&quit_item);
    app_menu_item.setSubmenu(Some(&app_menu));
    menu_bar.addItem(&app_menu_item);

    let app = NSApplication::sharedApplication(mtm);
    app.setMainMenu(Some(&menu_bar));
}
