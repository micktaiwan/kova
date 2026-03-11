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

/// Pre-opened fd for the crash log file, set once at startup.
/// Used by the signal handler which cannot safely open files.
static CRASH_LOG_FD: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(-1);

/// Install signal handlers for SIGSEGV, SIGBUS, SIGABRT to log before dying.
/// These signals bypass Rust's panic handler, so we need raw signal handlers.
fn install_crash_signal_handlers() {
    // Pre-open the log file for the signal handler (async-signal-safe requirement)
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    let path = format!("{}/Library/Logs/Kova/kova.log\0", home);
    let fd = unsafe {
        libc::open(path.as_ptr() as *const libc::c_char, libc::O_WRONLY | libc::O_APPEND | libc::O_CREAT, 0o644)
    };
    if fd >= 0 {
        CRASH_LOG_FD.store(fd, std::sync::atomic::Ordering::Relaxed);
    }

    extern "C" fn crash_handler(sig: libc::c_int) {
        // Only async-signal-safe operations: write() to pre-opened fd, raise, signal.
        let msg: &[u8] = match sig {
            libc::SIGSEGV => b"\n=== CRASH SIGNAL: SIGSEGV ===\n",
            libc::SIGBUS  => b"\n=== CRASH SIGNAL: SIGBUS ===\n",
            libc::SIGABRT => b"\n=== CRASH SIGNAL: SIGABRT ===\n",
            _             => b"\n=== CRASH SIGNAL: UNKNOWN ===\n",
        };

        let fd = CRASH_LOG_FD.load(std::sync::atomic::Ordering::Relaxed);
        if fd >= 0 {
            unsafe { libc::write(fd, msg.as_ptr() as *const libc::c_void, msg.len()) };
        }
        // Also write to stderr (fd 2 is always valid)
        unsafe { libc::write(2, msg.as_ptr() as *const libc::c_void, msg.len()) };

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
            #[allow(deprecated)]
            libc::mach_task_self_,
            libc::MACH_TASK_BASIC_INFO,
            &mut info as *mut _ as *mut i32,
            &mut count,
        );
        if kr == 0 { info.resident_size as f64 / (1024.0 * 1024.0) } else { -1.0 }
    }
}

const LOG_MAX_BYTES: u64 = 2 * 1024 * 1024; // 2 MB
const LOG_KEEP_BYTES: usize = 512 * 1024; // keep last 512 KB after rotation

fn rotate_log_if_needed(path: &std::path::Path) {
    let meta = match fs::metadata(path) {
        Ok(m) => m,
        Err(_) => return,
    };
    if meta.len() <= LOG_MAX_BYTES {
        return;
    }
    if let Ok(data) = fs::read(path) {
        let start = data.len().saturating_sub(LOG_KEEP_BYTES);
        // Find next newline to avoid cutting a line in half
        let start = data[start..].iter().position(|&b| b == b'\n')
            .map(|p| start + p + 1)
            .unwrap_or(start);
        let _ = fs::write(path, &data[start..]);
    }
}

fn setup_logging() {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    let log_dir = PathBuf::from(home).join("Library/Logs/Kova");
    fs::create_dir_all(&log_dir).expect("cannot create log dir");
    let log_path = log_dir.join("kova.log");
    rotate_log_if_needed(&log_path);
    let log_file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .expect("cannot open log file");

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
    log::info!("========== Kova starting ==========");
    install_crash_signal_handlers();

    // Log panics to file with backtrace before aborting
    std::panic::set_hook(Box::new(|info| {
        let bt = std::backtrace::Backtrace::force_capture();
        log::error!("PANIC: {}\n{}", info, bt);
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
