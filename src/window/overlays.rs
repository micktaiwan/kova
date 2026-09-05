//! The three inline text overlays: the scrollback filter (`Cmd+F`), and the
//! tab and pane rename editors. Each one owns a query or a draft, a cursor,
//! and the keys it answers while it holds the keyboard.

use super::*;

pub(super) struct FilterState {
    pub(super) query: String,
    pub(super) matches: Vec<FilterMatch>,
    /// Where ↑/↓ currently sits in this run's filter history; `None` when the
    /// query on screen is the one being typed rather than a recalled one.
    pub(super) history_pos: Option<usize>,
    /// The typed query set aside while browsing history, put back on ↓ past the
    /// most recent entry.
    pub(super) draft: String,
}

pub(super) struct RenameTabState {
    pub(super) input: String,
    pub(super) cursor: usize, // char index
}

pub(super) struct RenamePaneState {
    pub(super) input: String,
    pub(super) cursor: usize, // char index
}

impl KovaView {
    pub(super) fn toggle_filter(&self) {
        let mut filter = self.ivars().filter.borrow_mut();
        if filter.is_some() {
            drop(filter);
            self.close_filter();
            return;
        }
        *filter = Some(FilterState {
            query: String::new(),
            matches: Vec::new(),
            history_pos: None,
            draft: String::new(),
        });
        drop(filter);
        // Mark dirty to trigger redraw
        if let Some(pane) = self.focused_pane() {
            pane.terminal.read().dirty.store(true, std::sync::atomic::Ordering::Relaxed);
        }
    }

    /// Close the filter overlay, remembering the query so ↑ can bring it back.
    /// Returns the state that was closed, for the callers that need its matches.
    /// The one exit point, so no path drops a query without recording it.
    pub(super) fn close_filter(&self) -> Option<FilterState> {
        let state = self.ivars().filter.borrow_mut().take();
        if let Some(state) = state.as_ref() {
            // The query the user was browsing from history is what is on
            // screen, and re-recording it just moves it back to the front.
            crate::search_history::record(
                crate::search_history::Scope::Filter,
                &state.query,
            );
        }
        if let Some(pane) = self.focused_pane() {
            pane.terminal.read().dirty.store(true, std::sync::atomic::Ordering::Relaxed);
        }
        state
    }

    /// Walk the filter's recall list. `older` is ↑ (further back), otherwise ↓.
    /// Returns true if the query changed and the matches need re-running.
    fn recall_filter_query(state: &mut FilterState, older: bool) -> bool {
        let history = crate::search_history::list(crate::search_history::Scope::Filter);
        if history.is_empty() {
            return false;
        }
        let next_pos = match (state.history_pos, older) {
            // Entering the list: keep what was typed so ↓ can restore it.
            (None, true) => Some(0),
            (None, false) => return false,
            (Some(i), true) => {
                if i + 1 >= history.len() {
                    return false; // already at the oldest
                }
                Some(i + 1)
            }
            // Past the most recent entry: back to what the user typed.
            (Some(0), false) => None,
            (Some(i), false) => Some(i - 1),
        };
        if state.history_pos.is_none() {
            state.draft = state.query.clone();
        }
        state.query = match next_pos {
            Some(i) => history[i].clone(),
            None => state.draft.clone(),
        };
        state.history_pos = next_pos;
        true
    }

    pub(super) fn handle_filter_key(&self, event: &NSEvent) {
        let key_code = event.keyCode();
        let chars = event.charactersIgnoringModifiers();
        let ch_str = chars.map(|s| s.to_string()).unwrap_or_default();
        let ch = ch_str.chars().next().unwrap_or('\0');

        let mut filter = self.ivars().filter.borrow_mut();
        let state = match filter.as_mut() {
            Some(s) => s,
            None => return,
        };

        // Arrows first: their `charactersIgnoringModifiers` is a private-use
        // char (U+F700…), which is neither a control char nor below ' ', so the
        // printable branch below would happily type it into the query.
        match key_code {
            0x7E | 0x7D => {
                // ↑/↓ walk the queries filtered earlier in this run.
                if !Self::recall_filter_query(state, key_code == 0x7E) {
                    return;
                }
                if let Some(pane) = self.focused_pane() {
                    let term = pane.terminal.read();
                    state.matches = term.search_lines(&state.query);
                    term.dirty.store(true, std::sync::atomic::Ordering::Relaxed);
                }
                return;
            }
            _ => {}
        }

        match ch {
            '\u{1B}' => {
                // Escape → close filter without scrolling
                drop(filter);
                self.close_filter();
                return;
            }
            '\r' => {
                // Enter → close filter and scroll to first match
                drop(filter);
                let first_match = self
                    .close_filter()
                    .and_then(|state| state.matches.first().map(|m| m.abs_line));
                if let Some(abs_line) = first_match {
                    if let Some(pane) = self.focused_pane() {
                        let mut term = pane.terminal.write();
                        term.scroll_to_abs_line(abs_line);
                    }
                }
                return;
            }
            '\u{7F}' | '\u{08}' => {
                // Backspace
                state.query.pop();
                // Editing a recalled query makes it the draft again, so ↓ does
                // not throw the edit away.
                state.history_pos = None;
            }
            c if is_typed_char(c) => {
                state.query.push(c);
                state.history_pos = None;
            }
            _ => return,
        }

        // Re-run search
        if let Some(pane) = self.focused_pane() {
            let term = pane.terminal.read();
            state.matches = term.search_lines(&state.query);
            term.dirty.store(true, std::sync::atomic::Ordering::Relaxed);
        }
    }

    pub(super) fn start_rename_tab(&self) {
        // Pre-fill with current tab title
        let current_title = {
            let tabs = self.ivars().tabs.borrow();
            let idx = self.ivars().active_tab.get();
            tabs.get(idx).map(|t| t.title()).unwrap_or_default()
        };
        let cursor = current_title.chars().count();
        *self.ivars().rename_tab.borrow_mut() = Some(RenameTabState {
            input: current_title,
            cursor,
        });
        self.mark_dirty();
    }

    pub(super) fn handle_rename_tab_key(&self, event: &NSEvent) {
        let key_code = event.keyCode();
        let chars = event.charactersIgnoringModifiers();
        let ch_str = chars.map(|s| s.to_string()).unwrap_or_default();
        let ch = ch_str.chars().next().unwrap_or('\0');

        let mut rename = self.ivars().rename_tab.borrow_mut();
        let state = match rename.as_mut() {
            Some(s) => s,
            None => return,
        };

        match key_code {
            123 => {
                // Left arrow
                if state.cursor > 0 { state.cursor -= 1; }
            }
            124 => {
                // Right arrow
                let len = state.input.chars().count();
                if state.cursor < len { state.cursor += 1; }
            }
            _ => match ch {
                '\u{1B}' => {
                    // Escape → cancel rename
                    *rename = None;
                    drop(rename);
                    self.mark_dirty();
                    return;
                }
                '\r' => {
                    // Enter → apply rename (empty = reset to auto)
                    let new_title = if state.input.trim().is_empty() {
                        None
                    } else {
                        Some(state.input.clone())
                    };
                    *rename = None;
                    drop(rename);
                    let mut tabs = self.ivars().tabs.borrow_mut();
                    let idx = self.ivars().active_tab.get();
                    if let Some(tab) = tabs.get_mut(idx) {
                        tab.custom_title = new_title;
                    }
                    drop(tabs);
                    self.mark_dirty();
                    return;
                }
                '\u{7F}' | '\u{08}' => {
                    // Backspace — remove char before cursor
                    if state.cursor > 0 {
                        if let Some((byte_idx, _)) = state.input.char_indices().nth(state.cursor - 1) {
                            state.input.remove(byte_idx);
                            state.cursor -= 1;
                        }
                    }
                }
                c if is_typed_char(c) => {
                    let byte_idx = state.input.char_indices()
                        .nth(state.cursor).map(|(i, _)| i)
                        .unwrap_or(state.input.len());
                    state.input.insert(byte_idx, c);
                    state.cursor += 1;
                }
                _ => return,
            }
        }
        drop(rename);
        self.mark_dirty();
    }

    pub(super) fn start_rename_pane(&self) {
        let current_title = {
            let tabs = self.ivars().tabs.borrow();
            let idx = self.ivars().active_tab.get();
            tabs.get(idx).and_then(|tab| {
                let pane = tab.pane(tab.focused_pane)?;
                if let Some(ref custom) = pane.custom_title {
                    Some(custom.clone())
                } else {
                    pane.terminal.read().title.clone()
                }
            }).unwrap_or_default()
        };
        let cursor = current_title.chars().count();
        *self.ivars().rename_pane.borrow_mut() = Some(RenamePaneState {
            input: current_title,
            cursor,
        });
        self.mark_dirty();
    }

    pub(super) fn handle_rename_pane_key(&self, event: &NSEvent) {
        let key_code = event.keyCode();
        let chars = event.charactersIgnoringModifiers();
        let ch_str = chars.map(|s| s.to_string()).unwrap_or_default();
        let ch = ch_str.chars().next().unwrap_or('\0');

        let mut rename = self.ivars().rename_pane.borrow_mut();
        let state = match rename.as_mut() {
            Some(s) => s,
            None => return,
        };

        match key_code {
            123 => {
                // Left arrow
                if state.cursor > 0 { state.cursor -= 1; }
            }
            124 => {
                // Right arrow
                let len = state.input.chars().count();
                if state.cursor < len { state.cursor += 1; }
            }
            _ => match ch {
                '\u{1B}' => {
                    // Escape → cancel rename
                    *rename = None;
                    drop(rename);
                    self.mark_dirty();
                    return;
                }
                '\r' => {
                    // Enter → apply rename (empty = reset to auto)
                    let new_title = if state.input.trim().is_empty() {
                        None
                    } else {
                        Some(state.input.clone())
                    };
                    *rename = None;
                    drop(rename);
                    let mut tabs = self.ivars().tabs.borrow_mut();
                    let idx = self.ivars().active_tab.get();
                    if let Some(tab) = tabs.get_mut(idx) {
                        if let Some(pane) = tab.pane_mut(tab.focused_pane) {
                            pane.custom_title = new_title;
                        }
                    }
                    drop(tabs);
                    self.mark_dirty();
                    return;
                }
                '\u{7F}' | '\u{08}' => {
                    // Backspace — remove char before cursor
                    if state.cursor > 0 {
                        if let Some((byte_idx, _)) = state.input.char_indices().nth(state.cursor - 1) {
                            state.input.remove(byte_idx);
                            state.cursor -= 1;
                        }
                    }
                }
                c if is_typed_char(c) => {
                    let byte_idx = state.input.char_indices()
                        .nth(state.cursor).map(|(i, _)| i)
                        .unwrap_or(state.input.len());
                    state.input.insert(byte_idx, c);
                    state.cursor += 1;
                }
                _ => return,
            }
        }
        drop(rename);
        self.mark_dirty();
    }

    pub(super) fn handle_filter_click(&self, _px: f32, py: f32) {
        let renderer = match self.ivars().renderer.get() {
            Some(r) => r,
            None => return,
        };
        let (_, cell_h) = renderer.read().cell_size();

        // The overlay starts with: 1 row search bar + matches below
        let match_start_y = {
            let panes_vp = self.panes_viewport();
            panes_vp.y + cell_h // search bar takes 1 row
        };

        let click_row = ((py - match_start_y) / cell_h).floor() as i32;
        if click_row < 0 {
            return;
        }

        if self.ivars().filter.borrow().is_none() {
            return;
        }
        let abs_line = self.close_filter().and_then(|state| {
            let idx = click_row as usize;
            state.matches.get(idx).map(|m| m.abs_line)
        });

        if let Some(abs_line) = abs_line {
            if let Some(pane) = self.focused_pane() {
                let mut term = pane.terminal.write();
                term.scroll_to_abs_line(abs_line);
            }
        }
    }
}
