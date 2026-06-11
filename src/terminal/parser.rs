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
    /// REP — repeat last printed char
    RepeatLastChar(u16),
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
    /// CSI > c — Secondary Device Attributes
    SecondaryDeviceAttributes,
    /// CSI > q — XTVERSION (terminal name and version)
    XtVersion,
    /// Charset designation: (is_g1, is_dec_graphics)
    SetCharset(bool, bool),
    /// HTS — set tab stop at cursor
    SetTabStop,
    /// TBC — clear tab stop(s)
    ClearTabStops(u16),
    /// CBT — cursor backward n tab stops
    BackTab(u16),
    /// SO/SI: shift to G1 (true) or G0 (false)
    ShiftCharset(bool),
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
    /// A trailing incomplete grapheme was held back at the last chunk end.
    /// Bounds the holdback to one chunk: if the stream goes quiet, the next
    /// flush shows the fragment instead of withholding it forever.
    held_tail: bool,
}

impl VteHandler {
    pub fn new(terminal: Arc<RwLock<TerminalState>>, pty_writer: Arc<OwnedFd>) -> Self {
        VteHandler {
            terminal,
            pty_writer,
            print_buf: String::new(),
            ops: Vec::with_capacity(256),
            held_tail: false,
        }
    }

    /// Flush the print buffer into a Print op.
    fn flush_print_buf(&mut self) {
        self.held_tail = false;
        if !self.print_buf.is_empty() {
            let buf = std::mem::take(&mut self.print_buf);
            self.ops.push(TermOp::Print(buf));
        }
    }

    /// Flush at PTY chunk end, holding back a trailing grapheme that the next
    /// chunk may still extend (ends with ZWJ, or an unpaired regional
    /// indicator). Flushing such a fragment would render it as a separate
    /// cluster with a different width than the completed grapheme, desyncing
    /// the row layout from the application's model.
    fn flush_print_buf_chunk_end(&mut self) {
        if self.print_buf.is_empty() {
            return;
        }
        let hold = Self::incomplete_tail_len(&self.print_buf);
        if hold == 0 || self.held_tail {
            // Complete tail, or already held once: flush everything. One
            // chunk of grace is enough — withholding longer would hide the
            // app's final output when the stream simply stops.
            self.flush_print_buf();
        } else if hold < self.print_buf.len() {
            let tail = self.print_buf.split_off(self.print_buf.len() - hold);
            self.flush_print_buf();
            self.print_buf = tail;
            self.held_tail = true;
        } else {
            // Entire buffer is one incomplete grapheme — keep it all
            self.held_tail = true;
        }
    }

    /// Byte length of a trailing fragment that may be completed by upcoming
    /// output: a grapheme ending in ZWJ, or the unpaired half of a flag.
    fn incomplete_tail_len(buf: &str) -> usize {
        use unicode_segmentation::UnicodeSegmentation;
        if buf.ends_with('\u{200D}') {
            // ZWJ joins with whatever comes next — hold the whole last grapheme
            return match buf.grapheme_indices(true).last() {
                Some((idx, _)) => buf.len() - idx,
                None => buf.len(),
            };
        }
        // Trailing run of regional indicators: odd count = half a flag pair
        let mut count = 0usize;
        for ch in buf.chars().rev() {
            if ('\u{1F1E6}'..='\u{1F1FF}').contains(&ch) {
                count += 1;
            } else {
                break;
            }
        }
        if count % 2 == 1 {
            4 // a regional indicator is always 4 bytes in UTF-8
        } else {
            0
        }
    }

    fn write_to_pty(&self, data: &[u8]) {
        let _ = rustix::io::write(&*self.pty_writer, data);
    }

    /// Take the write lock once, replay all buffered ops, release the lock,
    /// then write any PTY responses.
    pub fn apply_ops(&mut self) {
        self.flush_print_buf_chunk_end();
        if self.ops.is_empty() {
            return;
        }

        // Collect PTY responses to write after releasing the lock
        let mut pty_responses: Vec<Vec<u8>> = Vec::new();

        {
            let mut term = self.terminal.write();
            let now_secs = std::time::SystemTime::now()
                .duration_since(std::time::SystemTime::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            term.last_activity_secs.store(now_secs, std::sync::atomic::Ordering::Relaxed);
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
                    TermOp::SetCursorCol(col) => term.set_cursor_col(col),
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
                            6 => term.set_origin_mode(on),
                            7 => term.set_auto_wrap(on),
                            12 => {} // Cursor blink — ignored
                            25 => {
                                term.cursor_visible = on;
                                term.dirty.store(true, std::sync::atomic::Ordering::Relaxed);
                            }
                            // Mouse tracking modes
                            1000 | 1002 | 1003 => {
                                if on {
                                    term.mouse_mode = mode;
                                } else if term.mouse_mode == mode {
                                    term.mouse_mode = 0;
                                }
                            }
                            // SGR extended mouse format
                            1006 => term.sgr_mouse = on,
                            1049 => {
                                if on { term.enter_alt_screen(); } else { term.leave_alt_screen(); }
                            }
                            // Legacy alt-screen variants (47/1047: switch
                            // without cursor save; 1048: cursor save only)
                            47 | 1047 => {
                                if on { term.enter_alt_screen(); } else { term.leave_alt_screen(); }
                            }
                            1048 => {
                                if on { term.save_cursor(); } else { term.restore_cursor(); }
                            }
                            1004 => term.focus_reporting = on,
                            2004 => term.bracketed_paste = on,
                            2026 => {
                                if on {
                                    term.synchronized_output = true;
                                    // Don't reset the timer on nested/repeated ?2026h
                                    // within an active window — otherwise the fallback
                                    // timeout never fires and the pane stays stale for
                                    // the whole burst. But if the previous window
                                    // already EXPIRED (a ?2026l was lost), this h is a
                                    // new burst: re-arm, or sync stays dead forever.
                                    let expired = term.sync_output_since
                                        .is_none_or(|s| s.elapsed().as_millis() >= 150);
                                    if expired {
                                        term.sync_output_since = Some(std::time::Instant::now());
                                    }
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
                    TermOp::RepeatLastChar(n) => term.repeat_last_char(n),
                    TermOp::ReverseIndex => term.reverse_index(),
                    TermOp::FullReset => {
                        let cols = term.cols;
                        let rows = term.rows;
                        let scrollback_limit = term.scrollback_limit;
                        let fg = term.default_fg;
                        let bg = term.default_bg;
                        let last_activity = term.last_activity_secs.clone();
                        *term = TerminalState::new(cols, rows, scrollback_limit, fg, bg);
                        term.last_activity_secs = last_activity;
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
                        log::debug!("OSC 133;C command started (terminal {})", term.terminal_id);
                        term.osc133_primed = true;
                        term.command_completed.store(false, std::sync::atomic::Ordering::Relaxed);
                        term.command_running.store(true, std::sync::atomic::Ordering::Relaxed);
                    }
                    TermOp::SetCommandCompleted => {
                        log::debug!("OSC 133;D command completed (terminal {})", term.terminal_id);
                        // The first D with no prior C is the shell's startup
                        // precmd — swallow it (no command actually completed).
                        // Later D-without-C (e.g. Claude Code's Stop hook)
                        // must still fire: the startup D already primed us.
                        if term.osc133_primed {
                            term.command_completed.store(true, std::sync::atomic::Ordering::Relaxed);
                            term.dirty.store(true, std::sync::atomic::Ordering::Relaxed);
                        } else {
                            term.osc133_primed = true;
                        }
                        term.command_running.store(false, std::sync::atomic::Ordering::Relaxed);
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
                    TermOp::SecondaryDeviceAttributes => {
                        // VT220-class, "firmware version" 100, no options
                        pty_responses.push(b"\x1b[>1;100;0c".to_vec());
                    }
                    TermOp::SetCharset(g1, dec) => term.set_charset(g1, dec),
                    TermOp::SetTabStop => term.set_tab_stop(),
                    TermOp::ClearTabStops(mode) => term.clear_tab_stops(mode),
                    TermOp::BackTab(n) => term.back_tab(n),
                    TermOp::ShiftCharset(g1) => term.shift_charset(g1),
                    TermOp::XtVersion => {
                        let v = env!("CARGO_PKG_VERSION");
                        pty_responses.push(format!("\x1bP>|Kova {}\x1b\\", v).into_bytes());
                    }
                    TermOp::ReportPrivateMode(mode) => {
                        let value = match mode {
                            1 => if term.cursor_keys_application { 1 } else { 2 },
                            7 => if term.auto_wrap { 1 } else { 2 },
                            25 => if term.cursor_visible { 1 } else { 2 },
                            1000 => if term.mouse_mode == 1000 { 1 } else { 2 },
                            1002 => if term.mouse_mode == 1002 { 1 } else { 2 },
                            1003 => if term.mouse_mode == 1003 { 1 } else { 2 },
                            1004 => if term.focus_reporting { 1 } else { 2 },
                            1006 => if term.sgr_mouse { 1 } else { 2 },
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
            // SO selects G1, SI selects G0 (DEC graphics for ncurses borders)
            0x0E => self.ops.push(TermOp::ShiftCharset(true)),
            0x0F => self.ops.push(TermOp::ShiftCharset(false)),
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
        // Cap OSC payloads to 4 KiB — a title or path longer than that is hostile,
        // and we'd rather drop it than allocate unbounded memory.
        const MAX_OSC_PAYLOAD: usize = 4096;
        if params.len() >= 2 {
            match params[0] {
                b"0" | b"2" => {
                    if params[1].len() > MAX_OSC_PAYLOAD { return; }
                    let title = String::from_utf8_lossy(params[1]).into_owned();
                    log::trace!("OSC title: {}", title);
                    self.ops.push(TermOp::SetTitle(title));
                }
                b"1" => {
                    if params[1].len() > MAX_OSC_PAYLOAD { return; }
                    let title = String::from_utf8_lossy(params[1]).into_owned();
                    log::trace!("OSC 1 sticky title: {}", title);
                    self.ops.push(TermOp::SetOsc1Title(title));
                }
                b"7" => {
                    // Current working directory: file://hostname/path
                    if params[1].len() > MAX_OSC_PAYLOAD { return; }
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
                    // C/D are logged with the terminal id when the op is applied.
                    match sub.first() {
                        Some(b'C') => self.ops.push(TermOp::CommandStarted),
                        Some(b'D') => self.ops.push(TermOp::SetCommandCompleted),
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
                    if params[1].len() > MAX_OSC_PAYLOAD { return; }
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
        // Parameter groups: colon subparameters (ITU SGR forms) arrive as one
        // group — only the 'm' arm needs them; everything else uses the flat list.
        let raw_groups: Vec<Vec<u16>> = params.iter().map(|p| p.to_vec()).collect();
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
                // Normalize parameter groups. Colon subparameters (ITU forms
                // like 38:2::R:G:B or 4:3) arrive as ONE group; flattening
                // them blindly corrupts the list (e.g. 4:0 flattened to 4;0
                // triggers a full SGR reset). Convert known colon forms to
                // their legacy layout and drop unsupported ones.
                let mut flat: Vec<u16> = Vec::new();
                for group in &raw_groups {
                    match group.as_slice() {
                        [] => flat.push(0),
                        [single] => flat.push(*single),
                        [head @ (38 | 48), 2, rest @ ..] if rest.len() >= 3 => {
                            // Colon form with optional colorspace id: the last
                            // three subparams are R, G, B
                            let rgb = &rest[rest.len() - 3..];
                            flat.extend_from_slice(&[*head, 2, rgb[0], rgb[1], rgb[2]]);
                        }
                        [head @ (38 | 48), 5, idx, ..] => {
                            flat.extend_from_slice(&[*head, 5, *idx]);
                        }
                        // Unsupported attribute with subparams (4:x underline
                        // styles, 58/59 underline color): drop the whole group
                        _ => {}
                    }
                }
                if raw_groups.is_empty() {
                    // CSI m with no parameters = SGR 0
                    flat.push(0);
                }
                if !flat.is_empty() {
                    self.ops.push(TermOp::SetSgr(flat));
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
            ('b', []) => {
                let n = params.first().copied().unwrap_or(1).max(1);
                self.ops.push(TermOp::RepeatLastChar(n));
            }
            ('Z', []) => {
                let n = params.first().copied().unwrap_or(1).max(1);
                self.ops.push(TermOp::BackTab(n));
            }
            ('g', []) => {
                let mode = params.first().copied().unwrap_or(0);
                self.ops.push(TermOp::ClearTabStops(mode));
            }
            ('c', []) | ('c', [b'?']) => {
                self.ops.push(TermOp::DeviceAttributes);
            }
            ('c', [b'>']) => {
                self.ops.push(TermOp::SecondaryDeviceAttributes);
            }
            ('q', [b'>']) => {
                self.ops.push(TermOp::XtVersion);
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
            (b'H', []) => self.ops.push(TermOp::SetTabStop),
            (b'7', []) => self.ops.push(TermOp::SaveCursor),
            (b'8', []) => self.ops.push(TermOp::RestoreCursor),
            (b'c', []) => self.ops.push(TermOp::FullReset),
            // Charset designation: '0' = DEC Special Graphics, anything else
            // (B, A, …) treated as ASCII. G2/G3 (*/+) are ignored.
            (b, [b'(']) => self.ops.push(TermOp::SetCharset(false, b == b'0')),
            (b, [b')']) => self.ops.push(TermOp::SetCharset(true, b == b'0')),
            (_, [b'*'] | [b'+']) => {}
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::terminal::{DEFAULT_BG, DEFAULT_FG};

    /// Feed raw bytes through the real vte parser into a TerminalState.
    fn drive(cols: u16, rows: u16, chunks: &[&[u8]]) -> Arc<RwLock<TerminalState>> {
        let term = Arc::new(RwLock::new(TerminalState::new(
            cols, rows, 100, DEFAULT_FG, DEFAULT_BG,
        )));
        let devnull = std::fs::OpenOptions::new()
            .write(true)
            .open("/dev/null")
            .unwrap();
        let writer: Arc<OwnedFd> = Arc::new(devnull.into());
        let mut parser = vte::Parser::new();
        let mut handler = VteHandler::new(term.clone(), writer);
        for chunk in chunks {
            parser.advance(&mut handler, chunk);
            handler.apply_ops();
        }
        term
    }

    fn cell(term: &Arc<RwLock<TerminalState>>, row: usize, col: usize) -> crate::terminal::Cell {
        term.read().visible_lines()[row][col].clone()
    }

    #[test]
    fn sgr_colon_underline_off_does_not_reset_colors() {
        // 4:0 (underline off, ITU form) must not be misread as SGR 0
        let t = drive(20, 5, &[b"\x1b[31mA\x1b[4:0mB"]);
        let red = AnsiColor::from_index(1).to_rgb();
        assert_eq!(cell(&t, 0, 0).fg, red);
        assert_eq!(cell(&t, 0, 1).fg, red, "4:0 must not reset the red foreground");
    }

    #[test]
    fn sgr_colon_truecolor_with_colorspace_id() {
        let t = drive(20, 5, &[b"\x1b[38:2::10:20:30mX"]);
        assert_eq!(cell(&t, 0, 0).fg, [10, 20, 30]);
    }

    #[test]
    fn sgr_legacy_semicolon_truecolor_still_works() {
        let t = drive(20, 5, &[b"\x1b[38;2;10;20;30mX"]);
        assert_eq!(cell(&t, 0, 0).fg, [10, 20, 30]);
    }

    #[test]
    fn ed3_clears_scrollback_not_screen() {
        let t = drive(10, 3, &[b"a\r\nb\r\nc\r\nd\r\ne\r\nf", b"\x1b[3J"]);
        let term = t.read();
        assert_eq!(term.scrollback_len(), 0, "3J must clear the scrollback");
        let lines = term.visible_lines();
        assert!(lines.iter().any(|l| l.iter().any(|c| c.c != ' ')), "3J must not clear the screen");
    }

    #[test]
    fn rep_via_csi_b() {
        let t = drive(20, 5, &[b"-\x1b[4b"]);
        let term = t.read();
        let text: String = term.visible_lines()[0].iter().map(|c| c.c).collect();
        assert!(text.starts_with("-----"), "CSI 4 b must repeat the dash, got {:?}", text);
    }

    #[test]
    fn split_combining_mark_across_chunks_merges() {
        let t = drive(20, 5, &[b"e", b"\xcc\x81x"]); // U+0301 then 'x' in chunk 2
        assert_eq!(cell(&t, 0, 0).cluster.as_deref(), Some("e\u{0301}"));
        assert_eq!(cell(&t, 0, 1).c, 'x');
    }

    #[test]
    fn zwj_holdback_completes_across_chunks() {
        // "👩" + ZWJ in chunk 1, "🚀" in chunk 2 → single cluster cell
        let t = drive(20, 5, &[
            "👩\u{200d}".as_bytes(),
            "🚀".as_bytes(),
        ]);
        assert_eq!(
            cell(&t, 0, 0).cluster.as_deref(),
            Some("👩\u{200d}🚀"),
            "ZWJ sequence split across chunks must render as one cluster"
        );
    }

    #[test]
    fn held_tail_flushes_when_stream_goes_quiet() {
        // Lone ZWJ-terminated grapheme, then an empty chunk (apply_ops with
        // no new data) — the held fragment must flush, not vanish forever.
        let t = drive(20, 5, &["A\u{200d}".as_bytes(), b""]);
        assert_eq!(cell(&t, 0, 0).c, 'A', "held grapheme must flush after one chunk of grace");
    }

    #[test]
    fn first_osc133_d_without_c_is_swallowed() {
        // Shell startup precmd emits a lone 133;D before any command —
        // it must not light the completion indicator.
        let t = drive(20, 5, &[b"\x1b]133;D\x07"]);
        assert!(!t.read().command_completed.load(std::sync::atomic::Ordering::Relaxed));
        // A later D without C (Claude Code Stop hook) must fire.
        let t = drive(20, 5, &[b"\x1b]133;D\x07", b"\x1b]133;D\x07"]);
        assert!(t.read().command_completed.load(std::sync::atomic::Ordering::Relaxed));
    }

    #[test]
    fn osc133_c_then_d_sets_completed() {
        let t = drive(20, 5, &[b"\x1b]133;C\x07\x1b]133;D\x07"]);
        assert!(t.read().command_completed.load(std::sync::atomic::Ordering::Relaxed));
        assert!(!t.read().command_running.load(std::sync::atomic::Ordering::Relaxed));
    }

    #[test]
    fn decom_makes_cup_region_relative() {
        // Region rows 2-4 (1-based: \x1b[2;4r is rows 2..4), origin mode on,
        // CUP 1;1 must land on the region top (row index 1), not screen top.
        let t = drive(10, 5, &[b"\x1b[2;4r\x1b[?6h\x1b[1;1HX"]);
        assert_eq!(cell(&t, 1, 0).c, 'X');
        assert_eq!(cell(&t, 0, 0).c, ' ');
    }

    #[test]
    fn cuu_stops_at_region_top_margin() {
        let t = drive(10, 5, &[b"\x1b[2;4r\x1b[3;1H\x1b[9AX"]);
        // cursor was inside region (row idx 2); CUU 9 must stop at region top (idx 1)
        assert_eq!(cell(&t, 1, 0).c, 'X');
    }

    /// Deterministic fuzz: pseudo-random byte streams (biased toward VT
    /// introducers) plus mid-stream resizes must never panic, and the
    /// terminal invariants must hold after every chunk.
    #[test]
    fn fuzz_parser_never_panics_and_invariants_hold() {
        // LCG — fixed seed, fully reproducible.
        let mut rng_state: u64 = 0x9E37_79B9_7F4A_7C15;
        let mut next = move |bound: u32| -> u32 {
            rng_state = rng_state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            ((rng_state >> 33) as u32) % bound
        };

        // Bytes that exercise the parser state machine, mixed with raw noise.
        const SPICE: &[&[u8]] = &[
            b"\x1b[", b"\x1b]", b"\x1bP", b"\x1b(0", b"\x1b)B", b"\x07", b"\x1b\\",
            b"\x1b[?", b"\x1b[>", b"\x1b[=", b";", b":", b"m", b"H", b"r", b"J", b"K",
            b"L", b"M", b"@", b"P", b"S", b"T", b"b", b"c", b"g", b"h", b"l", b"n",
            b"\x1b[99999999;99999999H", b"\x1b[38;2;999;999;999m", b"\x1b[38:5:300m",
            b"\x1b[?1049h", b"\x1b[?1049l", b"\x1b[?2004h", b"\x1b[3J", b"\x1b[2J",
            b"\x1b]8;;https://x\x07", b"\x1b]0;title\x07", b"\x1b]52;c;Zm9v\x07",
            b"\xf0\x9f\x91\xa9", b"\xe2\x80\x8d", b"\xf0\x9f", b"\xcc\x81", b"\xff\xfe",
            b"\r", b"\n", b"\t", b"\x08", b"\x0e", b"\x0f", b"\x1b7", b"\x1b8", b"\x1bM",
            b"\x1b[0;1;4;7;38;5;42;48;2;1;2;3m", b"\x1b[10000000b", b"\x1b[1;1;1;1;1;1r",
        ];

        for round in 0..64u32 {
            let cols = 2 + next(118) as u16;
            let rows = 1 + next(49) as u16;
            let term = Arc::new(RwLock::new(TerminalState::new(
                cols, rows, 50, DEFAULT_FG, DEFAULT_BG,
            )));
            let devnull = std::fs::OpenOptions::new().write(true).open("/dev/null").unwrap();
            let writer: Arc<OwnedFd> = Arc::new(devnull.into());
            let mut parser = vte::Parser::new();
            let mut handler = VteHandler::new(term.clone(), writer);

            for _chunk_idx in 0..8 {
                let mut chunk: Vec<u8> = Vec::with_capacity(512);
                for _ in 0..(64 + next(448)) {
                    if next(3) == 0 {
                        chunk.extend_from_slice(SPICE[next(SPICE.len() as u32) as usize]);
                    } else {
                        chunk.push(next(256) as u8);
                    }
                }
                parser.advance(&mut handler, &chunk);
                handler.apply_ops();

                // Occasionally resize mid-stream (reflow path under fire)
                if next(4) == 0 {
                    let nc = 2 + next(118) as u16;
                    let nr = 1 + next(49) as u16;
                    term.write().resize(nc, nr);
                }

                let t = term.read();
                assert!(t.cursor_x < t.cols, "round {}: cursor_x {} >= cols {}", round, t.cursor_x, t.cols);
                assert!(t.cursor_y < t.rows, "round {}: cursor_y {} >= rows {}", round, t.cursor_y, t.rows);
                assert_eq!(t.grid.len(), t.rows as usize, "round {}: grid height drifted", round);
                for (i, row) in t.grid.iter().enumerate() {
                    assert_eq!(row.cells.len(), t.cols as usize, "round {}: grid row {} width drifted", round, i);
                }
                assert!(t.scroll_offset >= 0 && t.scroll_offset as usize <= t.scrollback.len(),
                    "round {}: scroll_offset {} out of [0, {}]", round, t.scroll_offset, t.scrollback.len());
            }
        }
    }
}
