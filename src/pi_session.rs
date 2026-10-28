//! Detect the Pi session running inside a pane, so a quit/relaunch cycle can
//! bring it back — the same contract `claude_session` and `codex_session` hold
//! for their agents.
//!
//! Neither of the two known detections works for Pi. It writes no file per
//! process (Claude's file names its pid) and it keeps its transcript closed
//! between writes (Codex's transcript is the open file handle). Two things are
//! left:
//!
//! * **Who is Pi.** The CLI runs under `node`, so the process name says nothing;
//!   what it does say is `argv[0]`, which Pi rewrites to `pi`. It sits directly
//!   under the pane's shell, so the pane's child list is enough to find it.
//! * **Which conversation.** Pi names each session file after the moment it
//!   created it, under
//!   `~/.pi/agent/sessions/--<cwd, / : \ replaced by ->--/<start>_<id>.jsonl`.
//!   A fresh `pi` therefore matches a file by start time, but a *resumed* one
//!   reuses a file named after the original conversation, and no start time
//!   matches any more. What holds the answer then is the environment Pi hands its
//!   children (`PI_SESSION_ID`): a command running under that process carries the
//!   exact conversation, and one lucky hit pins the pane for the rest of its life
//!   (`LEARNED`). Commands only exist while they run, so when none is running the
//!   file written most recently in that directory is the last resort — and a
//!   snapshot that hands two panes the same conversation drops it for both (see
//!   `session.rs`), because guessing there would open one pane on the other's
//!   transcript.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use parking_lot::Mutex;

/// How far apart a session file's name and the process start may be before the
/// file is considered to belong to another conversation. `pi` writes the header
/// right after starting, and a launcher script adds a moment on top.
const START_MATCH_SECS: u64 = 10;

/// How far up the process tree to look for the pane's shell.
const MAX_ANCESTRY_DEPTH: usize = 3;

/// `for_shell` is asked once per pane and per tick; the answer costs a child-list
/// read plus one environment read per child. Same cache length as the other two
/// agents.
const CACHE_TTL: Duration = Duration::from_secs(1);

/// Name `argv[0]` reports for the CLI.
const PROCESS_NAME: &str = "pi";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Session {
    pub id: String,
    /// The title Pi shows for the conversation in its own `/resume` picker: the
    /// latest `/name`, else the first user message (`title_of`). `None` until
    /// one exists — a session nobody named and nobody has typed into.
    pub name: Option<String>,
}

/// The process scan's last answer, valid for `CACHE_TTL`.
static CACHE: Mutex<Option<(Instant, HashMap<u32, Session>)>> = Mutex::new(None);

/// Conversation ids learned from the environment of a `pi`'s children, kept per
/// process so a pane whose commands have all finished still knows its session:
/// `pid → (process start, session id)`. The start time guards against PID reuse.
static LEARNED: Mutex<Option<HashMap<u32, (u64, String)>>> = Mutex::new(None);

/// The directory Pi keeps the sessions of `cwd` in.
///
/// `session-manager.ts` builds it as `--${cwd without its leading slash, with
/// / \ : replaced by -}--`, and this must stay byte-identical: a mismatch only
/// means "no session found", so it fails quietly rather than wrongly.
pub fn session_dir(home: &Path, cwd: &str) -> PathBuf {
    let stripped = cwd.strip_prefix('/').unwrap_or(cwd);
    let encoded: String = stripped
        .chars()
        .map(|c| {
            if c == '/' || c == '\\' || c == ':' {
                '-'
            } else {
                c
            }
        })
        .collect();
    home.join(".pi/agent/sessions")
        .join(format!("--{}--", encoded))
}

/// Conversation id out of a session file name.
///
/// `2026-10-26T19-35-09-247Z_01a179b5-a2bf-72fa-8cb8-6209f1e8a168.jsonl`
/// → `01a179b5-a2bf-72fa-8cb8-6209f1e8a168`. The timestamp has no underscore, so
/// the first one splits name from id.
pub fn session_id_from_file_name(name: &str) -> Option<String> {
    let (stamp, id) = name.strip_suffix(".jsonl")?.split_once('_')?;
    if parse_stamp(stamp).is_none() || !is_safe_session_id(id) {
        return None;
    }
    Some(id.to_string())
}

/// Instant a session file name carries, in Unix seconds.
///
/// `2026-10-26T19-35-09-247Z`, i.e. ISO 8601 with dashes where the colons were.
fn parse_stamp(stamp: &str) -> Option<u64> {
    let rest = stamp.strip_suffix('Z')?;
    let (date, time) = rest.split_once('T')?;
    let mut d = date.split('-');
    let year: i64 = d.next()?.parse().ok()?;
    let month: u32 = d.next()?.parse().ok()?;
    let day: u32 = d.next()?.parse().ok()?;
    let mut t = time.split('-');
    let hour: u64 = t.next()?.parse().ok()?;
    let minute: u64 = t.next()?.parse().ok()?;
    let second: u64 = t.next()?.parse().ok()?;
    if !(1..=12).contains(&month)
        || !(1..=31).contains(&day)
        || hour > 23
        || minute > 59
        || second > 59
    {
        return None;
    }
    // Days from the civil date (Howard Hinnant's algorithm), then the clock.
    let y = if month <= 2 { year - 1 } else { year };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (month as i64 + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146097 + doe - 719468;
    if days < 0 {
        return None;
    }
    Some(days as u64 * 86400 + hour * 3600 + minute * 60 + second)
}

/// One candidate conversation of a directory.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Candidate {
    pub id: String,
    /// The transcript itself, where the conversation's title is written.
    pub path: PathBuf,
    /// From the file name: when the conversation was created.
    pub created: u64,
    /// When the file was last written.
    pub modified: u64,
}

/// The conversation a `pi` process created when it started at `started`, if the
/// directory holds one: the file whose creation time is closest to the process
/// start. `None` when every file predates the process, which is what a resumed
/// conversation looks like.
pub fn pick(candidates: &[Candidate], started: u64) -> Option<String> {
    candidates
        .iter()
        .filter_map(|c| {
            let gap = c.created.abs_diff(started);
            (gap <= START_MATCH_SECS).then_some((gap, c))
        })
        .min_by_key(|(gap, _)| *gap)
        .map(|(_, c)| c.id.clone())
}

/// The conversation being appended to, whatever its age: the file written last.
pub fn pick_most_recent(candidates: &[Candidate]) -> Option<String> {
    candidates
        .iter()
        .max_by_key(|c| c.modified)
        .map(|c| c.id.clone())
}

/// Conversations of `dir`. A directory that does not exist, or holds no readable
/// session, yields nothing: Pi was started with `--no-session` or stores its
/// sessions elsewhere (`--session-dir`, `PI_CODING_AGENT_SESSION_DIR`), and the
/// pane then keeps its last command.
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
        let Some(created) = name
            .strip_suffix(".jsonl")
            .and_then(|stem| stem.split_once('_'))
            .and_then(|(stamp, _)| parse_stamp(stamp))
        else {
            continue;
        };
        let modified = entry
            .metadata()
            .ok()
            .and_then(|m| m.modified().ok())
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

/// What the transcript scanned so far says about the conversation's title.
///
/// Pi titles a session the way its own `/resume` picker shows it
/// (`session.name ?? session.firstMessage`, `session-manager.ts`): the latest
/// `/name`, else the first thing the user typed. Both live in the transcript,
/// which only ever grows, so this keeps the byte offset already read: a
/// session running for hours costs one read of its file, then one read of each
/// appended line.
#[derive(Clone, Default)]
struct TitleParts {
    /// Bytes consumed, up to the last complete line: the tail of a file being
    /// written is a partial line, and parsing it would drop it silently.
    scanned: u64,
    /// Latest `session_info` name. An empty one clears it, as `/name` does.
    name: Option<String>,
    /// First user message, once one was written.
    first: Option<String>,
}

/// Session file → title parts read so far. Two panes can share a file (a
/// resumed conversation), and they must not contradict each other.
static TITLES: Mutex<Option<HashMap<PathBuf, TitleParts>>> = Mutex::new(None);

/// One pass at a transcript reads at most this much. A file larger than that is
/// still read — only across several passes — except for a single line longer
/// than the cap, which is skipped rather than stalled on.
const MAX_TITLE_SCAN_BYTES: u64 = 4 * 1024 * 1024;

/// The title Pi wrote for the conversation at `path`: its `/name`, else the
/// first user message, trimmed to a row-sized line. `None` for a conversation
/// nobody named and nobody has typed into yet.
pub fn title_of(path: &Path) -> Option<String> {
    use std::io::{Read, Seek, SeekFrom};

    let mut cache = TITLES.lock();
    let parts = cache
        .get_or_insert_with(HashMap::new)
        .entry(path.to_path_buf())
        .or_default();
    let mut file = std::fs::File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    if parts.scanned > len {
        // Replaced, not appended to (a fork that started over): start again
        // rather than splicing two unrelated halves.
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
            // A line longer than the cap: skip it, or every pass would reread it.
            buf.len() as u64
        } else {
            0
        };
    }
    parts.name.clone().or_else(|| parts.first.clone())
}

/// Fold one complete transcript line into `parts`.
fn read_title_line(line: &[u8], parts: &mut TitleParts) {
    // Two JSON parses per line of a multi-megabyte transcript would stall the
    // frame this runs on, so a line that cannot matter is not parsed. Pi writes
    // compact JSON (`{"type":"message",…`), which is what these look for.
    let named = line.windows(14).any(|w| w == b"\"session_info\"");
    if !named && parts.first.is_some() {
        return;
    }
    if !named && !line.windows(6).any(|w| w == b"\"user\"") {
        return;
    }
    let Ok(entry) = serde_json::from_slice::<serde_json::Value>(line) else {
        return;
    };
    if named && entry["type"] == "session_info" {
        let name = entry["name"].as_str().map(str::trim).unwrap_or_default();
        parts.name = (!name.is_empty()).then(|| name.to_string());
        return;
    }
    if parts.first.is_none()
        && entry["type"] == "message"
        && entry["message"]["role"] == "user"
    {
        let Some(text) = user_message_text(&entry["message"]["content"]) else {
            return;
        };
        let title = crate::claude_history::title_from_prompt(&text);
        if !title.trim().is_empty() {
            parts.first = Some(title);
        }
    }
}

/// Text of a Pi message's content: a plain string, or the text blocks of the
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

/// BSD info of a live process — parent and start time.
pub(crate) fn bsd_info(pid: u32) -> Option<libc::proc_bsdinfo> {
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let ret = unsafe {
        libc::proc_pidinfo(
            pid as i32,
            libc::PROC_PIDTBSDINFO,
            0,
            &mut info as *mut _ as *mut libc::c_void,
            std::mem::size_of::<libc::proc_bsdinfo>() as i32,
        )
    };
    (ret > 0).then_some(info)
}

/// Working directory of a live process.
pub(crate) fn cwd_of(pid: u32) -> Option<String> {
    let mut info: libc::proc_vnodepathinfo = unsafe { std::mem::zeroed() };
    let ret = unsafe {
        libc::proc_pidinfo(
            pid as i32,
            libc::PROC_PIDVNODEPATHINFO,
            0,
            &mut info as *mut _ as *mut libc::c_void,
            std::mem::size_of::<libc::proc_vnodepathinfo>() as i32,
        )
    };
    if ret <= 0 {
        return None;
    }
    // `vip_path` is a flat NUL-terminated buffer that `libc` declares as a
    // 32×32 matrix; flatten it back before reading.
    let bytes: Vec<u8> = info
        .pvi_cdir
        .vip_path
        .iter()
        .flat_map(|chunk| chunk.iter())
        .map(|&c| c as u8)
        .take_while(|&c| c != 0)
        .collect();
    if bytes.is_empty() {
        return None;
    }
    Some(String::from_utf8_lossy(&bytes).to_string())
}

/// Live processes whose parent is `pid`.
pub(crate) fn children_of(pid: u32) -> Vec<u32> {
    // Ask for the size first, then read with room to spare: children come and go
    // between the two calls.
    let bytes = unsafe { libc::proc_listchildpids(pid as libc::pid_t, std::ptr::null_mut(), 0) };
    if bytes <= 0 {
        return Vec::new();
    }
    let mut children = vec![0i32; bytes as usize / std::mem::size_of::<i32>() + 16];
    let size = (children.len() * std::mem::size_of::<i32>()) as i32;
    let got = unsafe {
        libc::proc_listchildpids(
            pid as libc::pid_t,
            children.as_mut_ptr() as *mut libc::c_void,
            size,
        )
    };
    if got <= 0 {
        return Vec::new();
    }
    let count = got as usize / std::mem::size_of::<i32>();
    children[..count]
        .iter()
        .filter(|&&c| c > 0)
        .map(|&c| c as u32)
        .collect()
}

/// The environment and arguments of a live process, as `sysctl KERN_PROCARGS2`
/// returns them. `None` when the process is gone, or when it is not ours to read.
pub(crate) fn args_and_env_of(pid: u32) -> Option<Vec<u8>> {
    let mut mib = [libc::CTL_KERN, libc::KERN_PROCARGS2, pid as libc::c_int];
    let mut size: libc::size_t = 0;
    let ret = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            mib.len() as u32,
            std::ptr::null_mut(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    if ret != 0 || size == 0 {
        return None;
    }
    let mut buf = vec![0u8; size as usize];
    let ret = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            mib.len() as u32,
            buf.as_mut_ptr() as *mut libc::c_void,
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    if ret != 0 {
        return None;
    }
    buf.truncate(size as usize);
    Some(buf)
}

/// `argv[0]` out of a `KERN_PROCARGS2` block: the argument count, the executable
/// path, then the arguments — behind padding NULs this platform leaves there.
pub fn argv0_in_blob(blob: &[u8]) -> Option<String> {
    let argc = i32::from_ne_bytes(blob.get(..4)?.try_into().ok()?);
    if argc <= 0 {
        return None;
    }
    // Past the count: the executable path ends at the first NUL after it (the
    // count itself carries NUL bytes, so the search must not start at 0).
    let mut i = blob.get(4..)?.iter().position(|&b| b == 0)? + 5;
    while blob.get(i) == Some(&0) {
        i += 1;
    }
    let rest = blob.get(i..)?;
    let end = rest.iter().position(|&b| b == 0)?;
    Some(String::from_utf8_lossy(&rest[..end]).into_owned())
}

/// The session id a process carries in its environment.
///
/// Pi resolves `PI_SESSION_ID` when it spawns each command, so the value lives in
/// the child, never in the `pi` process itself. Entries are NUL-separated, so
/// splitting the whole block keeps a script that merely *mentions* the name from
/// matching: only an entry that starts with it counts.
pub fn session_id_in_env_blob(blob: &[u8]) -> Option<String> {
    const NEEDLE: &[u8] = b"PI_SESSION_ID=";
    blob.split(|&b| b == 0)
        .filter(|entry| entry.starts_with(NEEDLE))
        .filter_map(|entry| std::str::from_utf8(&entry[NEEDLE.len()..]).ok())
        .map(str::trim)
        .find(|id| is_safe_session_id(id))
        .map(str::to_string)
}

/// True if the block belongs to a Pi process: `argv[0]` is `pi`, whatever the
/// executable behind it is.
pub fn is_pi_process(blob: &[u8]) -> bool {
    let Some(argv0) = argv0_in_blob(blob) else {
        return false;
    };
    argv0.rsplit('/').next() == Some(PROCESS_NAME)
}

/// True if `id` is safe to splice into a command line — same guard, and same
/// reason, as on the Claude side: this string is typed into a PTY.
fn is_safe_session_id(id: &str) -> bool {
    !id.is_empty()
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// The conversation `pi` holds, from the cheapest exact source to the least.
fn session_of(pid: u32, start: u64, cwd: &str, home: &Path) -> Option<Session> {
    let dir = session_dir(home, cwd);
    if let Some(id) = children_of(pid)
        .into_iter()
        .find_map(|child| args_and_env_of(child).and_then(|blob| session_id_in_env_blob(&blob)))
    {
        LEARNED
            .lock()
            .get_or_insert_with(HashMap::new)
            .insert(pid, (start, id.clone()));
        let known = candidates(&dir);
        return Some(titled(id, &known));
    }
    if let Some(id) = LEARNED
        .lock()
        .as_ref()
        .and_then(|map| map.get(&pid))
        .filter(|(known_start, _)| *known_start == start)
        .map(|(_, id)| id.clone())
    {
        let known = candidates(&dir);
        return Some(titled(id, &known));
    }
    let known = candidates(&dir);
    pick(&known, start)
        .or_else(|| pick_most_recent(&known))
        .map(|id| titled(id, &known))
}

/// A session with the title Pi gave it, when its file is still there.
fn titled(id: String, known: &[Candidate]) -> Session {
    let name = known
        .iter()
        .find(|c| c.id == id)
        .and_then(|c| title_of(&c.path));
    Session { id, name }
}

/// Runtimes `pi` can run under: the CLI is a Node program, so the process name
/// says `node` and only `argv[0]` names Pi. Listing them keeps the scan down to a
/// handful of environment reads instead of one per process on the machine.
const RUNTIMES: [&str; 4] = ["node", "bun", "deno", "pi"];

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

/// Every PID alive right now.
pub(crate) fn live_pids() -> Vec<u32> {
    // Ask for the size first, then read with room to spare: processes come and
    // go between the two calls.
    let bytes = unsafe { libc::proc_listpids(PROC_ALL_PIDS, 0, std::ptr::null_mut(), 0) };
    if bytes <= 0 {
        return Vec::new();
    }
    let mut pids = vec![0i32; bytes as usize / std::mem::size_of::<i32>() + 64];
    let size = (pids.len() * std::mem::size_of::<i32>()) as i32;
    let got = unsafe {
        libc::proc_listpids(
            PROC_ALL_PIDS,
            0,
            pids.as_mut_ptr() as *mut libc::c_void,
            size,
        )
    };
    if got <= 0 {
        return Vec::new();
    }
    let count = got as usize / std::mem::size_of::<i32>();
    pids[..count]
        .iter()
        .filter(|&&p| p > 0)
        .map(|&p| p as u32)
        .collect()
}

/// `PROC_ALL_PIDS`, from `sys/proc_info.h`; absent from `libc`.
const PROC_ALL_PIDS: u32 = 1;

/// Parent PID of a live process.
pub(crate) fn parent_of(pid: u32) -> Option<u32> {
    bsd_info(pid).map(|info| info.pbi_ppid)
}

/// Map "ancestor PID → Pi session" over the live Pi processes.
fn scan_uncached() -> HashMap<u32, Session> {
    let mut map = HashMap::new();
    let own_pid = std::process::id();
    let home = PathBuf::from(std::env::var_os("HOME").unwrap_or_default());

    let pids: Vec<u32> = live_pids()
        .into_iter()
        .filter(|&pid| process_name(pid).is_some_and(|name| RUNTIMES.contains(&name.as_str())))
        .collect();
    {
        // Forget processes that exited, or a reused PID would inherit a session.
        if let Some(learned) = LEARNED.lock().as_mut() {
            learned.retain(|pid, _| pids.contains(pid));
        }
    }

    for pid in pids {
        let Some(blob) = args_and_env_of(pid) else {
            continue;
        };
        if !is_pi_process(&blob) {
            continue;
        }
        let Some(info) = bsd_info(pid) else { continue };
        let Some(cwd) = cwd_of(pid) else { continue };
        let Some(session) = session_of(pid, info.pbi_start_tvsec, &cwd, &home) else {
            continue;
        };
        let mut current = pid;
        for _ in 0..MAX_ANCESTRY_DEPTH {
            let Some(parent) = parent_of(current) else {
                break;
            };
            if parent <= 1 || parent == own_pid {
                break;
            }
            map.insert(parent, session.clone());
            current = parent;
        }
    }
    map
}

/// The Pi session running under `shell_pid`, if any. Shares the process scan's
/// one-second cache with every other pane, like the Codex side.
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
    // A pane that just started `pi` must not wait for the scan's next second, and
    // a pane that closed it must stop claiming it: ask the live machine directly
    // when the cached answer says nothing.
    if guard.is_none() {
        guard = detect(shell_pid);
    }
    guard
}

/// Look for Pi under `shell_pid` without the cache: its children, then its
/// children's children (a launcher script may sit in between).
fn detect(shell_pid: u32) -> Option<Session> {
    let home = PathBuf::from(std::env::var_os("HOME").unwrap_or_default());
    let mut frontier = children_of(shell_pid);
    for _ in 0..2 {
        let mut next = Vec::new();
        for pid in frontier {
            let Some(blob) = args_and_env_of(pid) else {
                continue;
            };
            if is_pi_process(&blob) {
                let info = bsd_info(pid)?;
                let cwd = cwd_of(pid)?;
                return session_of(pid, info.pbi_start_tvsec, &cwd, &home);
            }
            next.extend(children_of(pid));
        }
        frontier = next;
    }
    None
}

/// The command line that reopens `session_id`, keeping the other flags the pane
/// was started with (`pi --model x` → `pi --session <id> --model x`).
pub fn resume_command(last_command: Option<&str>, session_id: &str) -> Option<String> {
    if !is_safe_session_id(session_id) {
        log::warn!("Refusing to build a resume line from a non-plain Pi session id");
        return None;
    }
    let base = last_command
        .map(str::trim)
        .filter(|cmd| is_pi_invocation(cmd))
        .map(strip_session_flags)
        .filter(|tokens| !tokens.is_empty())
        .unwrap_or_else(|| vec![PROCESS_NAME.to_string()]);
    let (command, flags) = base.split_first()?;
    let mut line = vec![
        command.clone(),
        "--session".to_string(),
        session_id.to_string(),
    ];
    line.extend(flags.iter().cloned());
    Some(line.join(" "))
}

/// True if `cmd` runs `pi`, and nothing but it: a pipeline or a list would run
/// something else after the resume.
fn is_pi_invocation(cmd: &str) -> bool {
    let Some(first) = cmd.split_whitespace().next() else {
        return false;
    };
    if cmd.contains('|') || cmd.contains(';') || cmd.contains('&') {
        return false;
    }
    first == PROCESS_NAME || first.ends_with("/pi")
}

/// Pi flags that choose a conversation, with the value that follows them.
const PI_FLAGS_WITH_VALUE: [&str; 3] = ["--session", "--session-id", "--fork"];
/// Pi flags that choose a conversation on their own.
const PI_FLAGS_ALONE: [&str; 5] = ["-c", "--continue", "-r", "--resume", "--no-session"];

/// Drop the flags that select a conversation, so appending `--session <id>`
/// cannot collide with what the previous invocation already carried.
fn strip_session_flags(cmd: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut tokens = cmd.split_whitespace();
    while let Some(token) = tokens.next() {
        if PI_FLAGS_WITH_VALUE.contains(&token) {
            tokens.next();
            continue;
        }
        if PI_FLAGS_ALONE.contains(&token)
            || token.starts_with("--session=")
            || token.starts_with("--session-id=")
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
    fn the_session_directory_follows_pis_own_encoding() {
        let home = Path::new("/Users/me");
        assert_eq!(
            session_dir(home, "/Users/me/projects/cto"),
            Path::new("/Users/me/.pi/agent/sessions/--Users-me-projects-cto--")
        );
        // Colons and dots are not touched by the encoding, only slashes.
        assert_eq!(
            session_dir(home, "/private/tmp/x:y.z"),
            Path::new("/Users/me/.pi/agent/sessions/--private-tmp-x-y.z--")
        );
    }

    #[test]
    fn the_conversation_id_is_read_after_the_first_underscore() {
        assert_eq!(
            session_id_from_file_name(
                "2026-10-26T19-35-09-247Z_01a179b5-a2bf-72fa-8cb8-6209f1e8a168.jsonl"
            )
            .as_deref(),
            Some("01a179b5-a2bf-72fa-8cb8-6209f1e8a168")
        );
        assert_eq!(session_id_from_file_name("notes.txt"), None);
        assert_eq!(
            session_id_from_file_name("2026-10-26T19-35-09-247Z_../../etc/passwd.jsonl"),
            None
        );
    }

    #[test]
    fn the_file_name_timestamp_is_read_as_unix_seconds() {
        // 2026-10-26T19:35:09Z
        assert_eq!(parse_stamp("2026-10-26T19-35-09-247Z"), Some(1793043309));
        assert_eq!(parse_stamp("1970-01-01T00-00-00-000Z"), Some(0));
        assert_eq!(parse_stamp("2026-13-26T19-35-09-247Z"), None);
    }

    fn candidate(id: &str, created: u64, modified: u64) -> Candidate {
        Candidate {
            id: id.to_string(),
            path: PathBuf::from(format!("/sessions/{id}.jsonl")),
            created,
            modified,
        }
    }

    #[test]
    fn a_session_created_when_the_process_started_wins() {
        // Two panes, same directory, two conversations: the start time tells
        // them apart.
        let candidates = [
            candidate("older", 1_000, 9_000),
            candidate("newer", 5_000, 5_500),
        ];
        assert_eq!(pick(&candidates, 5_001).as_deref(), Some("newer"));
        assert_eq!(pick(&candidates, 999).as_deref(), Some("older"));
    }

    #[test]
    fn a_resumed_session_is_the_file_written_last() {
        // Every file predates the process: it resumed one of them, and the one
        // still being appended to is the only candidate left.
        let candidates = [
            candidate("idle", 1_000, 2_000),
            candidate("live", 3_000, 8_000),
        ];
        assert_eq!(pick(&candidates, 9_000), None);
        assert_eq!(pick_most_recent(&candidates).as_deref(), Some("live"));
    }

    #[test]
    fn no_session_file_means_no_resume() {
        assert_eq!(pick(&[], 5_000), None);
        assert_eq!(pick_most_recent(&[]), None);
    }

    /// The layout `sysctl KERN_PROCARGS2` returns: argument count, executable
    /// path, arguments, then the environment.
    fn blob(argc: i32, exec: &str, args: &[&str], env: &[&str]) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&argc.to_ne_bytes());
        out.extend_from_slice(exec.as_bytes());
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
    fn a_pi_process_is_recognised_by_its_argv0() {
        // Pi runs under node and rewrites its own `argv[0]`.
        let pi = blob(
            2,
            "/opt/homebrew/bin/node",
            &["pi", "-c"],
            &["HOME=/Users/me"],
        );
        assert!(is_pi_process(&pi));
        let node = blob(
            2,
            "/opt/homebrew/bin/node",
            &["/opt/homebrew/bin/node", "x.js"],
            &[],
        );
        assert!(!is_pi_process(&node));
        let other = blob(1, "/usr/bin/vim", &["vim"], &[]);
        assert!(!is_pi_process(&other));
    }

    #[test]
    fn the_session_id_is_read_out_of_a_commands_environment() {
        let blob = blob(
            2,
            "/bin/bash",
            &["/bin/bash", "-c"],
            &[
                "HOME=/Users/me",
                "PI_SESSION_ID=01a179b5-a2bf-72fa-8cb8-6209f1e8a168",
                "PWD=/w",
            ],
        );
        assert_eq!(
            session_id_in_env_blob(&blob).as_deref(),
            Some("01a179b5-a2bf-72fa-8cb8-6209f1e8a168")
        );
    }

    #[test]
    fn a_script_that_mentions_the_variable_is_not_an_environment_entry() {
        let mentioning = blob(1, "/bin/bash", &["echo PI_SESSION_ID=not-an-entry"], &[]);
        assert_eq!(session_id_in_env_blob(&mentioning), None);
        assert_eq!(
            session_id_in_env_blob(b"PI_SESSION_ID=bad id; rm -rf ~\0"),
            None
        );
    }

    #[test]
    fn the_resume_line_keeps_the_other_flags() {
        assert_eq!(
            resume_command(Some("pi"), "abc").as_deref(),
            Some("pi --session abc")
        );
        assert_eq!(
            resume_command(Some("pi --model deepseek-flash"), "abc").as_deref(),
            Some("pi --session abc --model deepseek-flash")
        );
        // A conversation already selected is replaced, not duplicated.
        assert_eq!(
            resume_command(Some("pi --model x -c"), "abc").as_deref(),
            Some("pi --session abc --model x")
        );
        assert_eq!(
            resume_command(Some("pi --session old --session-dir /tmp/s"), "abc").as_deref(),
            Some("pi --session abc --session-dir /tmp/s")
        );
        // Nothing to reuse: a bare command is enough.
        assert_eq!(
            resume_command(Some("vim foo"), "abc").as_deref(),
            Some("pi --session abc")
        );
        assert_eq!(
            resume_command(Some("pi | tee log"), "abc").as_deref(),
            Some("pi --session abc")
        );
    }

    #[test]
    fn a_non_plain_id_never_reaches_the_shell() {
        assert_eq!(resume_command(Some("pi"), "abc; rm -rf ~"), None);
        assert_eq!(resume_command(Some("pi"), ""), None);
    }

    /// A transcript of our own, under a name no other test uses: `title_of`
    /// remembers what it read, so two tests must never share a path.
    fn transcript(test: &str, lines: &[&str]) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "kova-pi-title-{}-{test}.jsonl",
            std::process::id()
        ));
        std::fs::write(&path, lines.join("\n") + "\n").unwrap();
        path
    }

    /// Pi's own header and system message, then whatever the test adds.
    const HEADER: [&str; 2] = [
        r#"{"type":"session","version":3,"id":"abc","timestamp":"2026-10-26T20:39:57.357Z","cwd":"/w"}"#,
        r#"{"type":"message","id":"m0","parentId":null,"timestamp":"2026-10-26T20:39:57.500Z","message":{"role":"system","content":[{"type":"text","text":"You are pi, be useful"}]}}"#,
    ];

    #[test]
    fn an_unnamed_session_is_titled_after_its_first_user_message() {
        let path = transcript(
            "first-message",
            &[
                HEADER[0],
                HEADER[1],
                r#"{"type":"message","id":"m1","parentId":"m0","timestamp":"2026-10-26T20:40:00.000Z","message":{"role":"user","content":[{"type":"text","text":"lis les modifs récentes de kana\nje cherche les numéros"}]}}"#,
                r#"{"type":"message","id":"m2","parentId":"m1","timestamp":"2026-10-26T20:40:01.000Z","message":{"role":"assistant","content":[{"type":"text","text":"ok"}]}}"#,
            ],
        );
        assert_eq!(title_of(&path).as_deref(), Some("lis les modifs récentes de kana"));
    }

    #[test]
    fn a_name_given_to_the_session_wins_over_its_first_message() {
        let path = transcript(
            "named",
            &[
                HEADER[0],
                r#"{"type":"message","id":"m1","parentId":null,"timestamp":"2026-10-26T20:40:00.000Z","message":{"role":"user","content":"first"}}"#,
                // `/name Refactor auth` — then a later one replaces it.
                r#"{"type":"session_info","id":"s1","parentId":"m1","timestamp":"2026-10-26T20:41:00.000Z","name":"Refactor auth"}"#,
                r#"{"type":"session_info","id":"s2","parentId":"s1","timestamp":"2026-10-26T20:42:00.000Z","name":"  Titres de Pi  "}"#,
            ],
        );
        assert_eq!(title_of(&path).as_deref(), Some("Titres de Pi"));
    }

    #[test]
    fn a_cleared_name_falls_back_to_the_first_message() {
        let path = transcript(
            "cleared",
            &[
                HEADER[0],
                r#"{"type":"message","id":"m1","parentId":null,"timestamp":"2026-10-26T20:40:00.000Z","message":{"role":"user","content":[{"type":"text","text":"premier prompt"}]}}"#,
                r#"{"type":"session_info","id":"s1","parentId":"m1","timestamp":"2026-10-26T20:41:00.000Z","name":"Titre"}"#,
                // `/name` with nothing after it clears the title, as Pi does.
                r#"{"type":"session_info","id":"s2","parentId":"s1","timestamp":"2026-10-26T20:42:00.000Z","name":"   "}"#,
            ],
        );
        assert_eq!(title_of(&path).as_deref(), Some("premier prompt"));
    }

    #[test]
    fn a_live_session_only_has_its_new_lines_read() {
        use std::io::Write;

        let path = transcript(
            "append",
            &[
                HEADER[0],
                r#"{"type":"message","id":"m1","parentId":null,"timestamp":"2026-10-26T20:40:00.000Z","message":{"role":"user","content":"premier prompt"}}"#,
            ],
        );
        assert_eq!(title_of(&path).as_deref(), Some("premier prompt"));
        let mut file = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
        writeln!(
            file,
            r#"{{"type":"session_info","id":"s1","parentId":"m1","timestamp":"2026-10-26T20:41:00.000Z","name":"Renommé"}}"#
        )
        .unwrap();
        assert_eq!(title_of(&path).as_deref(), Some("Renommé"));
    }

    #[test]
    fn a_half_written_line_is_not_read_yet() {
        use std::io::Write;

        let path = transcript("partial", &[HEADER[0]]);
        assert_eq!(title_of(&path), None);
        let mut file = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
        // The writer got as far as the middle of the name.
        write!(file, "{}", r#"{"type":"session_info","id":"s1","name":"Tit"#).unwrap();
        assert_eq!(title_of(&path), None, "no complete line, nothing to show");
        writeln!(file, "{}", r#"re"}"#).unwrap();
        assert_eq!(title_of(&path).as_deref(), Some("Titre"));
    }

    #[test]
    fn a_conversation_nobody_typed_into_has_no_title() {
        let path = transcript("empty", &[HEADER[0], HEADER[1]]);
        assert_eq!(title_of(&path), None);
    }

    #[test]
    fn the_title_is_what_the_session_file_says() {
        let known = [
            candidate("older", 1_000, 9_000),
            candidate("newer", 5_000, 5_500),
        ];
        // The files behind these ids do not exist here: the session is still
        // reported, only without a title.
        let session = titled("newer".to_string(), &known);
        assert_eq!(session.id, "newer");
        assert_eq!(session.name, None);
    }
}

