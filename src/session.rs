use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

use crate::config::Config;
use crate::pane::{alloc_tab_id, Column, Pane, PaneId, Tab};

const SESSION_VERSION: u32 = 6;

/// Multi-window session format (v3 — flat columns).
#[derive(Serialize, Deserialize)]
pub struct Session {
    pub version: u32,
    pub windows: Vec<WindowSession>,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct WindowSession {
    pub tabs: Vec<SavedTab>,
    pub active_tab: usize,
    /// Window frame: (x, y, width, height) in screen points.
    #[serde(default)]
    pub frame: Option<(f64, f64, f64, f64)>,
}

/// Legacy single-window session format (v1) — kept for backward compat loading.
#[derive(Deserialize)]
struct SessionV1 {
    pub version: u32,
    pub active_tab: usize,
    pub tabs: Vec<SavedTab>,
}

// ---------------------------------------------------------------
// New column-based save format
// ---------------------------------------------------------------

#[derive(Clone, Serialize, Deserialize)]
pub struct SavedTab {
    /// New format (v4): flat columns with pane lists.
    #[serde(default)]
    pub flat_columns: Option<Vec<SavedFlatColumn>>,
    /// Legacy format: columns + weights (v3).
    #[serde(default)]
    pub columns: Option<Vec<SavedColumn>>,
    #[serde(default)]
    pub column_weights: Option<Vec<f32>>,
    #[serde(default)]
    pub custom_weights: Option<Vec<bool>>,
    /// Legacy format: single tree (v2).
    #[serde(default)]
    pub tree: Option<SavedTree>,
    pub focused_leaf_index: usize,
    pub custom_title: Option<String>,
    pub color: Option<usize>,
    #[serde(default)]
    pub virtual_width_override: Option<f32>,
    #[serde(default)]
    pub scroll_offset_x: Option<f32>,
    /// Backing scale factor in effect when `virtual_width_override` and
    /// `scroll_offset_x` were captured — both are in pixels, so they mean a
    /// different physical size on a display with another scale. `restore_saved_tab`
    /// converts them to the target display. Absent in sessions written before
    /// this field existed: they are then taken as already matching.
    #[serde(default)]
    pub geometry_scale: Option<f32>,
    /// The tab's subjects in creation order, empty ones included (`Tab::subjects`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub subjects: Vec<String>,
    /// Each subject's color, an index into the tab palette (`Tab::subject_colors`).
    /// Absent in sessions written before: colors are then drawn on restore.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub subject_colors: std::collections::BTreeMap<String, usize>,
}

/// Flat column format (v4): a column is a list of panes with row weights.
#[derive(Clone, Serialize, Deserialize)]
pub struct SavedFlatColumn {
    pub panes: Vec<SavedPane>,
    pub row_weights: Vec<f32>,
    #[serde(default)]
    pub custom_row_weights: Option<Vec<bool>>,
}

/// A single pane in the flat format.
#[derive(Clone, Serialize, Deserialize)]
pub struct SavedPane {
    pub cwd: Option<String>,
    #[serde(default)]
    pub last_command: Option<String>,
    #[serde(default)]
    pub custom_title: Option<String>,
    #[serde(default)]
    pub minimized: bool,
    /// Title the running app had set (OSC 0/2) — for Claude Code, the name of
    /// the conversation. Restored so a pane that has not been resumed yet still
    /// says what it was, instead of falling back to its directory name. The
    /// first title the app emits once relaunched replaces it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Claude Code session running in this pane when the snapshot was taken
    /// (v5). Superseded by `agent_session`, which names the agent instead of
    /// assuming Claude; still read so a session file written before v6 restores
    /// its conversations, and no longer written.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claude_session: Option<String>,
    /// Agent conversation running in this pane when the snapshot was taken
    /// (v6) — Claude Code or Codex. On restore the pane gets that agent's
    /// resume line instead of the last typed command, so the conversation can
    /// be picked up where it stopped. `None` for a pane that was at a shell
    /// prompt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_session: Option<SavedAgentSession>,
    /// The subject of its tab the pane is filed under (`Pane::subject`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subject: Option<String>,
}

/// The conversation a pane was holding, and which agent to hand it back to.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct SavedAgentSession {
    pub agent: crate::agent_session::Agent,
    pub id: String,
    /// Claude's `CLAUDE_CONFIG_DIR` when the session did not run under the
    /// default one, so the resume line reopens it there (see
    /// `claude_session::resume_command`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_dir: Option<String>,
}

/// Legacy column format (v3) — kept for backward compat reading.
#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum SavedColumn {
    Leaf {
        cwd: Option<String>,
        #[serde(default)]
        last_command: Option<String>,
        #[serde(default)]
        custom_title: Option<String>,
        #[serde(default)]
        minimized: bool,
    },
    VSplit {
        top: Box<SavedColumn>,
        bottom: Box<SavedColumn>,
        ratio: f32,
        #[serde(default)]
        custom_ratio: bool,
    },
}

/// Legacy tree format — kept for backward compat reading.
#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum SavedTree {
    Leaf {
        cwd: Option<String>,
        #[serde(default)]
        last_command: Option<String>,
        #[serde(default)]
        custom_title: Option<String>,
        #[serde(default)]
        minimized: bool,
    },
    HSplit { left: Box<SavedTree>, right: Box<SavedTree>, ratio: f32, #[serde(default)] root: bool, #[serde(default)] custom_ratio: bool },
    VSplit { top: Box<SavedTree>, bottom: Box<SavedTree>, ratio: f32, #[serde(default)] root: bool, #[serde(default)] custom_ratio: bool },
}

fn session_path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    PathBuf::from(home).join(".config/kova/session.json")
}

// ---------------------------------------------------------------
// Snapshot (save)
// ---------------------------------------------------------------

/// One pane as a snapshot saves it: where it stood, what it was running, and
/// which agent conversation it held.
///
/// The conversation is read off the live process, so this must run while the
/// pane's children are alive: Claude Code deletes its session file on exit, and
/// a dead Codex no longer holds its transcript open — unreadable once the PTY
/// is reaped. `will_terminate` saves before `shutdown_all` for the app, and the
/// closed-pane undo records a pane before it is removed for the same reason.
pub fn saved_pane(p: &Pane) -> SavedPane {
    SavedPane {
        cwd: p.cwd(),
        last_command: p.last_command(),
        custom_title: p.custom_title.clone(),
        minimized: p.minimized,
        title: p.osc_title(),
        claude_session: None,
        agent_session: session_to_save(crate::agent_session::for_shell(p.pty.pid()), p.unregistered_session()),
        subject: p.subject.clone(),
    }
}

fn snapshot_flat_column(col: &Column) -> SavedFlatColumn {
    SavedFlatColumn {
        panes: col.panes.iter().map(saved_pane).collect(),
        row_weights: col.row_weights.clone(),
        custom_row_weights: if col.custom_row_weights.iter().any(|&cw| cw) {
            Some(col.custom_row_weights.clone())
        } else {
            None
        },
    }
}

/// Drop a Pi or omp conversation that two panes claim at once.
///
/// Pi writes no file per process, and omp's exact source (its open transcript)
/// can be missing right after start, so both fall back to reading the directory
/// and the start time. Two panes in the same directory then leave that guess
/// with one file to hand out, and it would go to both — one pane reopening the
/// other's transcript. Neither is better than a pane that starts its own
/// session, so both keep their last command instead. Other agents are left
/// alone: their detection names the process itself, so the same id twice there
/// would mean the same conversation really was open twice.
fn drop_shared_guessed_sessions(windows: &mut [WindowSession]) {
    let mut counts: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for window in windows.iter() {
        for pane in window
            .tabs
            .iter()
            .flat_map(|tab| tab.flat_columns.iter().flatten())
            .flat_map(|col| col.panes.iter())
        {
            if let Some(session) = pane.agent_session.as_ref().filter(|s| {
                matches!(
                    s.agent,
                    crate::agent_session::Agent::Pi | crate::agent_session::Agent::Omp
                )
            }) {
                *counts.entry(session.id.clone()).or_default() += 1;
            }
        }
    }
    if counts.values().all(|&count| count < 2) {
        return;
    }
    for window in windows.iter_mut() {
        for pane in window
            .tabs
            .iter_mut()
            .flat_map(|tab| tab.flat_columns.iter_mut().flatten())
            .flat_map(|col| col.panes.iter_mut())
        {
            let shared = pane
                .agent_session
                .as_ref()
                .filter(|s| {
                    matches!(
                        s.agent,
                        crate::agent_session::Agent::Pi | crate::agent_session::Agent::Omp
                    )
                })
                .is_some_and(|s| counts.get(&s.id).copied().unwrap_or(0) > 1);
            if shared {
                pane.agent_session = None;
            }
        }
    }
}

/// The conversation a pane is saved with: the agent running in it, or else the
/// one a launch restore left asleep there, or woke but whose agent has not
/// registered yet (`Surface::unregistered_session`) — no agent is seen running,
/// but that is the conversation to bring back next time too.
fn session_to_save(
    live: Option<crate::agent_session::AgentSession>,
    dormant: Option<SavedAgentSession>,
) -> Option<SavedAgentSession> {
    live.map(|s| SavedAgentSession { agent: s.agent, id: s.id, config_dir: s.config_dir })
        .or(dormant)
}

/// Find the flat leaf index of a pane by id across all columns.
fn leaf_index_of_tab(tab: &Tab) -> usize {
    let mut idx = 0;
    let target = tab.focused_pane;
    for col in &tab.columns {
        for pane in &col.panes {
            if pane.id == target {
                return idx;
            }
            idx += 1;
        }
    }
    0
}

/// Snapshot a single tab into a SavedTab (public for recent_projects).
pub fn snapshot_tab(tab: &Tab) -> SavedTab {
    let focused_leaf_index = leaf_index_of_tab(tab);
    SavedTab {
        flat_columns: Some(tab.columns.iter().map(snapshot_flat_column).collect()),
        columns: None,
        column_weights: Some(tab.column_weights.clone()),
        custom_weights: if tab.custom_weights.iter().any(|&cw| cw) { Some(tab.custom_weights.clone()) } else { None },
        tree: None,
        focused_leaf_index,
        custom_title: tab.custom_title.clone(),
        color: tab.color,
        virtual_width_override: if tab.virtual_width_override > 0.0 { Some(tab.virtual_width_override) } else { None },
        scroll_offset_x: if tab.scroll_offset_x != 0.0 { Some(tab.scroll_offset_x) } else { None },
        geometry_scale: if tab.geometry_scale > 0.0 { Some(tab.geometry_scale) } else { None },
        subjects: tab.subjects.clone(),
        subject_colors: tab.subject_colors.iter().map(|(k, v)| (k.clone(), *v)).collect(),
    }
}

/// Maximum number of session backups to keep.
const SESSION_HISTORY_COUNT: usize = 10;

fn session_lock_path() -> PathBuf {
    session_path().with_file_name("session.lock")
}

/// Take the session lock, or `None` if another Kova already holds it.
///
/// The lock lives on its own file rather than on `session.json`, which `load`
/// deletes: a lock held on a deleted inode protects nothing.
fn try_acquire_lock(path: &std::path::Path) -> Option<std::fs::File> {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .ok()?;
    file.try_lock().ok()?;
    Some(file)
}

/// Whether this process owns the session file.
///
/// One session file cannot describe two Kova at once: both would autosave into
/// it every 30s and the last one to quit would be the only one restored, while
/// the second instance would also have started by restoring a clone of the
/// first one's tabs. So the first Kova to start owns the session, and any other
/// one neither reads nor writes it — it opens a fresh window and forgets it on
/// quit. The lock is released by the kernel when the process dies, crash
/// included, so a stale lock file is never a problem.
pub fn owns_session() -> bool {
    static LOCK: std::sync::OnceLock<Option<std::fs::File>> = std::sync::OnceLock::new();
    LOCK.get_or_init(|| {
        let held = try_acquire_lock(&session_lock_path());
        if held.is_none() {
            log::warn!(
                "Another Kova owns {}: this instance starts fresh and will not save its session",
                session_path().display()
            );
        }
        held
    })
    .is_some()
}

/// Save all windows to a single session file.
/// Save the current session, rotating backups (`session.1.json` → … → `session.10.json`).
/// Use this on user-initiated checkpoints — quit, kill, manual save.
pub fn save(windows: &[WindowSession]) {
    save_internal(windows, true);
}

/// Save the current session WITHOUT rotating backups. Use this for the periodic
/// autosave: if Kova ever serializes a corrupted state (failed restore, race),
/// the rotated history of clean checkpoints remains intact and recoverable.
pub fn save_periodic(windows: &[WindowSession]) {
    save_internal(windows, false);
}

fn save_internal(windows: &[WindowSession], rotate: bool) {
    if !owns_session() {
        return;
    }
    let mut windows = windows.to_vec();
    drop_shared_guessed_sessions(&mut windows);
    let session = Session {
        version: SESSION_VERSION,
        windows,
    };

    let path = session_path();
    if let Some(parent) = path.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            log::warn!("Failed to create session dir: {}", e);
            return;
        }
    }
    match serde_json::to_string_pretty(&session) {
        Ok(json) => {
            if rotate {
                rotate_session_backups(&path, json.as_bytes());
            }
            // Skip the write if the file is byte-identical — autosaves of an
            // idle Kova would otherwise re-write the same JSON every 30s.
            if let Ok(existing) = std::fs::read(&path) {
                if existing == json.as_bytes() {
                    return;
                }
            }
            if let Err(e) = write_owner_only(&path, &json) {
                log::warn!("Failed to write session file: {}", e);
            } else {
                log::info!("Session saved to {} ({} window(s))", path.display(), session.windows.len());
            }
        }
        Err(e) => log::warn!("Failed to serialize session: {}", e),
    }
}

/// Write the session file so only its owner can read it.
///
/// It holds every pane's cwd and the last command line typed in it — a secret
/// passed on a command line ends up here — and the rotated backups keep months of
/// them. A macOS home directory is world-readable, so the file has to carry its
/// own mode. `mode()` only applies when the file is created, hence the explicit
/// tightening for a file written before this, done before the new bytes land.
pub fn write_owner_only(path: &std::path::Path, json: &str) -> std::io::Result<()> {
    use std::io::Write;
    #[cfg(unix)]
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    options.mode(0o600);
    let mut file = options.open(path)?;
    #[cfg(unix)]
    file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    file.write_all(json.as_bytes())
}

/// Read one of the small owner-only json files next to the session (bookmarks,
/// anchors). Missing or unreadable reads as the default: these lists are
/// conveniences, never a reason to refuse to open an overlay or to start.
pub fn load_owner_only<T: Default + serde::de::DeserializeOwned>(
    path: &std::path::Path,
    what: &str,
) -> T {
    let Ok(data) = std::fs::read_to_string(path) else {
        return T::default();
    };
    match serde_json::from_str(&data) {
        Ok(v) => v,
        Err(e) => {
            log::warn!("Failed to parse {} ({}); starting from an empty {}", path.display(), e, what);
            T::default()
        }
    }
}

/// Write one of those lists back, creating its directory, owner-readable only:
/// they hold working directories and conversation ids, same as the session file.
pub fn save_owner_only<T: serde::Serialize>(path: &std::path::Path, what: &str, value: &T) {
    if let Some(parent) = path.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            log::warn!("Failed to create {}: {}", parent.display(), e);
            return;
        }
    }
    let json = match serde_json::to_string_pretty(value) {
        Ok(json) => json,
        Err(e) => {
            log::warn!("Failed to serialize {}: {}", what, e);
            return;
        }
    };
    // Keep the version being replaced one file away, then land the new bytes
    // through a temporary file. These lists are hand-curated and nothing else
    // on this machine records them: a write interrupted halfway used to leave a
    // truncated file, which reads back as an empty list and takes the whole list
    // with it on the next save.
    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("list");
    let _ = std::fs::copy(path, path.with_file_name(format!("{}.1.json", stem)));
    let tmp = path.with_file_name(format!("{}.tmp.json", stem));
    if let Err(e) = write_owner_only(&tmp, &json) {
        log::warn!("Failed to write {}: {}", tmp.display(), e);
        return;
    }
    if let Err(e) = std::fs::rename(&tmp, path) {
        log::warn!("Failed to replace {}: {}", path.display(), e);
    }
}

/// Rotate session.json -> session.1.json -> session.2.json -> ...
fn rotate_session_backups(path: &std::path::Path, new_content: &[u8]) {
    if !path.exists() {
        return;
    }
    let parent = path.parent().unwrap();
    let backup = |n: usize| {
        if n == 0 { path.to_path_buf() }
        else { parent.join(format!("session.{}.json", n)) }
    };

    if let Ok(latest) = std::fs::read(&backup(1)) {
        if new_content == latest {
            return;
        }
    }

    let _ = std::fs::remove_file(backup(SESSION_HISTORY_COUNT));
    for i in (1..SESSION_HISTORY_COUNT).rev() {
        let _ = std::fs::rename(backup(i), backup(i + 1));
    }
    let _ = std::fs::rename(backup(0), backup(1));
}

/// Loaded window data ready for restoration.
pub struct RestoredWindow {
    pub tabs: Vec<Tab>,
    pub active_tab: usize,
    pub frame: Option<(f64, f64, f64, f64)>,
    /// Tabs not yet spawned (deferred for progressive loading).
    pub deferred_tabs: Vec<(usize, SavedTab)>,
}

/// Print available session backups to stdout.
pub fn list_session_backups() {
    let main_path = session_path();
    let dir = main_path.parent().unwrap();

    print_session_entry(&main_path, "current");
    for i in 1..=SESSION_HISTORY_COUNT {
        let path = dir.join(format!("session.{}.json", i));
        print_session_entry(&path, &format!("{:>7}", i));
    }
}

fn print_session_entry(path: &std::path::Path, label: &str) {
    let Ok(data) = std::fs::read_to_string(path) else { return };
    let modified = std::fs::metadata(path).ok()
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| {
            let secs = d.as_secs();
            format!("{}h{:02} UTC", (secs % 86400) / 3600, (secs % 3600) / 60)
        })
        .unwrap_or_default();
    let summary = match serde_json::from_str::<Session>(&data) {
        Ok(s) => {
            let tabs: usize = s.windows.iter().map(|w| w.tabs.len()).sum();
            format!("{} window(s), {} tab(s)", s.windows.len(), tabs)
        }
        Err(_) => "(invalid)".into(),
    };
    println!("  {}  {} {}", label, modified, summary);
}

/// Whether a session file of this version can still be read. Every format
/// from v2 up to the current one is: the fields added by later versions are
/// all `#[serde(default)]`, and the ones dropped (`claude_session`) are still
/// deserialized. Comparing against `SESSION_VERSION` rather than listing the
/// versions one by one means a bump no longer silently drops the session it
/// was meant to migrate — v5 was left out of the list when v6 landed, and a
/// whole window of tabs came back as a single empty tab.
fn is_readable_version(v: u32) -> bool {
    (2..=SESSION_VERSION).contains(&v)
}

pub fn load(backup: Option<usize>) -> Option<Session> {
    if !owns_session() {
        return None;
    }
    let path = match backup {
        Some(n) => {
            let dir = session_path().parent().unwrap().to_path_buf();
            let p = dir.join(format!("session.{}.json", n));
            log::info!("Restoring session backup #{}", n);
            p
        }
        None => session_path(),
    };
    let data = std::fs::read_to_string(&path).ok()?;

    // Try v4/v3 first, then v2, then v1
    let session: Session = if let Ok(s) = serde_json::from_str::<Session>(&data) {
        if is_readable_version(s.version) {
            s
        } else if s.version == 1 {
            log::warn!("Session v1 with windows field, ignoring");
            return None;
        } else {
            log::warn!("Unknown session version {}, ignoring", s.version);
            return None;
        }
    } else if let Ok(v1) = serde_json::from_str::<SessionV1>(&data) {
        if v1.version == 1 {
            log::info!("Migrating v1 session to v3");
            Session {
                version: SESSION_VERSION,
                windows: vec![WindowSession {
                    tabs: v1.tabs,
                    active_tab: v1.active_tab,
                    frame: None,
                }],
            }
        } else {
            log::warn!("Unknown session version {}, ignoring", v1.version);
            return None;
        }
    } else {
        log::warn!("Failed to parse session file");
        return None;
    };

    // Destructive: the loaded file is consumed. Leave a trace so a lost
    // session (e.g. a --session N backup) can be diagnosed from the log.
    log::info!("Session file {} deleted after load", path.display());
    let _ = std::fs::remove_file(&path);
    Some(session)
}

// ---------------------------------------------------------------
// Restore
// ---------------------------------------------------------------

/// The command line to pre-type into a restored pane.
///
/// A pane that was running an agent when the snapshot was taken gets that
/// agent's resume line; any other pane keeps the last command that was actually
/// run in it. The session id recorded at snapshot time always wins over one the
/// last command happens to name, because that older line can be several
/// restarts old while the recorded id is the conversation that was live at quit.
///
/// Nothing is executed here: the caller writes the command without a newline,
/// so it waits for the user to press Enter — except an agent's resume line at
/// app launch (`runs_on_launch`).
fn restore_command(sp: &SavedPane) -> Option<String> {
    // A refused id (see `is_safe_session_id`) leaves the pane exactly where a
    // pane that was never running an agent lands: its own last command.
    agent_resume_line(sp).or_else(|| sp.last_command.clone())
}

/// The resume line of the agent the pane was running at snapshot time, if any.
fn agent_resume_line(sp: &SavedPane) -> Option<String> {
    let session = saved_agent_session(sp)?;
    crate::agent_session::resume_command(session.agent, &session.id, sp.last_command.as_deref(), session.config_dir.as_deref())
}

/// The agent conversation the pane was running at snapshot time, if any.
fn saved_agent_session(sp: &SavedPane) -> Option<SavedAgentSession> {
    // A file written before v6 only knew about Claude.
    sp.agent_session.clone().or_else(|| {
        sp.claude_session.as_ref().map(|id| SavedAgentSession {
            agent: crate::agent_session::Agent::Claude,
            id: id.clone(),
            config_dir: None,
        })
    })
}

/// Whether Kova presses Enter on the restored line itself, at app launch.
///
/// Only an agent's resume line qualifies, never a pane's last command: that one
/// can be named by any program printing an OSC 7777 on its tty. And the resume
/// line keeps the flags of the `claude …` that was last run, which come from the
/// same place — so the whole line must be plain words, nothing the shell would
/// expand or redirect (`$(…)`, backticks, `>`, quotes). Anything else waits for
/// the user's Enter, as before.
fn runs_on_launch(sp: &SavedPane) -> bool {
    agent_resume_line(sp).is_some_and(|line| is_plain_command(&line))
}

fn is_plain_command(line: &str) -> bool {
    line.chars().all(|c| c == ' ' || c.is_ascii_alphanumeric() || "-_./=:,@+".contains(c))
}

/// The line a reopened pane comes back with, and whether Kova should press
/// Enter on it rather than leave it waiting at the prompt.
///
/// Same rule as a restore at launch: only an agent's resume line runs by
/// itself, and only when it is plain words. A bare shell pane gets its last
/// command pre-typed and nothing else — Kova does not replay a command the user
/// may not have meant to run again (a `rm`, a deploy).
pub fn reopen_line(sp: &SavedPane) -> (Option<String>, bool) {
    (restore_command(sp), runs_on_launch(sp))
}

/// Restore a flat column (v4 format).
/// `at_launch`: the tab comes back with the app (not from Cmd+O), so the
/// agents it was running resume without waiting for Enter — each one the
/// first time its pane is on screen (`Surface::wake`), not all at launch.
fn restore_flat_column(saved: &SavedFlatColumn, cols: u16, rows: u16, config: &Config, at_launch: bool) -> Option<(Column, Vec<PaneId>)> {
    let mut panes = Vec::new();
    let mut ids = Vec::new();
    for sp in &saved.panes {
        let mut pane = match Pane::spawn(cols, rows, config, sp.cwd.as_deref()) {
            Ok(p) => p,
            Err(e) => {
                log::warn!("Pane::spawn failed (cwd={:?}): {}", sp.cwd, e);
                return None;
            }
        };
        let id = pane.id;
        let command = restore_command(sp);
        {
            let mut term = pane.terminal.write();
            if let Some(cmd) = command {
                pane.pending_command.set(Some(cmd.clone()));
                term.last_command = Some(cmd);
            }
            term.title = sp.title.clone();
        }
        if at_launch && runs_on_launch(sp) {
            if let Some(session) = saved_agent_session(sp) {
                pane.sleep_until_shown(session);
            }
        }
        pane.custom_title = sp.custom_title.clone();
        pane.minimized = sp.minimized;
        pane.subject = sp.subject.clone();
        panes.push(pane);
        ids.push(id);
    }
    if panes.is_empty() { return None; }
    let n = panes.len();
    let row_weights = if saved.row_weights.len() == n {
        saved.row_weights.clone()
    } else {
        log::warn!("Session: row_weights len {} != panes len {}, using equal weights", saved.row_weights.len(), n);
        vec![1.0; n]
    };
    let custom_row_weights = saved.custom_row_weights.clone()
        .filter(|v| v.len() == n)
        .unwrap_or_else(|| vec![false; n]);
    Some((Column { panes, row_weights, custom_row_weights }, ids))
}

/// Restore a saved column tree (v3 legacy format) into a flat Column.
fn restore_column(saved: &SavedColumn, cols: u16, rows: u16, config: &Config) -> Option<(Column, Vec<PaneId>)> {
    // Flatten the tree into a flat column by collecting all leaves depth-first,
    // converting ratios to weights.
    let mut panes = Vec::new();
    let mut weights = Vec::new();
    let mut ids = Vec::new();
    flatten_saved_column(saved, 1.0, cols, rows, config, &mut panes, &mut weights, &mut ids)?;
    if panes.is_empty() { return None; }
    let custom_row_weights = vec![false; panes.len()];
    Some((Column { panes, row_weights: weights, custom_row_weights }, ids))
}

/// Recursively flatten a SavedColumn tree into panes + weights.
/// `weight_share` is this node's share of the parent's height.
fn flatten_saved_column(
    saved: &SavedColumn,
    weight_share: f32,
    cols: u16,
    rows: u16,
    config: &Config,
    panes: &mut Vec<Pane>,
    weights: &mut Vec<f32>,
    ids: &mut Vec<PaneId>,
) -> Option<()> {
    match saved {
        SavedColumn::Leaf { cwd, last_command, custom_title, minimized } => {
            let mut pane = match Pane::spawn(cols, rows, config, cwd.as_deref()) {
                Ok(p) => p,
                Err(e) => {
                    log::warn!("Pane::spawn failed (cwd={:?}): {}", cwd, e);
                    return None;
                }
            };
            let id = pane.id;
            if let Some(cmd) = last_command {
                pane.pending_command.set(Some(cmd.clone()));
                pane.terminal.write().last_command = Some(cmd.clone());
            }
            pane.custom_title = custom_title.clone();
            pane.minimized = *minimized;
            panes.push(pane);
            weights.push(weight_share);
            ids.push(id);
            Some(())
        }
        SavedColumn::VSplit { top, bottom, ratio, .. } => {
            flatten_saved_column(top, weight_share * ratio, cols, rows, config, panes, weights, ids)?;
            flatten_saved_column(bottom, weight_share * (1.0 - ratio), cols, rows, config, panes, weights, ids)?;
            Some(())
        }
    }
}

/// Restore from legacy SavedTree format, flattening HSplits into columns.
fn restore_legacy_tree(saved: &SavedTree, cols: u16, rows: u16, config: &Config) -> Option<(Vec<Column>, Vec<f32>, Vec<PaneId>)> {
    match saved {
        SavedTree::Leaf { cwd, last_command, custom_title, minimized } => {
            let mut pane = match Pane::spawn(cols, rows, config, cwd.as_deref()) {
                Ok(p) => p,
                Err(e) => {
                    log::warn!("Pane::spawn failed (cwd={:?}): {}", cwd, e);
                    return None;
                }
            };
            let id = pane.id;
            if let Some(cmd) = last_command {
                pane.pending_command.set(Some(cmd.clone()));
                pane.terminal.write().last_command = Some(cmd.clone());
            }
            pane.custom_title = custom_title.clone();
            pane.minimized = *minimized;
            Some((vec![Column::new(pane)], vec![1.0], vec![id]))
        }
        SavedTree::HSplit { left, right, ratio, .. } => {
            let (mut left_cols, mut left_weights, mut left_ids) = restore_legacy_tree(left, cols, rows, config)?;
            let (right_cols, right_weights, right_ids) = restore_legacy_tree(right, cols, rows, config)?;
            // Scale weights: left side gets ratio, right side gets 1-ratio
            let left_sum: f32 = left_weights.iter().sum();
            let right_sum: f32 = right_weights.iter().sum();
            if left_sum > 0.0 {
                for w in &mut left_weights {
                    *w = *w / left_sum * ratio;
                }
            }
            let right_ratio = 1.0 - ratio;
            let scaled_right: Vec<f32> = if right_sum > 0.0 {
                right_weights.iter().map(|w| w / right_sum * right_ratio).collect()
            } else {
                right_weights
            };
            left_cols.extend(right_cols);
            left_weights.extend(scaled_right);
            left_ids.extend(right_ids);
            Some((left_cols, left_weights, left_ids))
        }
        SavedTree::VSplit { top, bottom, ratio, custom_ratio, .. } => {
            // VSplit within a column — restore as a single flat column
            let saved_col = SavedColumn::VSplit {
                top: Box::new(saved_tree_to_saved_column(top)),
                bottom: Box::new(saved_tree_to_saved_column(bottom)),
                ratio: *ratio,
                custom_ratio: *custom_ratio,
            };
            let (col, ids) = restore_column(&saved_col, cols, rows, config)?;
            Some((vec![col], vec![1.0], ids))
        }
    }
}

/// Convert a legacy SavedTree to SavedColumn (for VSplit branches).
fn saved_tree_to_saved_column(tree: &SavedTree) -> SavedColumn {
    match tree {
        SavedTree::Leaf { cwd, last_command, custom_title, minimized } => {
            SavedColumn::Leaf {
                cwd: cwd.clone(),
                last_command: last_command.clone(),
                custom_title: custom_title.clone(),
                minimized: *minimized,
            }
        }
        SavedTree::HSplit { left, .. } => {
            // HSplit inside a VSplit: flatten by taking the left side as top
            // This is a lossy conversion but should be rare
            log::warn!("HSplit nested inside VSplit during legacy migration — flattening");
            saved_tree_to_saved_column(left)
        }
        SavedTree::VSplit { top, bottom, ratio, custom_ratio, .. } => {
            SavedColumn::VSplit {
                top: Box::new(saved_tree_to_saved_column(top)),
                bottom: Box::new(saved_tree_to_saved_column(bottom)),
                ratio: *ratio,
                custom_ratio: *custom_ratio,
            }
        }
    }
}

fn count_panes_in_saved_column(col: &SavedColumn) -> usize {
    match col {
        SavedColumn::Leaf { .. } => 1,
        SavedColumn::VSplit { top, bottom, .. } => {
            count_panes_in_saved_column(top) + count_panes_in_saved_column(bottom)
        }
    }
}

pub fn count_panes_in_saved_tab(tab: &SavedTab) -> usize {
    if let Some(ref flat_cols) = tab.flat_columns {
        flat_cols.iter().map(|c| c.panes.len()).sum()
    } else if let Some(ref cols) = tab.columns {
        cols.iter().map(count_panes_in_saved_column).sum()
    } else {
        1 // legacy format, approximate
    }
}

/// Restore a single saved tab. Used by recent projects and session restore;
/// `at_launch` is true only for the latter (see `restore_flat_column`).
pub fn restore_saved_tab(saved: &SavedTab, cols: u16, rows: u16, config: &Config, at_launch: bool) -> Option<Tab> {
    let (columns, column_weights, custom_weights, pane_ids) = if let Some(ref flat_cols) = saved.flat_columns {
        // New flat format (v4)
        let weights = saved.column_weights.clone()
            .filter(|v| v.len() == flat_cols.len())
            .unwrap_or_else(|| vec![1.0; flat_cols.len()]);
        let cweights = saved.custom_weights.clone()
            .filter(|v| v.len() == flat_cols.len())
            .unwrap_or_else(|| vec![false; flat_cols.len()]);
        let mut all_ids = Vec::new();
        let mut cols_vec = Vec::new();
        for fc in flat_cols {
            let (col, ids) = restore_flat_column(fc, cols, rows, config, at_launch)?;
            cols_vec.push(col);
            all_ids.extend(ids);
        }
        (cols_vec, weights, cweights, all_ids)
    } else if let Some(ref saved_cols) = saved.columns {
        // Legacy format (v3) — flatten SavedColumn trees into flat Columns
        let weights = saved.column_weights.clone()
            .filter(|v| v.len() == saved_cols.len())
            .unwrap_or_else(|| vec![1.0; saved_cols.len()]);
        let cweights = saved.custom_weights.clone()
            .filter(|v| v.len() == saved_cols.len())
            .unwrap_or_else(|| vec![false; saved_cols.len()]);
        let mut all_ids = Vec::new();
        let mut cols_vec = Vec::new();
        for sc in saved_cols {
            let (col, ids) = restore_column(sc, cols, rows, config)?;
            cols_vec.push(col);
            all_ids.extend(ids);
        }
        (cols_vec, weights, cweights, all_ids)
    } else if let Some(ref tree) = saved.tree {
        // Legacy format (v2) — flatten HSplits
        let (cols_vec, weights, ids) = restore_legacy_tree(tree, cols, rows, config)?;
        let cweights = vec![false; cols_vec.len()];
        (cols_vec, weights, cweights, ids)
    } else {
        log::warn!("SavedTab has neither flat_columns, columns, nor tree");
        return None;
    };

    let focused_pane = if saved.focused_leaf_index < pane_ids.len() {
        pane_ids[saved.focused_leaf_index]
    } else {
        *pane_ids.first()?
    };
    let mut tab = Tab {
        id: alloc_tab_id(),
        columns,
        column_weights,
        custom_weights,
        focused_pane,
        custom_title: saved.custom_title.clone(),
        color: saved.color,
        has_bell: false,
        has_completion: false,
        has_running: false,
        fg_running_cache: false,
        minimized_stack: Vec::new(),
        scroll_offset_x: saved.scroll_offset_x.unwrap_or(0.0),
        virtual_width_override: saved.virtual_width_override.unwrap_or(0.0),
        // Pixels of the display this tab was saved on; `normalize_tab_geometry`
        // converts them to the display it actually lands on.
        geometry_scale: saved.geometry_scale.unwrap_or(0.0),
        cell_h: std::cell::Cell::new(0.0),
        subjects: saved.subjects.clone(),
        subject_colors: saved.subject_colors.iter().map(|(k, v)| (k.clone(), *v)).collect(),
    };
    tab.heal_subjects();
    tab.rebuild_minimized_stack();
    Some(tab)
}

fn restore_window_tabs(ws: &WindowSession, config: &Config) -> Option<(Vec<Tab>, usize, Vec<(usize, SavedTab)>)> {
    let cols = config.terminal.columns;
    let rows = config.terminal.rows;
    let total = ws.tabs.len();
    if total == 0 {
        log::warn!("Skipping saved window with no tabs");
        return None;
    }
    let active_idx = ws.active_tab.min(total - 1);

    // Restore the active tab first (priority)
    let active_tab = match restore_saved_tab(&ws.tabs[active_idx], cols, rows, config, true) {
        Some(tab) => tab,
        None => {
            log::warn!("Failed to restore active tab, falling back to sequential restore");
            // Fallback: try all tabs sequentially
            let mut tabs = Vec::new();
            for saved_tab in &ws.tabs {
                if let Some(tab) = restore_saved_tab(saved_tab, cols, rows, config, true) {
                    tabs.push(tab);
                }
            }
            if tabs.is_empty() { return None; }
            return Some((tabs, 0, Vec::new()));
        }
    };

    // Build tab list: placeholders at deferred positions, real tab at active position
    let mut tabs = Vec::with_capacity(total);
    let mut deferred = Vec::new();
    let mut active_tab = Some(active_tab);
    for (i, saved_tab) in ws.tabs.iter().enumerate() {
        if i == active_idx {
            tabs.push(active_tab.take().unwrap());
        } else {
            // Create a placeholder tab (dummy PTY, no shell process) so tab indices are stable
            match crate::pane::Tab::placeholder(config) {
                Ok(mut placeholder) => {
                    // Copy visual metadata so the tab bar looks right
                    placeholder.custom_title = saved_tab.custom_title.clone();
                    placeholder.color = saved_tab.color;
                    tabs.push(placeholder);
                    deferred.push((i, saved_tab.clone()));
                }
                Err(e) => log::warn!("Failed to create placeholder tab {}: {}", i, e),
            }
        }
    }

    if tabs.is_empty() { return None; }
    Some((tabs, active_idx, deferred))
}

/// Every saved pane of a session, flat column format and the legacy trees.
fn panes_mut(session: &mut Session) -> impl Iterator<Item = &mut SavedPane> {
    session
        .windows
        .iter_mut()
        .flat_map(|window| window.tabs.iter_mut())
        .flat_map(|tab| tab.flat_columns.iter_mut().flatten())
        .flat_map(|column| column.panes.iter_mut())
}

/// Name the conversation of every pane the snapshot left without one.
///
/// The agent of a pane is named at snapshot time by looking at the processes
/// alive in it. When that lookup cannot see the agent — a Kova built before the
/// agent was known, a probe that missed at that instant — the pane is saved with
/// its last command alone, and `omp` on its own is a *fresh* conversation on
/// restore: the user comes back to an empty pane and reads it as lost, while the
/// old transcript sits untouched on disk.
///
/// The pane does keep the title its agent wrote, though, and an omp transcript
/// carries its own (see `omp_session::session_by_title`), so the conversation is
/// recoverable with no process alive. Two guards keep that honest: the last
/// command in the pane must still be that very agent's (a pane where the agent
/// was quit and another command typed holds a stale title, and keeps its last
/// command), and the match must be unique.
fn recover_agent_sessions_by_title(session: &mut Session, home: &Path) {
    for pane in panes_mut(session) {
        if pane.agent_session.is_some() {
            continue;
        }
        let (Some(cwd), Some(title)) = (pane.cwd.as_deref(), pane.title.as_deref()) else {
            continue;
        };
        let Some(command) = pane.last_command.as_deref() else {
            continue;
        };
        if !crate::omp_session::is_omp_invocation(command) {
            continue;
        }
        let Some(found) = crate::omp_session::session_by_title(home, cwd, title) else {
            continue;
        };
        log::info!("Restore: pane at {} matched omp {} by its title", cwd, found.id);
        pane.agent_session = Some(SavedAgentSession {
            agent: crate::agent_session::Agent::Omp,
            id: found.id,
            config_dir: None,
        });
    }
    drop_shared_recovered_sessions(session);
}

/// Give up on an omp conversation two panes of the restored session claim.
///
/// Same rule as `drop_shared_guessed_sessions`, on the other side of the file:
/// resuming one transcript in two panes writes the same conversation from two
/// processes, so neither gets it and both keep their last command.
fn drop_shared_recovered_sessions(session: &mut Session) {
    let mut counts: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for pane in panes_mut(session) {
        if let Some(id) = pane
            .agent_session
            .as_ref()
            .filter(|s| s.agent == crate::agent_session::Agent::Omp)
            .map(|s| s.id.clone())
        {
            *counts.entry(id).or_default() += 1;
        }
    }
    if counts.values().all(|&count| count < 2) {
        return;
    }
    for pane in panes_mut(session) {
        let shared = pane
            .agent_session
            .as_ref()
            .filter(|s| s.agent == crate::agent_session::Agent::Omp)
            .is_some_and(|s| counts.get(&s.id).copied().unwrap_or(0) > 1);
        if shared {
            pane.agent_session = None;
        }
    }
}

/// Restore a multi-window session. Returns a list of windows to create.
pub fn restore_session(session: Session, config: &Config) -> Option<Vec<RestoredWindow>> {
    let mut windows = Vec::new();
    let mut session = session;
    let home = PathBuf::from(std::env::var_os("HOME").unwrap_or_default());
    recover_agent_sessions_by_title(&mut session, &home);

    for ws in &session.windows {
        if let Some((tabs, active_tab, deferred_tabs)) = restore_window_tabs(ws, config) {
            windows.push(RestoredWindow {
                tabs,
                active_tab,
                frame: ws.frame,
                deferred_tabs,
            });
        }
    }

    if windows.is_empty() {
        return None;
    }

    Some(windows)
}

#[cfg(test)]
mod tests {

    #[test]
    fn a_second_holder_of_the_session_lock_is_refused() {
        let path = std::env::temp_dir().join(format!("kova-lock-{}.lock", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let first = try_acquire_lock(&path).expect("the first Kova takes the lock");
        assert!(try_acquire_lock(&path).is_none(), "a second Kova must be refused");
        drop(first);
        assert!(try_acquire_lock(&path).is_some(), "the lock is free once the owner is gone");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn every_version_up_to_the_current_one_is_still_readable() {
        for v in 2..=SESSION_VERSION {
            assert!(is_readable_version(v), "v{v} must still load");
        }
        assert!(!is_readable_version(1), "v1 is handled by the legacy path");
        assert!(!is_readable_version(SESSION_VERSION + 1));
    }
    use super::*;

    fn pane(cwd: &str) -> SavedPane {
        SavedPane {
            cwd: Some(cwd.to_string()),
            last_command: Some("claude".to_string()),
            custom_title: None,
            minimized: false,
            title: None,
            claude_session: None,
            agent_session: None,
            subject: None,
        }
    }

    #[test]
    fn a_reopened_pane_resumes_its_conversation_but_only_types_a_shell_command() {
        // An agent pane: the resume line comes back and Kova presses Enter, so
        // the conversation is there without a second gesture.
        let mut agent = pane("/work/app");
        agent.last_command = Some("omp".into());
        agent.agent_session = Some(SavedAgentSession {
            agent: crate::agent_session::Agent::Omp,
            id: "01a17eab-44d5-7315-878f-1c81149f5013".into(),
            config_dir: None,
        });
        let (line, run) = reopen_line(&agent);
        assert_eq!(
            line.as_deref(),
            Some("omp --resume 01a17eab-44d5-7315-878f-1c81149f5013"),
            "the conversation, not the last command"
        );
        assert!(run, "an agent resume line runs itself");

        // A bare shell pane: its last command waits at the prompt. Kova does
        // not re-run what the user may not have meant to run again, whatever
        // the line is.
        let mut shell = pane("/work/app");
        shell.last_command = Some("npm run dev > /tmp/log".into());
        let (line, run) = reopen_line(&shell);
        assert_eq!(line.as_deref(), Some("npm run dev > /tmp/log"), "the last command is offered");
        assert!(!run, "a pane with no conversation runs nothing");
    }

    #[test]
    fn a_pane_saved_before_v5_still_loads() {
        let saved: SavedPane = serde_json::from_str(
            r#"{"cwd":"/a","last_command":"ls","custom_title":null,"minimized":false}"#,
        )
        .expect("a v4 pane must still parse");
        assert_eq!(saved.cwd.as_deref(), Some("/a"));
        assert_eq!(saved.title, None);
        assert_eq!(saved.claude_session, None);
    }

    #[test]
    fn a_pane_with_nothing_to_add_serializes_like_before() {
        let json: serde_json::Value = serde_json::to_value(pane("/a")).unwrap();
        let keys: Vec<&String> = json.as_object().unwrap().keys().collect();
        assert!(!keys.contains(&&"title".to_string()), "unexpected keys: {keys:?}");
        assert!(!keys.contains(&&"claude_session".to_string()), "unexpected keys: {keys:?}");
    }

    #[test]
    fn subjects_survive_a_save_and_old_files_load_without_them() {
        let old: SavedTab = serde_json::from_str(
            r#"{"flat_columns":[{"panes":[{"cwd":"/a"}],"row_weights":[1.0]}],"focused_leaf_index":0,"custom_title":null,"color":null}"#,
        )
        .expect("a tab saved before subjects must still parse");
        assert!(old.subjects.is_empty());
        assert_eq!(old.flat_columns.as_ref().unwrap()[0].panes[0].subject, None);

        let mut tab = old.clone();
        tab.subjects = vec!["auth".into(), "empty one".into()];
        tab.flat_columns.as_mut().unwrap()[0].panes[0].subject = Some("auth".into());
        let back: SavedTab = serde_json::from_str(&serde_json::to_string(&tab).unwrap()).unwrap();
        assert_eq!(back.subjects, ["auth", "empty one"], "empty subjects are kept too");
        assert_eq!(back.flat_columns.unwrap()[0].panes[0].subject.as_deref(), Some("auth"));
        assert!(back.subject_colors.is_empty(), "a session from before colors has none");

        tab.subject_colors = [("auth".to_string(), 4), ("empty one".to_string(), 1)].into_iter().collect();
        let back: SavedTab = serde_json::from_str(&serde_json::to_string(&tab).unwrap()).unwrap();
        assert_eq!(back.subject_colors, tab.subject_colors, "colors survive a save");

        // Nothing filed: the file does not grow.
        let json = serde_json::to_value(&old).unwrap();
        assert!(json.get("subjects").is_none());
        assert!(json.get("subject_colors").is_none());
        assert!(serde_json::to_value(pane("/a")).unwrap().get("subject").is_none());
    }

    #[test]
    fn a_pane_still_asleep_is_saved_with_its_conversation() {
        use crate::agent_session::{Agent, AgentSession};
        let asleep = Some(SavedAgentSession { agent: Agent::Codex, id: "sleeping".into(), config_dir: None });
        let saved = session_to_save(None, asleep.clone()).expect("not forgotten");
        assert_eq!((saved.agent, saved.id.as_str()), (Agent::Codex, "sleeping"));

        let live = AgentSession {
            pid: None,
            agent: Agent::Claude,
            id: "live".into(),
            name: None,
            peer_name: None,
            config_dir: None,
        };
        assert_eq!(
            session_to_save(Some(live), asleep).unwrap().id,
            "live",
            "a running agent wins"
        );
        assert!(session_to_save(None, None).is_none());
    }

    #[test]
    fn the_session_file_is_written_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let path = std::env::temp_dir().join(format!("kova-session-mode-{}.json", std::process::id()));
        // A file an older build left world-readable must be tightened, not just
        // created tight.
        std::fs::write(&path, "old").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();

        write_owner_only(&path, "{}").unwrap();

        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{}");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_pane_that_was_at_a_shell_prompt_keeps_its_last_command() {
        let mut sp = pane("/a");
        sp.last_command = Some("npm run dev".into());
        assert_eq!(restore_command(&sp).as_deref(), Some("npm run dev"));
    }

    /// A home holding one stored omp conversation for `/nowhere/proj`, titled
    /// `title`, plus the pane title omp writes for it on the terminal.
    fn omp_home_with_title(test: &str, title: &str) -> (std::path::PathBuf, String) {
        let home = std::env::temp_dir().join(format!("kova-session-title-{}-{test}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).unwrap();
        let dir = crate::omp_session::session_dir(&home, "/nowhere/proj");
        std::fs::create_dir_all(&dir).unwrap();
        let id = "01a17eab-44d5-7315-878f-1c81149f5013";
        let slot = serde_json::json!({"type": "title", "v": 1, "title": title, "source": "auto"});
        std::fs::write(dir.join(format!("2026-10-27T15-54-09-646Z_{id}.jsonl")), format!("{slot}\n"))
            .unwrap();
        (home, format!("π > {title}"))
    }

    fn panes_of(session: &Session) -> Vec<&SavedPane> {
        session
            .windows
            .iter()
            .flat_map(|w| w.tabs.iter())
            .flat_map(|t| t.flat_columns.iter().flatten())
            .flat_map(|c| c.panes.iter())
            .collect()
    }

    #[test]
    fn a_pane_whose_agent_was_not_named_comes_back_with_its_conversation() {
        let (home, pane_title) = omp_home_with_title("recovered", "🔑 PRIV: Passe sur les besoins");
        let id = "01a17eab-44d5-7315-878f-1c81149f5013";

        let mut dormant = pane("/nowhere/proj");
        dormant.last_command = Some("omp".into());
        dormant.title = Some(pane_title.clone());
        // The agent was quit here and another command run since: the stale
        // title must not bring it back.
        let mut moved_on = dormant.clone();
        moved_on.last_command = Some("npm run dev".into());
        // A pane the snapshot already named keeps its own conversation.
        let mut named = dormant.clone();
        named.agent_session = Some(SavedAgentSession {
            agent: crate::agent_session::Agent::Codex,
            id: "codex-id".into(),
            config_dir: None,
        });

        let mut session = Session {
            version: SESSION_VERSION,
            windows: vec![window_with(vec![dormant, moved_on, named])],
        };
        recover_agent_sessions_by_title(&mut session, &home);

        let panes = panes_of(&session);
        assert_eq!(panes[0].agent_session.as_ref().map(|s| (s.agent, s.id.as_str())), Some((crate::agent_session::Agent::Omp, id)));
        assert_eq!(restore_command(panes[0]).as_deref(), Some("omp --resume 01a17eab-44d5-7315-878f-1c81149f5013"));
        assert!(panes[1].agent_session.is_none(), "a stale title names nothing");
        assert_eq!(panes[2].agent_session.as_ref().map(|s| s.id.as_str()), Some("codex-id"));
        // And the recovered pane resumes on its own at launch.
        assert!(runs_on_launch(panes[0]));
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn one_conversation_shared_by_two_panes_comes_back_in_neither() {
        let (home, pane_title) = omp_home_with_title("shared", "📂 SESS: même titre");
        let mut twice = pane("/nowhere/proj");
        twice.last_command = Some("omp".into());
        twice.title = Some(pane_title);
        let mut session = Session {
            version: SESSION_VERSION,
            windows: vec![window_with(vec![twice.clone(), twice])],
        };
        recover_agent_sessions_by_title(&mut session, &home);

        let panes = panes_of(&session);
        assert!(panes.iter().all(|p| p.agent_session.is_none()), "two panes, one transcript");
        assert!(panes.iter().all(|p| restore_command(p).as_deref() == Some("omp")));
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn a_pane_that_was_running_claude_code_gets_its_resume_line() {
        let mut sp = pane("/a");
        sp.claude_session = Some("live-id".into());
        assert_eq!(restore_command(&sp).as_deref(), Some("claude --resume live-id"));
    }

    #[test]
    fn the_session_recorded_at_quit_beats_the_one_the_last_command_names() {
        // The pane came back with a resume line at some earlier restart and was
        // never run; a newer conversation has been live in it since.
        let mut sp = pane("/a");
        sp.last_command = Some("claude --resume days-old-id".into());
        sp.claude_session = Some("live-id".into());
        assert_eq!(restore_command(&sp).as_deref(), Some("claude --resume live-id"));
    }

    #[test]
    fn a_session_id_that_could_carry_a_second_command_falls_back_to_the_last_command() {
        // The id is replayed from a file on disk, so this guard has to hold even
        // when the file predates the one that refuses to write such an id.
        let mut sp = pane("/a");
        sp.last_command = Some("npm run dev".into());
        sp.claude_session = Some("live-id\nrm -rf ~".into());
        assert_eq!(restore_command(&sp).as_deref(), Some("npm run dev"));
    }

    #[test]
    fn a_pane_that_was_running_codex_gets_a_codex_resume_line() {
        let mut sp = pane("/a");
        sp.last_command = Some("codex".into());
        sp.agent_session = Some(SavedAgentSession {
            agent: crate::agent_session::Agent::Codex,
            id: "01a07651-015e-78a3-97f2-2eaf0f0cd663".into(),
            config_dir: None,
        });
        assert_eq!(
            restore_command(&sp).as_deref(),
            Some("codex resume 01a07651-015e-78a3-97f2-2eaf0f0cd663")
        );
    }

    fn window_with(panes: Vec<SavedPane>) -> WindowSession {
        WindowSession {
            tabs: vec![SavedTab {
                flat_columns: Some(vec![SavedFlatColumn {
                    panes,
                    row_weights: Vec::new(),
                    custom_row_weights: None,
                }]),
                columns: None,
                column_weights: None,
                custom_weights: None,
                tree: None,
                focused_leaf_index: 0,
                custom_title: None,
                color: None,
                virtual_width_override: None,
                scroll_offset_x: None,
                geometry_scale: None,
                subjects: Vec::new(),
                subject_colors: Default::default(),
            }],
            active_tab: 0,
            frame: None,
        }
    }

    fn pi_pane(cwd: &str, id: &str) -> SavedPane {
        let mut sp = pane(cwd);
        sp.last_command = Some("pi".into());
        sp.agent_session = Some(SavedAgentSession {
            agent: crate::agent_session::Agent::Pi,
            id: id.into(),
            config_dir: None,
        });
        sp
    }

    #[test]
    fn two_panes_claiming_the_same_pi_conversation_both_drop_it() {
        let mut windows = vec![window_with(vec![
            pi_pane("/a", "same"),
            pi_pane("/a", "same"),
        ])];
        drop_shared_guessed_sessions(&mut windows);
        let panes = &windows[0].tabs[0].flat_columns.as_ref().unwrap()[0].panes;
        assert!(
            panes.iter().all(|p| p.agent_session.is_none()),
            "neither pane reopens a shared conversation"
        );
        assert!(
            panes
                .iter()
                .all(|p| restore_command(p).as_deref() == Some("pi"))
        );
    }

    #[test]
    fn distinct_pi_conversations_are_left_alone() {
        let mut windows = vec![window_with(vec![
            pi_pane("/a", "one"),
            pi_pane("/b", "two"),
        ])];
        drop_shared_guessed_sessions(&mut windows);
        let panes = &windows[0].tabs[0].flat_columns.as_ref().unwrap()[0].panes;
        assert_eq!(
            restore_command(&panes[0]).as_deref(),
            Some("pi --session one")
        );
        assert_eq!(
            restore_command(&panes[1]).as_deref(),
            Some("pi --session two")
        );
    }

    #[test]
    fn the_agent_recorded_at_quit_beats_the_legacy_claude_field() {
        // A file written by an older build carries `claude_session`; one written
        // now carries `agent_session`, and that is the pane's real agent.
        let mut sp = pane("/a");
        sp.claude_session = Some("old-claude-id".into());
        sp.agent_session = Some(SavedAgentSession {
            agent: crate::agent_session::Agent::Codex,
            id: "codex-id".into(),
            config_dir: None,
        });
        assert_eq!(restore_command(&sp).as_deref(), Some("codex resume codex-id"));
    }

    #[test]
    fn a_claude_session_from_another_config_dir_resumes_through_its_launcher() {
        let mut sp = pane("/a");
        sp.last_command = Some("proxy-cc".into());
        sp.agent_session = Some(SavedAgentSession {
            agent: crate::agent_session::Agent::Claude,
            id: "abc".into(),
            config_dir: Some("/Users/x/.claude-proxy".into()),
        });
        assert_eq!(restore_command(&sp).as_deref(), Some("proxy-cc --resume abc"));
        assert!(runs_on_launch(&sp));

        // The config dir survives a round trip through the session file, and a
        // file without it (every older one) still reads.
        let json = serde_json::to_string(sp.agent_session.as_ref().unwrap()).unwrap();
        let back: SavedAgentSession = serde_json::from_str(&json).unwrap();
        assert_eq!(back.config_dir.as_deref(), Some("/Users/x/.claude-proxy"));
        let old: SavedAgentSession = serde_json::from_str(r#"{"agent":"claude","id":"abc"}"#).unwrap();
        assert_eq!(old.config_dir, None);
    }

    #[test]
    fn a_pane_with_nothing_recorded_is_left_alone() {
        let mut sp = pane("/a");
        sp.last_command = None;
        assert_eq!(restore_command(&sp), None);
    }

    #[test]
    fn only_a_plain_agent_resume_line_runs_by_itself_at_launch() {
        let mut sp = pane("/a");
        sp.last_command = Some("claude --model opus".into());
        sp.claude_session = Some("live-id".into());
        assert!(runs_on_launch(&sp), "a resume line with plain flags runs");

        let mut codex = pane("/a");
        codex.agent_session = Some(SavedAgentSession {
            agent: crate::agent_session::Agent::Codex,
            id: "codex-id".into(),
            config_dir: None,
        });
        assert!(runs_on_launch(&codex));

        // A shell command, even a harmless one, still waits for Enter.
        let mut shell = pane("/a");
        shell.last_command = Some("npm run dev".into());
        assert!(!runs_on_launch(&shell));

        // A refused id falls back to the last command: not run.
        let mut bad_id = pane("/a");
        bad_id.last_command = Some("ls".into());
        bad_id.claude_session = Some("live-id\nrm -rf ~".into());
        assert!(!runs_on_launch(&bad_id));

        // Flags carried from the last `claude …` that the shell would expand.
        let mut expands = pane("/a");
        expands.last_command = Some("claude --append-system-prompt $(cat ~/.ssh/id_rsa)".into());
        expands.claude_session = Some("live-id".into());
        assert!(restore_command(&expands).is_some_and(|l| l.contains("$(")), "the line is still offered");
        assert!(!runs_on_launch(&expands), "but not run");
    }


}
