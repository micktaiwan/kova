//! Detect the session of Mickael's own coding-agent harness (`~/projects/perso/tazu`)
//! running inside a pane — the same contract `omp_session` holds for omp.
//!
//! Tazu is a Python program, so its process is called `python`, not
//! `tazu`: it is recognised by its arguments (a script ending in
//! `/bin/tazu`, or `-m tazu`). Which conversation it holds
//! is exact, as with omp: it keeps its journal
//! (`<data dir>/sessions/<id>.jsonl`, `~/.tazu` by default) open with a writer
//! for the whole life of the session, so the file descriptor names the
//! conversation, resumed ones included. No start-time guess, no "newest file".
//!
//! `kova ask`, which runs inside a tool call, reads the conversation from the
//! environment Tazu gives its commands (`TAZU_SESSION_ID`, see `ask.rs`).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use parking_lot::Mutex;

/// How far up the process tree to look for the pane's shell.
const MAX_ANCESTRY_DEPTH: usize = 3;

/// `for_shell` is asked once per pane and per tick; same cache length as the other agents.
const CACHE_TTL: Duration = Duration::from_secs(1);

/// `PROC_PIDFDVNODEPATHINFO`, from `sys/proc_info.h`; absent from `libc`.
const PROC_PIDFDVNODEPATHINFO: libc::c_int = 2;

/// `MAXPATHLEN`, from `sys/param.h`.
const MAXPATHLEN: usize = 1024;

/// `FWRITE`, from `sys/fcntl.h`: the descriptor is open for writing.
const FWRITE: u32 = 0x0002;

/// Name of the variable Tazu puts in the environment of its commands.
pub const SESSION_ENV: &str = "TAZU_SESSION_ID";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Session {
    pub id: String,
    /// The conversation's title: the latest `title` entry of the journal, else the first user message.
    pub name: Option<String>,
}

/// The process scan's last answer, valid for `CACHE_TTL`.
static CACHE: Mutex<Option<(Instant, HashMap<u32, Session>)>> = Mutex::new(None);

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

/// True if `id` is safe to splice into a command line — this string is typed into a PTY.
fn is_safe_session_id(id: &str) -> bool {
    !id.is_empty()
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// Conversation id out of a journal file name: `20261101-132200-ab12.jsonl` → `20261101-132200-ab12`.
pub fn session_id_from_file_name(name: &str) -> Option<String> {
    let id = name.strip_suffix(".jsonl")?;
    is_safe_session_id(id).then(|| id.to_string())
}

/// True if `path` is a Tazu journal: `<data dir>/sessions/<id>.jsonl`.
fn is_journal_path(path: &Path) -> bool {
    path.parent()
        .and_then(Path::file_name)
        .is_some_and(|dir| dir == "sessions")
        && path
            .file_name()
            .and_then(|n| n.to_str())
            .and_then(session_id_from_file_name)
            .is_some()
}

/// The arguments of a `KERN_PROCARGS2` block (argv, without the executable path or the environment).
pub fn argv_in_blob(blob: &[u8]) -> Vec<String> {
    let Some(count) = blob
        .get(..4)
        .and_then(|b| b.try_into().ok())
        .map(i32::from_ne_bytes)
        .filter(|&n| n > 0)
    else {
        return Vec::new();
    };
    // Past the count: the executable path ends at the first NUL after it (the
    // count itself carries NUL bytes, so the search must not start at 0).
    let Some(end) = blob.get(4..).and_then(|b| b.iter().position(|&x| x == 0)) else {
        return Vec::new();
    };
    let mut i = end + 5;
    while blob.get(i) == Some(&0) {
        i += 1;
    }
    blob.get(i..)
        .unwrap_or_default()
        .split(|&b| b == 0)
        .take(count as usize)
        .map(|arg| String::from_utf8_lossy(arg).into_owned())
        .collect()
}

/// True if the block belongs to a Tazu process: a Python interpreter running
/// Tazu script or package.
pub fn is_tazu_process(blob: &[u8]) -> bool {
    let argv = argv_in_blob(blob);
    let Some(interpreter) = argv.first().and_then(|a| a.rsplit('/').next()) else {
        return false;
    };
    if !interpreter.to_ascii_lowercase().starts_with("python") {
        return false;
    }
    let mut rest = argv.iter().skip(1);
    while let Some(arg) = rest.next() {
        if arg == "-m" {
            return rest.next().is_some_and(|m| m == "tazu");
        }
        if arg.starts_with('-') {
            continue;
        }
        return arg.ends_with("/bin/tazu");
    }
    false
}

/// The journal Tazu holds open for writing in `pid`, if any.
fn open_journal_in(pid: u32) -> Option<PathBuf> {
    let size = unsafe {
        libc::proc_pidinfo(pid as i32, libc::PROC_PIDLISTFDS, 0, std::ptr::null_mut(), 0)
    };
    if size <= 0 {
        return None;
    }
    let mut fds = vec![
        libc::proc_fdinfo {
            proc_fd: 0,
            proc_fdtype: 0
        };
        size as usize / std::mem::size_of::<libc::proc_fdinfo>() + 16
    ];
    let want = (fds.len() * std::mem::size_of::<libc::proc_fdinfo>()) as i32;
    let size = unsafe {
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
        if got <= 0 || info.pfi.fi_openflags & FWRITE == 0 {
            continue;
        }
        let raw = unsafe { std::ffi::CStr::from_ptr(info.pvip.vip_path.as_ptr()) };
        let path = PathBuf::from(raw.to_string_lossy().into_owned());
        if is_journal_path(&path) {
            return Some(path);
        }
    }
    None
}

/// The title of the conversation journaled at `path`: the latest `{"type":"title"}` entry, else the first
/// user message. `None` for a conversation nobody has typed into yet.
pub fn title_of(path: &Path) -> Option<String> {
    use std::io::{BufRead, BufReader};
    let file = std::fs::File::open(path).ok()?;
    let (mut named, mut first) = (None, None);
    for line in BufReader::new(file).lines().map_while(Result::ok) {
        // Cheap gate first: a journal is mostly tool output.
        let titled = line.contains("\"type\": \"title\"") || line.contains("\"type\":\"title\"");
        if !titled && first.is_some() {
            continue;
        }
        let Ok(entry) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        match entry["type"].as_str().unwrap_or_default() {
            "title" => {
                let title = entry["title"].as_str().unwrap_or("").trim();
                named = (!title.is_empty()).then(|| title.to_string());
            }
            "message" if first.is_none() && entry["message"]["role"] == "user" => {
                let text = entry["message"]["content"].as_str().unwrap_or("");
                let title = crate::claude_history::title_from_prompt(text);
                if !title.trim().is_empty() {
                    first = Some(title);
                }
            }
            _ => {}
        }
    }
    named.or(first)
}

fn session_of(pid: u32) -> Option<Session> {
    let path = open_journal_in(pid)?;
    let id = session_id_from_file_name(path.file_name()?.to_str()?)?;
    Some(Session {
        id,
        name: title_of(&path),
    })
}

/// Short process name, as `proc_name` reports it.
fn process_name(pid: u32) -> Option<String> {
    let mut buf = [0u8; 256];
    let ret = unsafe {
        libc::proc_name(pid as i32, buf.as_mut_ptr() as *mut libc::c_void, buf.len() as u32)
    };
    (ret > 0).then(|| String::from_utf8_lossy(&buf[..ret as usize]).to_string())
}

/// Map "ancestor PID → tazu session" over the live tazu processes.
fn scan_uncached() -> HashMap<u32, Session> {
    let mut map: HashMap<u32, (usize, Session)> = HashMap::new();
    let own_pid = std::process::id();
    let pids: Vec<u32> = crate::pi_session::live_pids()
        .into_iter()
        .filter(|&pid| {
            process_name(pid).is_some_and(|name| name.to_ascii_lowercase().starts_with("python"))
        })
        .collect();
    for pid in pids {
        let Some(blob) = crate::pi_session::args_and_env_of(pid) else {
            continue;
        };
        if !is_tazu_process(&blob) {
            continue;
        }
        let Some(session) = session_of(pid) else {
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

/// Tazu session running under `shell_pid`, if any.
pub fn for_shell(shell_pid: u32) -> Option<Session> {
    let mut cache = CACHE.lock();
    let fresh = cache
        .as_ref()
        .is_some_and(|(at, _)| at.elapsed() < CACHE_TTL);
    if !fresh {
        *cache = Some((Instant::now(), scan_uncached()));
    }
    cache
        .as_ref()
        .and_then(|(_, map)| map.get(&shell_pid).cloned())
}

/// True if `cmd` runs Tazu, and nothing but it.
pub(crate) fn is_tazu_invocation(cmd: &str) -> bool {
    let Some(first) = cmd.split_whitespace().next() else {
        return false;
    };
    if cmd.contains('|') || cmd.contains(';') || cmd.contains('&') {
        return false;
    }
    first == "tazu" || first.ends_with("/tazu")
}

/// Flags that take one value, so a value is never mistaken for the prompt.
const VALUE_FLAGS: [&str; 15] = [
    "--model",
    "--base-url",
    "--provider-only",
    "-C",
    "--cwd",
    "--max-turns",
    "--timeout",
    "--context-window",
    "--append-system-prompt",
    "--hooks",
    "--hooks-settings",
    "--mcp-config",
    "--data-dir",
    "--output-format",
    "--resume",
];

/// Flags that choose a conversation or a non-interactive run, dropped from a resume line.
const DROPPED_ALONE: [&str; 4] = ["-c", "--continue", "-p", "--print"];

/// The flags worth keeping from the previous invocation: not the conversation choice, not a prompt.
fn kept_flags(cmd: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut tokens = cmd.split_whitespace();
    while let Some(token) = tokens.next() {
        if !token.starts_with('-') {
            continue; // the prompt of a one-shot run
        }
        let takes_value = VALUE_FLAGS.contains(&token);
        let drops = token == "--resume" || token == "--fixture" || DROPPED_ALONE.contains(&token);
        if token == "--price" {
            let (a, b) = (tokens.next(), tokens.next());
            out.extend([token.to_string()].into_iter().chain(a.map(str::to_string)).chain(b.map(str::to_string)));
        } else if takes_value || token == "--fixture" {
            let value = tokens.next();
            if !drops {
                out.extend([token.to_string()].into_iter().chain(value.map(str::to_string)));
            }
        } else if !drops && !token.starts_with("--resume=") && !token.starts_with("--fixture=") {
            out.push(token.to_string());
        }
    }
    out
}

/// The command line that reopens `session_id`, keeping the other flags the pane was started with
/// (`tazu --model x` → `tazu --resume <id> --model x`).
pub fn resume_command(last_command: Option<&str>, session_id: &str) -> Option<String> {
    if !is_safe_session_id(session_id) {
        log::warn!("Refusing to build a resume line from a non-plain tazu session id");
        return None;
    }
    let invocation = last_command.map(str::trim).filter(|c| is_tazu_invocation(c));
    let command = invocation
        .and_then(|c| c.split_whitespace().next())
        .unwrap_or("tazu");
    let mut line = vec![command.to_string(), "--resume".to_string(), session_id.to_string()];
    if let Some(cmd) = invocation {
        line.extend(kept_flags(cmd));
    }
    Some(line.join(" "))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A `KERN_PROCARGS2` block: count, executable path, padding, argv, environment.
    fn blob(exec: &str, argv: &[&str], env: &[&str]) -> Vec<u8> {
        let mut out = (argv.len() as i32).to_ne_bytes().to_vec();
        out.extend(exec.as_bytes());
        out.extend([0, 0, 0]);
        for arg in argv.iter().chain(env) {
            out.extend(arg.as_bytes());
            out.push(0);
        }
        out
    }

    #[test]
    fn the_tazu_is_told_by_its_arguments() {
        let script = ["/p/.venv/bin/python", "/Users/me/projects/perso/tazu/bin/tazu", "--model", "m"];
        assert!(is_tazu_process(&blob("/p/.venv/bin/python", &script, &[])));
        let module = ["python3", "-m", "tazu", "-p", "x"];
        assert!(is_tazu_process(&blob("/usr/bin/python3", &module, &[])));
        // Another Python program, or something that merely mentions the word.
        let other = ["python3", "server.py", "tazu"];
        assert!(!is_tazu_process(&blob("/usr/bin/python3", &other, &[])));
        let editor = ["vim", "/x/bin/tazu"];
        assert!(!is_tazu_process(&blob("/usr/bin/vim", &editor, &[])));
        assert!(!is_tazu_process(&[]));
    }

    #[test]
    fn a_journal_is_a_sessions_file_named_by_a_plain_id() {
        assert!(is_journal_path(Path::new("/h/.tazu/sessions/20261101-132200-ab12.jsonl")));
        assert!(!is_journal_path(Path::new("/h/.tazu/transcripts/20261101-132200-ab12.jsonl")));
        assert!(!is_journal_path(Path::new("/h/.tazu/sessions/notes.txt")));
        assert_eq!(session_id_from_file_name("a b.jsonl"), None);
    }

    /// One test, in sequence: the descriptors it looks at belong to the whole test process, so two tests
    /// opening journals at once would see each other's.
    #[test]
    fn the_open_journal_of_a_process_names_its_conversation() {
        let dir = std::env::temp_dir().join(format!("kova-tazu-{}/sessions", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("20261101-132200-ab12.jsonl");
        std::fs::write(&path, "{}\n").unwrap();
        let reader = std::fs::File::open(&path).unwrap();
        assert_eq!(open_journal_in(std::process::id()), None, "a reader is not the writer");
        drop(reader);
        let held = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
        let found = open_journal_in(std::process::id()).expect("the journal this process holds");
        assert_eq!(found.file_name(), path.file_name());
        drop(held);
        assert_eq!(open_journal_in(std::process::id()), None, "a closed journal names nothing");
        let _ = std::fs::remove_dir_all(dir.parent().unwrap());
    }

    #[test]
    fn the_title_is_the_latest_title_entry_else_the_first_prompt() {
        let path = std::env::temp_dir().join(format!("kova-tazu-title-{}.jsonl", std::process::id()));
        let meta = r#"{"type": "meta", "id": "x", "cwd": "/w"}"#;
        let first = r#"{"type": "message", "message": {"role": "user", "content": "fix the build"}}"#;
        std::fs::write(&path, format!("{meta}\n{first}\n")).unwrap();
        assert_eq!(title_of(&path).as_deref(), Some("fix the build"));
        let renamed = r#"{"type": "title", "title": "Build repair"}"#;
        std::fs::write(&path, format!("{meta}\n{first}\n{renamed}\n")).unwrap();
        assert_eq!(title_of(&path).as_deref(), Some("Build repair"));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn the_resume_line_keeps_the_flags_but_not_the_conversation_or_the_prompt() {
        let id = "20261101-132200-ab12";
        assert_eq!(resume_command(None, id).as_deref(), Some("tazu --resume 20261101-132200-ab12"));
        assert_eq!(
            resume_command(Some("bin/tazu --model foo/bar --sandbox"), id).as_deref(),
            Some("bin/tazu --resume 20261101-132200-ab12 --model foo/bar --sandbox")
        );
        assert_eq!(
            resume_command(Some("tazu --resume old-1 -c -p \"do it\" --price 1 2"), id).as_deref(),
            Some("tazu --resume 20261101-132200-ab12 --price 1 2")
        );
        // Not a Tazu line: back to the bare command.
        assert_eq!(resume_command(Some("vim x"), id).as_deref(), Some("tazu --resume 20261101-132200-ab12"));
        assert_eq!(resume_command(None, "id\nrm -rf ~"), None);
    }

    /// Run by hand against a live Tazu (Tazu test suite does it): the process whose pid is in
    /// `TAZU_TEST_SHELL_PID` plays the pane's shell, and its child is a running tazu.
    #[test]
    #[ignore]
    fn a_live_tazu_under_a_shell_is_found_with_its_conversation() {
        let shell: u32 = std::env::var("TAZU_TEST_SHELL_PID").unwrap().parse().unwrap();
        let expected = std::env::var("TAZU_TEST_SESSION_ID").unwrap();
        let found = for_shell(shell).expect("a Tazu session under that shell");
        assert_eq!(found.id, expected);
        assert_eq!(found.name.as_deref(), std::env::var("TAZU_TEST_TITLE").ok().as_deref());
    }
}
