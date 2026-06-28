//! Unix socket IPC server for external process control.
//!
//! Listens on `/tmp/kova-{pid}.sock` and accepts JSON commands from clients.
//! Each connection is one request → one response (newline-delimited JSON).
//! All window/pane mutations are forwarded to the main thread via mpsc channel.

use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::sync::mpsc;

/// Maximum length of a single JSON line from a client (64 KB).
const MAX_LINE_LEN: usize = 65536;

/// Filter for commands that act on a set of panes.
pub enum PaneFilter {
    /// All panes across all windows.
    All,
    /// Specific pane IDs (preserves caller's order, including duplicates).
    Ids(Vec<u32>),
}

/// A command received from an IPC client.
pub enum IpcCommand {
    /// Create a new split in the focused pane of the active window.
    Split {
        direction: String,
        cmd: Option<String>,
        cwd: Option<String>,
    },
    /// List all panes across all windows.
    ListPanes,
    /// Close a pane by its ID.
    ClosePaneById(u32),
    /// Write text to a pane's PTY.
    SendKeys { pane_id: u32, text: String },
    /// Focus a pane by its ID (switching tab/window if needed).
    FocusPane(u32),
    /// Create a new tab with an optional CWD and command.
    NewTab {
        cwd: Option<String>,
        cmd: Option<String>,
    },
    /// Set the custom title of the tab containing the given pane.
    /// `title: None` clears the custom title (falls back to auto-derived title).
    SetTabTitle {
        pane_id: u32,
        title: Option<String>,
    },
    /// Return the rendered text of the requested panes.
    GetPaneContent {
        panes: PaneFilter,
        mode: String,
        trim_trailing_blank_lines: bool,
    },
    /// Return the size (chars + bytes) the equivalent `GetPaneContent` would produce.
    /// Lets the caller decide whether the payload is worth fetching — no cap is enforced.
    CountPaneContent {
        panes: PaneFilter,
        mode: String,
        trim_trailing_blank_lines: bool,
    },
    /// Block until a shell command in `pane_id` reports completion via OSC 133;D,
    /// or until `timeout_ms` elapses. Returns immediately if the flag is already set.
    WaitForCompletion {
        pane_id: u32,
        timeout_ms: u64,
    },
    /// List all tabs across all windows.
    ListTabs,
    /// Close a tab by ID. Refuses if it would close the last tab (would terminate the app).
    CloseTab(u32),
    /// Merge `source_tab_id` into `target_tab_id`: source columns are appended to target,
    /// then the source tab is removed. Both tabs must live in the same window.
    MergeTab {
        source_tab_id: u32,
        target_tab_id: u32,
    },
    /// Swap two panes. Both must live in the same tab.
    /// Same column → swap inside the column. Different columns → swap the whole columns.
    SwapPane {
        pane_id_a: u32,
        pane_id_b: u32,
    },
    /// Adjust the ratio of the split containing `pane_id`.
    /// `axis = "horizontal"` resizes the column; `axis = "vertical"` resizes the row.
    /// `direction = "grow" | "shrink"`. `amount_pct` is in [0.1, 50.0].
    ResizePane {
        pane_id: u32,
        axis: String,
        direction: String,
        amount_pct: f32,
    },
    /// Set/clear a pane's custom title (sticky — equivalent to OSC 1 or Cmd+Option+R).
    /// `title: None` clears the custom title (pane falls back to OSC 0/2 or auto-derived).
    RenamePane {
        pane_id: u32,
        title: Option<String>,
    },
    /// Trigger any keyboard action by its stable name (see `action_from_ipc_name`).
    /// `pane_id` optionally targets (and focuses) a specific pane's window first;
    /// without it, the action runs against the key window.
    DispatchAction {
        action: String,
        pane_id: Option<u32>,
    },
    /// Merge every tab of `source_window` into `target_window`, then close the
    /// now-empty source window. Windows are addressed by the index reported in
    /// `list-tabs` / `list-panes` (`"window"` field).
    MergeWindow {
        source_window: usize,
        target_window: usize,
    },
}

/// How long the IPC connection thread should wait for the main thread's response.
/// Most commands reply within microseconds; `wait-for-completion` may legitimately
/// take up to its requested timeout, so we extend the deadline accordingly.
pub fn command_recv_timeout(cmd: &IpcCommand) -> std::time::Duration {
    match cmd {
        IpcCommand::WaitForCompletion { timeout_ms, .. } => {
            // Add a 2s buffer so the main thread always has time to send back
            // the timeout response itself before the connection gives up.
            std::time::Duration::from_millis(timeout_ms.saturating_add(2_000))
        }
        _ => std::time::Duration::from_secs(10),
    }
}

/// Response sent back to the IPC client.
pub enum IpcResponse {
    Ok { data: Option<serde_json::Value> },
    Error { message: String },
}

impl IpcResponse {
    fn to_json(&self) -> serde_json::Value {
        match self {
            IpcResponse::Ok { data } => {
                let mut obj = serde_json::json!({"ok": true});
                if let Some(d) = data {
                    obj["data"] = d.clone();
                }
                obj
            }
            IpcResponse::Error { message } => {
                serde_json::json!({"ok": false, "error": message})
            }
        }
    }
}

/// A pending IPC request: the command plus a channel to send the response back.
pub type IpcRequest = (IpcCommand, mpsc::Sender<IpcResponse>);

/// Guard that removes the socket file on drop.
struct SocketCleanup {
    path: PathBuf,
}

impl Drop for SocketCleanup {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
        log::debug!("IPC socket removed: {}", self.path.display());
    }
}

/// Start the IPC server on a background thread.
///
/// Returns the receiver end of the channel — the main thread polls this
/// in its timer tick to process commands.
pub fn start(
) -> mpsc::Receiver<IpcRequest> {
    let (tx, rx) = mpsc::channel::<IpcRequest>();

    std::thread::Builder::new()
        .name("ipc-listener".into())
        .spawn(move || {
            let path = socket_path();

            // Remove stale socket from a previous crash
            if path.exists() {
                let _ = std::fs::remove_file(&path);
            }

            // Tighten umask so the socket inode is born owner-only (closes the
            // TOCTOU window between bind() and the chmod below).
            #[cfg(unix)]
            let prev_umask = unsafe { libc::umask(0o077) };

            let listener = match UnixListener::bind(&path) {
                Ok(l) => l,
                Err(e) => {
                    #[cfg(unix)]
                    unsafe { libc::umask(prev_umask); }
                    log::error!("IPC: failed to bind {}: {}", path.display(), e);
                    return;
                }
            };

            #[cfg(unix)]
            unsafe { libc::umask(prev_umask); }

            // Belt and suspenders: enforce 0o600 even if umask didn't take.
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let perms = std::fs::Permissions::from_mode(0o600);
                let _ = std::fs::set_permissions(&path, perms);
            }

            // Guard ensures cleanup even on panic
            let _cleanup = SocketCleanup { path: path.clone() };

            log::info!("IPC: listening on {}", path.display());

            for stream in listener.incoming() {
                let stream = match stream {
                    Ok(s) => s,
                    Err(e) => {
                        log::warn!("IPC: accept error: {}", e);
                        continue;
                    }
                };

                let tx = tx.clone();
                std::thread::Builder::new()
                    .name("ipc-conn".into())
                    .spawn(move || {
                        handle_connection(stream, tx);
                    })
                    .ok();
            }
        })
        .expect("failed to spawn IPC listener thread");

    rx
}

/// Handle a single client connection: read one JSON line, dispatch, respond.
fn handle_connection(
    stream: std::os::unix::net::UnixStream,
    tx: mpsc::Sender<IpcRequest>,
) {
    // Set a read timeout so a misbehaving client doesn't block the thread forever
    let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(5)));

    let mut reader = BufReader::new(&stream);
    let mut writer = &stream;

    let mut buf: Vec<u8> = Vec::new();
    loop {
        buf.clear();
        // Bound each read so an unterminated line can't grow memory without
        // limit — the length check must happen BEFORE the full line is buffered.
        match std::io::Read::by_ref(&mut reader)
            .take((MAX_LINE_LEN + 2) as u64)
            .read_until(b'\n', &mut buf)
        {
            Ok(0) => break,
            Ok(_) => {}
            Err(e) => {
                log::debug!("IPC: read error: {}", e);
                break;
            }
        }
        while matches!(buf.last(), Some(b'\n') | Some(b'\r')) {
            buf.pop();
        }
        if buf.len() > MAX_LINE_LEN {
            let resp = IpcResponse::Error { message: "request too large".to_string() };
            let _ = writeln!(writer, "{}", resp.to_json());
            break;
        }

        let line = String::from_utf8_lossy(&buf).trim().to_string();
        if line.is_empty() {
            continue;
        }

        let response = match parse_command(&line) {
            Ok(cmd) => {
                let timeout = command_recv_timeout(&cmd);
                // Create a one-shot response channel
                let (resp_tx, resp_rx) = mpsc::channel::<IpcResponse>();
                if tx.send((cmd, resp_tx)).is_err() {
                    IpcResponse::Error {
                        message: "app shutting down".to_string(),
                    }
                } else {
                    // Block until the main thread sends a response (or channel drops)
                    match resp_rx.recv_timeout(timeout) {
                        Ok(r) => r,
                        Err(_) => IpcResponse::Error {
                            message: "timeout waiting for response".to_string(),
                        },
                    }
                }
            }
            Err(msg) => IpcResponse::Error { message: msg },
        };

        let json = response.to_json().to_string();
        if writeln!(writer, "{}", json).is_err() {
            break;
        }
        let _ = writer.flush();
    }
}

/// The set of accepted top-level fields for a command (besides `cmd`, which is
/// always legitimate). Returns `None` for an unknown command, so the dispatcher's
/// `unknown command` arm handles it instead of an `unknown field` error.
///
/// Must stay in sync with the per-command parsing in `parse_command` (and with
/// `parse_pane_content_args` for the two pane-content commands).
fn allowed_fields(cmd: &str) -> Option<&'static [&'static str]> {
    Some(match cmd {
        "split" => &["direction", "command", "cwd"],
        "list-panes" => &[],
        "close-pane" => &["pane_id"],
        "send-keys" => &["pane_id", "text"],
        "focus-pane" => &["pane_id"],
        "new-tab" => &["cwd", "command"],
        "set-tab-title" => &["pane_id", "title"],
        "get-pane-content" | "count-pane-content" => {
            &["panes", "mode", "trim_trailing_blank_lines"]
        }
        "wait-for-completion" => &["pane_id", "timeout_ms"],
        "list-tabs" => &[],
        "close-tab" => &["tab_id"],
        "merge-tab" => &["source_tab_id", "target_tab_id"],
        "swap-pane" => &["pane_id_a", "pane_id_b"],
        "resize-pane" => &["pane_id", "axis", "direction", "amount_pct"],
        "rename-pane" => &["pane_id", "title"],
        "dispatch-action" => &["action", "pane_id"],
        "merge-window" => &["source_window", "target_window"],
        _ => return None,
    })
}

/// Parse a JSON line into an IpcCommand.
fn parse_command(line: &str) -> Result<IpcCommand, String> {
    let v: serde_json::Value =
        serde_json::from_str(line).map_err(|e| format!("invalid JSON: {}", e))?;

    let cmd = v
        .get("cmd")
        .and_then(|c| c.as_str())
        .ok_or_else(|| "missing \"cmd\" field".to_string())?;

    // Reject unknown fields BEFORE the per-command parsing below. Without this,
    // a stray key (e.g. `pane_id` on a command that expects `panes`) is silently
    // ignored and the command does something other than asked — a muted failure.
    // Unknown commands are left to the match's `unknown command` arm.
    if let Some(allowed) = allowed_fields(cmd) {
        if let Some(obj) = v.as_object() {
            for key in obj.keys() {
                if key != "cmd" && !allowed.contains(&key.as_str()) {
                    return Err(format!("unknown field \"{}\" for command \"{}\"", key, cmd));
                }
            }
        }
    }

    match cmd {
        "split" => {
            let direction = v
                .get("direction")
                .and_then(|d| d.as_str())
                .unwrap_or("horizontal")
                .to_string();
            if direction != "horizontal" && direction != "vertical" {
                return Err(format!("invalid direction: {}", direction));
            }
            let cmd_str = v.get("command").and_then(|c| c.as_str()).map(String::from);
            let cwd = v.get("cwd").and_then(|c| c.as_str()).map(String::from);
            if let Some(ref p) = cwd {
                let path = std::path::Path::new(p);
                if !path.is_absolute() {
                    return Err(format!("cwd must be absolute: {}", p));
                }
                if !path.is_dir() {
                    return Err(format!("cwd does not exist or is not a directory: {}", p));
                }
            }
            Ok(IpcCommand::Split {
                direction,
                cmd: cmd_str,
                cwd,
            })
        }
        "list-panes" => Ok(IpcCommand::ListPanes),
        "close-pane" => {
            let pane_id = v
                .get("pane_id")
                .and_then(|p| p.as_u64())
                .ok_or_else(|| "missing \"pane_id\" field".to_string())?
                as u32;
            Ok(IpcCommand::ClosePaneById(pane_id))
        }
        "send-keys" => {
            let pane_id = v
                .get("pane_id")
                .and_then(|p| p.as_u64())
                .ok_or_else(|| "missing \"pane_id\" field".to_string())?
                as u32;
            let text = v
                .get("text")
                .and_then(|t| t.as_str())
                .ok_or_else(|| "missing \"text\" field".to_string())?
                .to_string();
            Ok(IpcCommand::SendKeys { pane_id, text })
        }
        "focus-pane" => {
            let pane_id = v
                .get("pane_id")
                .and_then(|p| p.as_u64())
                .ok_or_else(|| "missing \"pane_id\" field".to_string())?
                as u32;
            Ok(IpcCommand::FocusPane(pane_id))
        }
        "new-tab" => {
            let cwd = v.get("cwd").and_then(|c| c.as_str()).map(String::from);
            let cmd_str = v.get("command").and_then(|c| c.as_str()).map(String::from);
            Ok(IpcCommand::NewTab { cwd, cmd: cmd_str })
        }
        "set-tab-title" => {
            let pane_id = v
                .get("pane_id")
                .and_then(|p| p.as_u64())
                .ok_or_else(|| "missing \"pane_id\" field".to_string())?
                as u32;
            let title = match v.get("title") {
                None | Some(serde_json::Value::Null) => None,
                Some(serde_json::Value::String(s)) => Some(s.clone()),
                Some(_) => return Err("\"title\" must be a string or null".to_string()),
            };
            Ok(IpcCommand::SetTabTitle { pane_id, title })
        }
        "get-pane-content" => {
            let (panes, mode, trim) = parse_pane_content_args(&v)?;
            Ok(IpcCommand::GetPaneContent { panes, mode, trim_trailing_blank_lines: trim })
        }
        "count-pane-content" => {
            let (panes, mode, trim) = parse_pane_content_args(&v)?;
            Ok(IpcCommand::CountPaneContent { panes, mode, trim_trailing_blank_lines: trim })
        }
        "wait-for-completion" => {
            let pane_id = v
                .get("pane_id")
                .and_then(|p| p.as_u64())
                .ok_or_else(|| "missing \"pane_id\" field".to_string())?
                as u32;
            // Default 30s, capped at 5 min — keeps the connection thread from
            // sitting on a half-dead client indefinitely.
            let timeout_ms = match v.get("timeout_ms") {
                None | Some(serde_json::Value::Null) => 30_000,
                Some(t) => t
                    .as_u64()
                    .ok_or_else(|| "\"timeout_ms\" must be a non-negative integer".to_string())?,
            };
            const MAX_TIMEOUT_MS: u64 = 300_000;
            if timeout_ms > MAX_TIMEOUT_MS {
                return Err(format!(
                    "\"timeout_ms\" too large ({}ms) — max is {}ms",
                    timeout_ms, MAX_TIMEOUT_MS
                ));
            }
            Ok(IpcCommand::WaitForCompletion { pane_id, timeout_ms })
        }
        "list-tabs" => Ok(IpcCommand::ListTabs),
        "close-tab" => {
            let tab_id = v
                .get("tab_id")
                .and_then(|p| p.as_u64())
                .ok_or_else(|| "missing \"tab_id\" field".to_string())?
                as u32;
            Ok(IpcCommand::CloseTab(tab_id))
        }
        "merge-tab" => {
            let source_tab_id = v
                .get("source_tab_id")
                .and_then(|p| p.as_u64())
                .ok_or_else(|| "missing \"source_tab_id\" field".to_string())?
                as u32;
            let target_tab_id = v
                .get("target_tab_id")
                .and_then(|p| p.as_u64())
                .ok_or_else(|| "missing \"target_tab_id\" field".to_string())?
                as u32;
            if source_tab_id == target_tab_id {
                return Err("source_tab_id and target_tab_id must differ".to_string());
            }
            Ok(IpcCommand::MergeTab { source_tab_id, target_tab_id })
        }
        "swap-pane" => {
            let pane_id_a = v
                .get("pane_id_a")
                .and_then(|p| p.as_u64())
                .ok_or_else(|| "missing \"pane_id_a\" field".to_string())?
                as u32;
            let pane_id_b = v
                .get("pane_id_b")
                .and_then(|p| p.as_u64())
                .ok_or_else(|| "missing \"pane_id_b\" field".to_string())?
                as u32;
            if pane_id_a == pane_id_b {
                return Err("pane_id_a and pane_id_b must differ".to_string());
            }
            Ok(IpcCommand::SwapPane { pane_id_a, pane_id_b })
        }
        "resize-pane" => {
            let pane_id = v
                .get("pane_id")
                .and_then(|p| p.as_u64())
                .ok_or_else(|| "missing \"pane_id\" field".to_string())?
                as u32;
            let axis = v
                .get("axis")
                .and_then(|a| a.as_str())
                .unwrap_or("horizontal")
                .to_string();
            if axis != "horizontal" && axis != "vertical" {
                return Err(format!("\"axis\" must be \"horizontal\" or \"vertical\" (got \"{}\")", axis));
            }
            let direction = v
                .get("direction")
                .and_then(|d| d.as_str())
                .ok_or_else(|| "missing \"direction\" field".to_string())?
                .to_string();
            if direction != "grow" && direction != "shrink" {
                return Err(format!("\"direction\" must be \"grow\" or \"shrink\" (got \"{}\")", direction));
            }
            let amount_pct = match v.get("amount_pct") {
                None | Some(serde_json::Value::Null) => 5.0_f32,
                Some(a) => {
                    let f = a
                        .as_f64()
                        .ok_or_else(|| "\"amount_pct\" must be a number".to_string())?
                        as f32;
                    if !(0.1..=50.0).contains(&f) {
                        return Err(format!("\"amount_pct\" must be in [0.1, 50.0] (got {})", f));
                    }
                    f
                }
            };
            Ok(IpcCommand::ResizePane { pane_id, axis, direction, amount_pct })
        }
        "rename-pane" => {
            let pane_id = v
                .get("pane_id")
                .and_then(|p| p.as_u64())
                .ok_or_else(|| "missing \"pane_id\" field".to_string())?
                as u32;
            let title = match v.get("title") {
                None | Some(serde_json::Value::Null) => None,
                Some(serde_json::Value::String(s)) => Some(s.clone()),
                Some(_) => return Err("\"title\" must be a string or null".to_string()),
            };
            Ok(IpcCommand::RenamePane { pane_id, title })
        }
        "dispatch-action" => {
            let action = v
                .get("action")
                .and_then(|a| a.as_str())
                .ok_or_else(|| "missing \"action\" field".to_string())?
                .to_string();
            let pane_id = match v.get("pane_id") {
                None | Some(serde_json::Value::Null) => None,
                Some(p) => Some(
                    p.as_u64()
                        .ok_or_else(|| "\"pane_id\" must be a non-negative integer".to_string())?
                        as u32,
                ),
            };
            Ok(IpcCommand::DispatchAction { action, pane_id })
        }
        "merge-window" => {
            let source_window = v
                .get("source_window")
                .and_then(|p| p.as_u64())
                .ok_or_else(|| "missing \"source_window\" field".to_string())?
                as usize;
            let target_window = v
                .get("target_window")
                .and_then(|p| p.as_u64())
                .ok_or_else(|| "missing \"target_window\" field".to_string())?
                as usize;
            if source_window == target_window {
                return Err("source_window and target_window must differ".to_string());
            }
            Ok(IpcCommand::MergeWindow { source_window, target_window })
        }
        other => Err(format!("unknown command: {}", other)),
    }
}

/// Shared parser for `get-pane-content` and `count-pane-content` arguments.
///
/// Returns `(panes, mode, trim_trailing_blank_lines)`. Defaults:
/// - `panes`: omitted / null → `All`; `"all"` → `All`; array of integers → `Ids`.
/// - `mode`: `"visible"` (must be one of `visible|scrollback|all`).
/// - `trim_trailing_blank_lines`: `true`.
fn parse_pane_content_args(
    v: &serde_json::Value,
) -> Result<(PaneFilter, String, bool), String> {
    let panes = match v.get("panes") {
        None | Some(serde_json::Value::Null) => PaneFilter::All,
        Some(serde_json::Value::String(s)) if s == "all" => PaneFilter::All,
        Some(serde_json::Value::String(s)) => {
            return Err(format!("\"panes\" string must be \"all\", got \"{}\"", s));
        }
        Some(serde_json::Value::Array(arr)) => {
            let mut ids = Vec::with_capacity(arr.len());
            for (i, item) in arr.iter().enumerate() {
                let id = item.as_u64().ok_or_else(|| {
                    format!("\"panes\"[{}] must be a non-negative integer", i)
                })?;
                ids.push(id as u32);
            }
            PaneFilter::Ids(ids)
        }
        Some(_) => {
            return Err(
                "\"panes\" must be the string \"all\" or an array of integer ids".to_string(),
            );
        }
    };

    let mode = v
        .get("mode")
        .and_then(|m| m.as_str())
        .unwrap_or("visible")
        .to_string();
    if mode != "visible" && mode != "scrollback" && mode != "all" {
        return Err(format!(
            "\"mode\" must be one of \"visible\", \"scrollback\", \"all\" (got \"{}\")",
            mode
        ));
    }

    let trim = match v.get("trim_trailing_blank_lines") {
        None | Some(serde_json::Value::Null) => true,
        Some(serde_json::Value::Bool(b)) => *b,
        Some(_) => {
            return Err("\"trim_trailing_blank_lines\" must be a boolean".to_string());
        }
    };

    Ok((panes, mode, trim))
}

/// The canonical socket path for this process.
pub fn socket_path() -> PathBuf {
    Path::new("/tmp").join(format!("kova-{}.sock", std::process::id()))
}

/// Remove the socket file (called from will_terminate for explicit cleanup).
pub fn cleanup() {
    let path = socket_path();
    if path.exists() {
        let _ = std::fs::remove_file(&path);
        log::debug!("IPC: socket cleaned up at {}", path.display());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn err(line: &str) -> String {
        parse_command(line).err().expect("expected an error")
    }

    #[test]
    fn rejects_unknown_field() {
        // The motivating bug: get-pane-content silently ignored pane_id and
        // defaulted panes to "all". It must now fail loudly.
        assert_eq!(
            err(r#"{"cmd":"get-pane-content","pane_id":232}"#),
            "unknown field \"pane_id\" for command \"get-pane-content\""
        );
        // count-pane-content shares the same allowed set.
        assert_eq!(
            err(r#"{"cmd":"count-pane-content","pane_id":1}"#),
            "unknown field \"pane_id\" for command \"count-pane-content\""
        );
        // A typo on a normal command.
        assert_eq!(
            err(r#"{"cmd":"send-keys","pane_id":1,"text":"x","panes":"all"}"#),
            "unknown field \"panes\" for command \"send-keys\""
        );
        // A command that takes no fields at all.
        assert_eq!(
            err(r#"{"cmd":"list-panes","pane_id":1}"#),
            "unknown field \"pane_id\" for command \"list-panes\""
        );
    }

    #[test]
    fn accepts_documented_fields() {
        assert!(parse_command(
            r#"{"cmd":"get-pane-content","panes":[1,2],"mode":"all","trim_trailing_blank_lines":false}"#
        )
        .is_ok());
        // `split`/`new-tab` use `command`, not `cmd`, for the shell command.
        assert!(parse_command(
            r#"{"cmd":"split","direction":"vertical","command":"ls"}"#
        )
        .is_ok());
        assert!(parse_command(r#"{"cmd":"list-panes"}"#).is_ok());
    }

    #[test]
    fn unknown_command_takes_precedence_over_field_check() {
        assert_eq!(err(r#"{"cmd":"bogus","whatever":1}"#), "unknown command: bogus");
    }
}
