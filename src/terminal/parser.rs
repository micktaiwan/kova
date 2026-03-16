use parking_lot::RwLock;
use std::os::fd::OwnedFd;
use std::sync::Arc;
use vte::{Params, Perform};

use super::{CursorShape, TerminalState};

/// Walk up from `path` to find `.git` and extract the branch name.
/// Supports both regular repos (`.git/HEAD`) and worktrees (`.git` file pointing to gitdir).
/// Returns `None` if not in a git repo.
pub fn resolve_git_branch(path: &str) -> Option<String> {
    let mut dir = std::path::PathBuf::from(path);
    loop {
        let git_path = dir.join(".git");
        if let Some(head_content) = read_git_head(&git_path) {
            return parse_head(&head_content);
        }
        if !dir.pop() {
            return None;
        }
    }
}

/// Read the HEAD content from a `.git` path (directory or worktree file).
fn read_git_head(git_path: &std::path::Path) -> Option<String> {
    if git_path.is_dir() {
        std::fs::read_to_string(git_path.join("HEAD")).ok()
    } else if git_path.is_file() {
        // Worktree: `.git` is a file containing "gitdir: <path>"
        let content = std::fs::read_to_string(git_path).ok()?;
        let gitdir = content.trim().strip_prefix("gitdir: ")?;
        std::fs::read_to_string(std::path::Path::new(gitdir).join("HEAD")).ok()
    } else {
        None
    }
}

/// Parse HEAD content into a branch name or short hash.
fn parse_head(content: &str) -> Option<String> {
    let content = content.trim();
    if let Some(ref_path) = content.strip_prefix("ref: refs/heads/") {
        Some(ref_path.to_string())
    } else {
        // Detached HEAD — show short hash
        Some(content.chars().take(7).collect())
    }
}

/// Buffered terminal operation. Accumulated during VTE parsing (no lock held),
/// then replayed in a single write lock acquisition.
enum TermOp {
    /// Text to display (grapheme clusters from print buffer)
    Print(String),
    // Control characters
    Backspace,
    Tab,
    Newline,
    CarriageReturn,
    Bell,
    // Cursor movement
    CursorUp(u16),
    CursorDown(u16),
    CursorForward(u16),
    CursorBackward(u16),
    SetCursorPos(u16, u16),
    /// Cursor Horizontal Absolute — needs cursor_y at replay time
    SetCursorCol(u16),
    /// Vertical Position Absolute — needs cursor_x at replay time
    SetCursorRow(u16),
    /// Cursor Next Line: down N + CR
    CursorNextLine(u16),
    /// Cursor Previous Line: up N + CR
    CursorPrevLine(u16),
    SaveCursor,
    RestoreCursor,
    // Erasing
    EraseInDisplay(u16),
    EraseInLine(u16),
    EraseChars(u16),
    // Lines
    InsertLines(u16),
    DeleteLines(u16),
    DeleteChars(u16),
    InsertChars(u16),
    // Scroll
    ScrollUp(u16),
    ScrollDown(u16),
    /// top, bottom (None = use term.rows as default)
    SetScrollRegion(u16, Option<u16>),
    // Modes
    /// DEC private mode: (mode_number, on/off)
    SetDecMode(u16, bool),
    /// SM/RM non-private mode: (mode_number, on/off)
    SetMode(u16, bool),
    // SGR
    SetSgr(Vec<u16>),
    // Cursor shape (DECSCUSR param)
    SetCursorShape(u16),
    // Screen
    ReverseIndex,
    FullReset,
    // Metadata
    SetTitle(String),
    SetOsc1Title(String),
    /// path, pre-resolved git_branch
    SetCwd(String, Option<String>),
    SetLastCommand(String),
    /// OSC 133;C — command started
    CommandStarted,
    /// OSC 133;D — command completed
    SetCommandCompleted,
    // Kitty keyboard protocol
    KittyKeyboardPush(u8),
    KittyKeyboardPop(u16),
    /// OSC 8 hyperlink — None clears, Some(url) sets
    SetHyperlink(Option<String>),
    // Responses — read state during replay, write to PTY after lock release
    CursorPositionReport,
    DeviceAttributes,
    ReportPrivateMode(u16),
    KittyKeyboardQuery,
}

pub struct VteHandler {
    terminal: Arc<RwLock<TerminalState>>,
    pty_writer: Arc<OwnedFd>,
    /// Buffer for consecutive print() calls. Flushed as a Print op
    /// before any non-print event.
    print_buf: String,
    /// Buffered operations — accumulated during parsing, replayed in apply_ops().
    ops: Vec<TermOp>,
}

impl VteHandler {
    pub fn new(terminal: Arc<RwLock<TerminalState>>, pty_writer: Arc<OwnedFd>) -> Self {
        VteHandler {
            terminal,
            pty_writer,
            print_buf: String::new(),
            ops: Vec::with_capacity(256),
        }
    }

    /// Flush the print buffer into a Print op.
    fn flush_print_buf(&mut self) {
        if !self.print_buf.is_empty() {
            let buf = std::mem::take(&mut self.print_buf);
            self.ops.push(TermOp::Print(buf));
        }
    }

    fn write_to_pty(&self, data: &[u8]) {
        let _ = rustix::io::write(&*self.pty_writer, data);
    }

    /// Take the write lock once, replay all buffered ops, release the lock,
    /// then write any PTY responses.
    pub fn apply_ops(&mut self) {
        self.flush_print_buf();
        if self.ops.is_empty() {
            return;
        }

        // Collect PTY responses to write after releasing the lock
        let mut pty_responses: Vec<Vec<u8>> = Vec::new();

        {
            let mut term = self.terminal.write();
            for op in self.ops.drain(..) {
                match op {
                    TermOp::Print(buf) => {
                        use unicode_segmentation::UnicodeSegmentation;
                        let char_count = buf.chars().count() as u64;
                        for cluster in buf.graphemes(true) {
                            term.put_cluster(cluster);
                        }
                        term.printable_chars.fetch_add(char_count, std::sync::atomic::Ordering::Relaxed);
                        super::pty::GLOBAL_PRINTABLE_CHARS.fetch_add(char_count, std::sync::atomic::Ordering::Relaxed);
                    }
                    TermOp::Backspace => term.backspace(),
                    TermOp::Tab => term.tab(),
                    TermOp::Newline => term.newline(),
                    TermOp::CarriageReturn => term.carriage_return(),
                    TermOp::Bell => {
                        term.bell.store(true, std::sync::atomic::Ordering::Relaxed);
                        term.dirty.store(true, std::sync::atomic::Ordering::Relaxed);
                    }
                    TermOp::CursorUp(n) => term.cursor_up(n),
                    TermOp::CursorDown(n) => term.cursor_down(n),
                    TermOp::CursorForward(n) => term.cursor_forward(n),
                    TermOp::CursorBackward(n) => term.cursor_backward(n),
                    TermOp::SetCursorPos(row, col) => term.set_cursor_pos(row, col),
                    TermOp::SetCursorCol(col) => {
                        let row = term.cursor_y;
                        term.set_cursor_pos(row, col);
                    }
                    TermOp::SetCursorRow(row) => {
                        let col = term.cursor_x;
                        term.set_cursor_pos(row, col);
                    }
                    TermOp::CursorNextLine(n) => {
                        term.cursor_down(n);
                        term.carriage_return();
                    }
                    TermOp::CursorPrevLine(n) => {
                        term.cursor_up(n);
                        term.carriage_return();
                    }
                    TermOp::SaveCursor => term.save_cursor(),
                    TermOp::RestoreCursor => term.restore_cursor(),
                    TermOp::EraseInDisplay(mode) => term.erase_in_display(mode),
                    TermOp::EraseInLine(mode) => term.erase_in_line(mode),
                    TermOp::EraseChars(n) => term.erase_chars(n),
                    TermOp::InsertLines(n) => term.insert_lines(n),
                    TermOp::DeleteLines(n) => term.delete_lines(n),
                    TermOp::DeleteChars(n) => term.delete_chars(n),
                    TermOp::InsertChars(n) => term.insert_chars(n),
                    TermOp::ScrollUp(n) => term.scroll_up_region(n),
                    TermOp::ScrollDown(n) => term.scroll_down_region(n),
                    TermOp::SetScrollRegion(top, bottom) => {
                        let bottom = bottom.unwrap_or(term.rows.saturating_sub(1));
                        term.set_scroll_region(top, bottom);
                    }
                    TermOp::SetDecMode(mode, on) => {
                        match mode {
                            1 => term.cursor_keys_application = on,
                            7 => term.auto_wrap = on,
                            12 => {} // Cursor blink — ignored
                            25 => {
                                term.cursor_visible = on;
                                term.dirty.store(true, std::sync::atomic::Ordering::Relaxed);
                            }
                            1049 => {
                                if on { term.enter_alt_screen(); } else { term.leave_alt_screen(); }
                            }
                            1004 => term.focus_reporting = on,
                            2004 => term.bracketed_paste = on,
                            2026 => {
                                if on {
                                    term.synchronized_output = true;
                                    term.sync_output_since = Some(std::time::Instant::now());
                                } else {
                                    term.synchronized_output = false;
                                    term.sync_output_since = None;
                                    term.dirty.store(true, std::sync::atomic::Ordering::Relaxed);
                                }
                            }
                            _ => log::debug!("unhandled DEC mode: {}", mode),
                        }
                    }
                    TermOp::SetMode(mode, on) => {
                        match mode {
                            4 => term.insert_mode = on,
                            _ => log::debug!("unhandled SM/RM mode: {}", mode),
                        }
                    }
                    TermOp::SetSgr(params) => term.set_sgr(&params),
                    TermOp::SetCursorShape(ps) => {
                        term.cursor_shape = match ps {
                            0 | 1 | 2 => CursorShape::Block,
                            3 | 4 => CursorShape::Underline,
                            5 | 6 => CursorShape::Bar,
                            _ => CursorShape::Block,
                        };
                        term.dirty.store(true, std::sync::atomic::Ordering::Relaxed);
                    }
                    TermOp::ReverseIndex => term.reverse_index(),
                    TermOp::FullReset => {
                        let cols = term.cols;
                        let rows = term.rows;
                        let scrollback_limit = term.scrollback_limit;
                        let fg = term.default_fg;
                        let bg = term.default_bg;
                        *term = TerminalState::new(cols, rows, scrollback_limit, fg, bg);
                    }
                    TermOp::SetTitle(title) => {
                        term.title = Some(title);
                        term.dirty.store(true, std::sync::atomic::Ordering::Relaxed);
                    }
                    TermOp::SetOsc1Title(title) => {
                        term.osc1_title = Some(title);
                        term.dirty.store(true, std::sync::atomic::Ordering::Relaxed);
                    }
                    TermOp::SetCwd(path, git_branch) => {
                        term.cwd = Some(path);
                        term.git_branch = git_branch;
                        term.dirty.store(true, std::sync::atomic::Ordering::Relaxed);
                    }
                    TermOp::SetLastCommand(cmd) => {
                        term.last_command = Some(cmd);
                    }
                    TermOp::SetHyperlink(url) => {
                        term.set_hyperlink(url);
                    }
                    TermOp::CommandStarted => {
                        term.command_completed.store(false, std::sync::atomic::Ordering::Relaxed);
                    }
                    TermOp::SetCommandCompleted => {
                        term.command_completed.store(true, std::sync::atomic::Ordering::Relaxed);
                        term.dirty.store(true, std::sync::atomic::Ordering::Relaxed);
                    }
                    TermOp::KittyKeyboardPush(flags) => {
                        term.kitty_keyboard_flags.push(flags);
                    }
                    TermOp::KittyKeyboardPop(n) => {
                        for _ in 0..n {
                            term.kitty_keyboard_flags.pop();
                        }
                    }
                    // --- Responses: read current state, buffer PTY write ---
                    TermOp::CursorPositionReport => {
                        let row = term.cursor_y + 1;
                        let col = term.cursor_x + 1;
                        pty_responses.push(format!("\x1b[{};{}R", row, col).into_bytes());
                    }
                    TermOp::DeviceAttributes => {
                        pty_responses.push(b"\x1b[?62;22c".to_vec());
                    }
                    TermOp::ReportPrivateMode(mode) => {
                        let value = match mode {
                            1 => if term.cursor_keys_application { 1 } else { 2 },
                            7 => if term.auto_wrap { 1 } else { 2 },
                            25 => if term.cursor_visible { 1 } else { 2 },
                            1004 => if term.focus_reporting { 1 } else { 2 },
                            1049 => if term.in_alt_screen { 1 } else { 2 },
                            2004 => if term.bracketed_paste { 1 } else { 2 },
                            2026 => if term.synchronized_output { 1 } else { 2 },
                            _ => 0,
                        };
                        pty_responses.push(format!("\x1b[?{};{}$y", mode, value).into_bytes());
                    }
                    TermOp::KittyKeyboardQuery => {
                        let flags = term.kitty_flags();
                        pty_responses.push(format!("\x1b[?{}u", flags).into_bytes());
                    }
                }
            }
        }
        // Write lock released — now send PTY responses without holding any lock
        for response in &pty_responses {
            self.write_to_pty(response);
        }
    }
}

impl Perform for VteHandler {
    fn print(&mut self, c: char) {
        self.print_buf.push(c);
    }

    fn execute(&mut self, byte: u8) {
        self.flush_print_buf();
        match byte {
            0x08 => self.ops.push(TermOp::Backspace),        // BS
            0x09 => self.ops.push(TermOp::Tab),              // HT
            0x0A | 0x0B | 0x0C => self.ops.push(TermOp::Newline), // LF, VT, FF
            0x0D => self.ops.push(TermOp::CarriageReturn),   // CR
            0x07 => {                                         // BEL
                log::trace!("BEL received");
                self.ops.push(TermOp::Bell);
            }
            _ => log::debug!("unhandled execute: byte=0x{:02X}", byte),
        }
    }

    fn hook(&mut self, params: &Params, intermediates: &[u8], _ignore: bool, action: char) {
        self.flush_print_buf();
        let params: Vec<u16> = params.iter().flat_map(|p| p.iter().map(|&v| v)).collect();
        log::debug!(
            "unhandled DCS hook: action={}, params={:?}, intermediates={:?}",
            action,
            params,
            intermediates
        );
    }
    fn put(&mut self, _byte: u8) { self.flush_print_buf(); }
    fn unhook(&mut self) { self.flush_print_buf(); }

    fn osc_dispatch(&mut self, params: &[&[u8]], _bell_terminated: bool) {
        self.flush_print_buf();
        if params.len() >= 2 {
            match params[0] {
                b"0" | b"2" => {
                    let title = String::from_utf8_lossy(params[1]).into_owned();
                    log::trace!("OSC title: {}", title);
                    self.ops.push(TermOp::SetTitle(title));
                }
                b"1" => {
                    let title = String::from_utf8_lossy(params[1]).into_owned();
                    log::trace!("OSC 1 sticky title: {}", title);
                    self.ops.push(TermOp::SetOsc1Title(title));
                }
                b"7" => {
                    // Current working directory: file://hostname/path
                    let uri = String::from_utf8_lossy(params[1]);
                    let path = if let Some(rest) = uri.strip_prefix("file://") {
                        rest.find('/').map(|i| rest[i..].to_string())
                    } else {
                        None
                    };
                    if let Some(path) = path {
                        // Filesystem I/O done during parsing — no lock held
                        let git_branch = resolve_git_branch(&path);
                        self.ops.push(TermOp::SetCwd(path, git_branch));
                    }
                }
                b"133" => {
                    let sub = params[1];
                    match sub.first() {
                        Some(b'C') => self.ops.push(TermOp::CommandStarted),
                        Some(b'D') => {
                            log::debug!("OSC 133;D command completed");
                            self.ops.push(TermOp::SetCommandCompleted);
                        }
                        _ => {}
                    }
                }
                b"8" => {
                    // OSC 8 ; params ; URI ST — hyperlinks
                    // params[1] = key-value params (ignored), params[2..] = URI
                    // vte splits on ';', so URIs containing ';' are split across params[2..]
                    if params.len() >= 3 {
                        // Join params[2..] with ';' to reconstruct URI
                        let uri_parts: Vec<&[u8]> = params[2..].to_vec();
                        let uri = if uri_parts.len() == 1 {
                            String::from_utf8_lossy(uri_parts[0]).into_owned()
                        } else {
                            uri_parts.iter()
                                .map(|p| String::from_utf8_lossy(p))
                                .collect::<Vec<_>>()
                                .join(";")
                        };
                        if uri.is_empty() {
                            log::trace!("OSC 8 hyperlink close");
                            self.ops.push(TermOp::SetHyperlink(None));
                        } else {
                            log::trace!("OSC 8 hyperlink: {}", uri);
                            self.ops.push(TermOp::SetHyperlink(Some(uri)));
                        }
                    }
                }
                b"7777" => {
                    let command = String::from_utf8_lossy(params[1]).into_owned();
                    if !command.is_empty() {
                        log::debug!("OSC 7777 last_command: {}", command);
                        self.ops.push(TermOp::SetLastCommand(command));
                    }
                }
                _ => {
                    let cmd = String::from_utf8_lossy(params[0]);
                    log::debug!("unhandled OSC: cmd={}, params_count={}", cmd, params.len());
                }
            }
        }
    }

    fn csi_dispatch(&mut self, params: &Params, intermediates: &[u8], _ignore: bool, action: char) {
        self.flush_print_buf();
        let params: Vec<u16> = params.iter().flat_map(|p| p.iter().map(|&v| v)).collect();

        match (action, intermediates) {
            ('A', []) => {
                let n = params.first().copied().unwrap_or(1).max(1);
                self.ops.push(TermOp::CursorUp(n));
            }
            ('B', []) => {
                let n = params.first().copied().unwrap_or(1).max(1);
                self.ops.push(TermOp::CursorDown(n));
            }
            ('C', []) => {
                let n = params.first().copied().unwrap_or(1).max(1);
                self.ops.push(TermOp::CursorForward(n));
            }
            ('D', []) => {
                let n = params.first().copied().unwrap_or(1).max(1);
                self.ops.push(TermOp::CursorBackward(n));
            }
            ('E', []) => {
                let n = params.first().copied().unwrap_or(1).max(1);
                self.ops.push(TermOp::CursorNextLine(n));
            }
            ('F', []) => {
                let n = params.first().copied().unwrap_or(1).max(1);
                self.ops.push(TermOp::CursorPrevLine(n));
            }
            ('G', []) => {
                let col = params.first().copied().unwrap_or(1).max(1) - 1;
                self.ops.push(TermOp::SetCursorCol(col));
            }
            ('H' | 'f', []) => {
                let row = params.first().copied().unwrap_or(1).max(1) - 1;
                let col = params.get(1).copied().unwrap_or(1).max(1) - 1;
                self.ops.push(TermOp::SetCursorPos(row, col));
            }
            ('J', []) => {
                let mode = params.first().copied().unwrap_or(0);
                self.ops.push(TermOp::EraseInDisplay(mode));
            }
            ('K', []) => {
                let mode = params.first().copied().unwrap_or(0);
                self.ops.push(TermOp::EraseInLine(mode));
            }
            ('L', []) => {
                let n = params.first().copied().unwrap_or(1).max(1);
                self.ops.push(TermOp::InsertLines(n));
            }
            ('M', []) => {
                let n = params.first().copied().unwrap_or(1).max(1);
                self.ops.push(TermOp::DeleteLines(n));
            }
            ('P', []) => {
                let n = params.first().copied().unwrap_or(1).max(1);
                self.ops.push(TermOp::DeleteChars(n));
            }
            ('S', []) => {
                let n = params.first().copied().unwrap_or(1).max(1);
                self.ops.push(TermOp::ScrollUp(n));
            }
            ('T', []) => {
                let n = params.first().copied().unwrap_or(1).max(1);
                self.ops.push(TermOp::ScrollDown(n));
            }
            ('X', []) => {
                let n = params.first().copied().unwrap_or(1).max(1);
                self.ops.push(TermOp::EraseChars(n));
            }
            ('d', []) => {
                let row = params.first().copied().unwrap_or(1).max(1) - 1;
                self.ops.push(TermOp::SetCursorRow(row));
            }
            ('m', []) => {
                if params.is_empty() {
                    self.ops.push(TermOp::SetSgr(vec![0]));
                } else {
                    self.ops.push(TermOp::SetSgr(params));
                }
            }
            ('r', []) => {
                let top = params.first().copied().unwrap_or(1).max(1) - 1;
                let bottom = params.get(1).map(|&b| b.max(1) - 1);
                self.ops.push(TermOp::SetScrollRegion(top, bottom));
            }
            ('s', []) => self.ops.push(TermOp::SaveCursor),
            ('u', []) => self.ops.push(TermOp::RestoreCursor),
            ('u', [b'>']) => {
                // Kitty keyboard protocol — push flags (or query if no params)
                if params.is_empty() {
                    self.ops.push(TermOp::KittyKeyboardQuery);
                } else {
                    self.ops.push(TermOp::KittyKeyboardPush(params[0] as u8));
                }
            }
            ('u', [b'<']) => {
                let n = params.first().copied().unwrap_or(1).max(1);
                self.ops.push(TermOp::KittyKeyboardPop(n));
            }
            ('u', [b'?']) => {
                self.ops.push(TermOp::KittyKeyboardQuery);
            }
            ('h', [b'?']) | ('l', [b'?']) => {
                let on = action == 'h';
                for &p in &params {
                    self.ops.push(TermOp::SetDecMode(p, on));
                }
            }
            ('h', []) | ('l', []) => {
                let on = action == 'h';
                for &p in &params {
                    self.ops.push(TermOp::SetMode(p, on));
                }
            }
            ('@', []) => {
                let n = params.first().copied().unwrap_or(1).max(1);
                self.ops.push(TermOp::InsertChars(n));
            }
            ('n', []) => {
                if params.first() == Some(&6) {
                    self.ops.push(TermOp::CursorPositionReport);
                }
            }
            ('c', []) | ('c', [b'?']) => {
                self.ops.push(TermOp::DeviceAttributes);
            }
            ('p', [b'?', b'$']) => {
                if let Some(&mode) = params.first() {
                    self.ops.push(TermOp::ReportPrivateMode(mode));
                }
            }
            ('q', [b' ']) => {
                let ps = params.first().copied().unwrap_or(0);
                self.ops.push(TermOp::SetCursorShape(ps));
            }
            _ => {
                log::debug!(
                    "unhandled CSI: action={}, params={:?}, intermediates={:?}",
                    action,
                    params,
                    intermediates
                );
            }
        }
    }

    fn esc_dispatch(&mut self, intermediates: &[u8], _ignore: bool, byte: u8) {
        self.flush_print_buf();
        match (byte, intermediates) {
            (b'M', []) => self.ops.push(TermOp::ReverseIndex),
            (b'7', []) => self.ops.push(TermOp::SaveCursor),
            (b'8', []) => self.ops.push(TermOp::RestoreCursor),
            (b'c', []) => self.ops.push(TermOp::FullReset),
            _ => {
                log::debug!(
                    "unhandled ESC: byte=0x{:02X}, intermediates={:?}",
                    byte,
                    intermediates
                );
            }
        }
    }
}

pub enum AnsiColor {
    Black,
    Red,
    Green,
    Yellow,
    Blue,
    Magenta,
    Cyan,
    White,
    BrightBlack,
    BrightRed,
    BrightGreen,
    BrightYellow,
    BrightBlue,
    BrightMagenta,
    BrightCyan,
    BrightWhite,
}

impl AnsiColor {
    pub fn from_index(idx: u8) -> Self {
        match idx {
            0 => Self::Black,
            1 => Self::Red,
            2 => Self::Green,
            3 => Self::Yellow,
            4 => Self::Blue,
            5 => Self::Magenta,
            6 => Self::Cyan,
            7 => Self::White,
            8 => Self::BrightBlack,
            9 => Self::BrightRed,
            10 => Self::BrightGreen,
            11 => Self::BrightYellow,
            12 => Self::BrightBlue,
            13 => Self::BrightMagenta,
            14 => Self::BrightCyan,
            15 => Self::BrightWhite,
            _ => Self::White,
        }
    }

    pub fn to_rgb(&self) -> [u8; 3] {
        match self {
            Self::Black => [0, 0, 0],
            Self::Red => [204, 51, 51],
            Self::Green => [51, 204, 51],
            Self::Yellow => [204, 204, 51],
            Self::Blue => [77, 77, 230],
            Self::Magenta => [204, 51, 204],
            Self::Cyan => [51, 204, 204],
            Self::White => [191, 191, 191],
            Self::BrightBlack => [102, 102, 102],
            Self::BrightRed => [255, 77, 77],
            Self::BrightGreen => [77, 255, 77],
            Self::BrightYellow => [255, 255, 77],
            Self::BrightBlue => [128, 128, 255],
            Self::BrightMagenta => [255, 77, 255],
            Self::BrightCyan => [77, 255, 255],
            Self::BrightWhite => [255, 255, 255],
        }
    }

    pub fn from_256(idx: u8) -> [u8; 3] {
        match idx {
            0..=15 => Self::from_index(idx).to_rgb(),
            16..=231 => {
                let idx = idx - 16;
                let r = (idx / 36) % 6;
                let g = (idx / 6) % 6;
                let b = idx % 6;
                [
                    if r == 0 { 0 } else { 55 + 40 * r },
                    if g == 0 { 0 } else { 55 + 40 * g },
                    if b == 0 { 0 } else { 55 + 40 * b },
                ]
            }
            232..=255 => {
                let v = 8 + 10 * (idx - 232);
                [v, v, v]
            }
        }
    }
}
