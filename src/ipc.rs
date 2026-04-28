//! Unix socket IPC server for external process control.
//!
//! Listens on `/tmp/kova-{pid}.sock` and accepts JSON commands from clients.
//! Each connection is one request → one response (newline-delimited JSON).
//! All window/pane mutations are forwarded to the main thread via mpsc channel.

use std::io::{BufRead, BufReader, Write};
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

    let reader = BufReader::new(&stream);
    let mut writer = &stream;

    for line in reader.lines() {
        let line = match line {
            Ok(l) => l,
            Err(e) => {
                log::debug!("IPC: read error: {}", e);
                break;
            }
        };

        if line.len() > MAX_LINE_LEN {
            let resp = IpcResponse::Error { message: "request too large".to_string() };
            let _ = writeln!(writer, "{}", resp.to_json());
            break;
        }

        let line = line.trim().to_string();
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

/// Parse a JSON line into an IpcCommand.
fn parse_command(line: &str) -> Result<IpcCommand, String> {
    let v: serde_json::Value =
        serde_json::from_str(line).map_err(|e| format!("invalid JSON: {}", e))?;

    let cmd = v
        .get("cmd")
        .and_then(|c| c.as_str())
        .ok_or_else(|| "missing \"cmd\" field".to_string())?;

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
