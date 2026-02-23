pub mod parser;
pub mod pty;

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use crate::terminal::parser::AnsiColor;

pub const DEFAULT_FG: [f32; 3] = [1.0, 1.0, 1.0];
pub const DEFAULT_BG: [f32; 3] = [0.1, 0.1, 0.12];

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

pub struct TerminalState {
    pub cols: u16,
    pub rows: u16,
    grid: Vec<Vec<Cell>>,
    scrollback: VecDeque<Vec<Cell>>,
    pub scrollback_limit: usize,
    pub cursor_x: u16,
    pub cursor_y: u16,
    scroll_offset: i32,
    // Config colors used as defaults
    pub default_fg: [f32; 3],
    pub default_bg: [f32; 3],
    blank: Cell,
    // SGR state
    current_fg: [f32; 3],
    current_bg: [f32; 3],
    reversed: bool,
    // Saved cursor
    saved_cursor: Option<(u16, u16)>,
    // Scroll region
    scroll_top: u16,
    scroll_bottom: u16,
    // Origin mode
    origin_mode: bool,
    // Cursor visibility (DECTCEM)
    pub cursor_visible: bool,
    // Incremented on every cursor move to reset blink phase
    pub cursor_move_epoch: AtomicU32,
    // Dirty flag for render optimization
    pub dirty: AtomicBool,
    // Alternate screen buffer
    alt_grid: Option<Vec<Vec<Cell>>>,
    alt_cursor: Option<(u16, u16)>,
    in_alt_screen: bool,
}

impl TerminalState {
    pub fn new(cols: u16, rows: u16, scrollback_limit: usize, fg: [f32; 3], bg: [f32; 3]) -> Self {
        let blank = Cell { c: ' ', fg, bg };
        let grid = vec![vec![blank.clone(); cols as usize]; rows as usize];
        TerminalState {
            cols,
            rows,
            grid,
            scrollback: VecDeque::new(),
            scrollback_limit,
            cursor_x: 0,
            cursor_y: 0,
            scroll_offset: 0,
            default_fg: fg,
            default_bg: bg,
            blank,
            current_fg: fg,
            current_bg: bg,
            reversed: false,
            saved_cursor: None,
            scroll_top: 0,
            scroll_bottom: rows.saturating_sub(1),
            origin_mode: false,
            cursor_visible: true,
            cursor_move_epoch: AtomicU32::new(0),
            dirty: AtomicBool::new(true),
            alt_grid: None,
            alt_cursor: None,
            in_alt_screen: false,
        }
    }

    pub fn visible_lines(&self) -> Vec<&Vec<Cell>> {
        if self.scroll_offset == 0 {
            self.grid.iter().collect()
        } else {
            let sb_len = self.scrollback.len() as i32;
            let offset = self.scroll_offset.min(sb_len);
            let sb_start = (sb_len - offset) as usize;
            let grid_end = (self.rows as i32 - offset).max(0) as usize;

            let mut lines: Vec<&Vec<Cell>> = Vec::with_capacity(self.rows as usize);
            for i in sb_start..self.scrollback.len() {
                lines.push(&self.scrollback[i]);
            }
            for i in 0..grid_end.min(self.grid.len()) {
                lines.push(&self.grid[i]);
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
        self.cursor_moved();
    }

    pub fn put_char(&mut self, c: char) {
        self.cursor_moved();
        self.scroll_offset = 0; // Auto-scroll on new output

        if self.cursor_x >= self.cols {
            self.cursor_x = 0;
            self.advance_line();
        }

        let row = self.cursor_y as usize;
        let col = self.cursor_x as usize;
        if row < self.grid.len() && col < self.grid[row].len() {
            self.grid[row][col] = Cell {
                c,
                fg: self.current_fg,
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
            let new_line = vec![self.blank.clone(); self.cols as usize];
            let insert_pos = bottom.min(self.grid.len());
            self.grid.insert(insert_pos, new_line);
        }

        // Ensure grid has correct number of rows
        self.grid.resize(self.rows as usize, vec![self.blank.clone(); self.cols as usize]);
    }

    fn scroll_down(&mut self, n: u16) {
        let top = self.scroll_top as usize;
        let bottom = self.scroll_bottom as usize;

        for _ in 0..n {
            if bottom < self.grid.len() {
                self.grid.remove(bottom);
            }
            let new_line = vec![self.blank.clone(); self.cols as usize];
            self.grid.insert(top, new_line);
        }

        self.grid.resize(self.rows as usize, vec![self.blank.clone(); self.cols as usize]);
    }

    pub fn set_sgr(&mut self, params: &[u16]) {
        let mut i = 0;
        while i < params.len() {
            match params[i] {
                0 => {
                    self.current_fg = self.default_fg;
                    self.current_bg = self.default_bg;
                    self.reversed = false;
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
                    for c in col..self.grid[row].len() {
                        self.grid[row][c] = self.blank.clone();
                    }
                    for r in (row + 1)..self.grid.len() {
                        for c in 0..self.grid[r].len() {
                            self.grid[r][c] = self.blank.clone();
                        }
                    }
                }
            }
            1 => {
                // Erase from start to cursor
                let row = self.cursor_y as usize;
                let col = self.cursor_x as usize;
                for r in 0..row {
                    if r < self.grid.len() {
                        for c in 0..self.grid[r].len() {
                            self.grid[r][c] = self.blank.clone();
                        }
                    }
                }
                if row < self.grid.len() {
                    for c in 0..=col.min(self.grid[row].len().saturating_sub(1)) {
                        self.grid[row][c] = self.blank.clone();
                    }
                }
            }
            2 | 3 => {
                // Erase entire display
                for row in &mut self.grid {
                    for cell in row.iter_mut() {
                        *cell = self.blank.clone();
                    }
                }
            }
            _ => {}
        }
    }

    pub fn erase_in_line(&mut self, mode: u16) {
        self.dirty.store(true, Ordering::Relaxed);
        let row = self.cursor_y as usize;
        if row >= self.grid.len() {
            return;
        }
        match mode {
            0 => {
                for c in (self.cursor_x as usize)..self.grid[row].len() {
                    self.grid[row][c] = self.blank.clone();
                }
            }
            1 => {
                for c in 0..=(self.cursor_x as usize).min(self.grid[row].len().saturating_sub(1)) {
                    self.grid[row][c] = self.blank.clone();
                }
            }
            2 => {
                for cell in self.grid[row].iter_mut() {
                    *cell = self.blank.clone();
                }
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
            self.grid.insert(row, vec![self.blank.clone(); self.cols as usize]);
        }
        self.grid.resize(self.rows as usize, vec![self.blank.clone(); self.cols as usize]);
    }

    pub fn delete_lines(&mut self, n: u16) {
        let row = self.cursor_y as usize;
        let bottom = self.scroll_bottom as usize;
        for _ in 0..n {
            if row < self.grid.len() {
                self.grid.remove(row);
            }
            let insert_pos = bottom.min(self.grid.len());
            self.grid.insert(insert_pos, vec![self.blank.clone(); self.cols as usize]);
        }
        self.grid.resize(self.rows as usize, vec![self.blank.clone(); self.cols as usize]);
    }

    pub fn delete_chars(&mut self, n: u16) {
        let row = self.cursor_y as usize;
        let col = self.cursor_x as usize;
        if row < self.grid.len() {
            for _ in 0..n {
                if col < self.grid[row].len() {
                    self.grid[row].remove(col);
                    self.grid[row].push(self.blank.clone());
                }
            }
        }
    }

    pub fn erase_chars(&mut self, n: u16) {
        let row = self.cursor_y as usize;
        let col = self.cursor_x as usize;
        if row < self.grid.len() {
            for i in 0..n as usize {
                if col + i < self.grid[row].len() {
                    self.grid[row][col + i] = self.blank.clone();
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
            vec![vec![self.blank.clone(); self.cols as usize]; self.rows as usize],
        );
        self.alt_grid = Some(alt_grid);
        self.cursor_x = 0;
        self.cursor_y = 0;
        self.scroll_offset = 0;
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
        self.scroll_offset = 0;
        self.dirty.store(true, Ordering::Relaxed);
    }

    fn cursor_moved(&self) {
        self.dirty.store(true, Ordering::Relaxed);
        self.cursor_move_epoch.fetch_add(1, Ordering::Relaxed);
    }

    pub fn scroll_offset(&self) -> i32 {
        self.scroll_offset
    }

    pub fn resize(&mut self, cols: u16, rows: u16) {
        if cols == self.cols && rows == self.rows {
            return;
        }
        let old_rows = self.rows as usize;
        self.cols = cols;
        self.rows = rows;

        // Resize each existing row to new column count
        for row in &mut self.grid {
            row.resize(cols as usize, self.blank.clone());
        }

        let new_rows = rows as usize;
        if new_rows > old_rows {
            // Add blank rows at the bottom
            self.grid.resize(new_rows, vec![self.blank.clone(); cols as usize]);
        } else if new_rows < old_rows {
            // Remove blank rows from the bottom first
            while self.grid.len() > new_rows {
                let is_blank = self.grid.last()
                    .map(|row| row.iter().all(|c| c.c == ' ' || c.c == '\0'))
                    .unwrap_or(true);
                if is_blank && self.grid.len() > self.cursor_y as usize + 1 {
                    self.grid.pop();
                } else {
                    break;
                }
            }
            // If still too many rows, push top rows into scrollback
            while self.grid.len() > new_rows {
                let line = self.grid.remove(0);
                if !self.in_alt_screen {
                    self.scrollback.push_back(line);
                }
                // Adjust cursor to track its content
                self.cursor_y = self.cursor_y.saturating_sub(1);
            }
        }

        // Clamp cursor
        self.cursor_x = self.cursor_x.min(cols.saturating_sub(1));
        self.cursor_y = self.cursor_y.min(rows.saturating_sub(1));

        // Reset scroll region to full screen
        self.scroll_top = 0;
        self.scroll_bottom = rows.saturating_sub(1);
        self.scroll_offset = 0;

        // Resize alt grid if active
        if let Some(ref mut alt_grid) = self.alt_grid {
            for row in alt_grid.iter_mut() {
                row.resize(cols as usize, self.blank.clone());
            }
            alt_grid.resize(new_rows, vec![self.blank.clone(); cols as usize]);
        }

        self.dirty.store(true, Ordering::Relaxed);
    }

    pub fn reverse_index(&mut self) {
        if self.cursor_y == self.scroll_top {
            self.scroll_down(1);
        } else if self.cursor_y > 0 {
            self.cursor_y -= 1;
        }
    }
}
