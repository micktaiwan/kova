pub mod parser;
pub mod pty;

use std::borrow::Cow;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::time::Instant;

use unicode_width::UnicodeWidthChar;

use crate::terminal::parser::AnsiColor;

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum CursorShape {
    Block,
    Underline,
    Bar,
}

pub const DEFAULT_FG: [u8; 3] = [255, 255, 255];
pub const DEFAULT_BG: [u8; 3] = [26, 26, 31];

/// What part of a pane's buffer to include in a text dump.
#[derive(Clone, Copy, Debug)]
pub enum DumpMode {
    /// Current visible grid only.
    Visible,
    /// Scrollback only (no grid).
    Scrollback,
    /// Scrollback followed by the current grid.
    All,
}

/// Result of a text dump: the rendered text plus pane geometry / cursor at dump time.
pub struct DumpResult {
    pub text: String,
    pub cols: u16,
    pub rows: u16,
    pub cursor_row: u16,
    pub cursor_col: u16,
}

/// Convert [u8; 3] color to [f32; 3] for GPU rendering.
/// Only called at render time — cells store compact [u8; 3] to save RAM.
/// (Cell is 48→32 bytes, saving ~300MB+ with 10k scrollback × multiple panes)
#[inline]
pub fn color_to_f32(c: [u8; 3]) -> [f32; 3] {
    [c[0] as f32 / 255.0, c[1] as f32 / 255.0, c[2] as f32 / 255.0]
}

/// Convert [f32; 3] color (from config/renderer) to compact [u8; 3] for cell storage.
#[inline]
pub fn color_to_u8(c: [f32; 3]) -> [u8; 3] {
    [(c[0] * 255.0) as u8, (c[1] * 255.0) as u8, (c[2] * 255.0) as u8]
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GridPos {
    /// Absolute line: 0 = first scrollback line, scrollback.len() = first grid line
    pub line: usize,
    pub col: u16,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SelectionMode {
    Normal,
    Word,
    Line,
}

#[derive(Clone, Debug)]
pub struct Selection {
    pub anchor: GridPos,
    pub end: GridPos,
    pub mode: SelectionMode,
}

/// DECSC/DECRC state. Per xterm, save/restore covers more than the position:
/// SGR attributes, autowrap, origin mode and the deferred-wrap flag. Apps
/// (zsh prompts, TUIs) rely on DECRC bringing their attributes back.
#[derive(Clone, Copy, Debug)]
struct SavedCursor {
    x: u16,
    y: u16,
    fg: [u8; 3],
    bg: [u8; 3],
    bold: bool,
    dim: bool,
    reversed: bool,
    pending_wrap: bool,
    origin_mode: bool,
    auto_wrap: bool,
    // Per xterm, DECSC also saves the charset designations and shift state
    g0_dec_graphics: bool,
    g1_dec_graphics: bool,
    active_charset_g1: bool,
}

/// Terminal cell — kept compact to minimize scrollback RAM usage.
/// Each field is chosen for size: [u8; 3] colors instead of [f32; 3] saves
/// 18 bytes/cell (48→32 bytes), which is ~300MB+ across 10k scrollback × multiple panes.
/// Colors are converted to [f32; 3] only at render time via color_to_f32().
/// DO NOT change fg/bg back to [f32; 3] without measuring RAM impact.
#[derive(Clone, Debug)]
pub struct Cell {
    pub c: char,
    /// Multi-codepoint grapheme cluster (e.g. flags, ZWJ sequences, skin tones).
    /// None for single-codepoint characters (the common case).
    pub cluster: Option<Box<str>>,
    pub fg: [u8; 3],
    pub bg: [u8; 3],
    /// OSC 8 hyperlink index into TerminalState::hyperlinks (0 = no link).
    pub hyperlink_id: u16,
}

impl Cell {
    pub fn is_blank(&self) -> bool {
        self.c == ' ' || self.c == '\0'
    }
}

impl Default for Cell {
    fn default() -> Self {
        Cell {
            c: ' ',
            cluster: None,
            fg: DEFAULT_FG,
            bg: DEFAULT_BG,
            hyperlink_id: 0,
        }
    }
}

#[derive(Clone, Debug)]
pub struct Row {
    pub cells: Vec<Cell>,
    pub wrapped: bool,
}

impl Row {
    pub fn new(cols: usize, blank: &Cell) -> Self {
        Row {
            cells: vec![blank.clone(); cols],
            wrapped: false,
        }
    }

    fn trim_trailing_blanks(&mut self, default_fg: [u8; 3], default_bg: [u8; 3]) {
        // '\0' wide-char continuations count as content: trimming one would
        // orphan its base (a wide glyph with no second column).
        let last = self.cells.iter().rposition(|c| {
            c.c != ' '
                || c.cluster.is_some()
                || c.fg != default_fg
                || c.bg != default_bg
        });
        match last {
            Some(idx) => {
                self.cells.truncate(idx + 1);
                self.cells.shrink_to_fit();
            }
            None => {
                self.cells.clear();
                self.cells.shrink_to_fit();
            }
        }
    }
}

static TERMINAL_ID_COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(1);

pub struct TerminalState {
    pub terminal_id: u32,
    pub cols: u16,
    pub rows: u16,
    grid: Vec<Row>,
    scrollback: VecDeque<Row>,
    pub scrollback_limit: usize,
    pub cursor_x: u16,
    pub cursor_y: u16,
    /// Deferred autowrap (xterm "last column flag"): set when a printed char
    /// fills the last column. The cursor visually stays on the last column;
    /// the wrap happens just before the NEXT printable char. Cleared by any
    /// explicit cursor movement (CR, LF, BS, CUP, CUU/CUD/CUF/CUB, …).
    /// Without this, cursor_x would sit at `cols` (out of grid) and cursor
    /// movements would wrap text one row below where the app drew it —
    /// shifting the whole screen on TUI diff-renderers (persistent glitches).
    pending_wrap: bool,
    scroll_offset: i32,
    user_scrolled: bool,
    // Config colors used as defaults
    pub default_fg: [u8; 3],
    pub default_bg: [u8; 3],
    blank: Cell,
    // SGR state
    current_fg: [u8; 3],
    current_bg: [u8; 3],
    reversed: bool,
    bold: bool,
    dim: bool,
    // Saved cursor
    saved_cursor: Option<SavedCursor>,
    // Scroll region
    scroll_top: u16,
    scroll_bottom: u16,
    // Origin mode
    origin_mode: bool,
    // Cursor visibility (DECTCEM)
    pub cursor_visible: bool,
    // Cursor shape (DECSCUSR)
    pub cursor_shape: CursorShape,
    // Incremented on every cursor move to reset blink phase
    pub cursor_move_epoch: AtomicU32,
    // Dirty flag for render optimization
    pub dirty: AtomicBool,
    // Alternate screen buffer
    alt_grid: Option<Vec<Row>>,
    alt_cursor: Option<(u16, u16)>,
    pub in_alt_screen: bool,
    // Focus reporting (DEC mode 1004)
    pub focus_reporting: bool,
    // Current working directory (set via OSC 7)
    pub cwd: Option<String>,
    // Git branch (resolved when CWD changes)
    pub git_branch: Option<String>,
    // Window title (set via OSC 0/2)
    pub title: Option<String>,
    // Sticky title (set via OSC 1) — propagated to pane.custom_title
    pub osc1_title: Option<String>,
    // Text selection
    pub selection: Option<Selection>,
    // Synchronized output (DEC mode 2026)
    pub synchronized_output: bool,
    pub sync_output_since: Option<Instant>,
    // Bracketed paste mode (DEC mode 2004)
    pub bracketed_paste: bool,
    // Cursor keys mode (DECCKM, mode 1) — true = application mode (ESC O), false = normal (CSI)
    pub cursor_keys_application: bool,
    // Auto-wrap mode (DECAWM, mode 7) — true = wrap at margin
    pub auto_wrap: bool,
    // Insert mode (SM 4) — true = insert, false = replace
    pub insert_mode: bool,
    // Bell received (BEL 0x07) — used for tab attention indicator
    pub bell: AtomicBool,
    // Command completed (OSC 133;D) — used for pane/tab completion indicator
    pub command_completed: AtomicBool,
    // Command running (between OSC 133;C and 133;D) — tab running indicator
    pub command_running: AtomicBool,
    // Last command executed (set via OSC 7777 from shell integration)
    pub last_command: Option<String>,
    // Mouse reporting modes
    // 0 = off, 1000 = button events, 1002 = button+motion, 1003 = all motion
    pub mouse_mode: u16,
    // SGR extended mouse format (mode 1006) — uses CSI < ... M/m instead of raw bytes
    pub sgr_mouse: bool,
    // Kitty keyboard protocol — stack of pushed flag sets
    pub kitty_keyboard_flags: Vec<u8>,
    // Printable character counter (displayed in status bars)
    pub printable_chars: AtomicU64,
    // Unix timestamp (seconds) of the last input or output activity on this pane.
    // Shared with Pty so writes update it without locking the terminal.
    pub last_activity_secs: std::sync::Arc<AtomicU64>,
    // Last printed graphic char (for REP, CSI Ps b)
    last_printed: Option<char>,
    // Charset designation (ESC ( 0 / ESC ) 0): true = DEC Special Graphics.
    // ncurses apps draw their borders through this (terminfo acsc/smacs).
    g0_dec_graphics: bool,
    g1_dec_graphics: bool,
    // Active charset slot (SI -> G0, SO -> G1)
    active_charset_g1: bool,
    // Tab stops, one per column (HTS/TBC; default every 8)
    tab_stops: Vec<bool>,
    // OSC 8 hyperlink state
    current_hyperlink: u16,
    /// Hyperlink URL table, indexed by hyperlink_id (slot 0 unused).
    hyperlinks: Vec<String>,
}

/// A single line matching a filter query.
#[derive(Clone, Debug)]
pub struct FilterMatch {
    pub abs_line: usize,
    pub text: String,
}

impl TerminalState {
    pub fn new(cols: u16, rows: u16, scrollback_limit: usize, fg: [u8; 3], bg: [u8; 3]) -> Self {
        let blank = Cell { c: ' ', cluster: None, fg, bg, hyperlink_id: 0 };
        let grid = (0..rows as usize).map(|_| Row::new(cols as usize, &blank)).collect();
        let terminal_id = TERMINAL_ID_COUNTER.fetch_add(1, Ordering::Relaxed);
        log::info!("TerminalState::new id={} cols={} rows={}", terminal_id, cols, rows);
        TerminalState {
            terminal_id,
            cols,
            rows,
            grid,
            scrollback: VecDeque::new(),
            scrollback_limit,
            cursor_x: 0,
            cursor_y: 0,
            pending_wrap: false,
            scroll_offset: 0,
            user_scrolled: false,
            default_fg: fg,
            default_bg: bg,
            blank,
            current_fg: fg,
            current_bg: bg,
            reversed: false,
            bold: false,
            dim: false,
            saved_cursor: None,
            scroll_top: 0,
            scroll_bottom: rows.saturating_sub(1),
            origin_mode: false,
            cursor_visible: true,
            cursor_shape: CursorShape::Block,
            cursor_move_epoch: AtomicU32::new(0),
            dirty: AtomicBool::new(true),
            alt_grid: None,
            alt_cursor: None,
            in_alt_screen: false,
            focus_reporting: false,
            cwd: None,
            git_branch: None,
            title: None,
            osc1_title: None,
            selection: None,
            synchronized_output: false,
            sync_output_since: None,
            bracketed_paste: false,
            cursor_keys_application: false,
            auto_wrap: true,
            insert_mode: false,
            bell: AtomicBool::new(false),
            command_completed: AtomicBool::new(false),
            command_running: AtomicBool::new(false),
            last_command: None,
            last_printed: None,
            g0_dec_graphics: false,
            g1_dec_graphics: false,
            active_charset_g1: false,
            tab_stops: (0..cols as usize).map(|i| i % 8 == 0).collect(),
            mouse_mode: 0,
            sgr_mouse: false,
            kitty_keyboard_flags: Vec::new(),
            printable_chars: AtomicU64::new(0),
            last_activity_secs: std::sync::Arc::new(AtomicU64::new(0)),
            current_hyperlink: 0,
            hyperlinks: vec![String::new()], // slot 0 = no hyperlink
        }
    }

    pub fn kitty_flags(&self) -> u8 {
        self.kitty_keyboard_flags.last().copied().unwrap_or(0)
    }

    /// Set or clear the active OSC 8 hyperlink.
    pub fn set_hyperlink(&mut self, url: Option<String>) {
        match url {
            None => self.current_hyperlink = 0,
            Some(u) => {
                // Reuse existing slot if URL already known
                if let Some(id) = self.hyperlinks.iter().position(|s| s == &u) {
                    self.current_hyperlink = id as u16;
                } else if self.hyperlinks.len() < u16::MAX as usize {
                    self.current_hyperlink = self.hyperlinks.len() as u16;
                    self.hyperlinks.push(u);
                }
            }
        }
    }

    /// Look up a hyperlink URL by ID.
    pub fn hyperlink_url(&self, id: u16) -> Option<&str> {
        if id == 0 { return None; }
        self.hyperlinks.get(id as usize).map(|s| s.as_str())
    }

    /// Render `row` as text into `out`. Wide-char continuations (`c == '\0'`)
    /// are skipped; multi-codepoint clusters are emitted via their cluster string.
    /// Trailing spaces on the line are trimmed unless the row is wrapped (in which
    /// case the next row is the logical continuation, so we don't trim or break).
    fn render_row(row: &Row, out: &mut String) {
        let line_start = out.len();
        for cell in &row.cells {
            if cell.c == '\0' {
                continue; // wide-char continuation column
            }
            if let Some(cluster) = &cell.cluster {
                out.push_str(cluster);
            } else {
                out.push(cell.c);
            }
        }
        if !row.wrapped {
            // Trim grid-padding spaces, then break the line.
            let trimmed_len = out[line_start..].trim_end().len();
            out.truncate(line_start + trimmed_len);
            out.push('\n');
        }
    }

    /// Build the rendered text representation of the requested rows.
    ///
    /// Per-line trailing spaces are always trimmed (grid is always padded to `cols`).
    /// `trim_trailing_blank_lines` controls whether fully-empty trailing lines at the
    /// end of the output are dropped.
    fn build_text(&self, mode: DumpMode, trim_trailing_blank_lines: bool) -> String {
        let mut text = String::new();
        let push_rows = |rows: &mut dyn Iterator<Item = &Row>, out: &mut String| {
            for row in rows {
                Self::render_row(row, out);
            }
        };

        match mode {
            DumpMode::Visible => push_rows(&mut self.grid.iter(), &mut text),
            DumpMode::Scrollback => push_rows(&mut self.scrollback.iter(), &mut text),
            DumpMode::All => {
                push_rows(&mut self.scrollback.iter(), &mut text);
                push_rows(&mut self.grid.iter(), &mut text);
            }
        }

        if trim_trailing_blank_lines {
            let trimmed_len = text.trim_end().len();
            text.truncate(trimmed_len);
            if !text.is_empty() {
                text.push('\n');
            }
        }

        text
    }

    /// Build a text dump of this pane's content. See `build_text` for trim semantics.
    pub fn dump_text(&self, mode: DumpMode, trim_trailing_blank_lines: bool) -> DumpResult {
        let text = self.build_text(mode, trim_trailing_blank_lines);
        DumpResult {
            text,
            cols: self.cols,
            rows: self.rows,
            cursor_row: self.cursor_y,
            cursor_col: self.cursor_x,
        }
    }

    /// Return `(chars, bytes)` that `dump_text` would produce with the same args.
    /// Builds the text and measures it — same code path as `dump_text` to keep the
    /// totals exact. The temporary string is dropped immediately.
    pub fn measure_text(&self, mode: DumpMode, trim_trailing_blank_lines: bool) -> (usize, usize) {
        let text = self.build_text(mode, trim_trailing_blank_lines);
        (text.chars().count(), text.len())
    }

    pub fn visible_lines(&self) -> Vec<Cow<'_, [Cell]>> {
        if self.scroll_offset == 0 {
            self.grid.iter().map(|r| Cow::Borrowed(r.cells.as_slice())).collect()
        } else {
            let sb_len = self.scrollback.len() as i32;
            let offset = self.scroll_offset.min(sb_len);
            let sb_start = (sb_len - offset) as usize;
            let grid_end = (self.rows as i32 - offset).max(0) as usize;

            let mut lines: Vec<Cow<'_, [Cell]>> = Vec::with_capacity(self.rows as usize);
            for i in sb_start..self.scrollback.len() {
                lines.push(Cow::Borrowed(self.scrollback[i].cells.as_slice()));
            }
            for i in 0..grid_end.min(self.grid.len()) {
                lines.push(Cow::Borrowed(self.grid[i].cells.as_slice()));
            }
            lines.truncate(self.rows as usize);
            lines
        }
    }

    pub fn scroll(&mut self, lines: i32) {
        if self.in_alt_screen {
            return; // No scrollback in alt screen
        }
        let max_offset = self.scrollback.len() as i32;
        let old_offset = self.scroll_offset;
        self.scroll_offset = (self.scroll_offset + lines).clamp(0, max_offset);
        if self.scroll_offset == max_offset && old_offset == max_offset {
            log::debug!("scroll: at max offset {}/{}, scrollback_limit={}", self.scroll_offset, max_offset, self.scrollback_limit);
        }
        // Log scroll state for cross-terminal debugging
        if self.scroll_offset > 0 && old_offset == 0 {
            // Just started scrolling — log terminal identity + first scrollback line
            let first_sb_text: String = self.scrollback.front()
                .map(|r| r.cells.iter().take(60).map(|c| c.c).collect())
                .unwrap_or_default();
            log::info!("SCROLL-START term_id={} sb_len={} offset={} cwd={:?} first_sb=\"{}\"",
                self.terminal_id, self.scrollback.len(), self.scroll_offset,
                self.cwd, first_sb_text);
        }
        self.user_scrolled = self.scroll_offset > 0;
        self.cursor_moved();
    }

    /// Designate G0/G1 (ESC ( 0, ESC ( B, ESC ) 0, ESC ) B).
    pub fn set_charset(&mut self, g1: bool, dec_graphics: bool) {
        if g1 {
            self.g1_dec_graphics = dec_graphics;
        } else {
            self.g0_dec_graphics = dec_graphics;
        }
    }

    /// SO (0x0E) shifts to G1, SI (0x0F) back to G0.
    pub fn shift_charset(&mut self, g1: bool) {
        self.active_charset_g1 = g1;
    }

    /// Map a char through the DEC Special Graphics set when active.
    /// This is how ncurses draws box borders (terminfo acsc): without the
    /// mapping they render as letters (qqq/xxx) instead of lines.
    fn map_charset(&self, c: char) -> char {
        let dec = if self.active_charset_g1 { self.g1_dec_graphics } else { self.g0_dec_graphics };
        if !dec {
            return c;
        }
        match c {
            '`' => '◆',
            'a' => '▒',
            'b' => '␉',
            'c' => '␌',
            'd' => '␍',
            'e' => '␊',
            'f' => '°',
            'g' => '±',
            'h' => '␤',
            'i' => '␋',
            'j' => '┘',
            'k' => '┐',
            'l' => '┌',
            'm' => '└',
            'n' => '┼',
            'o' => '⎺',
            'p' => '⎻',
            'q' => '─',
            'r' => '⎼',
            's' => '⎽',
            't' => '├',
            'u' => '┤',
            'v' => '┴',
            'w' => '┬',
            'x' => '│',
            'y' => '≤',
            'z' => '≥',
            '{' => 'π',
            '|' => '≠',
            '}' => '£',
            '~' => '·',
            _ => c,
        }
    }

    /// Colors actually written into cells: bold/dim applied to the logical
    /// foreground, then fg/bg swapped if reverse video (SGR 7) is active.
    fn effective_colors(&self) -> ([u8; 3], [u8; 3]) {
        let mut fg = self.current_fg;
        if self.dim {
            fg = [fg[0] / 2, fg[1] / 2, fg[2] / 2];
        }
        if self.bold {
            fg = [
                (fg[0] as u16 * 13 / 10).min(255) as u8,
                (fg[1] as u16 * 13 / 10).min(255) as u8,
                (fg[2] as u16 * 13 / 10).min(255) as u8,
            ];
        }
        if self.reversed {
            (self.current_bg, fg)
        } else {
            (fg, self.current_bg)
        }
    }

    pub fn put_char(&mut self, c: char) {
        let c = self.map_charset(c);
        if c >= '\u{2500}' && c <= '\u{257F}' {
            log::trace!("put_char box-drawing: '{}' U+{:04X} at ({}, {})", c, c as u32, self.cursor_x, self.cursor_y);
        }
        self.cursor_moved();

        let char_width = UnicodeWidthChar::width(c).unwrap_or(1) as u16;

        // Standalone zero-width char (combining mark split from its base by a
        // PTY chunk boundary): attach to the previously written cell instead
        // of overwriting the cell under the cursor.
        if char_width == 0 {
            self.merge_zero_width(&c.to_string());
            return;
        }

        // Deferred autowrap from a previous char that filled the last column
        if self.pending_wrap {
            self.pending_wrap = false;
            if self.auto_wrap {
                let row = self.cursor_y as usize;
                if row < self.grid.len() {
                    self.grid[row].wrapped = true;
                }
                self.cursor_x = 0;
                self.advance_line();
            }
            // DECAWM off: stay on last column, overwrite
        }

        // Wide char at last column: wrap before writing
        if char_width == 2 && self.cursor_x == self.cols - 1 && self.auto_wrap {
            // Fill last column with a BCE blank, then wrap
            let row = self.cursor_y as usize;
            if row < self.grid.len() {
                let col = self.cursor_x as usize;
                if col < self.grid[row].cells.len() {
                    self.grid[row].cells[col] = self.bce_blank();
                }
                self.grid[row].wrapped = true;
            }
            self.cursor_x = 0;
            self.advance_line();
        }

        let row = self.cursor_y as usize;
        let col = self.cursor_x as usize;
        if row < self.grid.len() && col < self.grid[row].cells.len() {
            // Insert mode: shift characters right before writing
            if self.insert_mode {
                let cells = &mut self.grid[row].cells;
                cells.pop(); // remove last to keep row length
                cells.insert(col, self.blank.clone());
            }
            let (fg, bg) = self.effective_colors();
            self.grid[row].cells[col] = Cell {
                c,
                cluster: None,
                fg,
                bg,
                hyperlink_id: self.current_hyperlink,
            };

            // Wide char: write placeholder '\0' in the next column
            if char_width == 2 && col + 1 < self.grid[row].cells.len() {
                self.grid[row].cells[col + 1] = Cell {
                    c: '\0',
                    cluster: None,
                    fg,
                    bg,
                    hyperlink_id: self.current_hyperlink,
                };
            }
        }
        let end_x = self.cursor_x + char_width;
        if end_x >= self.cols {
            // Char filled the last column: cursor stays on it, wrap is deferred
            self.cursor_x = self.cols - 1;
            self.pending_wrap = true;
        } else {
            self.cursor_x = end_x;
        }
        self.last_printed = Some(c);
    }

    /// REP (CSI Ps b): repeat the last printed graphic character n times.
    /// xterm-256color terminfo advertises `rep`, so terminfo-driven apps
    /// emit it to compress runs — dropping it loses characters.
    pub fn repeat_last_char(&mut self, n: u16) {
        if let Some(c) = self.last_printed {
            for _ in 0..n {
                self.put_char(c);
            }
        }
    }

    /// Write a grapheme cluster (possibly multi-codepoint) at the cursor position.
    pub fn put_cluster(&mut self, cluster: &str) {
        use unicode_width::UnicodeWidthStr;

        let mut chars = cluster.chars();
        let first = match chars.next() {
            Some(c) => c,
            None => return,
        };

        // Single-char cluster: delegate to put_char (fast path)
        if chars.next().is_none() {
            self.put_char(first);
            return;
        }

        // Multi-codepoint cluster
        let raw_width = UnicodeWidthStr::width(cluster) as u16;

        self.cursor_moved();

        // Zero-width cluster (combining sequence split from its base by a
        // PTY chunk boundary): attach to the previously written cell.
        if raw_width == 0 {
            self.merge_zero_width(cluster);
            return;
        }
        let display_width = raw_width.max(1);

        // Deferred autowrap from a previous char that filled the last column
        if self.pending_wrap {
            self.pending_wrap = false;
            if self.auto_wrap {
                let row = self.cursor_y as usize;
                if row < self.grid.len() {
                    self.grid[row].wrapped = true;
                }
                self.cursor_x = 0;
                self.advance_line();
            }
        }

        // Wide cluster at last column: wrap before writing
        if display_width >= 2 && self.cursor_x + display_width > self.cols && self.auto_wrap {
            let row = self.cursor_y as usize;
            if row < self.grid.len() {
                self.grid[row].wrapped = true;
            }
            self.cursor_x = 0;
            self.advance_line();
        }

        let row = self.cursor_y as usize;
        let col = self.cursor_x as usize;
        if row < self.grid.len() && col < self.grid[row].cells.len() {
            let (fg, bg) = self.effective_colors();

            self.grid[row].cells[col] = Cell {
                c: first,
                cluster: Some(cluster.into()),
                fg,
                bg,
                hyperlink_id: self.current_hyperlink,
            };

            // Write '\0' sentinel for remaining columns
            for i in 1..display_width as usize {
                if col + i < self.grid[row].cells.len() {
                    self.grid[row].cells[col + i] = Cell {
                        c: '\0',
                        cluster: None,
                        fg,
                        bg,
                        hyperlink_id: self.current_hyperlink,
                    };
                }
            }
        }
        let end_x = self.cursor_x + display_width;
        if end_x >= self.cols {
            // Cluster filled the last column: cursor stays on it, wrap is deferred
            self.cursor_x = self.cols - 1;
            self.pending_wrap = true;
        } else {
            self.cursor_x = end_x;
        }
        // REP after a multi-codepoint cluster must not repeat a stale char
        self.last_printed = None;
    }

    /// Append a zero-width sequence (combining marks, ZWJ tail) to the last
    /// written cell. The target is the cell just left of the cursor — or the
    /// cursor cell itself when a deferred wrap is pending (the cursor is then
    /// still ON the last written column). Walks back over wide-char '\0'
    /// continuation cells to reach the base.
    fn merge_zero_width(&mut self, seq: &str) {
        let row = self.cursor_y as usize;
        if row >= self.grid.len() {
            return;
        }
        let mut col = if self.pending_wrap {
            self.cursor_x as usize
        } else {
            match (self.cursor_x as usize).checked_sub(1) {
                Some(c) => c,
                None => return,
            }
        };
        while col > 0 && self.grid[row].cells.get(col).map_or(false, |cell| cell.c == '\0') {
            col -= 1;
        }
        let (old_w, merged) = {
            let cell = match self.grid[row].cells.get_mut(col) {
                Some(c) => c,
                None => return,
            };
            if cell.c == ' ' || cell.c == '\0' {
                return; // nothing to attach to
            }
            let mut s: String = match &cell.cluster {
                Some(cl) => cl.to_string(),
                None => cell.c.to_string(),
            };
            let old_w = {
                use unicode_width::UnicodeWidthStr;
                UnicodeWidthStr::width(s.as_str()).max(1)
            };
            s.push_str(seq);
            cell.cluster = Some(s.clone().into());
            (old_w, s)
        };
        // A variation selector can promote the grapheme to wide (e.g. text
        // presentation -> emoji presentation): claim the next column with a
        // '\0' continuation so the footprint matches the app's wcwidth.
        let new_w = {
            use unicode_width::UnicodeWidthStr;
            UnicodeWidthStr::width(merged.as_str()).max(1)
        };
        if new_w > old_w && col + 1 < self.grid[row].cells.len() {
            let (fg, bg, link) = {
                let c = &self.grid[row].cells[col];
                (c.fg, c.bg, c.hyperlink_id)
            };
            self.grid[row].cells[col + 1] = Cell {
                c: '\0',
                cluster: None,
                fg,
                bg,
                hyperlink_id: link,
            };
            if !self.pending_wrap {
                let end = (col as u16) + new_w as u16;
                if end >= self.cols {
                    self.cursor_x = self.cols - 1;
                    self.pending_wrap = true;
                } else {
                    self.cursor_x = end;
                }
            }
        }
    }

    pub fn newline(&mut self) {
        self.pending_wrap = false;
        self.advance_line();
    }

    pub fn carriage_return(&mut self) {
        self.cursor_x = 0;
        self.pending_wrap = false;
        self.cursor_moved();
    }

    pub fn backspace(&mut self) {
        self.pending_wrap = false;
        if self.cursor_x > 0 {
            self.cursor_x -= 1;
            self.cursor_moved();
        }
    }

    pub fn tab(&mut self) {
        let next = ((self.cursor_x as usize + 1)..self.cols as usize)
            .find(|&i| self.tab_stops.get(i).copied().unwrap_or(i % 8 == 0));
        self.cursor_x = next.map_or(self.cols.saturating_sub(1), |i| i as u16);
        // Per DEC STD 070 / xterm, HT does NOT reset the Last Column Flag
        self.cursor_moved();
    }

    /// CBT (CSI Z) — move back n tab stops.
    pub fn back_tab(&mut self, n: u16) {
        for _ in 0..n {
            let prev = (0..self.cursor_x as usize)
                .rev()
                .find(|&i| self.tab_stops.get(i).copied().unwrap_or(i % 8 == 0));
            self.cursor_x = prev.map_or(0, |i| i as u16);
        }
        self.cursor_moved();
    }

    /// HTS (ESC H) — set a tab stop at the cursor column.
    pub fn set_tab_stop(&mut self) {
        let col = self.cursor_x as usize;
        if col < self.tab_stops.len() {
            self.tab_stops[col] = true;
        }
    }

    /// TBC (CSI g) — clear the tab stop at the cursor (0) or all (3).
    pub fn clear_tab_stops(&mut self, mode: u16) {
        match mode {
            0 => {
                let col = self.cursor_x as usize;
                if col < self.tab_stops.len() {
                    self.tab_stops[col] = false;
                }
            }
            3 => self.tab_stops.iter_mut().for_each(|s| *s = false),
            _ => {}
        }
    }

    fn advance_line(&mut self) {
        if self.cursor_y == self.scroll_bottom {
            self.scroll_up(1);
        } else if self.cursor_y < self.rows - 1 {
            self.cursor_y += 1;
        }
    }

    fn push_to_scrollback(&mut self, mut row: Row) {
        row.trim_trailing_blanks(self.default_fg, self.default_bg);
        self.scrollback.push_back(row);
        if self.scrollback.len() > self.scrollback_limit {
            // Buffer full: 1 in, 1 out — net zero, viewport stays put.
            self.scrollback.pop_front();
        } else if self.scroll_offset > 0 {
            // Buffer still growing: shift viewport to keep the same content visible.
            self.scroll_offset += 1;
        }
    }

    fn scroll_up(&mut self, n: u16) {
        let top = self.scroll_top as usize;
        let bottom = self.scroll_bottom as usize;
        let fill = self.bce_blank();

        for _ in 0..n {
            if top < self.grid.len() {
                let line = self.grid.remove(top);
                if top == 0 && !self.in_alt_screen {
                    self.push_to_scrollback(line);
                }
            }
            let new_line = Row::new(self.cols as usize, &fill);
            let insert_pos = bottom.min(self.grid.len());
            self.grid.insert(insert_pos, new_line);
        }

        // Ensure grid has correct number of rows
        self.grid.resize(self.rows as usize, Row::new(self.cols as usize, &self.blank));
        self.dirty.store(true, Ordering::Relaxed);
    }

    fn scroll_down(&mut self, n: u16) {
        let top = self.scroll_top as usize;
        let bottom = self.scroll_bottom as usize;
        let fill = self.bce_blank();

        for _ in 0..n {
            if bottom < self.grid.len() {
                self.grid.remove(bottom);
            }
            let new_line = Row::new(self.cols as usize, &fill);
            self.grid.insert(top, new_line);
        }

        self.grid.resize(self.rows as usize, Row::new(self.cols as usize, &self.blank));
        self.dirty.store(true, Ordering::Relaxed);
    }

    pub fn set_sgr(&mut self, params: &[u16]) {
        let mut i = 0;
        while i < params.len() {
            match params[i] {
                0 => {
                    self.current_fg = self.default_fg;
                    self.current_bg = self.default_bg;
                    self.reversed = false;
                    self.bold = false;
                    self.dim = false;
                }
                1 => self.bold = true,
                2 => self.dim = true,
                22 => {
                    self.bold = false;
                    self.dim = false;
                }
                // Reverse video is a flag applied at write time (effective_colors),
                // not a physical swap — a swap corrupts colors set while reversed.
                7 => self.reversed = true,
                27 => self.reversed = false,
                30..=37 => {
                    self.current_fg = AnsiColor::from_index((params[i] - 30) as u8).to_rgb();
                }
                38 => {
                    if i + 2 < params.len() && params[i + 1] == 5 {
                        self.current_fg = AnsiColor::from_256(params[i + 2] as u8);
                        i += 2;
                    } else if i + 4 < params.len() && params[i + 1] == 2 {
                        self.current_fg = [
                            params[i + 2] as u8,
                            params[i + 3] as u8,
                            params[i + 4] as u8,
                        ];
                        i += 4;
                    }
                }
                39 => self.current_fg = self.default_fg,
                40..=47 => {
                    self.current_bg = AnsiColor::from_index((params[i] - 40) as u8).to_rgb();
                }
                48 => {
                    if i + 2 < params.len() && params[i + 1] == 5 {
                        self.current_bg = AnsiColor::from_256(params[i + 2] as u8);
                        i += 2;
                    } else if i + 4 < params.len() && params[i + 1] == 2 {
                        self.current_bg = [
                            params[i + 2] as u8,
                            params[i + 3] as u8,
                            params[i + 4] as u8,
                        ];
                        i += 4;
                    }
                }
                49 => self.current_bg = self.default_bg,
                90..=97 => {
                    self.current_fg = AnsiColor::from_index((params[i] - 90 + 8) as u8).to_rgb();
                }
                100..=107 => {
                    self.current_bg = AnsiColor::from_index((params[i] - 100 + 8) as u8).to_rgb();
                }
                _ => {}
            }
            i += 1;
        }
    }

    pub fn erase_in_display(&mut self, mode: u16) {
        self.pending_wrap = false;
        self.dirty.store(true, Ordering::Relaxed);
        let fill = self.bce_blank();
        match mode {
            0 => {
                // Erase from cursor to end
                let row = self.cursor_y as usize;
                let col = self.cursor_x as usize;
                if row < self.grid.len() {
                    for c in col..self.grid[row].cells.len() {
                        self.grid[row].cells[c] = fill.clone();
                    }
                    self.grid[row].wrapped = false;
                    for r in (row + 1)..self.grid.len() {
                        for c in 0..self.grid[r].cells.len() {
                            self.grid[r].cells[c] = fill.clone();
                        }
                        self.grid[r].wrapped = false;
                    }
                }
            }
            1 => {
                // Erase from start to cursor
                let row = self.cursor_y as usize;
                let col = self.cursor_x as usize;
                for r in 0..row {
                    if r < self.grid.len() {
                        for c in 0..self.grid[r].cells.len() {
                            self.grid[r].cells[c] = fill.clone();
                        }
                        self.grid[r].wrapped = false;
                    }
                }
                if row < self.grid.len() {
                    for c in 0..=col.min(self.grid[row].cells.len().saturating_sub(1)) {
                        self.grid[row].cells[c] = fill.clone();
                    }
                }
            }
            3 => {
                // ED 3 (xterm): erase the scrollback only — the screen is
                // untouched. Claude Code's /clear emits 2J+3J; aliasing 3J to
                // 2J left stale UI snapshots in the scrollback forever.
                self.scrollback.clear();
                self.reset_scroll();
            }
            2 => {
                // Erase entire display — push current content to scrollback first
                if !self.in_alt_screen {
                    // Find last row with visible content to avoid trailing blanks
                    // (colored-bg cells are visible content)
                    let default_bg = self.default_bg;
                    let last_content = self.grid.iter().rposition(|row|
                        row.cells.iter().any(|c| !c.is_blank() || c.bg != default_bg)
                    );
                    if let Some(last) = last_content {
                        log::debug!("ED 2/3: pushing {} rows to scrollback (scrollback_len={})", last + 1, self.scrollback.len());
                        let rows: Vec<Row> = self.grid[..=last].to_vec();
                        for row in rows {
                            self.push_to_scrollback(row);
                        }
                    }
                }
                for row in &mut self.grid {
                    for cell in row.cells.iter_mut() {
                        *cell = fill.clone();
                    }
                    row.wrapped = false;
                }
            }
            _ => {}
        }
    }

    /// Clear scrollback buffer and visible screen, reset cursor to top-left.
    pub fn clear_scrollback_and_screen(&mut self) {
        self.dirty.store(true, Ordering::Relaxed);
        self.scrollback.clear();
        self.reset_scroll();
        for row in &mut self.grid {
            for cell in row.cells.iter_mut() {
                *cell = self.blank.clone();
            }
            row.wrapped = false;
        }
        self.cursor_x = 0;
        self.cursor_y = 0;
        self.pending_wrap = false;
    }

    pub fn erase_in_line(&mut self, mode: u16) {
        self.pending_wrap = false;
        self.dirty.store(true, Ordering::Relaxed);
        let row = self.cursor_y as usize;
        if row >= self.grid.len() {
            return;
        }
        let fill = self.bce_blank();
        match mode {
            0 => {
                for c in (self.cursor_x as usize)..self.grid[row].cells.len() {
                    self.grid[row].cells[c] = fill.clone();
                }
                self.grid[row].wrapped = false;
            }
            1 => {
                for c in 0..=(self.cursor_x as usize).min(self.grid[row].cells.len().saturating_sub(1)) {
                    self.grid[row].cells[c] = fill.clone();
                }
            }
            2 => {
                for cell in self.grid[row].cells.iter_mut() {
                    *cell = fill.clone();
                }
                self.grid[row].wrapped = false;
            }
            _ => {}
        }
    }

    pub fn cursor_up(&mut self, n: u16) {
        self.pending_wrap = false;
        // Per xterm: a cursor inside the scroll region stops at its top margin
        let limit = if self.cursor_y >= self.scroll_top { self.scroll_top } else { 0 };
        self.cursor_y = self.cursor_y.saturating_sub(n).max(limit);
        self.cursor_moved();
    }

    pub fn cursor_down(&mut self, n: u16) {
        self.pending_wrap = false;
        // Per xterm: a cursor inside the scroll region stops at its bottom margin
        let limit = if self.cursor_y <= self.scroll_bottom {
            self.scroll_bottom
        } else {
            self.rows.saturating_sub(1)
        };
        self.cursor_y = self.cursor_y.saturating_add(n).min(limit);
        self.cursor_moved();
    }

    pub fn cursor_forward(&mut self, n: u16) {
        self.cursor_x = self.cursor_x.saturating_add(n).min(self.cols.saturating_sub(1));
        self.pending_wrap = false;
        self.cursor_moved();
    }

    pub fn cursor_backward(&mut self, n: u16) {
        self.cursor_x = self.cursor_x.saturating_sub(n);
        self.pending_wrap = false;
        self.cursor_moved();
    }

    pub fn set_cursor_pos(&mut self, row: u16, col: u16) {
        // DECOM: row is relative to the scroll region and clamped inside it
        if self.origin_mode {
            self.cursor_y = (self.scroll_top.saturating_add(row)).min(self.scroll_bottom);
        } else {
            self.cursor_y = row.min(self.rows.saturating_sub(1));
        }
        self.cursor_x = col.min(self.cols.saturating_sub(1));
        self.pending_wrap = false;
        self.cursor_moved();
    }

    /// DECAWM (DEC private mode 7). Disabling autowrap also clears a pending
    /// deferred wrap — there is nothing left to defer.
    pub fn set_auto_wrap(&mut self, on: bool) {
        self.auto_wrap = on;
        if !on {
            self.pending_wrap = false;
        }
    }

    /// CHA/HPA — set the cursor column without touching the row. Routing
    /// this through set_cursor_pos would re-apply the DECOM origin offset to
    /// an already-absolute row.
    pub fn set_cursor_col(&mut self, col: u16) {
        self.cursor_x = col.min(self.cols.saturating_sub(1));
        self.pending_wrap = false;
        self.cursor_moved();
    }

    /// DECOM (DEC private mode 6). Set/reset homes the cursor — to the
    /// region origin when set, to the screen origin when reset.
    pub fn set_origin_mode(&mut self, on: bool) {
        self.origin_mode = on;
        self.cursor_x = 0;
        self.cursor_y = if on { self.scroll_top } else { 0 };
        self.pending_wrap = false;
        self.cursor_moved();
    }

    pub fn save_cursor(&mut self) {
        self.saved_cursor = Some(SavedCursor {
            x: self.cursor_x,
            y: self.cursor_y,
            fg: self.current_fg,
            bg: self.current_bg,
            bold: self.bold,
            dim: self.dim,
            reversed: self.reversed,
            pending_wrap: self.pending_wrap,
            origin_mode: self.origin_mode,
            auto_wrap: self.auto_wrap,
            g0_dec_graphics: self.g0_dec_graphics,
            g1_dec_graphics: self.g1_dec_graphics,
            active_charset_g1: self.active_charset_g1,
        });
    }

    pub fn restore_cursor(&mut self) {
        if let Some(sc) = self.saved_cursor {
            // Clamp: the screen may have shrunk since save_cursor. An
            // out-of-bounds cursor_y makes put_char silently drop all output.
            self.cursor_x = sc.x.min(self.cols.saturating_sub(1));
            self.cursor_y = sc.y.min(self.rows.saturating_sub(1));
            self.current_fg = sc.fg;
            self.current_bg = sc.bg;
            self.bold = sc.bold;
            self.dim = sc.dim;
            self.reversed = sc.reversed;
            self.pending_wrap = sc.pending_wrap && self.cursor_x == self.cols.saturating_sub(1);
            self.origin_mode = sc.origin_mode;
            self.auto_wrap = sc.auto_wrap;
            self.g0_dec_graphics = sc.g0_dec_graphics;
            self.g1_dec_graphics = sc.g1_dec_graphics;
            self.active_charset_g1 = sc.active_charset_g1;
        }
    }

    /// IL — Insert Line(s). Per ECMA-48 / xterm: only effective when the cursor
    /// is within the scroll region. Shifts lines [cursor_y .. scroll_bottom]
    /// downward by `n` (clamped), filling with blanks. Cursor moves to column 0.
    pub fn insert_lines(&mut self, n: u16) {
        let row = self.cursor_y;
        if row < self.scroll_top || row > self.scroll_bottom {
            return;
        }
        let max_n = self.scroll_bottom - row + 1;
        let n = n.min(max_n);
        let row_u = row as usize;
        let bottom_u = self.scroll_bottom as usize;
        let fill = self.bce_blank();
        for _ in 0..n {
            if bottom_u < self.grid.len() {
                self.grid.remove(bottom_u);
            }
            self.grid.insert(row_u, Row::new(self.cols as usize, &fill));
        }
        self.grid.resize(self.rows as usize, Row::new(self.cols as usize, &self.blank));
        self.cursor_x = 0;
        self.pending_wrap = false;
        self.cursor_moved();
    }

    /// DL — Delete Line(s). Per ECMA-48 / xterm: only effective when the cursor
    /// is within the scroll region. Removes `n` lines starting at cursor_y,
    /// shifting lines below up and appending blanks at scroll_bottom.
    /// Cursor moves to column 0.
    pub fn delete_lines(&mut self, n: u16) {
        let row = self.cursor_y;
        if row < self.scroll_top || row > self.scroll_bottom {
            return;
        }
        let max_n = self.scroll_bottom - row + 1;
        let n = n.min(max_n);
        let row_u = row as usize;
        let bottom_u = self.scroll_bottom as usize;
        let fill = self.bce_blank();
        for _ in 0..n {
            if row_u < self.grid.len() {
                self.grid.remove(row_u);
            }
            let insert_pos = bottom_u.min(self.grid.len());
            self.grid.insert(insert_pos, Row::new(self.cols as usize, &fill));
        }
        self.grid.resize(self.rows as usize, Row::new(self.cols as usize, &self.blank));
        self.cursor_x = 0;
        self.pending_wrap = false;
        self.cursor_moved();
    }

    pub fn delete_chars(&mut self, n: u16) {
        let row = self.cursor_y as usize;
        let col = self.cursor_x as usize;
        let fill = self.bce_blank();
        if row < self.grid.len() {
            for _ in 0..n {
                if col < self.grid[row].cells.len() {
                    self.grid[row].cells.remove(col);
                    self.grid[row].cells.push(fill.clone());
                }
            }
        }
        self.dirty.store(true, Ordering::Relaxed);
    }

    pub fn insert_chars(&mut self, n: u16) {
        let row = self.cursor_y as usize;
        let col = self.cursor_x as usize;
        let fill = self.bce_blank();
        if row < self.grid.len() {
            let cells = &mut self.grid[row].cells;
            for _ in 0..n {
                if col < cells.len() {
                    cells.pop();
                    cells.insert(col, fill.clone());
                }
            }
        }
        self.dirty.store(true, Ordering::Relaxed);
    }

    pub fn erase_chars(&mut self, n: u16) {
        self.pending_wrap = false;
        let row = self.cursor_y as usize;
        let col = self.cursor_x as usize;
        let fill = self.bce_blank();
        if row < self.grid.len() {
            for i in 0..n as usize {
                if col + i < self.grid[row].cells.len() {
                    self.grid[row].cells[col + i] = fill.clone();
                }
            }
        }
        self.dirty.store(true, Ordering::Relaxed);
    }

    pub fn set_scroll_region(&mut self, top: u16, bottom: u16) {
        let max_row = self.rows.saturating_sub(1);
        let bottom_clamped = bottom.min(max_row);
        // Per DEC VT spec: an invalid region (top >= bottom) resets to full screen.
        if top >= bottom_clamped {
            self.scroll_top = 0;
            self.scroll_bottom = max_row;
        } else {
            self.scroll_top = top;
            self.scroll_bottom = bottom_clamped;
        }
        self.cursor_x = 0;
        self.cursor_y = if self.origin_mode { self.scroll_top } else { 0 };
        self.pending_wrap = false;
        self.cursor_moved();
    }

    pub fn scroll_up_region(&mut self, n: u16) {
        self.scroll_up(n);
    }

    pub fn scroll_down_region(&mut self, n: u16) {
        self.scroll_down(n);
    }

    pub fn enter_alt_screen(&mut self) {
        if self.in_alt_screen {
            return;
        }
        self.in_alt_screen = true;
        self.alt_cursor = Some((self.cursor_x, self.cursor_y));
        let alt_grid = std::mem::replace(
            &mut self.grid,
            (0..self.rows as usize).map(|_| Row::new(self.cols as usize, &self.blank)).collect(),
        );
        self.alt_grid = Some(alt_grid);
        self.cursor_x = 0;
        self.cursor_y = 0;
        self.pending_wrap = false;
        self.reset_scroll();
        self.dirty.store(true, Ordering::Relaxed);
    }

    pub fn leave_alt_screen(&mut self) {
        if !self.in_alt_screen {
            return;
        }
        self.in_alt_screen = false;
        if let Some(grid) = self.alt_grid.take() {
            self.grid = grid;
        }
        if let Some((x, y)) = self.alt_cursor.take() {
            // Clamp: the screen may have been resized while in alt screen
            self.cursor_x = x.min(self.cols.saturating_sub(1));
            self.cursor_y = y.min(self.rows.saturating_sub(1));
        }
        self.pending_wrap = false;
        self.reset_scroll();
        self.dirty.store(true, Ordering::Relaxed);
    }

    fn cursor_moved(&self) {
        self.dirty.store(true, Ordering::Relaxed);
        self.cursor_move_epoch.fetch_add(1, Ordering::Relaxed);
    }

    pub fn scroll_offset(&self) -> i32 {
        self.scroll_offset
    }

    pub fn reset_scroll(&mut self) {
        self.scroll_offset = 0;
        self.user_scrolled = false;
    }

    /// Blank cell used by erase ops and scrolled-in lines. Per xterm BCE
    /// (back-color erase), erased cells take the CURRENT SGR background,
    /// not the default. Identical to `blank` when no background is set.
    fn bce_blank(&self) -> Cell {
        // Same background printed cells get (bold/dim applied to the logical
        // fg before a reverse swap) — erased and printed cells must match.
        let (_, bg) = self.effective_colors();
        if bg == self.blank.bg {
            self.blank.clone()
        } else {
            Cell { c: ' ', cluster: None, fg: self.blank.fg, bg, hyperlink_id: 0 }
        }
    }

    /// Soft reset: restore rendering-critical state to sane defaults without
    /// clearing grid content or scrollback. Fixes persistent display corruption
    /// (wrong scroll region, hidden cursor, stuck SGR attributes).
    pub fn soft_reset(&mut self) {
        self.pending_wrap = false;
        self.g0_dec_graphics = false;
        self.g1_dec_graphics = false;
        self.active_charset_g1 = false;
        self.scroll_top = 0;
        self.scroll_bottom = self.rows.saturating_sub(1);
        self.cursor_visible = true;
        self.origin_mode = false;
        self.auto_wrap = true;
        self.insert_mode = false;
        self.current_fg = self.default_fg;
        self.current_bg = self.default_bg;
        self.reversed = false;
        self.bold = false;
        self.dim = false;
        self.synchronized_output = false;
        self.sync_output_since = None;
        self.dirty.store(true, std::sync::atomic::Ordering::Relaxed);
    }

    pub fn resize(&mut self, new_cols: u16, new_rows: u16) {
        if new_cols == self.cols && new_rows == self.rows {
            return;
        }

        let old_cols = self.cols;
        self.cols = new_cols;
        self.rows = new_rows;

        // Tab stops follow the width (default stops for new columns)
        if (new_cols as usize) < self.tab_stops.len() {
            self.tab_stops.truncate(new_cols as usize);
        } else {
            let from = self.tab_stops.len();
            self.tab_stops.extend((from..new_cols as usize).map(|i| i % 8 == 0));
        }

        // Alt screen: the visible (alt) grid is truncated/padded — TUIs fully
        // repaint on SIGWINCH. The scrollback is still reflowed so it doesn't
        // sit at a stale width when the primary screen returns.
        if self.in_alt_screen {
            if new_cols != old_cols && !self.scrollback.is_empty() {
                let sb: Vec<Row> = self.scrollback.drain(..).collect();
                let mut reflowed = Self::reflow_rows(sb, old_cols as usize, new_cols as usize, &self.blank);
                for row in reflowed.iter_mut() {
                    row.trim_trailing_blanks(self.default_fg, self.default_bg);
                }
                self.scrollback = reflowed.into();
            }
            for row in &mut self.grid {
                row.cells.resize(new_cols as usize, self.blank.clone());
            }
            let nr = new_rows as usize;
            self.grid.resize(nr, Row::new(new_cols as usize, &self.blank));

            // alt_grid holds the SAVED PRIMARY screen here. When shrinking,
            // drop rows from the TOP into scrollback (the bottom holds the
            // shell prompt the user returns to) and shift the saved cursor.
            if let Some(ref mut alt_grid) = self.alt_grid {
                for row in alt_grid.iter_mut() {
                    row.cells.resize(new_cols as usize, self.blank.clone());
                }
                // Drop blank rows from the bottom first (below the saved
                // cursor) — pushing them from the top would bury the prompt
                // in the scrollback under empty lines.
                let default_bg = self.default_bg;
                while alt_grid.len() > nr {
                    let saved_y = self.alt_cursor.map_or(0, |(_, y)| y as usize);
                    let is_blank_row = alt_grid.last()
                        .map(|row| row.cells.iter().all(|c| c.is_blank() && c.bg == default_bg))
                        .unwrap_or(true);
                    if is_blank_row && alt_grid.len() > saved_y + 1 {
                        alt_grid.pop();
                    } else {
                        break;
                    }
                }
                while alt_grid.len() > nr {
                    let mut line = alt_grid.remove(0);
                    line.trim_trailing_blanks(self.default_fg, self.default_bg);
                    self.scrollback.push_back(line);
                    if let Some((_, ref mut y)) = self.alt_cursor {
                        *y = y.saturating_sub(1);
                    }
                }
                while self.scrollback.len() > self.scrollback_limit {
                    self.scrollback.pop_front();
                }
                // Growing: pull rows back from the scrollback into the top of
                // the saved primary grid (mirror of the shrink path)
                let mut pulled: Vec<Row> = Vec::new();
                while alt_grid.len() + pulled.len() < nr {
                    if let Some(mut row) = self.scrollback.pop_back() {
                        row.cells.resize(new_cols as usize, self.blank.clone());
                        pulled.push(row);
                    } else {
                        break;
                    }
                }
                if !pulled.is_empty() {
                    let count = pulled.len() as u16;
                    pulled.reverse();
                    alt_grid.splice(0..0, pulled);
                    if let Some((_, ref mut y)) = self.alt_cursor {
                        *y = (*y + count).min(new_rows.saturating_sub(1));
                    }
                }
                alt_grid.resize(nr, Row::new(new_cols as usize, &self.blank));
            }

            self.cursor_x = self.cursor_x.min(new_cols.saturating_sub(1));
            self.cursor_y = self.cursor_y.min(new_rows.saturating_sub(1));
            self.pending_wrap = false;
            self.scroll_top = 0;
            self.scroll_bottom = new_rows.saturating_sub(1);
            self.reset_scroll();
            self.dirty.store(true, Ordering::Relaxed);
            return;
        }

        let nr = new_rows as usize;
        if new_cols != old_cols {
            // --- Reflow: scrollback + grid as ONE logical stream, so lines
            // that wrap across the scrollback/grid boundary stay joined.
            // Reflowing them separately severed those lines permanently. ---
            let cursor_row = self.cursor_y as usize;
            let cursor_col = self.cursor_x as usize;

            let sb: Vec<Row> = self.scrollback.drain(..).collect();
            let sb_len = sb.len();
            let mut stream: Vec<Row> = sb;
            stream.extend(std::mem::take(&mut self.grid));
            let cursor_stream_row = sb_len + cursor_row;

            // Cursor as (logical line index, offset within that line) over
            // the unified stream. Trimmed wrapped rows count as old_cols —
            // rows_to_logical_lines pads them back before joining.
            let mut logical_line_idx: usize = 0;
            let mut offset_in_logical: usize = 0;
            let mut line_start_row: usize = 0;
            {
                let mut cells_in_current_logical: usize = 0;
                for (i, row) in stream.iter().enumerate() {
                    if i == cursor_stream_row {
                        offset_in_logical = cells_in_current_logical + cursor_col;
                        break;
                    }
                    let effective_len = if row.wrapped && row.cells.len() < old_cols as usize {
                        old_cols as usize
                    } else {
                        row.cells.len()
                    };
                    cells_in_current_logical += effective_len;
                    if !row.wrapped {
                        logical_line_idx += 1;
                        cells_in_current_logical = 0;
                        line_start_row = i + 1;
                    }
                }
            }

            // Snapshot the cursor's logical line (same per-row padding as
            // rows_to_logical_lines) to replay the chunking for relocation
            let cursor_line_cells: Vec<Cell> = {
                let mut cells: Vec<Cell> = Vec::new();
                let mut j = line_start_row;
                while j < stream.len() {
                    let row = &stream[j];
                    let mut rc: Vec<Cell> = row.cells.clone();
                    if row.wrapped && rc.len() < old_cols as usize {
                        rc.resize(old_cols as usize, self.blank.clone());
                    }
                    cells.extend(rc);
                    if !row.wrapped {
                        break;
                    }
                    j += 1;
                }
                cells
            };

            let mut reflowed = Self::reflow_rows(stream, old_cols as usize, new_cols as usize, &self.blank);

            // Locate the cursor: first row of its logical line in the
            // reflowed stream + pad-aware offset within the line
            let (row_within, col_within) = Self::locate_in_wrapped_line(
                &cursor_line_cells,
                new_cols as usize,
                offset_in_logical,
                &self.blank,
            );
            let line_first_row = {
                let mut ll = 0;
                let mut i = 0;
                while i < reflowed.len() && ll < logical_line_idx {
                    if !reflowed[i].wrapped {
                        ll += 1;
                    }
                    i += 1;
                }
                i
            };
            let cursor_new_row = (line_first_row + row_within).min(reflowed.len().saturating_sub(1));
            let new_cx: u16 = (col_within as u16).min(new_cols.saturating_sub(1));

            // Drop trailing blank rows before splitting — otherwise they
            // count against new_rows and push visible content into the
            // scrollback while blank rows stay on screen.
            let default_bg = self.default_bg;
            while reflowed.len() > nr {
                let is_blank = reflowed.last()
                    .map(|row| row.cells.iter().all(|c| c.is_blank() && c.bg == default_bg))
                    .unwrap_or(true);
                if is_blank && reflowed.len() > cursor_new_row + 1 {
                    reflowed.pop();
                } else {
                    break;
                }
            }

            // Split: the grid takes the bottom new_rows rows of the stream,
            // but never starts below the cursor (it must stay on screen).
            let mut grid_start = reflowed.len().saturating_sub(nr);
            if cursor_new_row < grid_start {
                grid_start = cursor_new_row;
            }
            let mut grid: Vec<Row> = reflowed.split_off(grid_start);
            // Cursor pinned above the natural bottom: drop blank bottom rows,
            // then truncate, to fit new_rows
            let cursor_in_grid = cursor_new_row - grid_start;
            while grid.len() > nr {
                let is_blank = grid.last()
                    .map(|row| row.cells.iter().all(|c| c.is_blank() && c.bg == default_bg))
                    .unwrap_or(true);
                if !is_blank && grid.len() <= cursor_in_grid + 1 {
                    break;
                }
                grid.pop();
            }
            grid.truncate(nr);
            while grid.len() < nr {
                grid.push(Row::new(new_cols as usize, &self.blank));
            }

            // Rows above the split become the scrollback (trimmed for RAM)
            for row in reflowed.iter_mut() {
                row.trim_trailing_blanks(self.default_fg, self.default_bg);
            }
            self.scrollback = reflowed.into();
            self.grid = grid;
            self.cursor_y = (cursor_in_grid.min(nr.saturating_sub(1))) as u16;
            self.cursor_x = new_cx;
        } else {
            // --- Rows-only resize ---
            // Remove blank rows from bottom first (colored-bg rows are content)
            let default_bg = self.default_bg;
            while self.grid.len() > nr {
                let is_blank = self.grid.last()
                    .map(|row| row.cells.iter().all(|c| c.is_blank() && c.bg == default_bg))
                    .unwrap_or(true);
                if is_blank && self.grid.len() > self.cursor_y as usize + 1 {
                    self.grid.pop();
                } else {
                    break;
                }
            }
            // Push excess top rows into scrollback
            while self.grid.len() > nr {
                let mut line = self.grid.remove(0);
                line.trim_trailing_blanks(self.default_fg, self.default_bg);
                self.scrollback.push_back(line);
                self.cursor_y = self.cursor_y.saturating_sub(1);
            }
            // Pull rows back from scrollback (O(n) via collect + splice)
            let mut pulled: Vec<Row> = Vec::new();
            while self.grid.len() + pulled.len() < nr {
                if let Some(mut row) = self.scrollback.pop_back() {
                    row.cells.resize(new_cols as usize, self.blank.clone());
                    pulled.push(row);
                } else {
                    break;
                }
            }
            if !pulled.is_empty() {
                let count = pulled.len();
                pulled.reverse();
                self.grid.splice(0..0, pulled);
                self.cursor_y = ((self.cursor_y as usize + count).min(new_rows as usize - 1)) as u16;
            }
            // Add blank rows if still needed
            while self.grid.len() < nr {
                self.grid.push(Row::new(new_cols as usize, &self.blank));
            }
        }

        // Clamp cursor
        self.cursor_x = self.cursor_x.min(new_cols.saturating_sub(1));
        self.cursor_y = self.cursor_y.min(new_rows.saturating_sub(1));
        self.pending_wrap = false;

        // Reset scroll region
        self.scroll_top = 0;
        self.scroll_bottom = new_rows.saturating_sub(1);
        self.reset_scroll();

        // Resize alt grid (no reflow)
        if let Some(ref mut alt_grid) = self.alt_grid {
            for row in alt_grid.iter_mut() {
                row.cells.resize(new_cols as usize, self.blank.clone());
            }
            alt_grid.resize(nr, Row::new(new_cols as usize, &self.blank));
        }

        // Trim scrollback
        while self.scrollback.len() > self.scrollback_limit {
            self.scrollback.pop_front();
        }

        self.dirty.store(true, Ordering::Relaxed);
    }

    // --- Reflow helpers ---

    /// Concatenate consecutive wrapped rows into logical lines.
    /// `old_cols` is used to pad trimmed wrapped rows back to full width
    /// so that column positions stay aligned across concatenated rows.
    fn rows_to_logical_lines(rows: Vec<Row>, old_cols: usize, blank: &Cell) -> Vec<Vec<Cell>> {
        let mut lines: Vec<Vec<Cell>> = Vec::new();
        let mut current: Vec<Cell> = Vec::new();
        for row in rows {
            if row.wrapped && row.cells.len() < old_cols {
                // Row was trimmed (shrink_to_fit) — pad back to old_cols
                // so the next row's content starts at the right column offset.
                let mut cells = row.cells;
                cells.resize(old_cols, blank.clone());
                current.extend(cells);
            } else {
                current.extend(row.cells);
            }
            if !row.wrapped {
                lines.push(current);
                current = Vec::new();
            }
        }
        if !current.is_empty() {
            lines.push(current);
        }
        lines
    }

    /// Wrap a logical line to new_cols, trimming trailing blanks.
    /// A cell only counts as blank when it is visually indistinguishable from
    /// the default blank — colored-bg spaces (BCE fills, painted bands) are
    /// content and must survive reflow.
    fn wrap_logical_line(cells: Vec<Cell>, new_cols: usize, blank: &Cell) -> Vec<Row> {
        // Trim trailing blank cells. '\0' continuations are content — see
        // trim_trailing_blanks.
        let len = cells.iter()
            .rposition(|c| c.c != ' ' || c.cluster.is_some() || c.bg != blank.bg)
            .map_or(0, |i| i + 1);

        if len == 0 {
            // Entirely blank logical line → single blank row (hard newline)
            return vec![Row::new(new_cols, blank)];
        }

        let trimmed = &cells[..len];
        let mut rows: Vec<Row> = Vec::new();
        let mut current: Vec<Cell> = Vec::with_capacity(new_cols);
        let mut i = 0;
        while i < trimmed.len() {
            // A wide char occupies (base, '\0' continuation) — never split
            // the pair across a row boundary
            let pair = trimmed[i].c != '\0'
                && i + 1 < trimmed.len()
                && trimmed[i + 1].c == '\0';
            let needed = if pair { 2 } else { 1 };
            if current.len() + needed > new_cols {
                if needed > new_cols {
                    // Wide char wider than the whole row: drop it
                    i += 2;
                    continue;
                }
                current.resize(new_cols, blank.clone());
                rows.push(Row { cells: current, wrapped: true });
                current = Vec::with_capacity(new_cols);
            }
            current.push(trimmed[i].clone());
            if pair {
                current.push(trimmed[i + 1].clone());
            }
            i += needed;
        }
        if !current.is_empty() || rows.is_empty() {
            current.resize(new_cols, blank.clone());
            rows.push(Row { cells: current, wrapped: true });
        }
        // Last row of a logical line is not wrapped (it ends with a hard newline)
        if let Some(last) = rows.last_mut() {
            last.wrapped = false;
        }
        rows
    }

    /// Locate original cell index `target` of a logical line in the output
    /// of wrap_logical_line, replaying the SAME pair-aware chunking (with
    /// its pad cells at wide-pair row boundaries). Returns
    /// (row_within_line, col). Walking the reflowed rows by raw lengths is
    /// wrong: pads shift every later cell by one.
    fn locate_in_wrapped_line(cells: &[Cell], new_cols: usize, target: usize, blank: &Cell) -> (usize, usize) {
        let len = cells.iter()
            .rposition(|c| c.c != ' ' || c.cluster.is_some() || c.bg != blank.bg)
            .map_or(0, |i| i + 1);
        let cols = new_cols.max(1);
        let mut row = 0usize;
        let mut col = 0usize;
        let mut i = 0usize;
        while i < len {
            let pair = cells[i].c != '\0' && i + 1 < len && cells[i + 1].c == '\0';
            let needed = if pair { 2 } else { 1 };
            if col + needed > cols {
                if needed > cols {
                    if target == i || target == i + 1 {
                        return (row, col.min(cols - 1));
                    }
                    i += 2;
                    continue;
                }
                row += 1;
                col = 0;
            }
            if target == i || (pair && target == i + 1) {
                return (row, col);
            }
            col += needed;
            i += needed;
        }
        // Cursor in the trimmed trailing blanks: stay on the line's last row
        let extra = target.saturating_sub(len);
        (row, (col + extra).min(cols - 1))
    }

    /// Reflow a set of rows to new column width.
    /// `old_cols` is the previous column width, used to re-pad trimmed wrapped rows.
    fn reflow_rows(rows: Vec<Row>, old_cols: usize, new_cols: usize, blank: &Cell) -> Vec<Row> {
        let logical = Self::rows_to_logical_lines(rows, old_cols, blank);
        let mut result = Vec::new();
        for line in logical {
            result.extend(Self::wrap_logical_line(line, new_cols, blank));
        }
        result
    }

    // --- Compact reflow helpers ---


    pub fn reverse_index(&mut self) {
        self.pending_wrap = false;
        if self.cursor_y == self.scroll_top {
            // scroll_down sets dirty internally
            self.scroll_down(1);
        } else if self.cursor_y > 0 {
            self.cursor_y -= 1;
            self.cursor_moved();
        }
    }

    // --- Selection ---

    pub fn scrollback_len(&self) -> usize {
        self.scrollback.len()
    }

    /// Estimated heap bytes used by this terminal (grid + scrollback + alt_grid).
    pub fn mem_bytes(&self) -> usize {
        let cell_size = std::mem::size_of::<Cell>();
        let row_overhead = std::mem::size_of::<Row>();
        let row_bytes = |rows: &[Row]| -> usize {
            rows.iter().map(|r| row_overhead + r.cells.capacity() * cell_size).sum::<usize>()
        };
        let grid = row_bytes(&self.grid);
        let sb: usize = self.scrollback.iter()
            .map(|r| row_overhead + r.cells.capacity() * cell_size)
            .sum();
        let alt = self.alt_grid.as_ref().map(|g| row_bytes(g)).unwrap_or(0);
        grid + sb + alt
    }

    pub fn row_at(&self, abs_line: usize) -> Option<Cow<'_, Row>> {
        let sb_len = self.scrollback.len();
        if abs_line < sb_len {
            Some(Cow::Borrowed(&self.scrollback[abs_line]))
        } else {
            self.grid.get(abs_line - sb_len).map(Cow::Borrowed)
        }
    }

    /// Returns (start_col, end_col) of the word at the given position.
    /// A "word" is a contiguous run of non-whitespace, non-delimiter characters,
    /// or a single delimiter/whitespace.
    pub fn word_bounds_at(&self, pos: GridPos) -> (u16, u16) {
        let Some(row) = self.row_at(pos.line) else { return (pos.col, pos.col) };
        let cells = &row.cells;
        let col = pos.col as usize;
        if col >= cells.len() { return (pos.col, pos.col) }

        let ch = cells[col].c;
        let is_word_char = |c: char| -> bool {
            c.is_alphanumeric() || c == '_' || c == '-' || c == '.' || c == '/' || c == '~'
        };

        if is_word_char(ch) {
            let mut start = col;
            while start > 0 && is_word_char(cells[start - 1].c) {
                start -= 1;
            }
            let mut end = col;
            while end + 1 < cells.len() && is_word_char(cells[end + 1].c) {
                end += 1;
            }
            (start as u16, end as u16)
        } else {
            // Single non-word char (space, delimiter) — select just that char
            (pos.col, pos.col)
        }
    }

    fn ordered_selection(&self) -> Option<(GridPos, GridPos)> {
        let sel = self.selection.as_ref()?;
        if (sel.anchor.line, sel.anchor.col) <= (sel.end.line, sel.end.col) {
            Some((sel.anchor, sel.end))
        } else {
            Some((sel.end, sel.anchor))
        }
    }

    pub fn is_selected(&self, abs_line: usize, col: u16) -> bool {
        let Some((start, end)) = self.ordered_selection() else { return false };
        if abs_line < start.line || abs_line > end.line { return false; }
        if start.line == end.line {
            col >= start.col && col <= end.col
        } else if abs_line == start.line {
            col >= start.col
        } else if abs_line == end.line {
            col <= end.col
        } else {
            true
        }
    }

    pub fn selected_text(&self) -> String {
        let Some((start, end)) = self.ordered_selection() else { return String::new() };
        let mut result = String::new();
        for line_idx in start.line..=end.line {
            let Some(row) = self.row_at(line_idx) else { continue };
            let cells = &row.cells;
            if cells.is_empty() {
                // Blank scrollback row (trimmed) — treat as empty line
                if line_idx < end.line && !row.wrapped {
                    result.push('\n');
                }
                continue;
            }
            let col_start = if line_idx == start.line { start.col as usize } else { 0 };
            let col_end = if line_idx == end.line {
                (end.col as usize).min(cells.len() - 1)
            } else {
                cells.len() - 1
            };
            if col_start <= col_end {
                let text: String = cells[col_start..=col_end].iter().map(|c| {
                    if let Some(ref cluster) = c.cluster {
                        cluster.to_string()
                    } else if c.c == '\0' {
                        String::new()
                    } else {
                        c.c.to_string()
                    }
                }).collect();
                result.push_str(text.trim_end());
            }
            // Only insert newline if this row is NOT soft-wrapped
            if line_idx < end.line && !row.wrapped {
                result.push('\n');
            }
        }
        result
    }

    /// Like selected_text() but joins consecutive non-empty lines with a space.
    /// Empty lines (paragraph breaks) are preserved as newlines.
    pub fn selected_text_joined(&self) -> String {
        let raw = self.selected_text();
        if raw.is_empty() { return raw; }
        let mut result = String::new();
        let mut prev_blank = false;
        for (i, line) in raw.split('\n').enumerate() {
            if i == 0 {
                result.push_str(line);
                prev_blank = line.trim().is_empty();
                continue;
            }
            if line.trim().is_empty() {
                // Empty line = paragraph break (collapse consecutive blanks)
                if !prev_blank {
                    result.push('\n');
                    result.push('\n');
                }
                prev_blank = true;
            } else if prev_blank {
                result.push_str(line);
                prev_blank = false;
            } else {
                // Consecutive non-empty lines → join with space
                result.push(' ');
                result.push_str(line.trim_start());
                prev_blank = false;
            }
        }
        // Collapse runs of multiple spaces into a single space
        let mut prev_space = false;
        result.retain(|c| {
            if c == ' ' {
                if prev_space { return false; }
                prev_space = true;
            } else {
                prev_space = false;
            }
            true
        });
        result
    }

    pub fn clear_selection(&mut self) {
        if self.selection.is_some() {
            self.selection = None;
            self.dirty.store(true, Ordering::Relaxed);
        }
    }

    /// Search all lines (scrollback + grid) for a case-insensitive query.
    /// Returns matching lines as (absolute_line_index, line_text).
    pub fn search_lines(&self, query: &str) -> Vec<FilterMatch> {
        if query.is_empty() {
            return Vec::new();
        }
        let query_lower = query.to_lowercase();
        let mut results = Vec::new();
        let sb_len = self.scrollback.len();

        // Search scrollback
        for (i, row) in self.scrollback.iter().enumerate() {
            let text: String = row.cells.iter().map(|c| c.c).collect::<String>().trim_end().to_string();
            if text.to_lowercase().contains(&query_lower) {
                results.push(FilterMatch { abs_line: i, text });
            }
        }


        // Search grid
        for (i, row) in self.grid.iter().enumerate() {
            let text: String = row.cells.iter().map(|c| c.c).collect::<String>().trim_end().to_string();
            if text.to_lowercase().contains(&query_lower) {
                results.push(FilterMatch { abs_line: sb_len + i, text });
            }
        }

        results
    }

    /// Set scroll_offset to center a given absolute line in the viewport.
    /// If the line is near the edges, it will be as close to center as possible
    /// while staying within valid scroll bounds.
    pub fn scroll_to_abs_line(&mut self, abs_line: usize) {
        let sb_len = self.scrollback.len();
        if abs_line >= sb_len {
            self.reset_scroll();
        } else {
            let half_screen = self.rows as i32 / 2;
            let offset = (sb_len as i32).saturating_sub(abs_line as i32).saturating_add(half_screen);
            self.scroll_offset = offset.clamp(0, sb_len as i32);
        }
        self.cursor_moved();
    }

    /// Find a URL at the given visible row and column.
    /// Returns (col_start, col_end_exclusive, url_string) if found.
    /// Works on char indices (1 cell = 1 char = 1 column).
    /// Returns whether a visible row has the soft-wrap flag set.
    fn visible_row_wrapped(&self, visible_row: usize) -> bool {
        if self.scroll_offset == 0 {
            self.grid.get(visible_row).map_or(false, |r| r.wrapped)
        } else {
            let sb_len = self.scrollback.len() as i32;
            let offset = self.scroll_offset.min(sb_len);
            let sb_start = (sb_len - offset) as usize;
            let sb_visible = self.scrollback.len() - sb_start;
            if visible_row < sb_visible {
                self.scrollback.get(sb_start + visible_row).map_or(false, |r| r.wrapped)
            } else {
                let grid_idx = visible_row - sb_visible;
                self.grid.get(grid_idx).map_or(false, |r| r.wrapped)
            }
        }
    }

    /// Detect a URL at a given visible row/col position.
    /// Returns per-row highlight segments and the full URL string.
    /// Checks OSC 8 hyperlinks first, then falls back to auto-detection.
    pub fn url_at(&self, visible_row: usize, col: u16) -> Option<(Vec<(usize, u16, u16)>, String)> {
        let display = self.visible_lines();
        let cells = display.get(visible_row)?;
        let col = col as usize;
        if col >= cells.len() {
            return None;
        }

        // --- OSC 8 hyperlink check ---
        let hid = cells[col].hyperlink_id;
        if hid != 0 {
            if let Some(url) = self.hyperlink_url(hid) {
                let url = url.to_string();
                let mut segments = Vec::new();
                // Scan all visible rows for contiguous cells with the same hyperlink_id
                for r in 0..display.len() {
                    let row_cells = &display[r];
                    let mut seg_start: Option<usize> = None;
                    for (c, cell) in row_cells.iter().enumerate() {
                        if cell.hyperlink_id == hid {
                            if seg_start.is_none() {
                                seg_start = Some(c);
                            }
                        } else if let Some(s) = seg_start.take() {
                            segments.push((r, s as u16, c as u16));
                        }
                    }
                    if let Some(s) = seg_start {
                        segments.push((r, s as u16, row_cells.len() as u16));
                    }
                }
                return Some((segments, url));
            }
        }

        // --- Fallback: auto-detect http(s):// URLs ---

        // Find the start of the logical line (go back while previous row was wrapped)
        let mut first_row = visible_row;
        while first_row > 0 && self.visible_row_wrapped(first_row - 1) {
            first_row -= 1;
        }

        // Find the end of the logical line (go forward while current row is wrapped)
        let num_visible = display.len();
        let mut last_row = visible_row;
        while last_row < num_visible - 1 && self.visible_row_wrapped(last_row) {
            last_row += 1;
        }

        // Build a Vec<char> from the logical line
        let cols_per_row = self.cols as usize;
        let mut chars: Vec<char> = Vec::new();
        for r in first_row..=last_row {
            if let Some(row_cells) = display.get(r) {
                chars.extend(row_cells.iter().map(|c| c.c));
                // Pad trimmed scrollback rows to full width
                for _ in row_cells.len()..cols_per_row {
                    chars.push(' ');
                }
            }
        }
        let len = chars.len();

        // Adjusted col position within the logical line
        let logical_col = (visible_row - first_row) * cols_per_row + col;

        let mut i = 0;
        while i < len {
            // Check for "https://" (8 chars) or "http://" (7 chars)
            let prefix_len = if i + 8 <= len && chars[i..i + 8] == ['h', 't', 't', 'p', 's', ':', '/', '/'] {
                8
            } else if i + 7 <= len && chars[i..i + 7] == ['h', 't', 't', 'p', ':', '/', '/'] {
                7
            } else {
                i += 1;
                continue;
            };

            let start = i;
            let mut end = start + prefix_len;

            // Extend to end of URL (stop at whitespace, common delimiters, or null/space)
            while end < len {
                let ch = chars[end];
                if ch <= ' ' || ch == '"' || ch == '\'' || ch == '<' || ch == '>' || ch == '`' || ch == '\0' {
                    break;
                }
                end += 1;
            }
            // Strip trailing punctuation
            while end > start {
                let ch = chars[end - 1];
                if ch == '.' || ch == ',' || ch == ';' || ch == ':' || ch == ')' || ch == ']' {
                    end -= 1;
                } else {
                    break;
                }
            }

            if logical_col >= start && logical_col < end && end > start {
                let url: String = chars[start..end].iter().collect();

                // Build per-row highlight segments
                let mut segments = Vec::new();
                for r in first_row..=last_row {
                    let row_start_in_logical = (r - first_row) * cols_per_row;
                    let row_end_in_logical = row_start_in_logical + cols_per_row;
                    // Intersect [start..end) with [row_start..row_end)
                    let seg_start = start.max(row_start_in_logical);
                    let seg_end = end.min(row_end_in_logical);
                    if seg_start < seg_end {
                        let col_start = (seg_start - row_start_in_logical) as u16;
                        let col_end = (seg_end - row_start_in_logical) as u16;
                        segments.push((r, col_start, col_end));
                    }
                }

                return Some((segments, url));
            }

            i = if end > start { end } else { start + 1 };
        }
        None
    }

    /// Number of empty rows to push content down when screen isn't full.
    /// Used by both renderer and hit-test to keep visual and logical positions in sync.
    /// Disabled in alt screen (TUI apps) and when content doesn't start at line 0
    /// (explicit cursor positioning via escape sequences).
    pub fn y_offset_rows(&self) -> usize {
        if self.in_alt_screen || self.scroll_offset != 0 {
            return 0;
        }
        // Use self.grid directly (scroll_offset == 0 means visible_lines() == grid)
        // A cell with a non-default background is visible content even when
        // its char is a space (colored bands painted by TUIs).
        let default_bg = self.default_bg;
        let has_content = |row: &Row| row.cells.iter().any(|c| !c.is_blank() || c.bg != default_bg);
        let first_used = self.grid.iter().position(has_content);
        if first_used != Some(0) {
            return 0;
        }
        let last_used = self.grid.iter().rposition(has_content)
            .map_or(0, |i| i + 1);
        // Shell-like flow only: the cursor sits on the last content row (the
        // prompt) or below it. A TUI that paints from row 0 but parks its
        // cursor higher up (e.g. Claude Code's input box above its status
        // line) addresses the screen absolutely — shifting its content down
        // would desync the display from the app's model and leave a large
        // blank band where the app drew rows.
        if (self.cursor_y as usize) + 1 < last_used {
            return 0;
        }
        if last_used < self.rows as usize {
            self.rows as usize - last_used
        } else {
            0
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FG: [u8; 3] = DEFAULT_FG;
    const BG: [u8; 3] = DEFAULT_BG;

    fn term(cols: u16, rows: u16) -> TerminalState {
        TerminalState::new(cols, rows, 100, FG, BG)
    }

    fn put_str(t: &mut TerminalState, s: &str) {
        for c in s.chars() {
            t.put_char(c);
        }
    }

    fn row_text(t: &TerminalState, row: usize) -> String {
        t.visible_lines()[row]
            .iter()
            .filter(|c| c.c != '\0')
            .map(|c| c.c)
            .collect::<String>()
            .trim_end()
            .to_string()
    }

    // --- Deferred autowrap (xterm "last column flag") ---

    #[test]
    fn full_width_line_keeps_cursor_on_last_column() {
        let mut t = term(10, 5);
        put_str(&mut t, "0123456789");
        assert_eq!(t.cursor_x, 9, "cursor must stay on the last column, not pass it");
        assert_eq!(t.cursor_y, 0);
    }

    #[test]
    fn pending_wrap_wraps_on_next_char() {
        let mut t = term(10, 5);
        put_str(&mut t, "0123456789X");
        assert_eq!(row_text(&t, 0), "0123456789");
        assert_eq!(row_text(&t, 1), "X");
        assert_eq!(t.cursor_x, 1);
        assert_eq!(t.cursor_y, 1);
    }

    #[test]
    fn cursor_up_after_full_width_line_prints_in_place() {
        // The Claude Code glitch: full-width line, CUU, print must NOT wrap
        // to the next row (one-row shift of everything the app draws after).
        let mut t = term(10, 5);
        t.set_cursor_pos(2, 0);
        put_str(&mut t, "0123456789"); // fills row 2, pending wrap
        t.cursor_up(1);
        t.put_char('A');
        assert_eq!(t.cursor_y, 1, "char must land on row 1 (no spurious wrap)");
        assert_eq!(row_text(&t, 1), "         A");
        assert_eq!(row_text(&t, 2), "0123456789", "row 2 must be untouched");
    }

    #[test]
    fn no_spurious_scroll_at_bottom_after_cursor_move() {
        let mut t = term(10, 3);
        t.set_cursor_pos(0, 0);
        put_str(&mut t, "TOP");
        t.set_cursor_pos(2, 0);
        put_str(&mut t, "0123456789"); // fills bottom row, pending wrap
        t.cursor_up(1);
        t.put_char('A'); // must not scroll the screen
        assert_eq!(row_text(&t, 0), "TOP", "screen must not scroll");
        assert_eq!(t.scrollback_len(), 0);
    }

    #[test]
    fn carriage_return_clears_pending_wrap() {
        let mut t = term(10, 5);
        put_str(&mut t, "0123456789");
        t.carriage_return();
        t.put_char('A');
        assert_eq!(t.cursor_y, 0);
        assert_eq!(row_text(&t, 0), "A123456789");
    }

    #[test]
    fn linefeed_after_full_width_keeps_column() {
        // xterm: LF clears the wrap flag, column stays on the last column
        let mut t = term(10, 5);
        put_str(&mut t, "0123456789");
        t.newline();
        t.put_char('A');
        assert_eq!(t.cursor_y, 1);
        assert_eq!(row_text(&t, 1), "         A", "char prints at the preserved column");
    }

    #[test]
    fn erase_in_line_reaches_last_column_with_pending_wrap() {
        let mut t = term(10, 5);
        put_str(&mut t, "0123456789");
        t.erase_in_line(0); // erase from cursor (last column) to end
        assert_eq!(row_text(&t, 0), "012345678", "last column must be erased");
    }

    #[test]
    fn wide_char_filling_last_columns_sets_pending_wrap() {
        let mut t = term(10, 5);
        put_str(&mut t, "01234567");
        t.put_char('日'); // cols 8-9
        assert_eq!(t.cursor_x, 9);
        t.put_char('X');
        assert_eq!(t.cursor_y, 1);
        assert_eq!(row_text(&t, 1), "X");
    }

    // --- Cursor save/restore ---

    #[test]
    fn restore_cursor_clamped_after_shrink() {
        let mut t = term(80, 30);
        t.set_cursor_pos(25, 70);
        t.save_cursor();
        t.resize(40, 10);
        t.restore_cursor();
        assert!(t.cursor_y < 10, "restored cursor_y must be clamped");
        assert!(t.cursor_x < 40, "restored cursor_x must be clamped");
        // Output after restore must not be silently dropped
        t.put_char('A');
        let lines = t.visible_lines();
        let found = lines.iter().any(|l| l.iter().any(|c| c.c == 'A'));
        assert!(found, "output after a clamped restore must be visible");
    }

    // --- BCE (back-color erase) ---

    #[test]
    fn erase_in_line_uses_current_bg() {
        let mut t = term(10, 5);
        t.set_sgr(&[41]); // red background
        t.erase_in_line(2);
        let lines = t.visible_lines();
        let red = AnsiColor::from_index(1).to_rgb();
        assert!(lines[0].iter().all(|c| c.bg == red), "EL must fill with current bg");
    }

    #[test]
    fn erase_with_default_bg_unchanged() {
        let mut t = term(10, 5);
        t.erase_in_line(2);
        let lines = t.visible_lines();
        assert!(lines[0].iter().all(|c| c.bg == BG));
    }

    // --- SGR reverse video ---

    #[test]
    fn color_set_inside_reverse_lands_in_logical_slot() {
        let mut t = term(10, 5);
        t.set_sgr(&[7]);  // reverse on
        t.set_sgr(&[34]); // blue foreground (logical)
        t.put_char('x');
        let blue = AnsiColor::from_index(4).to_rgb();
        {
            let lines = t.visible_lines();
            assert_eq!(lines[0][0].bg, blue, "logical fg must display as bg under reverse");
            assert_eq!(lines[0][0].fg, BG, "logical bg must display as fg under reverse");
        }
        // Reverse off: same color now displays as foreground
        t.set_sgr(&[27]);
        t.put_char('y');
        let lines = t.visible_lines();
        assert_eq!(lines[0][1].fg, blue);
        assert_eq!(lines[0][1].bg, BG);
    }

    // --- Zero-width merge (chunk-boundary combining marks) ---

    #[test]
    fn standalone_combining_mark_merges_into_previous_cell() {
        let mut t = term(10, 5);
        t.put_char('e');
        t.put_char('\u{0301}'); // combining acute accent, arrives standalone
        assert_eq!(t.cursor_x, 1, "zero-width char must not advance the cursor");
        let lines = t.visible_lines();
        assert_eq!(lines[0][0].cluster.as_deref(), Some("e\u{0301}"));
        assert_eq!(lines[0][1].c, ' ', "next cell must stay untouched");
    }

    #[test]
    fn combining_mark_after_pending_wrap_targets_last_column() {
        let mut t = term(10, 5);
        put_str(&mut t, "0123456789");
        t.put_char('\u{0301}');
        let lines = t.visible_lines();
        assert_eq!(lines[0][9].cluster.as_deref(), Some("9\u{0301}"));
        assert_eq!(t.cursor_y, 0, "no wrap from a zero-width char");
    }

    // --- Bottom-anchoring gravity ---

    #[test]
    fn gravity_active_for_shell_flow() {
        let mut t = term(10, 10);
        put_str(&mut t, "$ "); // prompt on row 0, cursor on it
        assert!(t.y_offset_rows() > 0, "shell prompt should be pushed down");
    }

    #[test]
    fn gravity_disabled_when_cursor_above_content() {
        // TUI layout: content from row 0 to 4, cursor parked on row 1
        let mut t = term(10, 10);
        for row in 0..5u16 {
            t.set_cursor_pos(row, 0);
            t.put_char('x');
        }
        t.set_cursor_pos(1, 1);
        assert_eq!(t.y_offset_rows(), 0, "absolute-positioned TUI must not be shifted");
    }

    #[test]
    fn gravity_counts_colored_bg_as_content() {
        let mut t = term(10, 10);
        t.put_char('x'); // row 0 content
        t.set_sgr(&[41]);
        t.set_cursor_pos(9, 0);
        t.erase_in_line(2); // bottom row painted red, chars blank
        t.set_cursor_pos(0, 1);
        assert_eq!(t.y_offset_rows(), 0, "colored bottom row is visible content");
    }

    // --- Alt-screen resize ---

    #[test]
    fn alt_screen_shrink_preserves_primary_bottom_rows() {
        let mut t = term(10, 6);
        for i in 0..6u16 {
            t.set_cursor_pos(i, 0);
            put_str(&mut t, &format!("line{}", i));
        }
        t.enter_alt_screen();
        t.resize(10, 3);
        t.leave_alt_screen();
        // The bottom rows (most recent: prompt) must survive, top goes to scrollback
        assert_eq!(row_text(&t, 0), "line3");
        assert_eq!(row_text(&t, 1), "line4");
        assert_eq!(row_text(&t, 2), "line5");
        assert_eq!(t.scrollback_len(), 3);
    }

    // --- Scroll regions (regression net) ---

    #[test]
    fn reflow_keeps_line_straddling_scrollback_boundary() {
        // A logical line wrapping from scrollback into the grid must stay
        // joined through a column resize.
        let mut t = term(10, 3);
        // Fill several rows so the first ones scroll out
        for i in 0..3u16 {
            t.set_cursor_pos(t.rows - 1, 0);
            put_str(&mut t, &format!("fill{}", i));
            t.newline();
            t.carriage_return();
        }
        // One long soft-wrapped line spanning 25 chars over 10 cols
        put_str(&mut t, "ABCDEFGHIJKLMNOPQRSTUVWXY");
        t.newline();
        t.carriage_return();
        // Push enough lines for the wrapped line to straddle the boundary
        put_str(&mut t, "tail1");
        t.newline();
        t.carriage_return();
        assert!(t.scrollback_len() > 0);
        // Resize wider: the 25-char line must reassemble
        t.resize(30, 3);
        let mut all = String::new();
        for i in 0..t.scrollback_len() {
            // visible_lines only shows the grid; dump everything instead
        }
        let dump = t.dump_text(DumpMode::All, true).text;
        assert!(
            dump.contains("ABCDEFGHIJKLMNOPQRSTUVWXY"),
            "soft-wrapped line must rejoin across the scrollback boundary, got:\n{}",
            dump
        );
        let _ = all;
    }

    #[test]
    fn reflow_never_splits_wide_char_pair() {
        let mut t = term(10, 4);
        // 3 narrow + wide chars so a 7-col reflow boundary lands mid-pair
        put_str(&mut t, "abc日本語x");
        t.resize(7, 4);
        // Every '\0' continuation must directly follow its wide base
        let lines = t.visible_lines();
        for line in lines.iter() {
            for (i, cell) in line.iter().enumerate() {
                if cell.c == '\0' {
                    assert!(i > 0, "continuation cell at row start");
                    let prev = &line[i - 1];
                    assert!(prev.c != ' ' && prev.c != '\0',
                        "continuation cell must follow a wide base");
                }
            }
        }
        let dump = t.dump_text(DumpMode::All, true).text;
        assert!(dump.contains("日"), "wide chars must survive reflow: {}", dump);
        assert!(dump.contains("語"), "wide chars must survive reflow: {}", dump);
    }

    #[test]
    fn vs16_promotion_claims_continuation_cell() {
        let mut t = term(10, 3);
        t.put_char('\u{2764}'); // ❤ text presentation, width 1
        let before = t.cursor_x;
        t.put_char('\u{FE0F}'); // VS16 arrives standalone (chunk split)
        let lines = t.visible_lines();
        assert_eq!(lines[0][0].cluster.as_deref(), Some("\u{2764}\u{FE0F}"));
        // If unicode_width promotes the pair to width 2, the next column
        // must be claimed by a continuation cell and the cursor advanced.
        use unicode_width::UnicodeWidthStr;
        let w = UnicodeWidthStr::width("\u{2764}\u{FE0F}");
        if w == 2 {
            assert_eq!(lines[0][1].c, '\0');
            assert_eq!(t.cursor_x, before + 1);
        }
    }

    #[test]
    fn shrink_keeps_content_on_screen_over_trailing_blanks() {
        // Workflow repro: 10x4, "abcdefgh" on row 0, resize(5,4).
        // The wrapped line must stay fully on screen; trailing blank rows
        // must absorb the extra height instead of pushing content to
        // the scrollback.
        let mut t = term(10, 4);
        put_str(&mut t, "abcdefgh");
        t.resize(5, 4);
        assert_eq!(t.scrollback_len(), 0, "no content may leak into scrollback");
        assert_eq!(row_text(&t, 0), "abcde");
        assert_eq!(row_text(&t, 1), "fgh");
    }

    #[test]
    fn reflow_keeps_trailing_wide_char_continuation() {
        // Workflow repro: "abc日" then resize(4,4) — the wide base must keep
        // its continuation cell and never sit alone on a row's last column.
        let mut t = term(10, 4);
        put_str(&mut t, "abc日");
        t.resize(4, 4);
        let lines = t.visible_lines();
        for line in lines.iter() {
            for (i, cell) in line.iter().enumerate() {
                if cell.c == '日' {
                    assert!(i + 1 < line.len() && line[i + 1].c == '\0',
                        "wide base must be followed by its continuation");
                }
            }
        }
        let dump = t.dump_text(DumpMode::All, true).text;
        assert!(dump.contains('日'));
    }

    #[test]
    fn reflow_cursor_lands_after_content_with_wide_pads() {
        // Workflow repro: "日本語x" (7 cells, cursor at 7), resize(5,4).
        // The pad inserted at the wide-pair row boundary must not shift the
        // cursor onto 'x' — typing after the resize must append, not
        // overwrite.
        let mut t = term(10, 4);
        put_str(&mut t, "日本語x");
        t.resize(5, 4);
        t.put_char('X');
        let dump = t.dump_text(DumpMode::All, true).text;
        assert!(dump.contains('語'), "no glyph may be overwritten: {}", dump);
        assert!(dump.contains("xX"), "cursor must land right after 'x': {}", dump);
    }

    #[test]
    fn decrc_restores_charset_state() {
        let mut t = term(20, 5);
        t.save_cursor();
        t.set_charset(false, true); // G0 = DEC graphics
        t.put_char('q');
        {
            let lines = t.visible_lines();
            assert_eq!(lines[0][0].c, '─', "q drawn in DEC graphics");
        }
        t.restore_cursor(); // back to (0,0) AND ASCII designation
        t.cursor_forward(1); // don't overwrite the first cell
        t.put_char('q');
        let lines = t.visible_lines();
        assert_eq!(lines[0][1].c, 'q', "after DECRC, q must be plain ASCII");
    }

    #[test]
    fn huge_cursor_moves_do_not_overflow() {
        let mut t = term(10, 5);
        t.cursor_down(u16::MAX);
        t.cursor_forward(u16::MAX);
        assert_eq!(t.cursor_y, 4);
        assert_eq!(t.cursor_x, 9);
    }

    #[test]
    fn tab_preserves_pending_wrap() {
        let mut t = term(10, 3);
        put_str(&mut t, "0123456789"); // pending wrap set
        t.tab(); // HT at last column: no stop beyond -> stays, flag kept
        t.put_char('A');
        assert_eq!(t.cursor_y, 1, "wrap must still fire after HT");
        assert_eq!(row_text(&t, 1), "A");
    }

    #[test]
    fn decrc_restores_sgr_attributes() {
        let mut t = term(10, 5);
        t.set_sgr(&[34]); // blue fg
        t.save_cursor();
        t.set_sgr(&[0]);  // reset
        t.restore_cursor();
        t.put_char('x');
        let blue = AnsiColor::from_index(4).to_rgb();
        let lines = t.visible_lines();
        assert_eq!(lines[0][0].fg, blue, "DECRC must restore SGR attributes");
    }

    #[test]
    fn reflow_keeps_colored_bg_cells() {
        let mut t = term(10, 5);
        t.set_sgr(&[41]); // red bg
        t.erase_in_line(2); // row 0 painted red via BCE
        t.set_sgr(&[0]);
        t.set_cursor_pos(0, 0);
        t.put_char('x'); // ensure row 0 counts as content
        t.resize(8, 5);  // cols change -> reflow
        let red = AnsiColor::from_index(1).to_rgb();
        let lines = t.visible_lines();
        assert!(
            lines[0].iter().skip(1).take(6).all(|c| c.bg == red),
            "colored background must survive reflow"
        );
    }

    #[test]
    fn rep_repeats_last_char() {
        let mut t = term(20, 5);
        t.put_char('-');
        t.repeat_last_char(5);
        assert_eq!(row_text(&t, 0), "------");
        assert_eq!(t.cursor_x, 6);
    }

    #[test]
    fn scroll_region_scrolls_only_region() {
        let mut t = term(10, 5);
        for i in 0..5u16 {
            t.set_cursor_pos(i, 0);
            put_str(&mut t, &format!("L{}", i));
        }
        t.set_scroll_region(1, 3);
        t.set_cursor_pos(3, 0); // bottom of region
        t.newline();            // scrolls region only
        assert_eq!(row_text(&t, 0), "L0", "above region untouched");
        assert_eq!(row_text(&t, 1), "L2");
        assert_eq!(row_text(&t, 2), "L3");
        assert_eq!(row_text(&t, 3), "", "scrolled-in blank row");
        assert_eq!(row_text(&t, 4), "L4", "below region untouched");
    }

}
