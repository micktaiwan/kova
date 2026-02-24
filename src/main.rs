mod app;
mod config;
mod input;
mod pane;
mod renderer;
mod session;
mod terminal;
mod window;

use objc2::{AnyThread, runtime::ProtocolObject};
use objc2_app_kit::{NSApplication, NSApplicationActivationPolicy, NSImage};
use objc2_foundation::{MainThreadMarker, NSData};

use log::LevelFilter;
use simplelog::{CombinedLogger, Config, TermLogger, TerminalMode, WriteLogger};
use std::fs;
use std::path::PathBuf;

static ICON_DATA: &[u8] = include_bytes!("../assets/kova.icns");

fn setup_logging() {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    let log_dir = PathBuf::from(home).join("Library/Logs/Kova");
    fs::create_dir_all(&log_dir).expect("cannot create log dir");
    let log_file =
        fs::File::create(log_dir.join("kova.log")).expect("cannot create log file");

    let level = std::env::var("RUST_LOG")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(LevelFilter::Debug);

    let mut loggers: Vec<Box<dyn simplelog::SharedLogger>> =
        vec![WriteLogger::new(level, Config::default(), log_file)];

    // Stderr logger only when RUST_LOG is set (dev in terminal)
    if std::env::var("RUST_LOG").is_ok() {
        loggers.push(TermLogger::new(
            level,
            Config::default(),
            TerminalMode::Stderr,
            simplelog::ColorChoice::Auto,
        ));
    }

    CombinedLogger::init(loggers).expect("cannot init logger");
}

fn main() {
    setup_logging();

    // Log panics to file before aborting
    std::panic::set_hook(Box::new(|info| {
        log::error!("PANIC: {}", info);
    }));

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
