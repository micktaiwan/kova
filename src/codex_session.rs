//! Detect the Codex CLI session running inside a pane, so a quit/relaunch cycle
//! can bring it back — the same contract `claude_session` holds for Claude Code.
//!
//! Codex leaves no per-process file to read: a live session is only visible
//! through the transcript it holds open, `~/.codex/sessions/<Y>/<M>/<D>/
//! rollout-<timestamp>-<uuid>.jsonl`, whose name carries the id `codex resume`
//! expects. So the scan walks the process table for `codex` processes, asks
//! each one for its open files, and reads the id out of that path.
//!
//! Two consequences worth knowing. The scan is heavier than the Claude one (a
//! `proc_name` per live process, then the file descriptors of the few that
//! match), hence the shared cache below. And a session that has not written its
//! first record yet has no transcript open, so it stays invisible for a moment.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use parking_lot::Mutex;

/// How far up the process tree to look for the pane's shell.
const MAX_ANCESTRY_DEPTH: usize = 3;

/// The scan is re-run at most this often — same reason as in `claude_session`:
/// a snapshot asks once per pane, and the pane title asks once per frame.
const CACHE_TTL: Duration = Duration::from_secs(1);

/// Name `proc_name` reports for the CLI. Codex ships as a single binary, so
/// unlike Claude Code (a version-numbered file) the name is stable.
const PROCESS_NAME: &str = "codex";

static CACHE: Mutex<Option<(Instant, HashMap<u32, String>)>> = Mutex::new(None);

/// `PROC_PIDFDVNODEPATHINFO` — not in the `libc` crate, from `sys/proc_info.h`.
const PROC_PIDFDVNODEPATHINFO: libc::c_int = 2;

/// `PROC_ALL_PIDS`, from `sys/proc_info.h`; likewise absent from `libc`.
const PROC_ALL_PIDS: u32 = 1;

/// `struct proc_fileinfo`, likewise absent from `libc`. Only its size matters
/// here: it is the header `vnode_fdinfowithpath` puts before the path, so
/// getting it wrong would read the path at the wrong offset.
#[repr(C)]
#[derive(Default)]
struct ProcFileInfo {
    fi_openflags: u32,
    fi_status: u32,
    fi_offset: libc::off_t,
    fi_type: i32,
    fi_guardflags: u32,
}

/// `struct vnode_fdinfowithpath` — what `PROC_PIDFDVNODEPATHINFO` fills in.
#[repr(C)]
struct VnodeFdInfoWithPath {
    pfi: ProcFileInfo,
    vip: libc::vnode_info_path,
}

/// Parent PID of a live process.
fn parent_of(pid: u32) -> Option<u32> {
    unsafe {
        let mut info: libc::proc_bsdinfo = std::mem::zeroed();
        let ret = libc::proc_pidinfo(
            pid as i32,
            libc::PROC_PIDTBSDINFO,
            0,
            &mut info as *mut _ as *mut libc::c_void,
            std::mem::size_of::<libc::proc_bsdinfo>() as i32,
        );
        if ret <= 0 {
            return None;
        }
        Some(info.pbi_ppid)
    }
}

/// Short process name, as `ps -o comm` shows it.
fn process_name(pid: u32) -> Option<String> {
    let mut buf = [0u8; 256];
    let ret = unsafe {
        libc::proc_name(pid as i32, buf.as_mut_ptr() as *mut libc::c_void, buf.len() as u32)
    };
    if ret <= 0 {
        return None;
    }
    Some(String::from_utf8_lossy(&buf[..ret as usize]).to_string())
}

/// Every PID alive right now.
fn live_pids() -> Vec<u32> {
    // Ask for the size first, then read with room to spare: processes come and
    // go between the two calls.
    let bytes = unsafe { libc::proc_listpids(PROC_ALL_PIDS, 0, std::ptr::null_mut(), 0) };
    if bytes <= 0 {
        return Vec::new();
    }
    let mut pids = vec![0i32; bytes as usize / std::mem::size_of::<i32>() + 64];
    let size = (pids.len() * std::mem::size_of::<i32>()) as i32;
    let got = unsafe {
        libc::proc_listpids(PROC_ALL_PIDS, 0, pids.as_mut_ptr() as *mut libc::c_void, size)
    };
    if got <= 0 {
        return Vec::new();
    }
    let count = got as usize / std::mem::size_of::<i32>();
    pids[..count].iter().filter(|&&p| p > 0).map(|&p| p as u32).collect()
}

/// Paths of the regular files a process holds open.
fn open_files(pid: u32) -> Vec<String> {
    let bytes = unsafe {
        libc::proc_pidinfo(pid as i32, libc::PROC_PIDLISTFDS, 0, std::ptr::null_mut(), 0)
    };
    if bytes <= 0 {
        return Vec::new();
    }
    let count = bytes as usize / std::mem::size_of::<libc::proc_fdinfo>();
    let mut fds: Vec<libc::proc_fdinfo> = vec![unsafe { std::mem::zeroed() }; count + 16];
    let size = (fds.len() * std::mem::size_of::<libc::proc_fdinfo>()) as i32;
    let got = unsafe {
        libc::proc_pidinfo(
            pid as i32,
            libc::PROC_PIDLISTFDS,
            0,
            fds.as_mut_ptr() as *mut libc::c_void,
            size,
        )
    };
    if got <= 0 {
        return Vec::new();
    }
    let got = got as usize / std::mem::size_of::<libc::proc_fdinfo>();

    let mut paths = Vec::new();
    for fd in fds.iter().take(got) {
        if fd.proc_fdtype != libc::PROX_FDTYPE_VNODE as u32 {
            continue;
        }
        let mut info: VnodeFdInfoWithPath = unsafe { std::mem::zeroed() };
        let ret = unsafe {
            libc::proc_pidfdinfo(
                pid as i32,
                fd.proc_fd,
                PROC_PIDFDVNODEPATHINFO,
                &mut info as *mut _ as *mut libc::c_void,
                std::mem::size_of::<VnodeFdInfoWithPath>() as i32,
            )
        };
        if ret <= 0 {
            continue;
        }
        let raw = &info.vip.vip_path;
        // The path is a flat NUL-terminated buffer that `libc` declares as a
        // 32×32 matrix; flatten it back before reading.
        let bytes: Vec<u8> = raw
            .iter()
            .flat_map(|chunk| chunk.iter())
            .map(|&c| c as u8)
            .take_while(|&c| c != 0)
            .collect();
        if bytes.is_empty() {
            continue;
        }
        paths.push(String::from_utf8_lossy(&bytes).to_string());
    }
    paths
}

/// Pull the session id out of a rollout transcript path.
///
/// `…/sessions/2026/09/06/rollout-2026-09-06T12-43-35-01a07651-015e-78a3-97f2-2eaf0f0cd663.jsonl`
/// → `01a07651-015e-78a3-97f2-2eaf0f0cd663`. The id is a UUID, so it is the
/// last five dash-separated groups of the stem — the timestamp before it is
/// dash-separated too, which is why counting from the end is the only way.
pub fn session_id_from_rollout(path: &str) -> Option<String> {
    if !path.contains("/.codex/sessions/") || !path.ends_with(".jsonl") {
        return None;
    }
    let stem = path.rsplit('/').next()?.strip_suffix(".jsonl")?;
    let parts: Vec<&str> = stem.split('-').collect();
    if parts.len() < 5 {
        return None;
    }
    let id = parts[parts.len() - 5..].join("-");
    if is_safe_session_id(&id) && id.len() == 36 {
        Some(id)
    } else {
        None
    }
}

/// Build a map of "ancestor PID → Codex session id" by walking the live
/// processes named `codex` and reading the transcript each holds open.
fn scan_uncached() -> HashMap<u32, String> {
    let mut map = HashMap::new();
    let own_pid = std::process::id();

    for pid in live_pids() {
        if process_name(pid).as_deref() != Some(PROCESS_NAME) {
            continue;
        }
        let Some(id) = open_files(pid).iter().find_map(|p| session_id_from_rollout(p)) else {
            continue;
        };
        let mut current = pid;
        for _ in 0..MAX_ANCESTRY_DEPTH {
            let Some(parent) = parent_of(current) else { break };
            if parent <= 1 || parent == own_pid {
                break;
            }
            map.insert(parent, id.clone());
            current = parent;
        }
    }
    map
}

/// The Codex session id running under `shell_pid`, if any.
pub fn for_shell(shell_pid: u32) -> Option<String> {
    let mut cache = CACHE.lock();
    let fresh = match cache.as_ref() {
        Some((at, _)) => at.elapsed() < CACHE_TTL,
        None => false,
    };
    if !fresh {
        *cache = Some((Instant::now(), scan_uncached()));
    }
    cache.as_ref().and_then(|(_, map)| map.get(&shell_pid).cloned())
}

/// True if `id` is safe to splice into a command line — same guard, and same
/// reason, as on the Claude side: this string is typed into a PTY.
fn is_safe_session_id(id: &str) -> bool {
    !id.is_empty()
        && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// Build the command line that reopens `session_id`.
///
/// Unlike `claude --resume`, `codex resume <id>` does not need to run from the
/// directory the session started in, but the pane is restored there anyway.
pub fn resume_command(session_id: &str) -> Option<String> {
    if !is_safe_session_id(session_id) {
        log::warn!("Refusing to build a Codex resume line from a non-plain session id");
        return None;
    }
    Some(format!("codex resume {}", session_id))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_rollout_path_yields_its_uuid() {
        let path = "/Users/x/.codex/sessions/2026/09/06/rollout-2026-09-06T12-43-35-01a07651-015e-78a3-97f2-2eaf0f0cd663.jsonl";
        assert_eq!(
            session_id_from_rollout(path).as_deref(),
            Some("01a07651-015e-78a3-97f2-2eaf0f0cd663")
        );
    }

    #[test]
    fn other_files_a_codex_process_holds_open_are_not_sessions() {
        assert!(session_id_from_rollout("/Users/x/.codex/logs_2.sqlite").is_none());
        assert!(session_id_from_rollout(
            "/Users/x/.codex/thread-writer-locks/01a07651-015e-78a3-97f2-2eaf0f0cd663.lock"
        )
        .is_none());
        // Right directory, but not a transcript name.
        assert!(session_id_from_rollout("/Users/x/.codex/sessions/2026/09/06/notes.jsonl").is_none());
    }

    #[test]
    fn a_resume_line_refuses_anything_but_a_plain_id() {
        assert_eq!(
            resume_command("01a07651-015e-78a3-97f2-2eaf0f0cd663").as_deref(),
            Some("codex resume 01a07651-015e-78a3-97f2-2eaf0f0cd663")
        );
        // A newline would end the line and run the rest with no Enter pressed.
        assert!(resume_command("id\nrm -rf ~").is_none());
        assert!(resume_command("").is_none());
    }

    /// Not part of the suite: it reads the real process table of this machine.
    /// Run it by hand with
    /// `cargo test codex_session -- --ignored --nocapture` while a Codex session
    /// is open in a terminal.
    #[test]
    #[ignore]
    fn finds_the_codex_sessions_running_right_now() {
        let t0 = std::time::Instant::now();
        let map = scan_uncached();
        println!("scan took {:?}, {} ancestor(s) mapped", t0.elapsed(), map.len());
        for (pid, id) in &map {
            println!("  pid {} → {}", pid, id);
        }
    }
}
