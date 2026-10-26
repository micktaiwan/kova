//! One terminal pane's own state, apart from where a window lays it out.
//!
//! A `Surface` is what Kova's terminal library hands each pane: the PTY, the
//! terminal state, and what Kova tracks about the program running in it. The
//! window side (`crate::pane::Pane`, tabs, columns) wraps it with layout state.
//! Its input API (`key`, `text`, `mouse`, `scroll`…) lives in `crate::input`.

use parking_lot::RwLock;
use std::cell::{Cell, RefCell};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use crate::config::Config;
use crate::terminal::pty::{ProcessInfo, Pty, SpawnEnv};
use crate::terminal::TerminalState;

pub type PaneId = u32;

static NEXT_PANE_ID: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(1);

fn alloc_pane_id() -> PaneId {
    NEXT_PANE_ID.fetch_add(1, Ordering::Relaxed)
}

/// Per-pane open-latency instrumentation. Splits the new-pane critical path so
/// "time to rectangle" (pane visible) and "time to prompt" (shell usable) can be
/// attributed separately — see notes/pane-open-perf.md.
///
/// The reference instant `entry` is captured at the very start of `Surface::spawn`.
/// The pre-spawn work in the split handlers (viewport float math, no syscalls)
/// is sub-microsecond and folded in. `entry` is set once and only read after,
/// so it is safe to share with the PTY reader thread, which records `shell-ready`.
/// Each milestone logs at most once (atomic-guarded) under the `PANE-OPEN` prefix;
/// grep the log for the full per-pane timeline.
pub struct PaneOpenTimer {
    entry: std::time::Instant,
    inserted_logged: AtomicBool,
    paint_logged: AtomicBool,
    ready_logged: AtomicBool,
}

impl PaneOpenTimer {
    pub fn new() -> Self {
        PaneOpenTimer {
            entry: std::time::Instant::now(),
            inserted_logged: AtomicBool::new(false),
            paint_logged: AtomicBool::new(false),
            ready_logged: AtomicBool::new(false),
        }
    }

    fn elapsed_ms(&self) -> f64 {
        self.entry.elapsed().as_secs_f64() * 1e3
    }

    /// Tree mutation done: the new pane now exists in the layout (main thread).
    /// Logged only for interactive splits (the split handlers call this);
    /// restore/initial panes never reach it, so its absence flags a restore.
    pub fn mark_inserted(&self, pane_id: PaneId) {
        if self.inserted_logged.swap(true, Ordering::Relaxed) {
            return;
        }
        log::info!("PANE-OPEN id={} tree-inserted +{:.1}ms", pane_id, self.elapsed_ms());
    }

    /// First frame the pane is submitted to the renderer = pane becomes visible
    /// (its loading overlay paints). This is the "time to rectangle".
    pub fn mark_first_paint(&self, pane_id: PaneId) {
        if self.paint_logged.swap(true, Ordering::Relaxed) {
            return;
        }
        log::info!("PANE-OPEN id={} first-paint +{:.1}ms (time-to-rectangle)", pane_id, self.elapsed_ms());
    }

    /// Shell emitted its first byte (first prompt) = shell is usable. This is
    /// the "time to prompt". Called from the PTY reader thread.
    pub fn mark_shell_ready(&self, pane_id: PaneId) {
        if self.ready_logged.swap(true, Ordering::Relaxed) {
            return;
        }
        log::info!("PANE-OPEN id={} shell-ready +{:.1}ms (time-to-prompt)", pane_id, self.elapsed_ms());
    }
}

impl Default for PaneOpenTimer {
    fn default() -> Self {
        Self::new()
    }
}

/// The "waiting for the user" flag of a pane, with whether the reader has
/// seen it since it was armed. The two live in one value because every
/// transition that arms the flag must also re-arm the read state: keeping
/// them apart is what let a pane the user had already read (or worse, one
/// that asked a *second* question) fall out of step with Cmd+J.
#[derive(Clone, Copy, Default, PartialEq, Debug)]
pub struct AwaitingFlag {
    since: Option<u64>,
    read: bool,
}

impl AwaitingFlag {
    /// The pane says it waits for the user. Idempotent on the timestamp — a
    /// second `Stop` on the same unanswered turn keeps the original
    /// waiting-since, so the status bar does not restart its clock — but never
    /// on the read state: a fresh question is unread again.
    fn armed(self, now: u64) -> Self {
        Self { since: Some(self.since.unwrap_or(now)), read: false }
    }

    /// The user has had the pane under the eye. Only the jump cycle cares;
    /// the pane keeps saying it waits.
    fn read(self) -> Self {
        Self { read: true, ..self }
    }

    /// Epoch seconds since which the pane has been waiting, if it is.
    fn since(self) -> Option<u64> {
        self.since
    }

    fn is_read(self) -> bool {
        self.read
    }
}

/// Wall-clock seconds since the epoch. Used to stamp when a pane started
/// waiting; a jump in system time only skews a displayed age, never a decision.
fn now_epoch_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// True if `title` begins with a Claude Code *working* marker immediately
/// followed by a space: an animated Braille spinner glyph (U+2800–U+28FF), or a
/// vertical half-circle spinner frame (`◐` U+25D0, `◑` U+25D1).
/// Claude Code prepends a spinner ONLY while it is actively generating or
/// running a tool; at the prompt it shows an asterisk-like idle marker
/// (`✳ Claude Code`) or a plain title instead. So a spinner — and NOT the
/// asterisk — is the reliable "the app is busy" signal.
fn is_working_marker(title: &str) -> bool {
    let mut chars = title.chars();
    matches!(
        chars.next(),
        Some(c) if ('\u{2800}'..='\u{28FF}').contains(&c)
            || matches!(c, '\u{25D0}' | '\u{25D1}')
    ) && chars.next() == Some(' ')
}

/// True if `title` begins with any Claude Code status marker followed by a
/// space: the Braille working spinner, an asterisk-like idle marker
/// (`*`, `✳ ` U+2733, `∗` U+2217), or a vertical half-circle spinner frame
/// (`◐` U+25D0, `◑` U+25D1). Used to strip the prefix for display so the
/// title neither jitters with the spinner nor carries a bare idle marker.
fn has_leading_marker(title: &str) -> bool {
    let mut chars = title.chars();
    matches!(
        chars.next(),
        Some(c) if ('\u{2800}'..='\u{28FF}').contains(&c)
            || matches!(c, '*' | '\u{2733}' | '\u{2217}' | '\u{25D0}' | '\u{25D1}')
    ) && chars.next() == Some(' ')
}

/// Strip a leading status marker that Claude Code prepends to the terminal
/// title (Braille working spinner or asterisk-like idle marker), plus its
/// trailing space. Only trims a single such prefix; leaves the rest untouched.
fn strip_activity_prefix(title: &str) -> &str {
    if has_leading_marker(title) {
        // Marker glyph (1–3 bytes) + one ASCII space (1 byte).
        let marker_len = title.chars().next().map_or(0, char::len_utf8);
        &title[marker_len + 1..]
    } else {
        title
    }
}

/// Clean up a raw foreground process name for display: drop surrounding
/// whitespace, keep only the last path component (some processes report a full
/// path), and drop the leading `-` a login shell carries (`-zsh`). Returns
/// `None` when nothing displayable is left, so callers can treat "no name" and
/// "empty name" the same way.
fn normalize_process_name(raw: &str) -> Option<String> {
    let name = raw.trim();
    let name = name.rsplit('/').next().unwrap_or(name);
    let name = name.strip_prefix('-').unwrap_or(name);
    (!name.is_empty()).then(|| name.to_string())
}

/// What to call a `claude` that runs from another config dir: the dir's
/// suffix, so `~/.claude-deepseek` reads `deepseek` — a launcher that points
/// Claude Code at another model or account names its config dir after it.
/// `None` keeps the process name (another program, or the default dir).
fn claude_flavor_name(process: &str, config_dir: Option<&str>) -> Option<String> {
    if process != "claude" {
        return None;
    }
    let base = config_dir?.trim_end_matches('/').rsplit('/').next()?;
    base.strip_prefix(".claude-")
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// Conversation name → custom pane title → non-empty OSC title → foreground
/// process → cwd basename → fallback. Blank names/titles count as absent.
fn derive_display_title(
    custom_title: Option<&str>,
    agent_name: Option<&str>,
    osc_title: Option<&str>,
    process: Option<&str>,
    cwd: Option<&str>,
    fallback: &str,
) -> String {
    // The current conversation name wins over a sticky pane title, which can
    // outlive the conversation it originally described. Claude's derived names
    // are filtered upstream; Codex's index does not distinguish name sources.
    if let Some(name) = agent_name.map(str::trim).filter(|n| !n.is_empty()) {
        return name.to_string();
    }
    if let Some(custom) = custom_title {
        return custom.to_string();
    }
    if let Some(title) = osc_title.filter(|t| !t.trim().is_empty()) {
        return strip_activity_prefix(title).to_string();
    }
    // Nothing named this pane: the binary running in it is still more telling
    // than the directory it was started from.
    if let Some(process) = process.map(str::trim).filter(|p| !p.is_empty()) {
        return process.to_string();
    }
    if let Some(cwd) = cwd {
        if let Some(base) = std::path::Path::new(cwd).file_name() {
            return base.to_string_lossy().to_string();
        }
    }
    fallback.to_string()
}

/// One terminal: its PTY, terminal state (screen, scrollback, selection,
/// scroll position), and what Kova knows about the program running in it —
/// the agent session, the foreground process, the "waiting for the user"
/// flag. Everything here belongs to the terminal itself, never to where a
/// host window places it: a `Pane` is a `Surface` plus its layout state
/// (minimized, custom title, Cmd+J bookkeeping).
pub struct Surface {
    pub id: PaneId,
    pub terminal: Arc<RwLock<TerminalState>>,
    pub pty: Pty,
    pub shell_exited: Arc<AtomicBool>,
    pub shell_ready: Arc<AtomicBool>,
    pub scroll_accumulator: Cell<f64>,
    /// Command to inject into PTY once shell is ready (for session restore).
    pub pending_command: Cell<Option<String>>,
    /// Press Enter after `pending_command` instead of leaving it for review.
    /// Only set by `run_pending_command`: for a line Kova built itself (the
    /// search palette's resume command), or the `command` a library host
    /// passed to `kova_pane_new`, which that host answers for.
    run_pending: Cell<bool>,
    /// The agent conversation a launch restore left asleep in this pane: its
    /// resume line waits in `pending_command`, neither typed nor run, until
    /// the pane is first on screen (`wake`). Kept here because no agent runs
    /// yet, so `agent_session` is `None` and the session file would otherwise
    /// forget the conversation (`session::snapshot_flat_column`).
    dormant: RefCell<Option<crate::session::SavedAgentSession>>,
    /// The conversation `wake` just started, until the probe sees its agent
    /// running (`set_agent_session`). Between the two, `agent_session` is still
    /// `None`: without this, a quit in that gap would save the pane empty.
    woken: RefCell<Option<crate::session::SavedAgentSession>>,
    /// Open-latency instrumentation (time-to-rectangle / time-to-prompt).
    pub open_timer: Arc<PaneOpenTimer>,
    /// The app in this pane told us it is waiting for the user (Claude Code
    /// pushes this from its `Stop` / permission-prompt hooks over IPC), plus
    /// whether the user has looked at it since. Never trusted blindly: see
    /// `is_awaiting` and `check_running`.
    pub awaiting: Cell<AwaitingFlag>,
    /// The coding-agent conversation running in this pane — Claude Code or
    /// Codex — with the id its `resume` takes and its conversation name.
    /// Refreshed on the same throttle as the foreground
    /// probe (`check_running`) rather than read per frame, because the
    /// lookup scans a directory or the process table. `None` = no agent in this
    /// pane; a conversation nobody named is `Some` with a `name` of `None`.
    pub agent_session: RefCell<Option<crate::agent_session::AgentSession>>,
    /// Id of the last Claude conversation `agent_session` saw in this pane.
    /// Unlike `agent_session`, it survives `claude` exiting (`/exit`) and the
    /// shell dying, so closing the pane can still warn about that
    /// conversation's open todos (`crate::raf`). Replaced only by the next
    /// Claude conversation seen here; a Codex one leaves it alone.
    pub last_claude_session: RefCell<Option<String>>,
    /// Name of the binary running in the foreground (`claude`, `nvim`, `ssh`…),
    /// `None` at a bare shell prompt. Cached because the status bar reads it on
    /// every frame while resolving it costs two syscalls: it is refreshed on the
    /// same ~0.5s throttle as the running-state probe, in `check_running`.
    fg_process: RefCell<Option<ProcessInfo>>,
    /// Working directory as of the last probe, for readers that run on every
    /// frame (the status bar's bookmark colour): the live `cwd()` is a
    /// `proc_pidinfo` syscall, 60 times a second per pane. Stored with the
    /// OSC 7 cwd seen at probe time, so a `cd` the shell reports re-probes on
    /// the next frame; dropped every ~0.5s by `check_running` for shells
    /// that report nothing. `None` = probe on next read.
    cached_cwd: RefCell<Option<(Option<String>, Option<String>)>>,
}

/// What one pass of [`Surface::check_running`] saw.
#[derive(Clone, Copy, Default, PartialEq, Debug)]
pub struct RunningState {
    /// The shell has exited: the pass dropped everything it knew about the
    /// program running here, and reports nothing running.
    pub exited: bool,
    /// OSC 133;C seen without its 133;D yet.
    pub command_running: bool,
    /// A process other than the shell owns the terminal. Only probed when
    /// the pass was asked to (`refresh_fg`); `false` otherwise.
    pub fg_running: bool,
}

impl Surface {
    pub fn spawn(cols: u16, rows: u16, config: &Config, working_dir: Option<&str>) -> Result<Self, Box<dyn std::error::Error>> {
        Self::spawn_with(cols, rows, config, working_dir, &SpawnEnv::default())
    }

    /// `spawn`, telling the shell `env` (its `KOVA_SOCKET`, extra variables).
    pub fn spawn_with(
        cols: u16,
        rows: u16,
        config: &Config,
        working_dir: Option<&str>,
        env: &SpawnEnv,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        // Reference instant for open-latency instrumentation — captured first so
        // it covers the whole spawn (TerminalState alloc + fork/exec + dups).
        let open_timer = Arc::new(PaneOpenTimer::new());
        let id = alloc_pane_id();
        let terminal = new_terminal(cols, rows, config);
        let shell_exited = Arc::new(AtomicBool::new(false));
        let shell_ready = Arc::new(AtomicBool::new(false));
        let pty = Pty::spawn(
            cols,
            rows,
            terminal.clone(),
            shell_exited.clone(),
            shell_ready.clone(),
            working_dir,
            id,
            open_timer.clone(),
            env,
        )?;
        Ok(Self::assemble(id, terminal, pty, shell_exited, shell_ready, open_timer))
    }

    /// Create a lightweight placeholder surface with a dummy PTY (no shell process).
    /// Used for deferred tab restore — avoids spawning a shell that would
    /// compete with the active tab's shells for zshrc loading time.
    pub fn placeholder(cols: u16, rows: u16, config: &Config) -> Result<Self, Box<dyn std::error::Error>> {
        let id = alloc_pane_id();
        let terminal = new_terminal(cols, rows, config);
        let pty = Pty::dummy()?;
        Ok(Self::assemble(
            id,
            terminal,
            pty,
            Arc::new(AtomicBool::new(false)),
            Arc::new(AtomicBool::new(true)), // placeholder is immediately "ready"
            Arc::new(PaneOpenTimer::new()),
        ))
    }

    /// Test-only: a surface without a shell whose PTY input is readable on the
    /// returned fd (see [`Pty::capture`]).
    #[cfg(test)]
    pub fn capture(cols: u16, rows: u16) -> (Self, std::os::fd::OwnedFd) {
        let config = Config::default();
        let terminal = new_terminal(cols, rows, &config);
        let (pty, reader) = Pty::capture();
        let surface = Self::assemble(
            alloc_pane_id(),
            terminal,
            pty,
            Arc::new(AtomicBool::new(false)),
            Arc::new(AtomicBool::new(true)),
            Arc::new(PaneOpenTimer::new()),
        );
        (surface, reader)
    }

    /// A fresh surface around an already created terminal and PTY: nothing
    /// pending, not waiting, no agent session or process probed yet.
    fn assemble(
        id: PaneId,
        terminal: Arc<RwLock<TerminalState>>,
        pty: Pty,
        shell_exited: Arc<AtomicBool>,
        shell_ready: Arc<AtomicBool>,
        open_timer: Arc<PaneOpenTimer>,
    ) -> Self {
        Surface {
            id,
            terminal,
            pty,
            shell_exited,
            shell_ready,
            scroll_accumulator: Cell::new(0.0),
            pending_command: Cell::new(None),
            run_pending: Cell::new(false),
            dormant: RefCell::new(None),
            woken: RefCell::new(None),
            open_timer,
            awaiting: Cell::new(AwaitingFlag::default()),
            agent_session: RefCell::new(None),
            last_claude_session: RefCell::new(None),
            fg_process: RefCell::new(None),
            cached_cwd: RefCell::new(None),
        }
    }

    pub fn cwd(&self) -> Option<String> {
        self.pty.cwd()
    }

    /// `cwd()` without a syscall per frame (see the `cached_cwd` field): at
    /// most one probe per ~0.5s, plus one when the shell reports a `cd`.
    pub fn cached_cwd(&self) -> Option<String> {
        let osc_cwd = self.terminal.read().cwd.clone();
        let mut slot = self.cached_cwd.borrow_mut();
        match slot.as_ref() {
            Some((seen, cwd)) if *seen == osc_cwd => cwd.clone(),
            _ => {
                let cwd = self.cwd();
                *slot = Some((osc_cwd, cwd.clone()));
                cwd
            }
        }
    }

    pub fn foreground_process_name(&self) -> Option<String> {
        self.pty.foreground_process_name()
    }

    /// Re-probe the foreground process and refresh the cached name. Returns
    /// whether a process other than the shell owns the terminal — the answer
    /// `check_running` needs, so both come out of a single probe.
    /// An unresolvable name caches as `None` (nothing to display) without
    /// changing the returned yes/no.
    fn refresh_fg_process(&self) -> bool {
        let (agent_pid, config_dir) = match self.agent_session.borrow().as_ref() {
            Some(s) => (s.pid, s.config_dir.clone()),
            None => (None, None),
        };
        let probe = self.pty.foreground_process_preferring(agent_pid);
        let running = probe.is_some();
        self.fg_process.replace(probe.and_then(|info| {
            let name = normalize_process_name(&info.name)?;
            let name = claude_flavor_name(&name, config_dir.as_deref()).unwrap_or(name);
            Some(ProcessInfo { name, ..info })
        }));
        running
    }

    /// Cached name and version of the foreground binary, `None` at a bare shell
    /// prompt. Up to ~0.5s stale (see `check_running`), which is what makes
    /// it free to read on every frame.
    pub fn fg_process(&self) -> Option<ProcessInfo> {
        self.fg_process.borrow().clone()
    }

    pub fn is_alive(&self) -> bool {
        !self.shell_exited.load(Ordering::Relaxed)
    }

    pub fn is_ready(&self) -> bool {
        self.shell_ready.load(Ordering::Relaxed)
    }

    pub fn last_command(&self) -> Option<String> {
        self.terminal.read().last_command.clone()
    }

    /// Title set by the running app (OSC 0/2), without the activity marker
    /// Claude Code prepends — so a saved title does not freeze a spinner frame.
    pub fn osc_title(&self) -> Option<String> {
        self.terminal
            .read()
            .title
            .as_deref()
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .map(|t| strip_activity_prefix(t).to_string())
    }

    /// Display title for this surface: agent session name > `custom_title`
    /// (the host's sticky name for it, if any) > OSC title > foreground
    /// process > CWD basename > fallback.
    pub fn display_title(&self, custom_title: Option<&str>, fallback: &str) -> String {
        let term = self.terminal.read();
        let session = self.agent_session.borrow();
        derive_display_title(
            custom_title,
            session.as_ref().and_then(|s| s.name.as_deref()),
            term.title.as_deref(),
            self.fg_process.borrow().as_ref().map(|p| p.name.as_str()),
            term.cwd.as_deref(),
            fallback,
        )
    }

    /// Re-read the agent conversation running in this pane. Called from the
    /// throttled probe pass (`check_running`), never per frame: the lookup is
    /// a cached scan of `~/.claude/sessions/`, then of the process table for
    /// Codex.
    pub fn refresh_agent_session(&self) {
        self.set_agent_session(crate::agent_session::for_shell(self.pty.pid()));
    }

    /// Store what the probe found, remembering a Claude conversation's id in
    /// `last_claude_session` so it outlives the session itself.
    pub(crate) fn set_agent_session(&self, found: Option<crate::agent_session::AgentSession>) {
        if found.is_some() {
            self.woken.replace(None);
        }
        if let Some(s) = found.as_ref().filter(|s| s.agent == crate::agent_session::Agent::Claude) {
            self.last_claude_session.replace(Some(s.id.clone()));
            if let Some(dir) = s.config_dir.as_deref() {
                if let Some(launcher) =
                    crate::claude_session::launcher(self.last_command().as_deref(), Some(dir))
                {
                    crate::launchers::remember(dir, &launcher);
                }
            }
        }
        *self.agent_session.borrow_mut() = found;
    }

    /// The Claude conversation this pane holds, or held last: the live one,
    /// else the one `claude` left behind when it exited.
    pub fn last_claude_session_id(&self) -> Option<String> {
        self.claude_session_id().or_else(|| self.last_claude_session.borrow().clone())
    }

    /// Check whether something runs in this surface. Two sources:
    /// - OSC 133;C/D from shell integration (precise prompt cycles) — note
    ///   that Claude Code's `Stop` hook sends `mark-completed` at the end of
    ///   each turn, which clears it too, so this flag alone dies while claude
    ///   is still open;
    /// - a foreground process group other than the shell (tcgetpgrp) — covers
    ///   claude, vim, any TUI, no shell integration needed. Only re-probed
    ///   when `refresh_fg` is true (one ioctl per pane); the same probe caches
    ///   the process name the status bar and the pane switcher display, and
    ///   the same throttle re-reads the agent session and drops the cached cwd.
    /// A surface whose shell exited reports nothing running and forgets what
    /// it knew of its program — a shell killed mid-command never emits
    /// 133;D, which would strand the OSC flag.
    ///
    /// This pass also reaps stale "waiting for the user" flags, because it is
    /// already paying for the one probe that answers the question: a pane
    /// marked waiting whose foreground process is gone (its Claude Code died
    /// without ever retracting the flag) is dropped here. Piggybacking costs
    /// nothing; a separate sweep would double the ioctls.
    pub fn check_running(&self, refresh_fg: bool) -> RunningState {
        if !self.is_alive() {
            self.clear_awaiting();
            self.fg_process.replace(None);
            self.agent_session.replace(None);
            return RunningState { exited: true, ..RunningState::default() };
        }
        let command_running = self.terminal.read().command_running.load(Ordering::Relaxed);
        let mut fg_running = false;
        if refresh_fg {
            self.refresh_agent_session();
            fg_running = self.refresh_fg_process();
            self.cached_cwd.replace(None);
            if !fg_running {
                // Back to a bare shell prompt: whatever was waiting is gone.
                self.clear_awaiting();
            }
        }
        RunningState { exited: false, command_running, fg_running }
    }

    /// Id of the agent conversation in this pane — the argument its `resume`
    /// takes. It is the only identifier here that outlives the pane, so an
    /// external client tying a pane to a subject must key on this, not on the
    /// pane id: a closed tab reopened tomorrow is the same subject.
    pub fn agent_session_id(&self) -> Option<String> {
        self.agent_session.borrow().as_ref().map(|s| s.id.clone())
    }

    /// Current conversation name, regardless of which agent owns it.
    pub fn agent_session_name(&self) -> Option<String> {
        self.agent_session.borrow().as_ref().and_then(|s| s.name.clone())
    }

    /// Which agent holds the conversation in this pane, if any.
    pub fn agent_kind(&self) -> Option<crate::agent_session::Agent> {
        self.agent_session.borrow().as_ref().map(|s| s.agent)
    }

    /// Id of the conversation only when it is a Claude one. What an external
    /// client hands to `claude --resume` or matches against a Claude session
    /// name; a Codex pane answers `None` rather than an id Claude cannot open.
    pub fn claude_session_id(&self) -> Option<String> {
        let session = self.agent_session.borrow();
        let session = session.as_ref()?;
        (session.agent == crate::agent_session::Agent::Claude).then(|| session.id.clone())
    }

    /// Name that `/rename` gave a Claude conversation; `None` for other agents.
    /// Exposed on its own rather than only through `display_title`, where it is
    /// one candidate among five and indistinguishable from the others.
    pub fn claude_session_name(&self) -> Option<String> {
        self.agent_session.borrow().as_ref()
            .filter(|s| s.agent == crate::agent_session::Agent::Claude)
            .and_then(|s| s.name.clone())
    }

    /// The non-default `CLAUDE_CONFIG_DIR` the pane's Claude session runs
    /// under, `None` for `~/.claude` or when no Claude runs here.
    pub fn claude_config_dir(&self) -> Option<String> {
        self.agent_session.borrow().as_ref()
            .filter(|s| s.agent == crate::agent_session::Agent::Claude)
            .and_then(|s| s.config_dir.clone())
    }

    /// Name other Claude sessions reach this one by with `SendMessage` /
    /// `ListAgents`: the session file's `name`, derived or set by `/rename`.
    /// `None` for other agents and before Claude Code has written a name.
    pub fn claude_peer_name(&self) -> Option<String> {
        self.agent_session.borrow().as_ref()
            .filter(|s| s.agent == crate::agent_session::Agent::Claude)
            .and_then(|s| s.peer_name.clone())
    }

    /// True if the app in this pane is actively working: its live OSC 0/2 title
    /// leads with one of Claude Code's animated spinners (see `is_working_marker`).
    /// The asterisk idle marker (`✳ Claude Code`) does NOT count. Reads the live
    /// OSC 0/2 title even when a sticky custom title shadows it in the display.
    pub fn is_working(&self) -> bool {
        self.terminal
            .read()
            .title
            .as_deref()
            .map_or(false, is_working_marker)
    }

    /// True if the app in this pane says it is waiting for the user, and
    /// nothing observed since contradicts it.
    ///
    /// The flag is *pushed* by Claude Code's hooks, but a pushed flag can
    /// outlive its truth (a session killed with -9 never gets to retract it),
    /// so it is only ever reported through checks Kova makes itself. Here: a
    /// pane whose Claude went back to work cannot be waiting, whatever the
    /// last hook said. The slower liveness reap lives in `check_running`.
    pub fn is_awaiting(&self) -> bool {
        self.awaiting.get().since().is_some() && !self.is_working()
    }

    /// Epoch seconds since which this pane has been waiting, if it is.
    pub fn awaiting_since(&self) -> Option<u64> {
        self.is_awaiting().then(|| self.awaiting.get().since()).flatten()
    }

    /// Mark the pane as waiting for the user, starting now (idempotent: an
    /// already-waiting pane keeps its original timestamp, so a second `Stop`
    /// on the same unanswered turn does not reset how long it has waited).
    pub fn set_awaiting(&self) {
        self.awaiting.set(self.awaiting.get().armed(now_epoch_secs()));
    }

    /// Drop the waiting flag — the user engaged, or the session is gone.
    pub fn clear_awaiting(&self) {
        self.awaiting.set(AwaitingFlag::default());
    }

    /// Record that the pane has been looked at while waiting (called by the
    /// frame loop on the focused pane of the key window).
    pub fn mark_awaiting_seen(&self) {
        self.awaiting.set(self.awaiting.get().read());
    }

    /// True if the pane waits for the user AND has not been looked at since.
    /// This — not `is_awaiting` — is what Cmd+J walks: a pane you have read
    /// keeps its marker but stops pulling the jump back to itself.
    pub fn is_awaiting_unseen(&self) -> bool {
        self.is_awaiting() && !self.awaiting.get().is_read()
    }

    /// An agent session lives in this pane, working or not. What Cmd+J's
    /// non-draining loop walks: an open session is an open loop whether it is
    /// chewing or waiting to be closed.
    pub fn has_agent_session(&self) -> bool {
        self.agent_session.borrow().is_some()
    }

    /// The mirror of `is_idle_agent`: an agent session that is actively
    /// working. Never a landing spot for the draining tiers — the loop walks it,
    /// but it is never announced as something asking for an answer.
    pub fn is_working_agent(&self) -> bool {
        self.agent_session.borrow().is_some() && self.is_working()
    }

    /// True if an agent session sits open in this pane and is not working.
    /// "Idle" is the absence of the working spinner (`is_working`), so a session
    /// chewing on something is never pulled up.
    ///
    /// This is what Cmd+J hands back once everything else is dealt with: an open
    /// session is either closed or picked up again, never quietly accumulated.
    pub fn is_idle_agent(&self) -> bool {
        self.agent_session.borrow().is_some() && !self.is_working()
    }

    /// Re-resolve the git branch of the shell's directory for the status bar,
    /// and redraw when it changed. The host calls it on its own slow cadence.
    pub fn refresh_git_branch(&self) {
        let term = self.terminal.read();
        let cwd = term.cwd.clone();
        let old_branch = term.git_branch.clone();
        drop(term);
        if let Some(ref cwd) = cwd {
            let new_branch = crate::terminal::parser::resolve_git_branch(cwd);
            if new_branch != old_branch {
                let mut term = self.terminal.write();
                term.git_branch = new_branch;
                term.dirty.store(true, std::sync::atomic::Ordering::Relaxed);
            }
        }
    }

    /// If the shell is ready and there's a pending command, write it to the PTY
    /// — without \r so the user can review before pressing Enter, unless
    /// `run_pending_command` asked for it to run.
    pub fn inject_pending_command(&self) {
        if !self.is_ready() || self.dormant.borrow().is_some() {
            return;
        }
        let cmd = self.pending_command.take();
        if let Some(command) = cmd {
            let line = injected_line(&command, self.run_pending.replace(false));
            self.pty.write(line.as_bytes());
        }
    }

    /// Have the pending command run instead of waiting on the prompt. A no-op
    /// when nothing is pending, so the flag never outlives its command.
    pub fn run_pending_command(&self) {
        let cmd = self.pending_command.take();
        if cmd.is_some() {
            self.run_pending.set(true);
        }
        self.pending_command.set(cmd);
    }

    /// Leave the pending resume line of conversation `id` asleep until `wake`:
    /// a launch restore does this instead of `run_pending_command`, so fifty
    /// restored agents do not all start at once.
    pub fn sleep_until_shown(&self, session: crate::session::SavedAgentSession) {
        *self.dormant.borrow_mut() = Some(session);
    }

    /// The conversation still asleep in this pane, if any (see `dormant`).
    pub fn dormant_session(&self) -> Option<(crate::agent_session::Agent, String)> {
        self.dormant.borrow().as_ref().map(|s| (s.agent, s.id.clone()))
    }

    pub fn dormant_session_id(&self) -> Option<String> {
        self.dormant.borrow().as_ref().map(|s| s.id.clone())
    }

    /// The pane is shown: run the resume line it was keeping. False when it was
    /// not asleep, so calling it on every frame costs a borrow.
    pub fn wake(&self) -> bool {
        let Some(session) = self.dormant.borrow_mut().take() else {
            return false;
        };
        self.woken.replace(Some(session));
        self.run_pending_command();
        true
    }

    /// Someone else took over this sleeping pane (keys sent over IPC): forget
    /// the resume line, so the first time the pane is shown does not type it
    /// into whatever they started. A pane that was not asleep is untouched.
    pub fn cancel_sleep(&self) {
        if self.dormant.borrow_mut().take().is_some() {
            self.pending_command.set(None);
        }
    }

    /// The conversation to save for this pane while no agent is seen running
    /// in it: the one still asleep, or the one just woken whose agent has not
    /// registered yet.
    pub fn unregistered_session(&self) -> Option<crate::session::SavedAgentSession> {
        self.dormant.borrow().clone().or_else(|| self.woken.borrow().clone())
    }
}

/// A pane's `(unread completion, bell)` for this frame; when `seen` (it is
/// under the eye), both are acknowledged first. Shared by Kova.app's frame
/// loop and the library's pane (`crate::embed`).
///
/// Rendering a background window does not mean its selected pane was read.
/// Keep the completion flag itself sticky for IPC wait-for-completion.
pub fn pane_attention(term: &TerminalState, seen: bool) -> (bool, bool) {
    use std::sync::atomic::Ordering::Relaxed;
    if seen {
        let had_bell = term.bell.swap(false, Relaxed);
        let had_completion = term.unread_completion();
        term.ack_completion();
        if had_bell || had_completion {
            term.dirty.store(true, Relaxed);
        }
    }
    (term.unread_completion(), term.bell.load(Relaxed))
}

/// A blank terminal state sized `cols`×`rows`, with the configured scrollback
/// and default colours.
fn new_terminal(cols: u16, rows: u16, config: &Config) -> Arc<RwLock<TerminalState>> {
    Arc::new(RwLock::new(TerminalState::new(
        cols,
        rows,
        config.terminal.scrollback,
        crate::terminal::color_to_u8(config.colors.foreground),
        crate::terminal::color_to_u8(config.colors.background),
    )))
}

/// What `inject_pending_command` writes: the line as is, plus Enter to run it.
fn injected_line(command: &str, run: bool) -> String {
    if run {
        format!("{}\r", command)
    } else {
        command.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::{
        AwaitingFlag, claude_flavor_name, derive_display_title, injected_line, is_working_marker,
        normalize_process_name, strip_activity_prefix,
    };

    #[test]
    fn a_read_waiting_pane_re_enters_the_jump_cycle_only_on_a_new_question() {
        let quiet = AwaitingFlag::default();
        assert_eq!(quiet.since(), None);
        assert!(!quiet.is_read());

        // Claude stops and asks: waiting, and unread.
        let asked = quiet.armed(100);
        assert_eq!(asked.since(), Some(100));
        assert!(!asked.is_read());

        // The eye lands on the pane: still waiting (reading is not answering),
        // but read — Cmd+J must now walk past it.
        let seen = asked.read();
        assert!(seen.is_read());
        assert_eq!(seen.since(), Some(100), "looking at a pane does not answer it");

        // A second question on the same unanswered turn makes it unread again,
        // without restarting the waiting clock.
        let asked_again = seen.armed(160);
        assert!(!asked_again.is_read(), "a fresh question must resurface the pane");
        assert_eq!(asked_again.since(), Some(100), "the waiting clock keeps its origin");
    }

    #[test]
    fn is_working_marker_only_on_a_spinner() {
        // Working: an animated Braille spinner glyph (U+2800–U+28FF) + space.
        assert!(is_working_marker("\u{2802} Revue de code")); // ⠂
        // Working: the vertical half-circle spinner frames + space.
        assert!(is_working_marker("\u{25D0} Configurer le suivi")); // ◐
        assert!(is_working_marker("\u{25D1} Configurer le suivi")); // ◑
        assert!(!is_working_marker("\u{25D0}glued"));
        assert!(is_working_marker("\u{2810} Comprendre"));    // ⠐
        assert!(is_working_marker("\u{28FF} x"));             // last frame of range
        assert!(is_working_marker("\u{2800} ")); // spinner + trailing space only
        // NOT working: the asterisk idle marker (this was the earlier bug).
        assert!(!is_working_marker("* Claude Code"));
        assert!(!is_working_marker("\u{2733} Claude Code"));
        assert!(!is_working_marker("\u{2217} foo"));
        // NOT working: plain titles, spinner without a space, empty.
        assert!(!is_working_marker("plain title"));
        assert!(!is_working_marker("\u{2802}glued"));
        assert!(!is_working_marker("\u{2802}"));
        assert!(!is_working_marker(""));
    }

    #[test]
    fn process_name_kept_as_is_when_already_a_bare_binary() {
        assert_eq!(normalize_process_name("claude").as_deref(), Some("claude"));
        assert_eq!(normalize_process_name("nvim").as_deref(), Some("nvim"));
    }

    #[test]
    fn process_name_keeps_only_the_last_path_component() {
        assert_eq!(normalize_process_name("/usr/bin/ssh").as_deref(), Some("ssh"));
    }

    #[test]
    fn process_name_drops_the_login_shell_dash() {
        assert_eq!(normalize_process_name("-zsh").as_deref(), Some("zsh"));
    }

    #[test]
    fn process_name_trims_surrounding_whitespace() {
        assert_eq!(normalize_process_name("  cargo \n").as_deref(), Some("cargo"));
    }

    #[test]
    fn unresolvable_process_name_yields_nothing_to_display() {
        // proc_name failing gives an empty string — must not render a blank label.
        assert_eq!(normalize_process_name(""), None);
        assert_eq!(normalize_process_name("   "), None);
        assert_eq!(normalize_process_name("-"), None);
    }

    #[test]
    fn strip_activity_prefix_trims_spinner_and_idle_marker() {
        // Braille working spinner stripped for a stable, non-jittering title.
        assert_eq!(strip_activity_prefix("\u{2802} Revue de code"), "Revue de code");
        assert_eq!(strip_activity_prefix("\u{2810} Comprendre"), "Comprendre");
        // Asterisk-like idle markers still stripped too.
        assert_eq!(strip_activity_prefix("* Add TimeComet.swift"), "Add TimeComet.swift");
        assert_eq!(strip_activity_prefix("\u{2733} Claude Code"), "Claude Code");
        assert_eq!(strip_activity_prefix("\u{2217} foo"), "foo");
        // Vertical half-circle spinner frames stripped as well.
        assert_eq!(strip_activity_prefix("\u{25D0} Configurer le suivi"), "Configurer le suivi");
        assert_eq!(strip_activity_prefix("\u{25D1} Configurer le suivi"), "Configurer le suivi");
        // No marker, or marker without a following space: left untouched.
        assert_eq!(strip_activity_prefix("plain title"), "plain title");
        assert_eq!(strip_activity_prefix("*already glued"), "*already glued");
        assert_eq!(strip_activity_prefix("\u{2802}glued"), "\u{2802}glued");
        // Only one prefix stripped; an inner asterisk stays.
        assert_eq!(strip_activity_prefix("* a * b"), "a * b");
        // Empty and marker-only edge cases don't panic.
        assert_eq!(strip_activity_prefix(""), "");
        assert_eq!(strip_activity_prefix("*"), "*");
        assert_eq!(strip_activity_prefix("* "), "");
    }

    #[test]
    fn claude_session_name_wins_over_everything() {
        let t = derive_display_title(
            Some("my pane"),
            Some("session"),
            Some("osc"),
            Some("claude"),
            Some("/home/x/proj"),
            "shell",
        );
        assert_eq!(t, "session", "a /rename must beat the pane's sticky title");
    }

    #[test]
    fn custom_title_wins_over_everything_below_the_claude_name() {
        let t = derive_display_title(
            Some("my pane"),
            None,
            Some("osc"),
            Some("claude"),
            Some("/home/x/proj"),
            "shell",
        );
        assert_eq!(t, "my pane");
    }

    #[test]
    fn claude_session_name_wins_over_osc_title() {
        // The OSC title a Claude pane carries is the generic "✳ Claude Code";
        // the name the user gave the session is what identifies the pane.
        let t = derive_display_title(
            None,
            Some("pane switcher"),
            Some("✳ Claude Code"),
            Some("claude"),
            Some("/home/x/proj"),
            "shell",
        );
        assert_eq!(t, "pane switcher");
    }

    #[test]
    fn blank_claude_session_name_falls_through_to_osc_title() {
        let t = derive_display_title(None, Some("   "), Some("vim"), None, Some("/home/x/proj"), "shell");
        assert_eq!(t, "vim");
    }

    #[test]
    fn blank_claude_session_name_leaves_the_custom_title_alone() {
        let t = derive_display_title(Some("my pane"), Some("   "), Some("vim"), None, None, "shell");
        assert_eq!(t, "my pane");
    }

    #[test]
    fn non_empty_osc_title_used() {
        let t = derive_display_title(None, None, Some("vim"), None, Some("/home/x/proj"), "shell");
        assert_eq!(t, "vim");
    }

    #[test]
    fn osc_title_wins_over_the_foreground_process() {
        // What the app calls itself says more than the name of its binary.
        let t = derive_display_title(None, None, Some("README.md"), Some("nvim"), Some("/home/x/proj"), "shell");
        assert_eq!(t, "README.md");
    }

    #[test]
    fn a_claude_from_another_config_dir_is_named_after_it() {
        assert_eq!(
            claude_flavor_name("claude", Some("/Users/x/.claude-deepseek")).as_deref(),
            Some("deepseek")
        );
        assert_eq!(
            claude_flavor_name("claude", Some("/Users/x/.claude-deepseek/")).as_deref(),
            Some("deepseek")
        );
    }

    #[test]
    fn the_process_name_stays_otherwise() {
        assert_eq!(claude_flavor_name("claude", None), None);
        assert_eq!(
            claude_flavor_name("claude", Some("/Users/x/.claude-")),
            None
        );
        assert_eq!(
            claude_flavor_name("claude", Some("/Users/x/.config/claude")),
            None
        );
        // Something else in the foreground (claude suspended): not renamed.
        assert_eq!(
            claude_flavor_name("vim", Some("/Users/x/.claude-deepseek")),
            None
        );
    }

    #[test]
    fn foreground_process_used_when_nothing_named_the_pane() {
        let t = derive_display_title(None, None, None, Some("nvim"), Some("/home/x/proj"), "shell");
        assert_eq!(t, "nvim");
        // A blank OSC title must not shadow the process either.
        let t = derive_display_title(None, None, Some("  "), Some("nvim"), Some("/home/x/proj"), "shell");
        assert_eq!(t, "nvim");
    }

    #[test]
    fn empty_osc_title_falls_back_to_cwd_basename() {
        let t = derive_display_title(None, None, Some(""), None, Some("/home/x/proj"), "shell");
        assert_eq!(t, "proj");
    }

    #[test]
    fn whitespace_osc_title_falls_back_to_cwd_basename() {
        let t = derive_display_title(None, None, Some("   "), None, Some("/home/x/proj"), "shell");
        assert_eq!(t, "proj");
    }

    #[test]
    fn blank_process_falls_through_to_cwd_basename() {
        let t = derive_display_title(None, None, None, Some("   "), Some("/home/x/proj"), "shell");
        assert_eq!(t, "proj");
    }

    #[test]
    fn claude_session_name_used_when_the_pane_has_no_other_title() {
        let t = derive_display_title(None, Some("kova-bc"), None, None, None, "shell");
        assert_eq!(t, "kova-bc");
    }

    #[test]
    fn no_title_no_cwd_uses_fallback() {
        assert_eq!(derive_display_title(None, None, None, None, None, "shell"), "shell");
        // Empty OSC title with no cwd must also reach the fallback, never blank.
        assert_eq!(derive_display_title(None, None, Some(""), None, None, "shell"), "shell");
    }

    #[test]
    fn a_sleeping_resume_line_is_neither_typed_nor_run_until_woken() {
        use crate::agent_session::Agent;
        use crate::input::tests::drain;
        let (surface, reader) = super::Surface::capture(80, 24);
        surface.pending_command.set(Some("claude --resume abc".into()));
        surface.sleep_until_shown(crate::session::SavedAgentSession { agent: Agent::Claude, id: "abc".into(), config_dir: None });
        assert_eq!(surface.dormant_session_id().as_deref(), Some("abc"));

        surface.inject_pending_command();
        assert!(drain(&reader).is_empty(), "asleep: nothing reaches the shell");

        assert!(surface.wake());
        assert_eq!(surface.dormant_session(), None);
        surface.inject_pending_command();
        assert_eq!(drain(&reader), b"claude --resume abc\r", "woken: typed and run");

        assert!(!surface.wake(), "a second wake is a no-op");
        surface.inject_pending_command();
        assert!(drain(&reader).is_empty());
    }

    #[test]
    fn keys_sent_to_a_sleeping_pane_cancel_its_resume_line() {
        use crate::agent_session::Agent;
        use crate::input::tests::drain;
        let (surface, reader) = super::Surface::capture(80, 24);
        surface.pending_command.set(Some("claude --resume abc".into()));
        surface.sleep_until_shown(crate::session::SavedAgentSession { agent: Agent::Claude, id: "abc".into(), config_dir: None });
        surface.cancel_sleep();
        assert_eq!(surface.dormant_session(), None);
        assert!(!surface.wake(), "nothing left to wake");
        surface.inject_pending_command();
        assert!(drain(&reader).is_empty(), "the resume line is gone");

        let (awake, reader) = super::Surface::capture(80, 24);
        awake.pending_command.set(Some("make".into()));
        awake.cancel_sleep();
        awake.inject_pending_command();
        assert_eq!(drain(&reader), b"make", "a pane that was not asleep keeps its line");
    }

    #[test]
    fn a_woken_conversation_is_kept_until_its_agent_registers() {
        use crate::agent_session::{Agent, AgentSession};
        let (surface, _reader) = super::Surface::capture(80, 24);
        surface.pending_command.set(Some("claude --resume abc".into()));
        surface.sleep_until_shown(crate::session::SavedAgentSession { agent: Agent::Claude, id: "abc".into(), config_dir: None });
        assert!(surface.wake());
        assert_eq!(surface.dormant_session(), None);
        assert_eq!(
            surface.unregistered_session().map(|s| s.id).as_deref(),
            Some("abc"),
            "claude not registered yet: a quit now still saves the conversation"
        );
        surface.set_agent_session(None);
        assert!(
            surface.unregistered_session().is_some(),
            "an empty probe forgets nothing"
        );
        surface.set_agent_session(Some(AgentSession {
            pid: None,
            agent: Agent::Claude,
            id: "abc".into(),
            name: None,
            peer_name: None,
            config_dir: None,
        }));
        assert_eq!(
            surface.unregistered_session(),
            None,
            "the live session takes over"
        );
    }

    #[test]
    fn a_pending_command_waits_for_enter_unless_asked_to_run() {
        assert_eq!(injected_line("claude --resume abc", false), "claude --resume abc");
        assert_eq!(injected_line("claude --resume abc", true), "claude --resume abc\r");
    }
}
