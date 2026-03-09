mod app;
mod config;
mod input;
mod keybindings;
mod pane;
mod recent_projects;
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

/// Install signal handlers for SIGSEGV, SIGBUS, SIGABRT to log a backtrace before dying.
/// These signals bypass Rust's panic handler, so we need raw signal handlers.
fn install_crash_signal_handlers() {
    use std::io::Write;

    extern "C" fn crash_handler(sig: libc::c_int) {
        // We're in a signal handler — only async-signal-safe operations.
        // Writing to a pre-opened fd and _exit are safe. backtrace is not
        // strictly safe but it's our best effort for a dying process.
        let sig_name = match sig {
            libc::SIGSEGV => "SIGSEGV",
            libc::SIGBUS => "SIGBUS",
            libc::SIGABRT => "SIGABRT",
            _ => "UNKNOWN",
        };

        // Write to the log file directly (the logger may not be signal-safe)
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
        let path = format!("{}/Library/Logs/Kova/kova.log", home);
        if let Ok(mut f) = std::fs::OpenOptions::new().append(true).open(&path) {
            let _ = writeln!(f, "\n=== CRASH SIGNAL: {} ===", sig_name);
            let bt = std::backtrace::Backtrace::force_capture();
            let _ = writeln!(f, "{}", bt);
            let _ = writeln!(f, "=== END CRASH ===\n");
        }

        // Also try stderr
        let _ = eprintln!("\nKova crashed with signal: {}", sig_name);

        // Re-raise with default handler to get a proper core dump / exit code
        unsafe {
            libc::signal(sig, libc::SIG_DFL);
            libc::raise(sig);
        }
    }

    unsafe {
        libc::signal(libc::SIGSEGV, crash_handler as libc::sighandler_t);
        libc::signal(libc::SIGBUS, crash_handler as libc::sighandler_t);
        libc::signal(libc::SIGABRT, crash_handler as libc::sighandler_t);
    }
}

static ICON_DATA: &[u8] = include_bytes!("../assets/kova.icns");

/// Process RSS (Resident Set Size) in MB via mach API.
pub(crate) fn get_rss_mb() -> f64 {
    unsafe {
        let mut info: libc::mach_task_basic_info_data_t = std::mem::zeroed();
        let mut count = (std::mem::size_of::<libc::mach_task_basic_info_data_t>()
            / std::mem::size_of::<libc::natural_t>()) as u32;
        let kr = libc::task_info(
            libc::mach_task_self_,
            libc::MACH_TASK_BASIC_INFO,
            &mut info as *mut _ as *mut i32,
            &mut count,
        );
        if kr == 0 { info.resident_size as f64 / (1024.0 * 1024.0) } else { -1.0 }
    }
}

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
    let args: Vec<String> = std::env::args().collect();

    if args.iter().any(|a| a == "--list-sessions") {
        session::list_session_backups();
        return;
    }

    // --session N → restore session.N.json instead of session.json
    let session_backup = args.windows(2)
        .find(|w| w[0] == "--session")
        .and_then(|w| w[1].parse::<usize>().ok());

    setup_logging();
    install_crash_signal_handlers();

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

    let delegate = app::AppDelegate::new(mtm, config, session_backup);
    let delegate_proto = ProtocolObject::from_ref(&*delegate);
    app.setDelegate(Some(delegate_proto));

    app.run();
}
