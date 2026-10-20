//! Kova's terminal, as a library (docs/embedding.md).
//!
//! Two clients share this code: Kova.app (`src/main.rs`, which drives
//! `app::AppDelegate`) and any other host that links the static library
//! through the C API of `capi` (`include/kova.h`) — e.g. a host app with one pane per document.

pub mod app;
pub mod capi;
pub mod config;
pub mod session;

mod agent_session;
mod anchors;
mod ask;
mod ask_card;
mod appkit_input;
mod bookmarks;
mod claude_history;
mod claude_session;
mod codex_session;
mod embed;
mod events;
mod input;
mod ipc;
mod keybindings;
mod notification;
mod pane;
mod pane_history;
mod raf;
mod recent_projects;
mod renderer;
mod resize_repair;
mod search_history;
mod surface;
mod tab_colors;
mod terminal;
mod update_check;
mod window;

/// `kova ask …` (see `ask::USAGE`): returns the exit code.
pub fn ask_cli(args: &[String]) -> i32 {
    ask::run_cli(args)
}

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
