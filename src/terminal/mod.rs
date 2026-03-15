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
        let last = self.cells.iter().rposition(|c| {
            (c.c != ' ' && c.c != '\0')
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
    saved_cursor: Option<(u16, u16)>,
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
    // Last command executed (set via OSC 7777 from shell integration)
    pub last_command: Option<String>,
    // Kitty keyboard protocol — stack of pushed flag sets
    pub kitty_keyboard_flags: Vec<u8>,
    // Printable character counter (displayed in status bars)
    pub printable_chars: AtomicU64,
}

/// A single line matching a filter query.
#[derive(Clone, Debug)]
pub struct FilterMatch {
    pub abs_line: usize,
    pub text: String,
}

impl TerminalState {
    pub fn new(cols: u16, rows: u16, scrollback_limit: usize, fg: [u8; 3], bg: [u8; 3]) -> Self {
        let blank = Cell { c: ' ', cluster: None, fg, bg };
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
            last_command: None,
            kitty_keyboard_flags: Vec::new(),
            printable_chars: AtomicU64::new(0),
        }
    }

    pub fn kitty_flags(&self) -> u8 {
        self.kitty_keyboard_flags.last().copied().unwrap_or(0)
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

    pub fn put_char(&mut self, c: char) {
        if c >= '\u{2500}' && c <= '\u{257F}' {
            log::trace!("put_char box-drawing: '{}' U+{:04X} at ({}, {})", c, c as u32, self.cursor_x, self.cursor_y);
        }
        self.cursor_moved();

        let char_width = UnicodeWidthChar::width(c).unwrap_or(1) as u16;

        // Wide char at last column: wrap before writing
        if char_width == 2 && self.cursor_x == self.cols - 1 && self.auto_wrap {
            // Fill last column with space, then wrap
            let row = self.cursor_y as usize;
            if row < self.grid.len() {
                let col = self.cursor_x as usize;
                if col < self.grid[row].cells.len() {
                    self.grid[row].cells[col] = self.blank.clone();
                }
                self.grid[row].wrapped = true;
            }
            self.cursor_x = 0;
            self.advance_line();
        }

        if self.cursor_x >= self.cols {
            if !self.auto_wrap {
                // DECAWM off: stay at last column, overwrite
                self.cursor_x = self.cols - 1;
            } else {
                // Mark current row as soft-wrapped before advancing
                let row = self.cursor_y as usize;
                if row < self.grid.len() {
                    self.grid[row].wrapped = true;
                }
                self.cursor_x = 0;
                self.advance_line();
            }
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
            self.grid[row].cells[col] = Cell {
                c,
                cluster: None,
                fg,
                bg: self.current_bg,
            };

            // Wide char: write placeholder '\0' in the next column
            if char_width == 2 && col + 1 < self.grid[row].cells.len() {
                self.grid[row].cells[col + 1] = Cell {
                    c: '\0',
                    cluster: None,
                    fg,
                    bg: self.current_bg,
                };
            }
        }
        self.cursor_x += char_width;
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
        let display_width = UnicodeWidthStr::width(cluster).max(1) as u16;

        self.cursor_moved();

        // Wide cluster at last column: wrap before writing
        if display_width >= 2 && self.cursor_x + display_width > self.cols && self.auto_wrap {
            let row = self.cursor_y as usize;
            if row < self.grid.len() {
                self.grid[row].wrapped = true;
            }
            self.cursor_x = 0;
            self.advance_line();
        }

        if self.cursor_x >= self.cols {
            if !self.auto_wrap {
                self.cursor_x = self.cols - 1;
            } else {
                let row = self.cursor_y as usize;
                if row < self.grid.len() {
                    self.grid[row].wrapped = true;
                }
                self.cursor_x = 0;
                self.advance_line();
            }
        }

        let row = self.cursor_y as usize;
        let col = self.cursor_x as usize;
        if row < self.grid.len() && col < self.grid[row].cells.len() {
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

            self.grid[row].cells[col] = Cell {
                c: first,
                cluster: Some(cluster.into()),
                fg,
                bg: self.current_bg,
            };

            // Write '\0' sentinel for remaining columns
            for i in 1..display_width as usize {
                if col + i < self.grid[row].cells.len() {
                    self.grid[row].cells[col + i] = Cell {
                        c: '\0',
                        cluster: None,
                        fg,
                        bg: self.current_bg,
                    };
                }
            }
        }
        self.cursor_x += display_width;
    }

    pub fn newline(&mut self) {
        self.advance_line();
    }

    pub fn carriage_return(&mut self) {
        self.cursor_x = 0;
        self.cursor_moved();
    }

    pub fn backspace(&mut self) {
        if self.cursor_x > 0 {
            self.cursor_x -= 1;
            self.cursor_moved();
        }
    }

    pub fn tab(&mut self) {
        let next_tab = ((self.cursor_x / 8) + 1) * 8;
        self.cursor_x = next_tab.min(self.cols - 1);
        self.cursor_moved();
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

        for _ in 0..n {
            if top < self.grid.len() {
                let line = self.grid.remove(top);
                if top == 0 && !self.in_alt_screen {
                    self.push_to_scrollback(line);
                }
            }
            let new_line = Row::new(self.cols as usize, &self.blank);
            let insert_pos = bottom.min(self.grid.len());
            self.grid.insert(insert_pos, new_line);
        }

        // Ensure grid has correct number of rows
        self.grid.resize(self.rows as usize, Row::new(self.cols as usize, &self.blank));
    }

    fn scroll_down(&mut self, n: u16) {
        let top = self.scroll_top as usize;
        let bottom = self.scroll_bottom as usize;

        for _ in 0..n {
            if bottom < self.grid.len() {
                self.grid.remove(bottom);
            }
            let new_line = Row::new(self.cols as usize, &self.blank);
            self.grid.insert(top, new_line);
        }

        self.grid.resize(self.rows as usize, Row::new(self.cols as usize, &self.blank));
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
                7 => {
                    if !self.reversed {
                        std::mem::swap(&mut self.current_fg, &mut self.current_bg);
                        self.reversed = true;
                    }
                }
                27 => {
                    if self.reversed {
                        std::mem::swap(&mut self.current_fg, &mut self.current_bg);
                        self.reversed = false;
                    }
                }
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
        self.dirty.store(true, Ordering::Relaxed);
        match mode {
            0 => {
                // Erase from cursor to end
                let row = self.cursor_y as usize;
                let col = self.cursor_x as usize;
                if row < self.grid.len() {
                    for c in col..self.grid[row].cells.len() {
                        self.grid[row].cells[c] = self.blank.clone();
                    }
                    self.grid[row].wrapped = false;
                    for r in (row + 1)..self.grid.len() {
                        for c in 0..self.grid[r].cells.len() {
                            self.grid[r].cells[c] = self.blank.clone();
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
                            self.grid[r].cells[c] = self.blank.clone();
                        }
                        self.grid[r].wrapped = false;
                    }
                }
                if row < self.grid.len() {
                    for c in 0..=col.min(self.grid[row].cells.len().saturating_sub(1)) {
                        self.grid[row].cells[c] = self.blank.clone();
                    }
                }
            }
            2 | 3 => {
                // Erase entire display — push current content to scrollback first
                if !self.in_alt_screen {
                    // Find last row with visible content to avoid trailing blanks
                    let last_content = self.grid.iter().rposition(|row|
                        row.cells.iter().any(|c| !c.is_blank())
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
                        *cell = self.blank.clone();
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
    }

    pub fn erase_in_line(&mut self, mode: u16) {
        self.dirty.store(true, Ordering::Relaxed);
        let row = self.cursor_y as usize;
        if row >= self.grid.len() {
            return;
        }
        match mode {
            0 => {
                for c in (self.cursor_x as usize)..self.grid[row].cells.len() {
                    self.grid[row].cells[c] = self.blank.clone();
                }
                self.grid[row].wrapped = false;
            }
            1 => {
                for c in 0..=(self.cursor_x as usize).min(self.grid[row].cells.len().saturating_sub(1)) {
                    self.grid[row].cells[c] = self.blank.clone();
                }
            }
            2 => {
                for cell in self.grid[row].cells.iter_mut() {
                    *cell = self.blank.clone();
                }
                self.grid[row].wrapped = false;
            }
            _ => {}
        }
    }

    pub fn cursor_up(&mut self, n: u16) {
        self.cursor_y = self.cursor_y.saturating_sub(n);
        self.cursor_moved();
    }

    pub fn cursor_down(&mut self, n: u16) {
        self.cursor_y = (self.cursor_y + n).min(self.rows - 1);
        self.cursor_moved();
    }

    pub fn cursor_forward(&mut self, n: u16) {
        self.cursor_x = (self.cursor_x + n).min(self.cols - 1);
        self.cursor_moved();
    }

    pub fn cursor_backward(&mut self, n: u16) {
        self.cursor_x = self.cursor_x.saturating_sub(n);
        self.cursor_moved();
    }

    pub fn set_cursor_pos(&mut self, row: u16, col: u16) {
        self.cursor_y = row.min(self.rows - 1);
        self.cursor_x = col.min(self.cols - 1);
        self.cursor_moved();
    }

    pub fn save_cursor(&mut self) {
        self.saved_cursor = Some((self.cursor_x, self.cursor_y));
    }

    pub fn restore_cursor(&mut self) {
        if let Some((x, y)) = self.saved_cursor {
            self.cursor_x = x;
            self.cursor_y = y;
        }
    }

    pub fn insert_lines(&mut self, n: u16) {
        let row = self.cursor_y as usize;
        let bottom = self.scroll_bottom as usize;
        for _ in 0..n {
            if bottom < self.grid.len() {
                self.grid.remove(bottom);
            }
            self.grid.insert(row, Row::new(self.cols as usize, &self.blank));
        }
        self.grid.resize(self.rows as usize, Row::new(self.cols as usize, &self.blank));
    }

    pub fn delete_lines(&mut self, n: u16) {
        let row = self.cursor_y as usize;
        let bottom = self.scroll_bottom as usize;
        for _ in 0..n {
            if row < self.grid.len() {
                self.grid.remove(row);
            }
            let insert_pos = bottom.min(self.grid.len());
            self.grid.insert(insert_pos, Row::new(self.cols as usize, &self.blank));
        }
        self.grid.resize(self.rows as usize, Row::new(self.cols as usize, &self.blank));
    }

    pub fn delete_chars(&mut self, n: u16) {
        let row = self.cursor_y as usize;
        let col = self.cursor_x as usize;
        if row < self.grid.len() {
            for _ in 0..n {
                if col < self.grid[row].cells.len() {
                    self.grid[row].cells.remove(col);
                    self.grid[row].cells.push(self.blank.clone());
                }
            }
        }
    }

    pub fn insert_chars(&mut self, n: u16) {
        let row = self.cursor_y as usize;
        let col = self.cursor_x as usize;
        if row < self.grid.len() {
            let cells = &mut self.grid[row].cells;
            for _ in 0..n {
                if col < cells.len() {
                    cells.pop();
                    cells.insert(col, self.blank.clone());
                }
            }
        }
        self.dirty.store(true, Ordering::Relaxed);
    }

    pub fn erase_chars(&mut self, n: u16) {
        let row = self.cursor_y as usize;
        let col = self.cursor_x as usize;
        if row < self.grid.len() {
            for i in 0..n as usize {
                if col + i < self.grid[row].cells.len() {
                    self.grid[row].cells[col + i] = self.blank.clone();
                }
            }
        }
    }

    pub fn set_scroll_region(&mut self, top: u16, bottom: u16) {
        self.scroll_top = top;
        self.scroll_bottom = bottom.min(self.rows - 1);
        self.cursor_x = 0;
        self.cursor_y = if self.origin_mode { self.scroll_top } else { 0 };
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
            self.cursor_x = x;
            self.cursor_y = y;
        }
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

    pub fn resize(&mut self, new_cols: u16, new_rows: u16) {
        if new_cols == self.cols && new_rows == self.rows {
            return;
        }

        let old_cols = self.cols;
        self.cols = new_cols;
        self.rows = new_rows;

        // Alt screen: no reflow, just truncate/pad
        if self.in_alt_screen {
            for row in &mut self.grid {
                row.cells.resize(new_cols as usize, self.blank.clone());
            }
            let nr = new_rows as usize;
            self.grid.resize(nr, Row::new(new_cols as usize, &self.blank));

            if let Some(ref mut alt_grid) = self.alt_grid {
                for row in alt_grid.iter_mut() {
                    row.cells.resize(new_cols as usize, self.blank.clone());
                }
                alt_grid.resize(nr, Row::new(new_cols as usize, &self.blank));
            }

            self.cursor_x = self.cursor_x.min(new_cols.saturating_sub(1));
            self.cursor_y = self.cursor_y.min(new_rows.saturating_sub(1));
            self.scroll_top = 0;
            self.scroll_bottom = new_rows.saturating_sub(1);
            self.reset_scroll();
            self.dirty.store(true, Ordering::Relaxed);
            return;
        }

        if new_cols != old_cols {
            // --- Reflow ---

            // Compute cursor position as (logical_line_index, offset_within_logical_line)
            // by walking the grid rows before reflow.
            let cursor_row = self.cursor_y as usize;
            let cursor_col = self.cursor_x as usize;
            let mut logical_line_idx: usize = 0;
            let mut offset_in_logical: usize = 0;
            {
                let mut cells_in_current_logical: usize = 0;
                for (i, row) in self.grid.iter().enumerate() {
                    if i == cursor_row {
                        offset_in_logical = cells_in_current_logical + cursor_col;
                        break;
                    }
                    cells_in_current_logical += row.cells.len();
                    if !row.wrapped {
                        logical_line_idx += 1;
                        cells_in_current_logical = 0;
                    }
                }
            }

            // Reflow scrollback
            let sb: Vec<Row> = self.scrollback.drain(..).collect();
            let reflowed_sb = Self::reflow_rows(sb, old_cols as usize, new_cols as usize, &self.blank);
            self.scrollback = reflowed_sb.into();

            // Reflow grid
            let grid = std::mem::take(&mut self.grid);
            let reflowed_grid = Self::reflow_rows(grid, old_cols as usize, new_cols as usize, &self.blank);

            // Find cursor in reflowed grid: skip logical_line_idx logical lines,
            // then walk offset_in_logical cells into that logical line.
            let mut new_cy: u16 = 0;
            let mut new_cx: u16 = 0;
            let mut ll = 0; // current logical line counter
            let mut i = 0;
            // Skip past logical_line_idx complete logical lines
            while i < reflowed_grid.len() && ll < logical_line_idx {
                if !reflowed_grid[i].wrapped {
                    ll += 1;
                }
                i += 1;
            }
            // Now walk offset_in_logical cells within the target logical line
            let mut remaining = offset_in_logical;
            while i < reflowed_grid.len() {
                let row_len = reflowed_grid[i].cells.len();
                if remaining < row_len || !reflowed_grid[i].wrapped {
                    new_cy = i as u16;
                    new_cx = (remaining as u16).min(new_cols.saturating_sub(1));
                    break;
                }
                remaining -= row_len;
                i += 1;
            }
            if i >= reflowed_grid.len() {
                // Cursor beyond grid — clamp to last row
                new_cy = reflowed_grid.len().saturating_sub(1) as u16;
                new_cx = 0;
            }

            self.grid = reflowed_grid;
            self.cursor_x = new_cx;
            self.cursor_y = new_cy;
        }

        // Adjust grid to new_rows
        let nr = new_rows as usize;
        // Remove blank rows from bottom first
        while self.grid.len() > nr {
            let is_blank = self.grid.last()
                .map(|row| row.cells.iter().all(|c| c.is_blank()))
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

        // Clamp cursor
        self.cursor_x = self.cursor_x.min(new_cols.saturating_sub(1));
        self.cursor_y = self.cursor_y.min(new_rows.saturating_sub(1));

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

    /// Wrap a logical line to new_cols, trimming trailing blanks
    fn wrap_logical_line(cells: Vec<Cell>, new_cols: usize, blank: &Cell) -> Vec<Row> {
        // Trim trailing blank cells
        let len = cells.iter().rposition(|c| !c.is_blank())
            .map_or(0, |i| i + 1);

        if len == 0 {
            // Entirely blank logical line → single blank row (hard newline)
            return vec![Row::new(new_cols, blank)];
        }

        let trimmed = &cells[..len];
        let mut rows = Vec::new();
        for chunk in trimmed.chunks(new_cols) {
            let mut row_cells = chunk.to_vec();
            row_cells.resize(new_cols, blank.clone());
            rows.push(Row { cells: row_cells, wrapped: true });
        }
        // Last row of a logical line is not wrapped (it ends with a hard newline)
        if let Some(last) = rows.last_mut() {
            last.wrapped = false;
        }
        rows
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
        if self.cursor_y == self.scroll_top {
            self.scroll_down(1);
        } else if self.cursor_y > 0 {
            self.cursor_y -= 1;
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
    /// Handles URLs that span multiple soft-wrapped rows.
    pub fn url_at(&self, visible_row: usize, col: u16) -> Option<(Vec<(usize, u16, u16)>, String)> {
        let display = self.visible_lines();
        let cells = display.get(visible_row)?;
        let col = col as usize;
        if col >= cells.len() {
            return None;
        }

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
        let has_content = |row: &Row| row.cells.iter().any(|c| !c.is_blank());
        let first_used = self.grid.iter().position(has_content);
        if first_used != Some(0) {
            return 0;
        }
        let last_used = self.grid.iter().rposition(has_content)
            .map_or(0, |i| i + 1);
        if last_used < self.rows as usize {
            self.rows as usize - last_used
        } else {
            0
        }
    }
}
