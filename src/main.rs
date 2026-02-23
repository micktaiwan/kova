mod app;
mod config;
mod input;
mod pane;
mod renderer;
mod terminal;
mod window;

use objc2::{AnyThread, runtime::ProtocolObject};
use objc2_app_kit::{NSApplication, NSApplicationActivationPolicy, NSImage};
use objc2_foundation::{MainThreadMarker, NSData};

static ICON_DATA: &[u8] = include_bytes!("../assets/kova.icns");

fn main() {
    env_logger::init();

    let config = config::Config::load();

    let mtm = MainThreadMarker::new().expect("must run on main thread");
    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(NSApplicationActivationPolicy::Regular);

    // Set app icon
    let data = NSData::with_bytes(ICON_DATA);
    if let Some(icon) = NSImage::initWithData(NSImage::alloc(), &data) {
        unsafe { app.setApplicationIconImage(Some(&icon)) };
    }

    let delegate = app::AppDelegate::new(mtm, config);
    let delegate_proto = ProtocolObject::from_ref(&*delegate);
    app.setDelegate(Some(delegate_proto));

    app.run();
}
