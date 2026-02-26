pub mod parser;
pub mod pty;

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::Instant;

use crate::terminal::parser::AnsiColor;

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum CursorShape {
    Block,
    Underline,
    Bar,
}

pub const DEFAULT_FG: [f32; 3] = [1.0, 1.0, 1.0];
pub const DEFAULT_BG: [f32; 3] = [0.1, 0.1, 0.12];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GridPos {
    /// Absolute line: 0 = first scrollback line, scrollback.len() = first grid line
    pub line: usize,
    pub col: u16,
}

#[derive(Clone, Debug)]
pub struct Selection {
    pub anchor: GridPos,
    pub end: GridPos,
}

#[derive(Clone, Debug)]
pub struct Cell {
    pub c: char,
    pub fg: [f32; 3],
    pub bg: [f32; 3],
}

impl Default for Cell {
    fn default() -> Self {
        Cell {
            c: ' ',
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
}

pub struct TerminalState {
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
    pub default_fg: [f32; 3],
    pub default_bg: [f32; 3],
    blank: Cell,
    // SGR state
    current_fg: [f32; 3],
    current_bg: [f32; 3],
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
    // Last command executed (set via OSC 7777 from shell integration)
    pub last_command: Option<String>,
}

/// A single line matching a filter query.
#[derive(Clone, Debug)]
pub struct FilterMatch {
    pub abs_line: usize,
    pub text: String,
}

impl TerminalState {
    pub fn new(cols: u16, rows: u16, scrollback_limit: usize, fg: [f32; 3], bg: [f32; 3]) -> Self {
        let blank = Cell { c: ' ', fg, bg };
        let grid = (0..rows as usize).map(|_| Row::new(cols as usize, &blank)).collect();
        TerminalState {
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
            selection: None,
            synchronized_output: false,
            sync_output_since: None,
            bracketed_paste: false,
            cursor_keys_application: false,
            auto_wrap: true,
            insert_mode: false,
            bell: AtomicBool::new(false),
            last_command: None,
        }
    }

    pub fn visible_lines(&self) -> Vec<&[Cell]> {
        if self.scroll_offset == 0 {
            self.grid.iter().map(|r| r.cells.as_slice()).collect()
        } else {
            let sb_len = self.scrollback.len() as i32;
            let offset = self.scroll_offset.min(sb_len);
            let sb_start = (sb_len - offset) as usize;
            let grid_end = (self.rows as i32 - offset).max(0) as usize;

            let mut lines: Vec<&[Cell]> = Vec::with_capacity(self.rows as usize);
            for i in sb_start..self.scrollback.len() {
                lines.push(&self.scrollback[i].cells);
            }
            for i in 0..grid_end.min(self.grid.len()) {
                lines.push(&self.grid[i].cells);
            }
            // No padding — truncate handles the upper bound
            lines.truncate(self.rows as usize);
            lines
        }
    }

    pub fn scroll(&mut self, lines: i32) {
        if self.in_alt_screen {
            return; // No scrollback in alt screen
        }
        let max_offset = self.scrollback.len() as i32;
        self.scroll_offset = (self.scroll_offset + lines).clamp(0, max_offset);
        self.user_scrolled = self.scroll_offset > 0;
        self.cursor_moved();
    }

    pub fn put_char(&mut self, c: char) {
        if c >= '\u{2500}' && c <= '\u{257F}' {
            log::trace!("put_char box-drawing: '{}' U+{:04X} at ({}, {})", c, c as u32, self.cursor_x, self.cursor_y);
        }
        self.cursor_moved();
        if !self.user_scrolled {
            self.reset_scroll();
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
                fg = [fg[0] * 0.5, fg[1] * 0.5, fg[2] * 0.5];
            }
            if self.bold {
                fg = [
                    (fg[0] * 1.3).min(1.0),
                    (fg[1] * 1.3).min(1.0),
                    (fg[2] * 1.3).min(1.0),
                ];
            }
            self.grid[row].cells[col] = Cell {
                c,
                fg,
                bg: self.current_bg,
            };
        }
        self.cursor_x += 1;
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

    fn scroll_up(&mut self, n: u16) {
        let top = self.scroll_top as usize;
        let bottom = self.scroll_bottom as usize;

        for _ in 0..n {
            if top < self.grid.len() {
                let line = self.grid.remove(top);
                if top == 0 && !self.in_alt_screen {
                    self.scrollback.push_back(line);
                    if self.scrollback.len() > self.scrollback_limit {
                        self.scrollback.pop_front();
                    }
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
                            params[i + 2] as f32 / 255.0,
                            params[i + 3] as f32 / 255.0,
                            params[i + 4] as f32 / 255.0,
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
                            params[i + 2] as f32 / 255.0,
                            params[i + 3] as f32 / 255.0,
                            params[i + 4] as f32 / 255.0,
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
                // Erase entire display
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
            let reflowed_sb = Self::reflow_rows(sb, new_cols as usize, &self.blank);
            self.scrollback = reflowed_sb.into();

            // Reflow grid
            let grid = std::mem::take(&mut self.grid);
            let reflowed_grid = Self::reflow_rows(grid, new_cols as usize, &self.blank);

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
                .map(|row| row.cells.iter().all(|c| c.c == ' ' || c.c == '\0'))
                .unwrap_or(true);
            if is_blank && self.grid.len() > self.cursor_y as usize + 1 {
                self.grid.pop();
            } else {
                break;
            }
        }
        // Push excess top rows into scrollback
        while self.grid.len() > nr {
            let line = self.grid.remove(0);
            self.scrollback.push_back(line);
            self.cursor_y = self.cursor_y.saturating_sub(1);
        }
        // Add blank rows if needed
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

    /// Concatenate consecutive wrapped rows into logical lines
    fn rows_to_logical_lines(rows: Vec<Row>) -> Vec<Vec<Cell>> {
        let mut lines: Vec<Vec<Cell>> = Vec::new();
        let mut current: Vec<Cell> = Vec::new();
        for row in rows {
            current.extend(row.cells);
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
        let len = cells.iter().rposition(|c| c.c != ' ' && c.c != '\0')
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

    /// Reflow a set of rows to new column width
    fn reflow_rows(rows: Vec<Row>, new_cols: usize, blank: &Cell) -> Vec<Row> {
        let logical = Self::rows_to_logical_lines(rows);
        let mut result = Vec::new();
        for line in logical {
            result.extend(Self::wrap_logical_line(line, new_cols, blank));
        }
        result
    }

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

    fn row_at(&self, abs_line: usize) -> Option<&Row> {
        let sb_len = self.scrollback.len();
        if abs_line < sb_len {
            Some(&self.scrollback[abs_line])
        } else {
            self.grid.get(abs_line - sb_len)
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
            let col_start = if line_idx == start.line { start.col as usize } else { 0 };
            let col_end = if line_idx == end.line {
                (end.col as usize).min(cells.len().saturating_sub(1))
            } else {
                cells.len().saturating_sub(1)
            };
            if col_start <= col_end {
                let text: String = cells[col_start..=col_end].iter().map(|c| c.c).collect();
                result.push_str(text.trim_end());
            }
            // Only insert newline if this row is NOT soft-wrapped
            if line_idx < end.line && !row.wrapped {
                result.push('\n');
            }
        }
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
    pub fn scroll_to_abs_line(&mut self, abs_line: usize) {
        let sb_len = self.scrollback.len();
        if abs_line >= sb_len {
            // Line is in the grid — scroll to bottom
            self.reset_scroll();
        } else {
            // Center the line in the viewport
            let half_screen = self.rows as i32 / 2;
            let offset = sb_len as i32 - abs_line as i32 + half_screen;
            self.scroll_offset = offset.clamp(0, sb_len as i32);
        }
        self.cursor_moved();
    }

    /// Find a URL at the given visible row and column.
    /// Returns (col_start, col_end_exclusive, url_string) if found.
    /// Works on char indices (1 cell = 1 char = 1 column).
    pub fn url_at(&self, visible_row: usize, col: u16) -> Option<(u16, u16, String)> {
        let display = self.visible_lines();
        let cells = display.get(visible_row)?;
        let col = col as usize;
        if col >= cells.len() {
            return None;
        }

        // Build a Vec<char> from cells for easy scanning
        let chars: Vec<char> = cells.iter().map(|c| c.c).collect();
        let len = chars.len();

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

            if col >= start && col < end && end > start {
                let url: String = chars[start..end].iter().collect();
                return Some((start as u16, end as u16, url));
            }

            i = if end > start { end } else { start + 1 };
        }
        None
    }

    /// Number of empty rows at top when screen isn't full (for pixel→grid conversion)
    pub fn y_offset_rows(&self) -> usize {
        if self.scroll_offset != 0 { return 0; }
        let display = self.visible_lines();
        let last_used = display.iter().rposition(|line|
            line.iter().any(|c| c.c != ' ' && c.c != '\0')
        ).map_or(0, |i| i + 1);
        if last_used < self.rows as usize {
            self.rows as usize - last_used
        } else {
            0
        }
    }
}
