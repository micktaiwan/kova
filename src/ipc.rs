//! Unix socket IPC server for external process control.
//!
//! Listens on `/tmp/kova-{pid}.sock` and accepts JSON commands from clients.
//! A connection is a stream of newline-delimited JSON requests, each answered by
//! one response line. All window/pane mutations are forwarded to the main thread
//! via mpsc channel.
//!
//! One command breaks that shape: `subscribe` turns the connection into a
//! one-way event stream (see the "Event subscriptions" section at the bottom).
//! After its response — a snapshot of the current state — Kova pushes one JSON
//! line per state change and never reads from that connection again.

use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{mpsc, Mutex};

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
    /// Set/clear the "this pane is waiting for the user" flag. Pushed by the
    /// app running in the pane (Claude Code's hooks) rather than guessed by
    /// Kova. `waiting: false` retracts it; Kova also retracts it on its own
    /// when the pane contradicts the claim (see `Pane::is_awaiting`).
    SetPaneStatus {
        pane_id: u32,
        waiting: bool,
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
    /// Post a desktop notification. Clicking it focuses `pane_id`.
    /// Kova posts it itself because it is the only process that can act on the
    /// click — see `crate::notification`.
    Notify {
        pane_id: Option<u32>,
        title: String,
        message: String,
        sound: bool,
    },
    /// Turn this connection into an event stream for the given topics.
    /// The main thread answers with a snapshot of the current state; every
    /// change after that is pushed as its own line. See `topic`.
    Subscribe {
        topics: u32,
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
            Ok(IpcCommand::Subscribe { topics }) => {
                // Register BEFORE asking the main thread for the snapshot. An event
                // that fires in between then lands in this subscriber's queue and is
                // delivered just after the snapshot: the client may see an edge it
                // already knows about (harmless — every event carries absolute state,
                // so applying it twice changes nothing), but it can never miss one.
                // The reverse order would open a real gap.
                let (sub_id, events) = register_subscriber(topics);
                let response = dispatch(&tx, IpcCommand::Subscribe { topics });
                // Only stream behind a snapshot the client actually got. If the
                // main thread refused (shutting down, timed out), streaming would
                // park this thread on a subscription no tick will ever feed.
                let live = matches!(response, IpcResponse::Ok { .. });
                let sent = writeln!(writer, "{}", response.to_json()).is_ok()
                    && writer.flush().is_ok();
                if live && sent {
                    // From here the connection is one-way: we never read from it
                    // again. A client that also needs to issue commands opens a
                    // second connection.
                    stream_events(&stream, events);
                }
                unregister_subscriber(sub_id);
                return;
            }
            Ok(cmd) => dispatch(&tx, cmd),
            Err(msg) => IpcResponse::Error { message: msg },
        };

        let json = response.to_json().to_string();
        if writeln!(writer, "{}", json).is_err() {
            break;
        }
        let _ = writer.flush();
    }
}

/// Hand one command to the main thread and block on its answer.
///
/// The main thread drains these in its render tick, so the wait is normally
/// microseconds; `command_recv_timeout` gives `wait-for-completion` the longer
/// deadline it legitimately needs.
fn dispatch(tx: &mpsc::Sender<IpcRequest>, cmd: IpcCommand) -> IpcResponse {
    let timeout = command_recv_timeout(&cmd);
    let (resp_tx, resp_rx) = mpsc::channel::<IpcResponse>();
    if tx.send((cmd, resp_tx)).is_err() {
        return IpcResponse::Error {
            message: "app shutting down".to_string(),
        };
    }
    match resp_rx.recv_timeout(timeout) {
        Ok(r) => r,
        Err(_) => IpcResponse::Error {
            message: "timeout waiting for response".to_string(),
        },
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
        "set-pane-status" => &["pane_id", "status"],
        "dispatch-action" => &["action", "pane_id"],
        "merge-window" => &["source_window", "target_window"],
        "notify" => &["pane_id", "title", "message", "sound"],
        "subscribe" => &["events"],
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
        "set-pane-status" => {
            let pane_id = v
                .get("pane_id")
                .and_then(|p| p.as_u64())
                .ok_or_else(|| "missing \"pane_id\" field".to_string())?
                as u32;
            let waiting = match v.get("status").and_then(|s| s.as_str()) {
                Some("waiting") => true,
                Some("none") => false,
                _ => {
                    return Err(
                        "\"status\" must be \"waiting\" or \"none\"".to_string()
                    )
                }
            };
            Ok(IpcCommand::SetPaneStatus { pane_id, waiting })
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
        "notify" => {
            let message = v
                .get("message")
                .and_then(|m| m.as_str())
                .ok_or_else(|| "missing \"message\" field".to_string())?
                .to_string();
            let title = v
                .get("title")
                .and_then(|t| t.as_str())
                .unwrap_or("Kova")
                .to_string();
            // Optional: a notification with no pane just brings Kova to the front.
            let pane_id = match v.get("pane_id") {
                None | Some(serde_json::Value::Null) => None,
                Some(p) => Some(
                    p.as_u64()
                        .ok_or_else(|| "\"pane_id\" must be a number".to_string())?
                        as u32,
                ),
            };
            let sound = match v.get("sound") {
                None | Some(serde_json::Value::Null) => false,
                Some(serde_json::Value::Bool(b)) => *b,
                Some(_) => return Err("\"sound\" must be a boolean".to_string()),
            };
            Ok(IpcCommand::Notify { pane_id, title, message, sound })
        }
        "subscribe" => {
            // Omitted / null = every topic. An explicit list is validated name by
            // name: a typo must fail loudly, exactly like an unknown field, rather
            // than leave the client waiting forever for events it will never get.
            let topics = match v.get("events") {
                None | Some(serde_json::Value::Null) => topic::ALL,
                Some(serde_json::Value::Array(items)) => {
                    if items.is_empty() {
                        return Err("\"events\" must not be empty (omit it to get every topic)".to_string());
                    }
                    let mut mask = 0u32;
                    for item in items {
                        let name = item
                            .as_str()
                            .ok_or_else(|| "\"events\" must be an array of strings".to_string())?;
                        match topic::from_name(name) {
                            Some(bit) => mask |= bit,
                            None => {
                                return Err(format!(
                                    "unknown event \"{}\" — known events: {}",
                                    name,
                                    topic::ALL_NAMES.join(", ")
                                ))
                            }
                        }
                    }
                    mask
                }
                Some(_) => return Err("\"events\" must be an array of strings".to_string()),
            };
            Ok(IpcCommand::Subscribe { topics })
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

// ─── Event subscriptions ────────────────────────────────────────────────────

/// The topics a client can subscribe to.
///
/// A bitmask rather than a list of strings because the publisher's "is anyone
/// listening?" test runs on Kova's render tick: it has to be one relaxed atomic
/// load, and cost nothing at all when nobody is subscribed (the normal case).
pub mod topic {
    /// The pane holding the user's attention changed — including losing it
    /// entirely when Kova stops being the active app.
    pub const FOCUS: u32 = 1 << 0;
    /// A pane's `awaiting` flag was raised or retracted.
    pub const PANE_STATUS: u32 = 1 << 1;
    /// A pane started or stopped working (the Braille spinner in its title).
    pub const PANE_WORKING: u32 = 1 << 2;
    /// A pane appeared.
    pub const PANE_OPEN: u32 = 1 << 3;
    /// A pane went away.
    pub const PANE_CLOSE: u32 = 1 << 4;

    pub const ALL: u32 = FOCUS | PANE_STATUS | PANE_WORKING | PANE_OPEN | PANE_CLOSE;

    /// Wire names, in bit order — `names()` relies on that ordering.
    pub const ALL_NAMES: [&str; 5] = [
        "focus",
        "pane-status",
        "pane-working",
        "pane-open",
        "pane-close",
    ];

    pub fn from_name(name: &str) -> Option<u32> {
        let bit = ALL_NAMES.iter().position(|n| *n == name)?;
        Some(1 << bit)
    }

    /// The names covered by `mask`, in bit order (echoed back in the subscribe
    /// snapshot so a client can see what it actually got).
    pub fn names(mask: u32) -> Vec<&'static str> {
        ALL_NAMES
            .iter()
            .enumerate()
            .filter(|(bit, _)| mask & (1 << bit) != 0)
            .map(|(_, name)| *name)
            .collect()
    }
}

/// How many events a subscriber may fall behind before Kova gives up on it.
const EVENT_QUEUE_CAP: usize = 256;

/// Silence after which a subscribed connection is sent a `ping`.
const EVENT_HEARTBEAT: std::time::Duration = std::time::Duration::from_secs(30);

/// How long a write to a subscriber may block before we call the client dead.
/// Only reached if it stopped reading and the socket buffer filled up.
const EVENT_WRITE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

struct Subscriber {
    id: u64,
    topics: u32,
    /// Bounded on purpose: `publish` runs on the main thread and must never
    /// block, so it `try_send`s and drops the client when the queue is full.
    tx: mpsc::SyncSender<String>,
}

static SUBSCRIBERS: Mutex<Vec<Subscriber>> = Mutex::new(Vec::new());
/// Union of every subscriber's topics — what `has_subscribers` reads.
static SUBSCRIBED_TOPICS: AtomicU32 = AtomicU32::new(0);
static NEXT_SUBSCRIBER_ID: AtomicU64 = AtomicU64::new(1);

/// A poisoned lock must not disable the event system for the rest of the run:
/// the data behind it is a plain Vec, and a panic mid-publish leaves it valid.
fn subscribers() -> std::sync::MutexGuard<'static, Vec<Subscriber>> {
    SUBSCRIBERS.lock().unwrap_or_else(|e| e.into_inner())
}

fn refresh_topic_mask(subs: &[Subscriber]) {
    let mask = subs.iter().fold(0u32, |acc, sub| acc | sub.topics);
    SUBSCRIBED_TOPICS.store(mask, Ordering::Relaxed);
}

fn register_subscriber(topics: u32) -> (u64, mpsc::Receiver<String>) {
    let (tx, rx) = mpsc::sync_channel::<String>(EVENT_QUEUE_CAP);
    let id = NEXT_SUBSCRIBER_ID.fetch_add(1, Ordering::Relaxed);
    let mut subs = subscribers();
    subs.push(Subscriber { id, topics, tx });
    refresh_topic_mask(&subs);
    log::info!("IPC: subscriber {} listening on {:?}", id, topic::names(topics));
    (id, rx)
}

fn unregister_subscriber(id: u64) {
    let mut subs = subscribers();
    subs.retain(|sub| sub.id != id);
    refresh_topic_mask(&subs);
    log::info!("IPC: subscriber {} gone", id);
}

/// True if at least one client wants any of `topics`. Lets the main thread skip
/// the whole state-diffing pass when nobody is listening.
pub fn has_subscribers(topics: u32) -> bool {
    SUBSCRIBED_TOPICS.load(Ordering::Relaxed) & topics != 0
}

/// Push one event to every subscriber of `topic`. Called from the main thread.
///
/// Never blocks and never touches a socket: it fills bounded queues that the
/// connection threads drain. A client too slow to keep up is dropped rather than
/// served a hole — it must reconnect, and a fresh `subscribe` hands it a
/// snapshot, so it resyncs instead of carrying a silently wrong view of the world.
pub fn publish(topic: u32, event: serde_json::Value) {
    if !has_subscribers(topic) {
        return;
    }
    let line = event.to_string();
    let mut subs = subscribers();
    let before = subs.len();
    subs.retain(|sub| {
        if sub.topics & topic == 0 {
            return true;
        }
        match sub.tx.try_send(line.clone()) {
            Ok(()) => true,
            Err(mpsc::TrySendError::Full(_)) => {
                log::warn!(
                    "IPC: subscriber {} fell {} events behind — dropping it",
                    sub.id,
                    EVENT_QUEUE_CAP
                );
                false
            }
            Err(mpsc::TrySendError::Disconnected(_)) => false,
        }
    });
    if subs.len() != before {
        refresh_topic_mask(&subs);
    }
}

/// Pump queued events to a subscribed client until its connection dies.
///
/// Runs on that connection's own thread, which is the whole point: the socket
/// write happens here, never on the main thread that produced the event.
fn stream_events(stream: &UnixStream, events: mpsc::Receiver<String>) {
    let mut writer = stream;
    let _ = stream.set_write_timeout(Some(EVENT_WRITE_TIMEOUT));
    loop {
        let line = match events.recv_timeout(EVENT_HEARTBEAT) {
            Ok(line) => line,
            // Nothing happened for a while. The ping is how we notice a client
            // that died without closing (the write fails), and how a client
            // notices Kova is gone (silence past its own watchdog).
            Err(mpsc::RecvTimeoutError::Timeout) => r#"{"event":"ping"}"#.to_string(),
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        };
        if writeln!(writer, "{}", line).is_err() || writer.flush().is_err() {
            break;
        }
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
    fn set_pane_status_parses_both_states() {
        assert!(matches!(
            parse_command(r#"{"cmd":"set-pane-status","pane_id":7,"status":"waiting"}"#),
            Ok(IpcCommand::SetPaneStatus { pane_id: 7, waiting: true })
        ));
        assert!(matches!(
            parse_command(r#"{"cmd":"set-pane-status","pane_id":7,"status":"none"}"#),
            Ok(IpcCommand::SetPaneStatus { pane_id: 7, waiting: false })
        ));
    }

    #[test]
    fn notify_defaults_title_pane_and_sound() {
        // A hook only has to supply the message; the rest has sane defaults so
        // that the shortest possible call still produces a usable notification.
        let parsed = parse_command(r#"{"cmd":"notify","message":"done"}"#);
        match parsed {
            Ok(IpcCommand::Notify { pane_id, title, message, sound }) => {
                assert_eq!(pane_id, None);
                assert_eq!(title, "Kova");
                assert_eq!(message, "done");
                assert!(!sound);
            }
            _ => panic!("notify should parse with only a message"),
        }
    }

    #[test]
    fn notify_reads_pane_title_and_sound() {
        let parsed = parse_command(
            r#"{"cmd":"notify","pane_id":42,"title":"Claude Code","message":"terminé","sound":true}"#,
        );
        match parsed {
            Ok(IpcCommand::Notify { pane_id, title, message, sound }) => {
                assert_eq!(pane_id, Some(42));
                assert_eq!(title, "Claude Code");
                assert_eq!(message, "terminé");
                assert!(sound);
            }
            _ => panic!("notify should parse all its fields"),
        }
    }

    #[test]
    fn notify_rejects_a_missing_or_mistyped_message() {
        assert_eq!(
            err(r#"{"cmd":"notify","pane_id":1}"#),
            "missing \"message\" field"
        );
        assert_eq!(
            err(r#"{"cmd":"notify","message":"x","sound":"yes"}"#),
            "\"sound\" must be a boolean"
        );
        assert_eq!(
            err(r#"{"cmd":"notify","message":"x","pane":1}"#),
            "unknown field \"pane\" for command \"notify\""
        );
    }

    #[test]
    fn set_pane_status_rejects_anything_else() {
        // A hook that mistypes the state must fail loudly rather than silently
        // clearing (or setting) the flag.
        assert_eq!(
            err(r#"{"cmd":"set-pane-status","pane_id":7,"status":"busy"}"#),
            "\"status\" must be \"waiting\" or \"none\""
        );
        assert_eq!(
            err(r#"{"cmd":"set-pane-status","pane_id":7}"#),
            "\"status\" must be \"waiting\" or \"none\""
        );
        assert_eq!(
            err(r#"{"cmd":"set-pane-status","status":"waiting"}"#),
            "missing \"pane_id\" field"
        );
    }

    #[test]
    fn unknown_command_takes_precedence_over_field_check() {
        assert_eq!(err(r#"{"cmd":"bogus","whatever":1}"#), "unknown command: bogus");
    }

    #[test]
    fn subscribe_without_events_takes_every_topic() {
        assert!(matches!(
            parse_command(r#"{"cmd":"subscribe"}"#),
            Ok(IpcCommand::Subscribe { topics }) if topics == topic::ALL
        ));
    }

    #[test]
    fn subscribe_builds_the_mask_from_the_named_events() {
        assert!(matches!(
            parse_command(r#"{"cmd":"subscribe","events":["focus","pane-close"]}"#),
            Ok(IpcCommand::Subscribe { topics }) if topics == topic::FOCUS | topic::PANE_CLOSE
        ));
    }

    #[test]
    fn subscribe_rejects_an_unknown_event() {
        // A typo here would otherwise leave the client waiting forever for events
        // that will never come — the failure has to be loud and immediate.
        assert_eq!(
            err(r#"{"cmd":"subscribe","events":["focous"]}"#),
            "unknown event \"focous\" — known events: focus, pane-status, pane-working, pane-open, pane-close"
        );
        assert_eq!(
            err(r#"{"cmd":"subscribe","events":[]}"#),
            "\"events\" must not be empty (omit it to get every topic)"
        );
        assert_eq!(
            err(r#"{"cmd":"subscribe","events":"focus"}"#),
            "\"events\" must be an array of strings"
        );
    }

    #[test]
    fn topic_names_round_trip_through_the_mask() {
        for name in topic::ALL_NAMES {
            let bit = topic::from_name(name).expect("every listed name must parse");
            assert_eq!(topic::names(bit), vec![name]);
        }
        assert_eq!(topic::names(topic::ALL), topic::ALL_NAMES.to_vec());
        assert_eq!(topic::from_name("nope"), None);
    }
}
