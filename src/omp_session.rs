//! Detect the omp session running inside a pane, so a quit/relaunch cycle can
//! bring it back — the same contract `claude_session`, `codex_session` and
//! `pi_session` hold for their agents.
//!
//! omp is a Pi fork and keeps Pi's transcript shape
//! (`~/.omp/agent/sessions/<bucket>/<created>_<id>.jsonl`), but it differs in
//! the two places that matter here:
//!
//! * **Which conversation.** omp holds its journal open with a writer for the
//!   whole life of the session, so the file descriptor of the process names the
//!   conversation exactly — no start-time guess, no "most recent file" lottery.
//!   That is the first source, and it works for a resumed conversation too.
//! * **The title.** omp keeps a fixed-width `type: "title"` slot as the first
//!   line and appends `title_change` records. Pi's `session_info` name does not
//!   exist here.
//!
//! The environment of omp's children (`PI_SESSION_FILE`, inherited from the Pi
//! bridge) is a second exact source for the moment the writer is not visible.
//! The start time is the last resort — a transcript created the instant the
//! process started — and never "the file written last", which names the other
//! pane's conversation: a fresh `omp` used to inherit it, and the window closed
//! the new pane as a duplicate of the pane next door (27/10/2026).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use parking_lot::Mutex;

/// How far up the process tree to look for the pane's shell.
const MAX_ANCESTRY_DEPTH: usize = 3;

/// `for_shell` is asked once per pane and per tick; same cache length as the
/// other agents.
const CACHE_TTL: Duration = Duration::from_secs(1);

/// Name `argv[0]` reports for the CLI.
const PROCESS_NAME: &str = "omp";

/// `PROC_PIDFDVNODEPATHINFO`, from `sys/proc_info.h`; absent from `libc`.
const PROC_PIDFDVNODEPATHINFO: libc::c_int = 2;

/// `MAXPATHLEN`, from `sys/param.h`.
const MAXPATHLEN: usize = 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Session {
    pub id: String,
    /// The title omp shows for the conversation: the fixed-width title slot,
    /// else the latest `title_change`, else the first user message.
    pub name: Option<String>,
}

/// The process scan's last answer, valid for `CACHE_TTL`.
static CACHE: Mutex<Option<(Instant, HashMap<u32, Session>)>> = Mutex::new(None);

/// Conversation ids learned when the writer was not visible, kept per process:
/// `pid → (process start, session id)`. The start time guards against PID reuse.
static LEARNED: Mutex<Option<HashMap<u32, (u64, String)>>> = Mutex::new(None);

/// `sys/proc_info.h`'s `struct proc_fileinfo`, absent from `libc`.
#[repr(C)]
#[derive(Clone, Copy)]
struct ProcFileInfo {
    fi_openflags: u32,
    fi_status: u32,
    fi_offset: i64,
    fi_type: i32,
    fi_guardflags: u32,
}

/// `struct vnode_info`.
#[repr(C)]
#[derive(Clone, Copy)]
struct VnodeInfo {
    vi_stat: libc::vinfo_stat,
    vi_type: i32,
    vi_pad: i32,
    vi_fsid: [i32; 2],
}

/// `struct vnode_info_path`.
#[repr(C)]
#[derive(Clone, Copy)]
struct VnodeInfoPath {
    vip_vi: VnodeInfo,
    vip_path: [libc::c_char; MAXPATHLEN],
}

/// `struct vnode_fdinfowithpath`.
#[repr(C)]
#[derive(Clone, Copy)]
struct VnodeFdInfoWithPath {
    pfi: ProcFileInfo,
    pvip: VnodeInfoPath,
}

/// The directory omp keeps the sessions of `cwd` in.
///
/// `session-paths.ts` encodes the canonical cwd three ways: `-<relative>` under
/// `$HOME`, `-tmp-<relative>` under the temp root, and `--<absolute without its
/// leading slash>--` otherwise. This must stay byte-identical: a mismatch only
/// means "no session found", so it fails quietly rather than wrongly.
pub fn session_dir(home: &Path, cwd: &str) -> PathBuf {
    let home = std::fs::canonicalize(home).unwrap_or_else(|_| home.to_path_buf());
    let cwd_path = std::fs::canonicalize(cwd).unwrap_or_else(|_| PathBuf::from(cwd));
    let encoded = |text: &str| -> String { text.replace(['/', '\\', ':'], "-") };
    let bucket = if let Ok(relative) = cwd_path.strip_prefix(&home) {
        format!("-{}", encoded(&relative.to_string_lossy()))
    } else {
        let temp =
            std::fs::canonicalize(std::env::temp_dir()).unwrap_or_else(|_| std::env::temp_dir());
        if let Ok(relative) = cwd_path.strip_prefix(&temp) {
            let relative = relative.to_string_lossy();
            if relative.is_empty() {
                "-tmp".to_string()
            } else {
                format!("-tmp-{}", encoded(&relative))
            }
        } else {
            let absolute = cwd_path.to_string_lossy();
            let stripped = absolute.strip_prefix('/').unwrap_or(&absolute);
            format!("--{}--", encoded(stripped))
        }
    };
    home.join(".omp/agent/sessions").join(bucket)
}

/// Conversation id out of an omp session file name.
///
/// `2026-10-27T15-54-09-646Z_01a17e11-ab6e-76b2-a4f3-e856a40686d7.jsonl`
/// → `01a17e11-ab6e-76b2-a4f3-e856a40686d7`. The timestamp has no underscore,
/// so the first one splits name from id.
pub fn session_id_from_file_name(name: &str) -> Option<String> {
    let (_, id) = name.strip_suffix(".jsonl")?.split_once('_')?;
    is_safe_session_id(id).then(|| id.to_string())
}

/// One candidate conversation of a directory. Same shape as Pi's, because the
/// file naming is the same.
type Candidate = crate::pi_session::Candidate;

fn candidates(dir: &Path) -> Vec<Candidate> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some(id) = session_id_from_file_name(&name) else {
            continue;
        };
        let Ok(meta) = entry.metadata() else { continue };
        let created = meta
            .created()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let modified = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs())
            .unwrap_or(0);
        out.push(Candidate {
            id,
            path: entry.path(),
            created,
            modified,
        });
    }
    out
}

/// The transcript omp holds open in `pid`, if any.
///
/// The journal writer stays open for the session's lifetime, so this is the
/// exact conversation — including one resumed from an older file. A subagent
/// session can be open at the same time; its header carries `parentSession`,
/// and the interactive parent is preferred.
fn open_transcript_in(pid: u32, omp_root: &Path) -> Option<PathBuf> {
    let mut size = unsafe {
        libc::proc_pidinfo(
            pid as i32,
            libc::PROC_PIDLISTFDS,
            0,
            std::ptr::null_mut(),
            0,
        )
    };
    if size <= 0 {
        return None;
    }
    let count = size as usize / std::mem::size_of::<libc::proc_fdinfo>();
    let mut fds = vec![
        libc::proc_fdinfo {
            proc_fd: 0,
            proc_fdtype: 0
        };
        count + 16
    ];
    let want = (fds.len() * std::mem::size_of::<libc::proc_fdinfo>()) as i32;
    size = unsafe {
        libc::proc_pidinfo(
            pid as i32,
            libc::PROC_PIDLISTFDS,
            0,
            fds.as_mut_ptr() as *mut libc::c_void,
            want,
        )
    };
    if size <= 0 {
        return None;
    }
    let count = size as usize / std::mem::size_of::<libc::proc_fdinfo>();

    let mut best: Option<(bool, u64, PathBuf)> = None;
    for fd in &fds[..count] {
        let mut info: VnodeFdInfoWithPath = unsafe { std::mem::zeroed() };
        let got = unsafe {
            libc::proc_pidfdinfo(
                pid as i32,
                fd.proc_fd,
                PROC_PIDFDVNODEPATHINFO,
                &mut info as *mut _ as *mut libc::c_void,
                std::mem::size_of::<VnodeFdInfoWithPath>() as i32,
            )
        };
        if got <= 0 {
            continue;
        }
        let raw = unsafe { std::ffi::CStr::from_ptr(info.pvip.vip_path.as_ptr()) };
        let path = PathBuf::from(raw.to_string_lossy().into_owned());
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        // `~/.omp` covers the default agent dir and every profile; a custom
        // `--session-dir` elsewhere is not found, and the pane keeps its last
        // command.
        if session_id_from_file_name(name).is_none() || !path.starts_with(omp_root) {
            continue;
        }
        let meta = std::fs::metadata(&path).ok();
        let modified = meta
            .as_ref()
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs())
            .unwrap_or(0);
        // A subagent's session names its parent; the interactive one does not.
        let parented = header_has_parent(&path);
        // Non-parented first (the interactive session), then the newest.
        let better = match &best {
            None => true,
            Some((parent, seen, _)) => (!parented, modified) > (!*parent, *seen),
        };
        if better {
            best = Some((parented, modified, path));
        }
    }
    best.map(|(_, _, path)| path)
}

/// True when the session header names a parent session (a subagent).
fn header_has_parent(path: &Path) -> bool {
    use std::io::Read;
    let Ok(mut file) = std::fs::File::open(path) else {
        return false;
    };
    let mut head = Vec::new();
    if file.by_ref().take(4096).read_to_end(&mut head).is_err() {
        return false;
    }
    for line in head.split(|&b| b == b'\n') {
        let Ok(entry) = serde_json::from_slice::<serde_json::Value>(line) else {
            continue;
        };
        if entry["type"] == "session" {
            return entry.get("parentSession").is_some();
        }
    }
    false
}

/// What the transcript scanned so far says about the conversation's title.
///
/// The title slot can be rewritten in place, so it is read fresh on every call
/// (256 bytes, no scan). `title_change` and the first user message are appended,
/// and the byte offset keeps that pass to the new lines only.
#[derive(Clone, Default)]
struct TitleParts {
    scanned: u64,
    /// Latest `title_change` or header title.
    named: Option<String>,
    /// Title carried by the session header, used when no slot exists.
    header: Option<String>,
    /// First user message.
    first: Option<String>,
}

/// Session file → title parts read so far.
static TITLES: Mutex<Option<HashMap<PathBuf, TitleParts>>> = Mutex::new(None);

/// One pass at a transcript reads at most this much.
const MAX_TITLE_SCAN_BYTES: u64 = 4 * 1024 * 1024;

/// The title omp shows for the conversation at `path`: the fixed-width title
/// slot, else the latest `/rename`, else the first user message. `None` for a
/// conversation nobody named and nobody has typed into yet.
pub fn title_of(path: &Path) -> Option<String> {
    use std::io::{Read, Seek, SeekFrom};

    let slot = title_slot(path);
    let mut cache = TITLES.lock();
    let parts = cache
        .get_or_insert_with(HashMap::new)
        .entry(path.to_path_buf())
        .or_default();
    let mut file = std::fs::File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    if parts.scanned > len {
        *parts = TitleParts::default();
    }
    if len > parts.scanned {
        let want = (len - parts.scanned).min(MAX_TITLE_SCAN_BYTES);
        file.seek(SeekFrom::Start(parts.scanned)).ok()?;
        let mut buf = vec![0u8; want as usize];
        let read = file.read(&mut buf).ok()?;
        buf.truncate(read);
        let whole = buf.iter().rposition(|&b| b == b'\n').map_or(0, |i| i + 1);
        for line in buf[..whole].split(|&b| b == b'\n') {
            if !line.is_empty() {
                read_title_line(line, parts);
            }
        }
        parts.scanned += if whole > 0 {
            whole as u64
        } else if buf.len() as u64 >= MAX_TITLE_SCAN_BYTES {
            buf.len() as u64
        } else {
            0
        };
    }
    slot.or_else(|| parts.named.clone())
        .or_else(|| parts.header.clone())
        .or_else(|| parts.first.clone())
}

/// The conversation of `cwd` whose title is the one `pane_title` ends with.
///
/// Last resort when neither the process nor its environment named the
/// conversation when the snapshot was taken: a Kova built before this agent was
/// known, or a probe that missed. The title omp writes on the terminal survives
/// in the session file, and every transcript on disk still carries its own — so
/// the pane can still be handed back the right conversation.
///
/// A unique match, or nothing: a title two conversations share names neither,
/// and a pane at a shell prompt keeps its bare `omp` like before rather than
/// reopening someone else's work. The id goes through `is_safe_session_id` (via
/// `session_id_from_file_name`), so it is safe to type into the shell.
pub fn session_by_title(home: &Path, cwd: &str, pane_title: &str) -> Option<Session> {
    let dir = session_dir(home, cwd);
    let mut found: Option<Session> = None;
    for candidate in candidates(&dir) {
        let Some(title) = title_of(&candidate.path) else {
            continue;
        };
        if !title_belongs_to(pane_title, &title) {
            continue;
        }
        if found.is_some() {
            // Two conversations carry this title: it names neither.
            return None;
        }
        found = Some(Session { id: candidate.id, name: Some(title) });
    }
    found
}

/// True when `pane_title` is `title`, or ends with it after a separator.
///
/// The terminal title omp sets carries a prefix of its own (`π > `), which this
/// must not have to know: only the conversation title is compared. The
/// separator rule keeps a short title from matching the tail of a word.
fn title_belongs_to(pane_title: &str, title: &str) -> bool {
    if title.is_empty() {
        return false;
    }
    if pane_title == title {
        return true;
    }
    pane_title
        .strip_suffix(title)
        .is_some_and(|prefix| prefix.ends_with(|c: char| !c.is_alphanumeric()))
}

/// The title carried by the fixed-width first line, if any.
fn title_slot(path: &Path) -> Option<String> {
    use std::io::Read;
    let mut file = std::fs::File::open(path).ok()?;
    let mut buf = [0u8; 512];
    let read = file.read(&mut buf).ok()?;
    let line = buf[..read].split(|&b| b == b'\n').next()?;
    let entry: serde_json::Value = serde_json::from_slice(line).ok()?;
    (entry["type"] == "title")
        .then(|| entry["title"].as_str().unwrap_or("").trim().to_string())
        .filter(|title| !title.is_empty())
}

/// Fold one complete transcript line into `parts`.
fn read_title_line(line: &[u8], parts: &mut TitleParts) {
    // Cheap gates first: a transcript is mostly tool output.
    let named = line.windows(7).any(|w| w == b"\"title\"")
        || line.windows(14).any(|w| w == b"\"title_change\"");
    let header = line.windows(16).any(|w| w == b"\"type\":\"session\"");
    if !named && !header && parts.first.is_some() {
        return;
    }
    if !named && !header && !line.windows(6).any(|w| w == b"\"user\"") {
        return;
    }
    let Ok(entry) = serde_json::from_slice::<serde_json::Value>(line) else {
        return;
    };
    match entry["type"].as_str().unwrap_or_default() {
        "title" | "title_change" => {
            let title = entry["title"].as_str().unwrap_or("").trim();
            parts.named = (!title.is_empty()).then(|| title.to_string());
        }
        "session" => {
            let title = entry["title"].as_str().map(str::trim).unwrap_or_default();
            if !title.is_empty() {
                parts.header = Some(title.to_string());
            }
        }
        "message" if parts.first.is_none() && entry["message"]["role"] == "user" => {
            let Some(text) = user_message_text(&entry["message"]["content"]) else {
                return;
            };
            let title = crate::claude_history::title_from_prompt(&text);
            if !title.trim().is_empty() {
                parts.first = Some(title);
            }
        }
        _ => {}
    }
}

/// Text of an omp message's content: a plain string, or the text blocks of the
/// array form (images and tool calls carry no title).
fn user_message_text(content: &serde_json::Value) -> Option<String> {
    if let Some(text) = content.as_str() {
        return Some(text.to_string());
    }
    let parts: Vec<&str> = content
        .as_array()?
        .iter()
        .filter(|block| block["type"] == "text")
        .filter_map(|block| block["text"].as_str())
        .collect();
    (!parts.is_empty()).then(|| parts.join(" "))
}

/// The session id a process carries in its environment. omp inherits Pi's
/// `PI_SESSION_ID`; `PI_SESSION_FILE` names the transcript itself.
pub fn session_id_in_env_blob(blob: &[u8]) -> Option<String> {
    const FILE: &[u8] = b"PI_SESSION_FILE=";
    const ID: &[u8] = b"PI_SESSION_ID=";
    blob.split(|&b| b == 0)
        .filter_map(|entry| {
            if let Some(path) = entry.strip_prefix(FILE) {
                let path = std::str::from_utf8(path).ok()?;
                return Path::new(path)
                    .file_name()
                    .and_then(|n| n.to_str())
                    .and_then(session_id_from_file_name);
            }
            let id = entry.strip_prefix(ID)?;
            let id = std::str::from_utf8(id).ok()?.trim();
            is_safe_session_id(id).then(|| id.to_string())
        })
        .next()
}

/// True if the block belongs to an omp process: `argv[0]` is `omp`.
pub fn is_omp_process(blob: &[u8]) -> bool {
    let Some(argv0) = crate::pi_session::argv0_in_blob(blob) else {
        return false;
    };
    argv0.rsplit('/').next() == Some(PROCESS_NAME)
}

/// True if `id` is safe to splice into a command line — this string is typed
/// into a PTY.
fn is_safe_session_id(id: &str) -> bool {
    !id.is_empty()
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// The conversation `omp` holds, from the most exact source to the least.
fn session_of(pid: u32, start: u64, cwd: &str, home: &Path) -> Option<Session> {
    let root = home.join(".omp");
    let dir = session_dir(home, cwd);
    if let Some(path) = open_transcript_in(pid, &root) {
        let id = session_id_from_file_name(path.file_name()?.to_str()?)?;
        LEARNED
            .lock()
            .get_or_insert_with(HashMap::new)
            .insert(pid, (start, id.clone()));
        return Some(titled(id, path));
    }
    let from_child = crate::pi_session::children_of(pid)
        .into_iter()
        .find_map(|child| {
            crate::pi_session::args_and_env_of(child).and_then(|blob| session_id_in_env_blob(&blob))
        });
    if let Some(id) = from_child {
        LEARNED
            .lock()
            .get_or_insert_with(HashMap::new)
            .insert(pid, (start, id.clone()));
        return Some(titled_in(id, &dir));
    }
    if let Some(id) = LEARNED
        .lock()
        .as_ref()
        .and_then(|map| map.get(&pid))
        .filter(|(known_start, _)| *known_start == start)
        .map(|(_, id)| id.clone())
    {
        return Some(titled_in(id, &dir));
    }
    created_at(&dir, start).map(|id| titled_in(id, &dir))
}

/// The conversation a process started at `started` created in `dir`, if any.
///
/// Only a transcript created at the process's own start names it. A transcript
/// merely *written last* names whoever wrote it — on this machine, the other
/// pane still holding that conversation — and inheriting it is not a near miss:
/// a brand-new `omp` came back as the pane next door's conversation, the window
/// saw one conversation in two panes and closed the new pane as a duplicate, so
/// no fresh `omp` could be opened at all (27/10/2026). A process with no
/// transcript of its own yet holds a conversation nobody can name: `None`.
fn created_at(dir: &Path, started: u64) -> Option<String> {
    crate::pi_session::pick(&candidates(dir), started)
}

/// A session with the title omp wrote, given the conversation id.
fn titled(id: String, path: PathBuf) -> Session {
    Session {
        id,
        name: title_of(&path),
    }
}

/// A session whose transcript is looked up by id in `dir`.
fn titled_in(id: String, dir: &Path) -> Session {
    let known = candidates(dir);
    let name = known
        .iter()
        .find(|c| c.id == id)
        .and_then(|c| title_of(&c.path));
    Session { id, name }
}

/// Short process name, as `proc_name` reports it.
fn process_name(pid: u32) -> Option<String> {
    let mut buf = [0u8; 256];
    let ret = unsafe {
        libc::proc_name(
            pid as i32,
            buf.as_mut_ptr() as *mut libc::c_void,
            buf.len() as u32,
        )
    };
    if ret <= 0 {
        return None;
    }
    Some(String::from_utf8_lossy(&buf[..ret as usize]).to_string())
}

/// Map "ancestor PID → omp session" over the live omp processes.
fn scan_uncached() -> HashMap<u32, Session> {
    let mut map: HashMap<u32, (usize, Session)> = HashMap::new();
    let own_pid = std::process::id();
    let home = PathBuf::from(std::env::var_os("HOME").unwrap_or_default());

    let pids: Vec<u32> = live_pids()
        .into_iter()
        .filter(|&pid| process_name(pid).is_some_and(|name| name == PROCESS_NAME))
        .collect();
    {
        if let Some(learned) = LEARNED.lock().as_mut() {
            learned.retain(|pid, _| pids.contains(pid));
        }
    }

    for pid in pids {
        let Some(blob) = crate::pi_session::args_and_env_of(pid) else {
            continue;
        };
        if !is_omp_process(&blob) {
            continue;
        }
        let Some(info) = crate::pi_session::bsd_info(pid) else {
            continue;
        };
        let Some(cwd) = crate::pi_session::cwd_of(pid) else {
            continue;
        };
        let Some(session) = session_of(pid, info.pbi_start_tvsec, &cwd, &home) else {
            continue;
        };
        let mut current = pid;
        for depth in 0..MAX_ANCESTRY_DEPTH {
            let Some(parent) = crate::pi_session::parent_of(current) else {
                break;
            };
            if parent <= 1 || parent == own_pid {
                break;
            }
            // A subagent's process sits below the interactive one. Keep the
            // shallowest owner of an ancestor, so its session never gets
            // overwritten by a child that merely shares the ancestry.
            let entry = map.entry(parent).or_insert((depth, session.clone()));
            if depth < entry.0 {
                *entry = (depth, session.clone());
            }
            current = parent;
        }
    }
    map.into_iter()
        .map(|(pid, (_, session))| (pid, session))
        .collect()
}

/// The omp session running under `shell_pid`, if any.
pub fn for_shell(shell_pid: u32) -> Option<Session> {
    let mut cache = CACHE.lock();
    let fresh = cache
        .as_ref()
        .is_some_and(|(at, _)| at.elapsed() < CACHE_TTL);
    if !fresh {
        *cache = Some((Instant::now(), scan_uncached()));
    }
    let mut guard = cache
        .as_ref()
        .and_then(|(_, map)| map.get(&shell_pid).cloned());
    if guard.is_none() {
        guard = detect(shell_pid);
    }
    guard
}

/// Look for omp under `shell_pid` without the cache.
fn detect(shell_pid: u32) -> Option<Session> {
    let home = PathBuf::from(std::env::var_os("HOME").unwrap_or_default());
    let mut frontier = crate::pi_session::children_of(shell_pid);
    for _ in 0..2 {
        let mut next = Vec::new();
        for pid in frontier {
            let Some(blob) = crate::pi_session::args_and_env_of(pid) else {
                continue;
            };
            if is_omp_process(&blob) {
                let info = crate::pi_session::bsd_info(pid)?;
                let cwd = crate::pi_session::cwd_of(pid)?;
                return session_of(pid, info.pbi_start_tvsec, &cwd, &home);
            }
            next.extend(crate::pi_session::children_of(pid));
        }
        frontier = next;
    }
    None
}

/// Every PID alive right now.
fn live_pids() -> Vec<u32> {
    crate::pi_session::live_pids()
}

/// The command line that reopens `session_id`, keeping the other flags the pane
/// was started with (`omp --model x` → `omp --resume <id> --model x`).
pub fn resume_command(last_command: Option<&str>, session_id: &str) -> Option<String> {
    if !is_safe_session_id(session_id) {
        log::warn!("Refusing to build a resume line from a non-plain omp session id");
        return None;
    }
    let base = last_command
        .map(str::trim)
        .filter(|cmd| is_omp_invocation(cmd))
        .map(strip_session_flags)
        .filter(|tokens| !tokens.is_empty())
        .unwrap_or_else(|| vec![PROCESS_NAME.to_string()]);
    let (command, flags) = base.split_first()?;
    let mut line = vec![
        command.clone(),
        "--resume".to_string(),
        session_id.to_string(),
    ];
    line.extend(flags.iter().cloned());
    Some(line.join(" "))
}

/// True if `cmd` runs omp, and nothing but it.
pub(crate) fn is_omp_invocation(cmd: &str) -> bool {
    let Some(first) = cmd.split_whitespace().next() else {
        return false;
    };
    if cmd.contains('|') || cmd.contains(';') || cmd.contains('&') {
        return false;
    }
    first == PROCESS_NAME || first.ends_with("/omp")
}

/// omp flags that choose a conversation, with the value that follows them.
const OMP_FLAGS_WITH_VALUE: [&str; 4] = ["--resume", "-r", "--session", "--fork"];
/// omp flags that choose a conversation on their own.
const OMP_FLAGS_ALONE: [&str; 6] = [
    "-c",
    "--continue",
    "--restore",
    "--no-session",
    "--from-claude",
    "--from-codex",
];

/// Drop the flags that select a conversation, so appending `--resume <id>`
/// cannot collide with what the previous invocation already carried.
fn strip_session_flags(cmd: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut tokens = cmd.split_whitespace();
    while let Some(token) = tokens.next() {
        if OMP_FLAGS_WITH_VALUE.contains(&token) {
            tokens.next();
            continue;
        }
        if OMP_FLAGS_ALONE.contains(&token)
            || token.starts_with("--resume=")
            || token.starts_with("--session=")
        {
            continue;
        }
        out.push(token.to_string());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_session_directory_follows_omps_own_encoding() {
        let home = Path::new("/Users/me");
        assert_eq!(
            session_dir(home, "/Users/me/projects/kova"),
            Path::new("/Users/me/.omp/agent/sessions/-projects-kova")
        );
        assert_eq!(
            session_dir(home, "/opt/work/app"),
            Path::new("/Users/me/.omp/agent/sessions/--opt-work-app--")
        );
    }

    #[test]
    fn the_conversation_id_is_read_after_the_first_underscore() {
        assert_eq!(
            session_id_from_file_name(
                "2026-10-27T15-54-09-646Z_01a17e11-ab6e-76b2-a4f3-e856a40686d7.jsonl"
            )
            .as_deref(),
            Some("01a17e11-ab6e-76b2-a4f3-e856a40686d7")
        );
        assert_eq!(session_id_from_file_name("notes.txt"), None);
        assert_eq!(
            session_id_from_file_name("2026-01-01T00-00-00-000Z_../../etc/passwd.jsonl"),
            None
        );
    }

    fn transcript(test: &str, lines: &[&str]) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "kova-omp-title-{}-{test}.jsonl",
            std::process::id()
        ));
        std::fs::write(&path, lines.join("\n") + "\n").unwrap();
        path
    }

    const SLOT: &str = r#"{"type":"title","v":1,"title":"Titre auto","source":"auto","updatedAt":"2026-10-27T15:55:22.981Z"}"#;
    const HEADER: &str = r#"{"type":"session","version":3,"id":"abc","timestamp":"2026-10-27T15:54:09.646Z","cwd":"/w"}"#;

    #[test]
    fn the_title_slot_names_the_conversation_first() {
        let path = transcript(
            "slot",
            &[
                SLOT,
                HEADER,
                r#"{"type":"message","id":"m1","parentId":null,"timestamp":"t","message":{"role":"user","content":"premier prompt"}}"#,
            ],
        );
        assert_eq!(title_of(&path).as_deref(), Some("Titre auto"));
    }

    #[test]
    fn a_title_change_wins_over_the_first_message() {
        let path = transcript(
            "change",
            &[
                r#"{"type":"title","v":1,"title":"","source":"auto","pad":""}"#,
                HEADER,
                r#"{"type":"message","id":"m1","parentId":null,"timestamp":"t","message":{"role":"user","content":"premier prompt"}}"#,
                r#"{"type":"title_change","id":"t1","parentId":"m1","timestamp":"t","title":"Renommé","source":"user"}"#,
            ],
        );
        assert_eq!(title_of(&path).as_deref(), Some("Renommé"));
    }

    #[test]
    fn a_cleared_title_falls_back_to_the_first_message() {
        let path = transcript(
            "clear",
            &[
                r#"{"type":"title","v":1,"title":"","source":"auto","pad":""}"#,
                HEADER,
                r#"{"type":"message","id":"m1","parentId":null,"timestamp":"t","message":{"role":"user","content":[{"type":"text","text":"lis les modifs"}]}}"#,
                r#"{"type":"title_change","id":"t1","parentId":"m1","timestamp":"t","title":"","source":"user"}"#,
            ],
        );
        assert_eq!(title_of(&path).as_deref(), Some("lis les modifs"));
    }

    #[test]
    fn a_live_session_only_has_its_new_lines_read() {
        use std::io::Write;

        let path = transcript(
            "append",
            &[
                r#"{"type":"title","v":1,"title":"","source":"auto","pad":""}"#,
                HEADER,
                r#"{"type":"message","id":"m1","parentId":null,"timestamp":"t","message":{"role":"user","content":"premier prompt"}}"#,
            ],
        );
        assert_eq!(title_of(&path).as_deref(), Some("premier prompt"));
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        writeln!(
            file,
            r#"{{"type":"title_change","id":"t1","parentId":"m1","timestamp":"t","title":"Renommé","source":"user"}}"#
        )
        .unwrap();
        assert_eq!(title_of(&path).as_deref(), Some("Renommé"));
    }

    #[test]
    fn a_half_written_line_is_not_read_yet() {
        use std::io::Write;

        let path = transcript(
            "partial",
            &[
                r#"{"type":"title","v":1,"title":"","source":"auto","pad":""}"#,
                HEADER,
            ],
        );
        assert_eq!(title_of(&path), None);
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        write!(file, r#"{{"type":"title_change","id":"t1","title":"Tit"#).unwrap();
        assert_eq!(title_of(&path), None);
        writeln!(file, "{}", r#"re"}"#).unwrap();
        assert_eq!(title_of(&path).as_deref(), Some("Titre"));
    }

    fn blob(args: &[&str], env: &[&str]) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&(args.len() as i32).to_ne_bytes());
        out.extend_from_slice(b"/opt/homebrew/bin/omp");
        out.extend_from_slice(b"\0\0\0");
        for arg in args {
            out.extend_from_slice(arg.as_bytes());
            out.push(0);
        }
        for entry in env {
            out.extend_from_slice(entry.as_bytes());
            out.push(0);
        }
        out
    }

    #[test]
    fn an_omp_process_is_recognised_by_its_argv0() {
        assert!(is_omp_process(&blob(&["omp"], &[])));
        assert!(!is_omp_process(&blob(&["pi"], &[])));
    }

    #[test]
    fn the_session_file_is_read_out_of_a_child_environment() {
        let blob = blob(
            &["/bin/bash", "-c"],
            &[
                "PI_SESSION_FILE=/Users/me/.omp/agent/sessions/-w/t_01a17e11-ab6e-76b2-a4f3-e856a40686d7.jsonl",
            ],
        );
        assert_eq!(
            session_id_in_env_blob(&blob).as_deref(),
            Some("01a17e11-ab6e-76b2-a4f3-e856a40686d7")
        );
    }

    #[test]
    fn the_resume_line_keeps_the_other_flags() {
        assert_eq!(
            resume_command(Some("omp"), "abc").as_deref(),
            Some("omp --resume abc")
        );
        assert_eq!(
            resume_command(Some("omp --model deepseek-flash"), "abc").as_deref(),
            Some("omp --resume abc --model deepseek-flash")
        );
        assert_eq!(
            resume_command(Some("omp --model x -c"), "abc").as_deref(),
            Some("omp --resume abc --model x")
        );
        assert_eq!(
            resume_command(Some("omp --session old --profile work"), "abc").as_deref(),
            Some("omp --resume abc --profile work")
        );
        assert_eq!(
            resume_command(Some("vim foo"), "abc").as_deref(),
            Some("omp --resume abc")
        );
        assert_eq!(
            resume_command(Some("omp | tee log"), "abc").as_deref(),
            Some("omp --resume abc")
        );
    }

    #[test]
    fn a_non_plain_id_never_reaches_the_shell() {
        assert_eq!(resume_command(Some("omp"), "abc; rm -rf ~"), None);
        assert_eq!(resume_command(Some("omp"), ""), None);
    }

    /// A home of its own, created before anything asks for a bucket:
    /// `session_dir` canonicalizes it, and `/var` → `/private/var` would
    /// otherwise split the same directory in two.
    fn home_for(test: &str) -> PathBuf {
        let home = std::env::temp_dir().join(format!("kova-omp-home-{}-{test}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).unwrap();
        home
    }

    /// One stored conversation of `cwd`, with the title slot omp writes first.
    fn stored(home: &Path, cwd: &str, id: &str, title: &str) {
        let dir = session_dir(home, cwd);
        std::fs::create_dir_all(&dir).unwrap();
        let slot = serde_json::json!({"type": "title", "v": 1, "title": title, "source": "auto"});
        std::fs::write(
            dir.join(format!("2026-10-27T15-54-09-646Z_{id}.jsonl")),
            format!("{slot}\n"),
        )
        .unwrap();
    }

    #[test]
    fn a_pane_title_names_its_conversation_when_no_process_can() {
        let home = home_for("by-title");
        let cwd = "/nowhere/proj";
        let id = "01a17eab-44d5-7315-878f-1c81149f5013";
        stored(&home, cwd, id, "🔑 PRIV: Passe sur les besoins en API privées");

        // omp prefixes the terminal title with a mark of its own: only the
        // conversation title has to match.
        let found = session_by_title(&home, cwd, "π > 🔑 PRIV: Passe sur les besoins en API privées")
            .expect("the pane title names the conversation");
        assert_eq!(found.id, id);
        assert_eq!(found.name.as_deref(), Some("🔑 PRIV: Passe sur les besoins en API privées"));

        assert!(session_by_title(&home, cwd, "π > 🪟 PANE: Reprendre la conv du pane 97").is_none());
        // Not a conversation of this directory.
        assert!(session_by_title(&home, "/nowhere/else", "π > 🔑 PRIV: Passe sur les besoins en API privées").is_none());
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn a_conversation_title_only_matches_after_a_separator() {
        assert!(title_belongs_to("π > notes", "notes"));
        assert!(title_belongs_to("notes", "notes"));
        assert!(!title_belongs_to("footnotes", "notes"), "a word tail is not a title");
        assert!(title_belongs_to("π > v2: notes", "notes"));
        assert!(!title_belongs_to("π > notes", ""));
    }

    #[test]
    fn a_title_two_conversations_share_names_neither() {
        let home = home_for("shared-title");
        let cwd = "/nowhere/proj";
        stored(&home, cwd, "01a17eab-44d5-7315-878f-1c81149f5013", "📂 SESS: même titre");
        stored(&home, cwd, "01a17eb4-ff7d-7169-aaa8-1a525acdde9e", "📂 SESS: même titre");
        assert!(session_by_title(&home, cwd, "π > 📂 SESS: même titre").is_none());
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn a_fresh_omp_does_not_inherit_the_newest_conversation_of_its_directory() {
        let home = home_for("fresh");
        let cwd = "/nowhere/proj";
        let id = "01a17ea2-19d3-7418-9629-6f5837843ae3";
        // Stored at 18:31:55, and written last: the other pane's conversation.
        stored(&home, cwd, id, "🪟 PANE: Reprendre la conv du pane 97");
        let dir = session_dir(&home, cwd);
        let created = candidates(&dir)
            .into_iter()
            .find(|c| c.id == id)
            .expect("the stored conversation")
            .created;

        // A process started right then created it: that one is its own.
        assert_eq!(created_at(&dir, created).as_deref(), Some(id));
        // A process started later found this conversation already there: it
        // belongs to another pane. The newest file used to be handed over
        // anyway, and the window then closed the brand-new pane as a duplicate.
        assert_eq!(created_at(&dir, created + 6 * 3600), None);
        assert_eq!(
            crate::pi_session::pick_most_recent(&candidates(&dir)).as_deref(),
            Some(id),
            "the lottery this replaces would still have claimed it"
        );
        let _ = std::fs::remove_dir_all(&home);
    }

    /// Manual check against a real, live omp: run it with
    /// `KOVA_OMP_PID=<pid> cargo test omp_session -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn a_live_omp_process_maps_to_its_transcript() {
        let pid: u32 = std::env::var("KOVA_OMP_PID")
            .ok()
            .and_then(|value| value.parse().ok())
            .expect("set KOVA_OMP_PID to a running omp process");
        let home = PathBuf::from(std::env::var_os("HOME").unwrap_or_default());
        let cwd = crate::pi_session::cwd_of(pid).expect("the process has a cwd");
        let start = crate::pi_session::bsd_info(pid)
            .expect("the process is alive")
            .pbi_start_tvsec;
        let session = session_of(pid, start, &cwd, &home).expect("a session");
        println!("pid {} → {} ({:?})", pid, session.id, session.name);
        assert!(session.id.len() > 8);
        if let Some(shell) = std::env::var("KOVA_SHELL_PID")
            .ok()
            .and_then(|value| value.parse().ok())
        {
            let from_shell = for_shell(shell).expect("the pane shell reaches the session");
            assert_eq!(from_shell.id, session.id);
            println!(
                "shell {} → {} ({:?})",
                shell, from_shell.id, from_shell.name
            );
        }
    }
}
