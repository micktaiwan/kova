use parking_lot::{RwLock, RwLockWriteGuard};
use std::os::fd::OwnedFd;
use std::sync::Arc;
use vte::{Params, Perform};

use super::{CursorShape, TerminalState};

/// Walk up from `path` to find `.git/HEAD` and extract the branch name.
/// Returns `None` if not in a git repo.
pub fn resolve_git_branch(path: &str) -> Option<String> {
    let mut dir = std::path::PathBuf::from(path);
    loop {
        let head = dir.join(".git/HEAD");
        if let Ok(content) = std::fs::read_to_string(&head) {
            let content = content.trim();
            if let Some(ref_path) = content.strip_prefix("ref: refs/heads/") {
                return Some(ref_path.to_string());
            }
            // Detached HEAD — show short hash
            return Some(content.chars().take(7).collect());
        }
        if !dir.pop() {
            return None;
        }
    }
}

pub struct VteHandler {
    terminal: Arc<RwLock<TerminalState>>,
    pty_writer: Arc<OwnedFd>,
    // SAFETY: The Arc<RwLock<TerminalState>> is held in `terminal` on the same struct,
    // guaranteeing the RwLock outlives this guard. We transmute the lifetime to 'static
    // to allow storing it alongside the Arc.
    pending_guard: Option<RwLockWriteGuard<'static, TerminalState>>,
}

impl VteHandler {
    pub fn new(terminal: Arc<RwLock<TerminalState>>, pty_writer: Arc<OwnedFd>) -> Self {
        VteHandler { terminal, pty_writer, pending_guard: None }
    }

    /// Lazily acquires the write lock on first call, reuses it on subsequent calls.
    fn term(&mut self) -> &mut TerminalState {
        if self.pending_guard.is_none() {
            let guard = self.terminal.write();
            // SAFETY: self.terminal (Arc) keeps the RwLock alive as long as self lives,
            // and pending_guard is always dropped before or with self.
            let guard: RwLockWriteGuard<'static, TerminalState> = unsafe { std::mem::transmute(guard) };
            self.pending_guard = Some(guard);
        }
        self.pending_guard.as_mut().unwrap()
    }

    /// Releases the write lock (if held). Called after parser.advance() and
    /// before PTY writes that should not hold the lock.
    pub fn release_guard(&mut self) {
        self.pending_guard = None;
    }

    fn write_to_pty(&self, data: &[u8]) {
        let _ = rustix::io::write(&*self.pty_writer, data);
    }
}

impl Perform for VteHandler {
    fn print(&mut self, c: char) {
        self.term().put_char(c);
    }

    fn execute(&mut self, byte: u8) {
        match byte {
            0x08 => self.term().backspace(),        // BS
            0x09 => self.term().tab(),              // HT
            0x0A | 0x0B | 0x0C => {                 // LF, VT, FF
                self.term().newline();
            }
            0x0D => self.term().carriage_return(),  // CR
            0x07 => {}                              // BEL - ignore
            _ => log::debug!("unhandled execute: byte=0x{:02X}", byte),
        }
    }

    fn hook(&mut self, params: &Params, intermediates: &[u8], _ignore: bool, action: char) {
        let params: Vec<u16> = params.iter().flat_map(|p| p.iter().map(|&v| v)).collect();
        log::debug!(
            "unhandled DCS hook: action={}, params={:?}, intermediates={:?}",
            action,
            params,
            intermediates
        );
    }
    fn put(&mut self, _byte: u8) {}
    fn unhook(&mut self) {}

    fn osc_dispatch(&mut self, params: &[&[u8]], _bell_terminated: bool) {
        // Handle OSC sequences (window title, etc.)
        if params.len() >= 2 {
            match params[0] {
                b"0" | b"2" => {
                    let title = String::from_utf8_lossy(params[1]).into_owned();
                    log::trace!("OSC title: {}", title);
                    let term = self.term();
                    term.title = Some(title);
                    term.dirty.store(true, std::sync::atomic::Ordering::Relaxed);
                }
                b"7" => {
                    // Current working directory: file://hostname/path
                    let uri = String::from_utf8_lossy(params[1]);
                    let path = if let Some(rest) = uri.strip_prefix("file://") {
                        // Skip hostname (everything up to the next '/')
                        rest.find('/').map(|i| rest[i..].to_string())
                    } else {
                        None
                    };
                    if let Some(path) = path {
                        // Release lock before filesystem I/O
                        self.release_guard();
                        let git_branch = resolve_git_branch(&path);
                        let term = self.term();
                        term.cwd = Some(path);
                        term.git_branch = git_branch;
                        term.dirty.store(true, std::sync::atomic::Ordering::Relaxed);
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
        let params: Vec<u16> = params.iter().flat_map(|p| p.iter().map(|&v| v)).collect();

        match (action, intermediates) {
            ('A', []) => {
                // Cursor Up
                let n = params.first().copied().unwrap_or(1).max(1);
                self.term().cursor_up(n);
            }
            ('B', []) => {
                // Cursor Down
                let n = params.first().copied().unwrap_or(1).max(1);
                self.term().cursor_down(n);
            }
            ('C', []) => {
                // Cursor Forward
                let n = params.first().copied().unwrap_or(1).max(1);
                self.term().cursor_forward(n);
            }
            ('D', []) => {
                // Cursor Backward
                let n = params.first().copied().unwrap_or(1).max(1);
                self.term().cursor_backward(n);
            }
            ('E', []) => {
                // Cursor Next Line
                let n = params.first().copied().unwrap_or(1).max(1);
                let term = self.term();
                term.cursor_down(n);
                term.carriage_return();
            }
            ('F', []) => {
                // Cursor Previous Line
                let n = params.first().copied().unwrap_or(1).max(1);
                let term = self.term();
                term.cursor_up(n);
                term.carriage_return();
            }
            ('G', []) => {
                // Cursor Horizontal Absolute
                let col = params.first().copied().unwrap_or(1).max(1) - 1;
                let term = self.term();
                let row = term.cursor_y;
                term.set_cursor_pos(row, col);
            }
            ('H' | 'f', []) => {
                // Cursor Position
                let row = params.first().copied().unwrap_or(1).max(1) - 1;
                let col = params.get(1).copied().unwrap_or(1).max(1) - 1;
                self.term().set_cursor_pos(row, col);
            }
            ('J', []) => {
                let mode = params.first().copied().unwrap_or(0);
                self.term().erase_in_display(mode);
            }
            ('K', []) => {
                let mode = params.first().copied().unwrap_or(0);
                self.term().erase_in_line(mode);
            }
            ('L', []) => {
                let n = params.first().copied().unwrap_or(1).max(1);
                self.term().insert_lines(n);
            }
            ('M', []) => {
                let n = params.first().copied().unwrap_or(1).max(1);
                self.term().delete_lines(n);
            }
            ('P', []) => {
                let n = params.first().copied().unwrap_or(1).max(1);
                self.term().delete_chars(n);
            }
            ('S', []) => {
                let n = params.first().copied().unwrap_or(1).max(1);
                self.term().scroll_up_region(n);
            }
            ('T', []) => {
                let n = params.first().copied().unwrap_or(1).max(1);
                self.term().scroll_down_region(n);
            }
            ('X', []) => {
                let n = params.first().copied().unwrap_or(1).max(1);
                self.term().erase_chars(n);
            }
            ('d', []) => {
                // Vertical Position Absolute
                let row = params.first().copied().unwrap_or(1).max(1) - 1;
                let term = self.term();
                let col = term.cursor_x;
                term.set_cursor_pos(row, col);
            }
            ('m', []) => {
                if params.is_empty() {
                    self.term().set_sgr(&[0]);
                } else {
                    self.term().set_sgr(&params);
                }
            }
            ('r', []) => {
                // Set scroll region
                let top = params.first().copied().unwrap_or(1).max(1) - 1;
                let term = self.term();
                let bottom = params.get(1).copied().unwrap_or(term.rows).max(1) - 1;
                term.set_scroll_region(top, bottom);
            }
            ('s', []) => self.term().save_cursor(),
            ('u', []) => self.term().restore_cursor(),
            ('u', [b'>']) => {
                // Kitty keyboard protocol query — respond with flags=0 (not supported)
                self.release_guard();
                self.write_to_pty(b"\x1b[?0u");
            }
            ('h', [b'?']) | ('l', [b'?']) => {
                // DEC Private Mode Set/Reset
                let term = self.term();
                for &p in &params {
                    match p {
                        1 => {
                            term.cursor_keys_application = action == 'h';
                        }
                        7 => {
                            term.auto_wrap = action == 'h';
                        }
                        12 => {} // Cursor blink
                        25 => {
                            term.cursor_visible = action == 'h';
                            term.dirty.store(true, std::sync::atomic::Ordering::Relaxed);
                        }
                        1049 => {
                            // Alternate screen buffer
                            if action == 'h' {
                                term.enter_alt_screen();
                            } else {
                                term.leave_alt_screen();
                            }
                        }
                        1004 => {
                            term.focus_reporting = action == 'h';
                        }
                        2004 => {
                            term.bracketed_paste = action == 'h';
                        }
                        2026 => {
                            if action == 'h' {
                                term.synchronized_output = true;
                                term.sync_output_since = Some(std::time::Instant::now());
                            } else {
                                term.synchronized_output = false;
                                term.sync_output_since = None;
                                term.dirty.store(true, std::sync::atomic::Ordering::Relaxed);
                            }
                        }
                        _ => log::debug!("unhandled DEC mode: {}", p),
                    }
                }
            }
            ('h', []) | ('l', []) => {
                // SM/RM — Set/Reset Mode (non-private)
                let term = self.term();
                for &p in &params {
                    match p {
                        4 => {
                            term.insert_mode = action == 'h';
                        }
                        _ => log::debug!("unhandled SM/RM mode: {}", p),
                    }
                }
            }
            ('@', []) => {
                // ICH — Insert Characters
                let n = params.first().copied().unwrap_or(1).max(1);
                self.term().insert_chars(n);
            }
            ('n', []) => {
                // Device Status Report
                if params.first() == Some(&6) {
                    // CPR - Cursor Position Report (1-based)
                    let term = self.term();
                    let row = term.cursor_y + 1;
                    let col = term.cursor_x + 1;
                    self.release_guard();
                    let response = format!("\x1b[{};{}R", row, col);
                    self.write_to_pty(response.as_bytes());
                }
            }
            ('c', []) | ('c', [b'?']) => {
                // DA1 — identify as VT220-compatible
                self.release_guard();
                self.write_to_pty(b"\x1b[?62;22c");
            }
            ('p', [b'?', b'$']) => {
                // DECRPM — Report Private Mode
                if let Some(&mode) = params.first() {
                    let term = self.term();
                    // 1 = set, 2 = reset, 0 = not recognized
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
                    self.release_guard();
                    let response = format!("\x1b[?{};{}$y", mode, value);
                    self.write_to_pty(response.as_bytes());
                }
            }
            ('q', [b' ']) => {
                // DECSCUSR — Set Cursor Style
                let ps = params.first().copied().unwrap_or(0);
                let term = self.term();
                term.cursor_shape = match ps {
                    0 | 1 | 2 => CursorShape::Block,
                    3 | 4 => CursorShape::Underline,
                    5 | 6 => CursorShape::Bar,
                    _ => CursorShape::Block,
                };
                term.dirty.store(true, std::sync::atomic::Ordering::Relaxed);
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
        match (byte, intermediates) {
            (b'M', []) => {
                // Reverse Index
                self.term().reverse_index();
            }
            (b'7', []) => self.term().save_cursor(),
            (b'8', []) => self.term().restore_cursor(),
            (b'c', []) => {
                // Full reset
                let term = self.term();
                let cols = term.cols;
                let rows = term.rows;
                let scrollback_limit = term.scrollback_limit;
                let fg = term.default_fg;
                let bg = term.default_bg;
                *term = TerminalState::new(cols, rows, scrollback_limit, fg, bg);
            }
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

    pub fn to_rgb(&self) -> [f32; 3] {
        match self {
            Self::Black => [0.0, 0.0, 0.0],
            Self::Red => [0.8, 0.2, 0.2],
            Self::Green => [0.2, 0.8, 0.2],
            Self::Yellow => [0.8, 0.8, 0.2],
            Self::Blue => [0.3, 0.3, 0.9],
            Self::Magenta => [0.8, 0.2, 0.8],
            Self::Cyan => [0.2, 0.8, 0.8],
            Self::White => [0.75, 0.75, 0.75],
            Self::BrightBlack => [0.4, 0.4, 0.4],
            Self::BrightRed => [1.0, 0.3, 0.3],
            Self::BrightGreen => [0.3, 1.0, 0.3],
            Self::BrightYellow => [1.0, 1.0, 0.3],
            Self::BrightBlue => [0.5, 0.5, 1.0],
            Self::BrightMagenta => [1.0, 0.3, 1.0],
            Self::BrightCyan => [0.3, 1.0, 1.0],
            Self::BrightWhite => [1.0, 1.0, 1.0],
        }
    }

    pub fn from_256(idx: u8) -> [f32; 3] {
        match idx {
            0..=15 => Self::from_index(idx).to_rgb(),
            16..=231 => {
                let idx = idx - 16;
                let r = (idx / 36) % 6;
                let g = (idx / 6) % 6;
                let b = idx % 6;
                [
                    if r == 0 { 0.0 } else { (55 + 40 * r) as f32 / 255.0 },
                    if g == 0 { 0.0 } else { (55 + 40 * g) as f32 / 255.0 },
                    if b == 0 { 0.0 } else { (55 + 40 * b) as f32 / 255.0 },
                ]
            }
            232..=255 => {
                let v = (8 + 10 * (idx - 232)) as f32 / 255.0;
                [v, v, v]
            }
        }
    }
}
