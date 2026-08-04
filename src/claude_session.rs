//! Detect the Claude Code session running inside a pane, so a quit/relaunch
//! cycle can bring it back instead of losing it.
//!
//! Claude Code writes one JSON file per live process in `~/.claude/sessions/`,
//! named after its PID and holding the `sessionId` needed by `claude --resume`.
//! The file disappears when the process exits, so the lookup must happen while
//! the session is still alive — i.e. during the session snapshot, before
//! `pty::shutdown_all()` reaps the children.
//!
//! Mapping a pane to its session goes the other way round from what one might
//! expect: instead of listing a shell's children, we read the (short) list of
//! live Claude processes and walk each one's ancestry back up to a pane shell.
//! That costs a handful of syscalls and works even when `claude` sits under a
//! wrapper process rather than directly under the shell.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use parking_lot::Mutex;

/// How far up the process tree to look for the pane's shell (claude → shell is
/// the normal case; the extra levels cover a wrapper script in between).
const MAX_ANCESTRY_DEPTH: usize = 3;

/// A session file records `startedAt` a moment after its process was exec'd,
/// so the two clocks are compared with slack. Measured drift is under a second.
const START_TIME_TOLERANCE_SECS: u64 = 30;

/// The scan is re-run at most this often. Snapshots happen on every autosave
/// (once per tab), so without this the same directory would be re-read dozens
/// of times per save for no reason.
const CACHE_TTL: Duration = Duration::from_secs(1);

static CACHE: Mutex<Option<(Instant, HashMap<u32, String>)>> = Mutex::new(None);

fn sessions_dir() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    PathBuf::from(home).join(".claude/sessions")
}

/// Parent PID and start time (epoch seconds) of a live process.
fn proc_info(pid: u32) -> Option<(u32, u64)> {
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
        Some((info.pbi_ppid, info.pbi_start_tvsec))
    }
}

/// Build a map of "ancestor PID → Claude session id" from `~/.claude/sessions/`.
/// A pane then finds its session by looking up its own shell PID.
fn scan_uncached() -> HashMap<u32, String> {
    let mut map = HashMap::new();
    let Ok(entries) = std::fs::read_dir(sessions_dir()) else {
        return map;
    };
    let own_pid = std::process::id();

    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let Ok(data) = std::fs::read_to_string(&path) else { continue };
        let Ok(json) = serde_json::from_str::<serde_json::Value>(&data) else { continue };
        let (Some(pid), Some(session_id), Some(started_at)) = (
            json.get("pid").and_then(|v| v.as_u64()).map(|v| v as u32),
            json.get("sessionId").and_then(|v| v.as_str()),
            json.get("startedAt").and_then(|v| v.as_u64()).map(|ms| ms / 1000),
        ) else {
            continue;
        };

        // The file is normally deleted when Claude Code exits, but one left
        // behind by a killed process would point at whatever later recycled
        // that PID. Identity is checked on the start time rather than the
        // process name: the executable is a version-numbered file
        // (~/.local/share/claude/versions/2.1.220), so its name is a version
        // string, not "claude".
        let Some((_, start_secs)) = proc_info(pid) else { continue };
        if start_secs.abs_diff(started_at) > START_TIME_TOLERANCE_SECS {
            log::debug!("Ignoring stale Claude session file {}", path.display());
            continue;
        }

        let mut current = pid;
        for _ in 0..MAX_ANCESTRY_DEPTH {
            let Some((parent, _)) = proc_info(current) else { break };
            if parent <= 1 || parent == own_pid {
                break;
            }
            map.insert(parent, session_id.to_string());
            current = parent;
        }
    }
    map
}

/// The Claude Code session id running under `shell_pid`, if any.
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

/// Build the command line that reopens `session_id`, reusing the flags of the
/// command that started it where possible.
///
/// `claude --resume <id>` only finds the conversation from the directory it was
/// started in, so the caller must inject this into a pane restored with the
/// original cwd.
pub fn resume_command(last_command: Option<&str>, session_id: &str) -> String {
    let base = last_command
        .map(str::trim)
        .filter(|cmd| is_claude_invocation(cmd))
        .map(|cmd| strip_session_flags(cmd))
        .filter(|tokens| !tokens.is_empty())
        .unwrap_or_else(|| vec!["claude".to_string()]);

    format!("{} --resume {}", base.join(" "), session_id)
}

/// True if `cmd` starts with a plain `claude` invocation (no alias, no env
/// prefix, no pipeline) — the only shape we can safely rewrite.
fn is_claude_invocation(cmd: &str) -> bool {
    let Some(first) = cmd.split_whitespace().next() else { return false };
    if cmd.contains('|') || cmd.contains(';') || cmd.contains('&') {
        return false;
    }
    first == "claude" || first.ends_with("/claude")
}

/// Drop the flags that select a conversation, so appending `--resume <id>`
/// cannot collide with what the previous invocation already carried.
fn strip_session_flags(cmd: &str) -> Vec<String> {
    let tokens: Vec<&str> = cmd.split_whitespace().collect();
    let mut out = Vec::with_capacity(tokens.len());
    let mut skip_value = false;

    for token in tokens {
        if skip_value {
            skip_value = false;
            // A flag follows: the previous option had no value after all.
            if !token.starts_with('-') {
                continue;
            }
        }
        match token {
            "-r" | "--resume" | "--session-id" | "--from-pr" => {
                skip_value = true;
            }
            "-c" | "--continue" | "--fork-session" => {}
            _ => out.push(token.to_string()),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Manual check against the live machine — the detection depends on Claude
    /// Code's on-disk layout, which no unit test can stand in for. Run it with
    /// at least one Claude Code session open:
    ///   cargo test -- --ignored --nocapture claude_sessions_are_detected
    #[test]
    #[ignore]
    fn claude_sessions_are_detected_on_this_machine() {
        let map = scan_uncached();
        for (pid, session) in &map {
            println!("  pid {} → {}", pid, session);
        }
        assert!(!map.is_empty(), "no live Claude Code session found");
    }

    #[test]
    fn resume_from_plain_claude() {
        assert_eq!(resume_command(Some("claude"), "abc"), "claude --resume abc");
    }

    #[test]
    fn resume_without_known_command() {
        assert_eq!(resume_command(None, "abc"), "claude --resume abc");
    }

    #[test]
    fn resume_keeps_original_flags() {
        assert_eq!(
            resume_command(Some("claude --dangerously-skip-permissions"), "abc"),
            "claude --dangerously-skip-permissions --resume abc"
        );
    }

    #[test]
    fn resume_replaces_previous_session_selection() {
        assert_eq!(resume_command(Some("claude --resume old-id"), "abc"), "claude --resume abc");
        assert_eq!(resume_command(Some("claude -r old-id"), "abc"), "claude --resume abc");
        assert_eq!(resume_command(Some("claude --continue"), "abc"), "claude --resume abc");
        assert_eq!(
            resume_command(Some("claude --session-id 1234 --debug"), "abc"),
            "claude --debug --resume abc"
        );
    }

    #[test]
    fn resume_keeps_a_flag_that_follows_a_valueless_resume() {
        // `claude --resume` with no id opens the picker; the next token is a flag.
        assert_eq!(resume_command(Some("claude --resume --debug"), "abc"), "claude --debug --resume abc");
    }

    #[test]
    fn resume_falls_back_when_the_command_is_not_a_plain_claude_call() {
        // An alias, an env prefix or a pipeline cannot be rewritten safely.
        assert_eq!(resume_command(Some("cc"), "abc"), "claude --resume abc");
        assert_eq!(resume_command(Some("RUST_LOG=debug claude"), "abc"), "claude --resume abc");
        assert_eq!(resume_command(Some("claude | tee log"), "abc"), "claude --resume abc");
        assert_eq!(resume_command(Some(""), "abc"), "claude --resume abc");
    }

    #[test]
    fn resume_accepts_an_absolute_path_to_claude() {
        assert_eq!(
            resume_command(Some("/opt/homebrew/bin/claude"), "abc"),
            "/opt/homebrew/bin/claude --resume abc"
        );
    }
}
