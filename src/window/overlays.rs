//! The three inline text overlays: the scrollback filter (`Cmd+F`), and the
//! tab and pane rename editors. Each one owns a query or a draft, a cursor,
//! and the keys it answers while it holds the keyboard.

use super::*;
use crate::terminal::TerminalState;

pub(crate) struct FilterState {
    pub(crate) query: String,
    pub(crate) matches: Vec<FilterMatch>,
    /// The listed match ↑/↓ has picked, once the search has rows. Enter jumps
    /// to this one and the list paints it. Back to 0 whenever the query
    /// changes: a new search starts on its first match, as Enter always did.
    pub(crate) selected: usize,
    /// Where ↑/↓ currently sits in this run's filter history; `None` when the
    /// query on screen is the one being typed rather than a recalled one.
    pub(crate) history_pos: Option<usize>,
    /// The typed query set aside while browsing history, put back on ↓ past the
    /// most recent entry.
    pub(crate) draft: String,
    /// The terminal (`terminal_id`) and its `content_generation` that
    /// `matches` were computed against. `matches` hold absolute line indices,
    /// which shift as soon as the full scrollback drops its oldest line or a
    /// resize reflows it: when this differs from the terminal's current pair,
    /// the matches are stale and must be recomputed before use.
    pub(crate) searched: Option<(u32, u64)>,
    /// When `matches` were last computed, for the per-frame throttle.
    pub(crate) last_run: Option<std::time::Instant>,
}

/// Shortest gap between two per-frame re-runs of the filter search. A pane
/// that redraws constantly (a spinner) changes its content every frame, and a
/// search walks the whole scrollback; this caps that to a few runs a second.
/// The list on screen may lag output by up to this much, which is harmless:
/// Enter and click re-resolve their pick under the write lock anyway.
pub(crate) const FILTER_REFRESH_INTERVAL: std::time::Duration = std::time::Duration::from_millis(150);

/// Whether the per-frame refresh should re-run the search now: only when the
/// matches are stale, and not sooner than `min_interval` after the last run.
pub(crate) fn should_rerun(
    stale: bool,
    last_run: Option<std::time::Instant>,
    now: std::time::Instant,
    min_interval: std::time::Duration,
) -> bool {
    stale && last_run.map_or(true, |t| now.saturating_duration_since(t) >= min_interval)
}

impl FilterState {
    /// An empty filter, as Ctrl+F opens it.
    pub(crate) fn new() -> Self {
        FilterState {
            query: String::new(),
            matches: Vec::new(),
            selected: 0,
            history_pos: None,
            draft: String::new(),
            searched: None,
            last_run: None,
        }
    }

    /// What the renderer draws for this filter. The recall hint shows only
    /// while the input is empty and there is something to recall: once the
    /// user types, the match count is the useful number.
    pub(crate) fn render_data(&self) -> crate::renderer::FilterRenderData {
        crate::renderer::FilterRenderData {
            query: self.query.clone(),
            matches: self.matches.clone(),
            selected: self.selected,
            hint: (self.query.is_empty()
                && !crate::search_history::is_empty(crate::search_history::Scope::Filter))
            .then(|| "↑ recent".to_string()),
        }
    }

    /// The filter is closing: its query joins the recall list (↑ brings it
    /// back). The query the user was browsing from history is what is on
    /// screen, and re-recording it just moves it back to the front.
    pub(crate) fn record_query(&self) {
        crate::search_history::record(crate::search_history::Scope::Filter, &self.query);
    }

    /// Scroll `term` to the match the user picked at `idx` in the list they
    /// saw (`resolve_pick`) and flash that line. Resolved, scrolled and flashed
    /// under the caller's one write lock: output landing between the two would
    /// shift the line again.
    pub(crate) fn jump_to_pick(&mut self, idx: usize, term: &mut TerminalState) {
        if let Some(abs_line) = self.resolve_pick(idx, term) {
            term.scroll_to_abs_line(abs_line);
            term.flash_line(abs_line);
        }
    }

    /// Recompute the matches against `term` and remember what they reflect.
    pub(crate) fn rerun(&mut self, term: &TerminalState) {
        self.matches = term.search_lines(&self.query);
        self.searched = Some((term.terminal_id, term.content_generation()));
        self.last_run = Some(std::time::Instant::now());
        // The pick is an index into the list drawn a moment ago: output or a
        // reflow just changed its length. Stay on a row that exists.
        if self.selected >= self.matches.len() {
            self.selected = self.matches.len().saturating_sub(1);
        }
    }

    /// Whether `matches` no longer reflect `term`'s current content.
    pub(crate) fn is_stale(&self, term: &TerminalState) -> bool {
        self.searched != Some((term.terminal_id, term.content_generation()))
    }

    /// Recompute the matches if `term` changed since the last search and the
    /// last run is at least `FILTER_REFRESH_INTERVAL` old (see
    /// `should_rerun`). Returns whether a recompute happened. Cheap when it
    /// does not: two integer compares and an instant compare, so the tick can
    /// call it every frame.
    pub(crate) fn refresh(&mut self, term: &TerminalState, now: std::time::Instant) -> bool {
        if !should_rerun(self.is_stale(term), self.last_run, now, FILTER_REFRESH_INTERVAL) {
            return false;
        }
        self.rerun(term);
        true
    }

    /// The absolute line of the match the user picked at `idx` in the list
    /// they were looking at, resolved against `term`'s current content: if
    /// output arrived (or a reflow happened) since that list was drawn, the
    /// search is re-run and the pick follows its line (see `remap_match_index`).
    pub(crate) fn resolve_pick(&mut self, idx: usize, term: &TerminalState) -> Option<usize> {
        let idx = if self.is_stale(term) {
            let old = std::mem::take(&mut self.matches);
            self.rerun(term);
            remap_match_index(&old, idx, &self.matches)?
        } else {
            idx
        };
        self.matches.get(idx).map(|m| m.abs_line)
    }
}

/// Where the match at `idx` in `old` sits in `new`, a later search for the same
/// query. Absolute line indices are not stable across searches (a full
/// scrollback drops a line per new one, a reflow re-wraps), so the match is
/// followed by its text: the `new` match with the same text whose line is the
/// closest to the old one (ties go to the earlier line). If no line with that
/// text survives (it was trimmed away), the position is kept, clamped to the
/// new list. `None` only when `new` is empty or `idx` was out of `old`.
pub(super) fn remap_match_index(old: &[FilterMatch], idx: usize, new: &[FilterMatch]) -> Option<usize> {
    let picked = old.get(idx)?;
    if new.is_empty() {
        return None;
    }
    let same_text = new
        .iter()
        .enumerate()
        .filter(|(_, m)| m.text == picked.text)
        .min_by_key(|(_, m)| (m.abs_line.abs_diff(picked.abs_line), m.abs_line));
    Some(match same_text {
        Some((i, _)) => i,
        None => idx.min(new.len() - 1),
    })
}

/// One key press while the filter holds the keyboard, as Kova.app's window
/// and a library pane both read it.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct FilterKey<'a> {
    /// ↑ (`Some(true)`, further back in the recall list) or ↓ (`Some(false)`).
    pub(crate) arrow: Option<bool>,
    /// The first char of the key without modifiers.
    pub(crate) ch: char,
    pub(crate) ctrl: bool,
    pub(crate) cmd: bool,
    /// Cmd or Ctrl is held: a shortcut, not typing.
    pub(crate) shortcut: bool,
    /// What the key types with its modifiers (Option composes), control
    /// chars dropped; empty for a dead key waiting for its second stroke.
    pub(crate) text: &'a str,
}

impl<'a> FilterKey<'a> {
    /// The key as the library's input path hands it over (`KeyEvent`):
    /// arrows are AppKit's function-key chars, `text` is whatever
    /// `typed_text` would keep of its characters.
    pub(crate) fn from_key_event(key: &crate::input::KeyEvent, text: &'a str) -> Self {
        let ch = key.unmod.as_deref().and_then(|s| s.chars().next()).unwrap_or('\0');
        FilterKey {
            arrow: match ch {
                '\u{F700}' => Some(true),
                '\u{F701}' => Some(false),
                _ => None,
            },
            ch,
            ctrl: key.combo.ctrl,
            cmd: key.combo.cmd,
            shortcut: key.combo.cmd || key.combo.ctrl,
            text,
        }
    }
}

/// The key as Kova.app's window reads it from the `NSEvent` (`text` is
/// `typed_text(event)`).
pub(super) fn filter_key_from_event<'a>(event: &NSEvent, text: &'a str) -> FilterKey<'a> {
    let ch_str = event.charactersIgnoringModifiers().map(|s| s.to_string()).unwrap_or_default();
    let flags = event.modifierFlags();
    FilterKey {
        arrow: match event.keyCode() {
            0x7E => Some(true),
            0x7D => Some(false),
            _ => None,
        },
        ch: ch_str.chars().next().unwrap_or('\0'),
        ctrl: flags.contains(NSEventModifierFlags::Control),
        cmd: flags.contains(NSEventModifierFlags::Command),
        shortcut: is_shortcut(event),
        text,
    }
}

/// The text a key types into the filter: its characters, control chars and
/// AppKit's function-key range dropped (`typed_text` for a `KeyEvent`).
pub(crate) fn filter_typed_text(key: &crate::input::KeyEvent) -> String {
    key.chars.as_deref().unwrap_or("").chars().filter(|&c| is_typed_char(c)).collect()
}

/// What a key did to an open filter; the caller carries it out.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum FilterKeyOutcome {
    /// Ctrl+F: the filter closes, as Escape (it toggles).
    Toggle,
    /// Escape: close without scrolling.
    Close,
    /// Enter: close and scroll to the picked match (`FilterState::selected`).
    Jump,
    /// The query changed: re-run the search and redraw.
    Rerun,
    /// ↑/↓ moved the pick through the listed matches: redraw the list.
    Navigate,
    /// Nothing to do.
    Ignored,
}

/// Apply one key to an open filter: edit the query; ↑/↓ pick among the listed
/// matches once the user has launched a search, and walk the recall list
/// otherwise (`history` is read only then). Says what the caller must do.
pub(crate) fn filter_key(
    state: &mut FilterState,
    key: &FilterKey,
    history: impl FnOnce() -> Vec<String>,
) -> FilterKeyOutcome {
    // Ctrl+F closes the filter it opened: keyDown routes here before the
    // window's own Ctrl+F toggle, which would otherwise never see it.
    if key.ch == 'f' && key.ctrl && !key.cmd {
        return FilterKeyOutcome::Toggle;
    }
    // Arrows first: their unmodified char is a private-use char (U+F700…),
    // which is neither a control char nor below ' ', so the printable branch
    // below would happily type it into the query.
    if let Some(older) = key.arrow {
        // No search of the user's own on screen: the empty query Cmd+F opens
        // on, or a query recalled earlier and not edited since. There is
        // nothing of theirs to walk, so ↑/↓ keep walking the queries filtered
        // earlier in this run (`history_pos` is Some exactly while the query
        // on screen came from that list).
        if state.query.is_empty() || state.history_pos.is_some() {
            return if recall_filter_query(state, older, &history()) {
                FilterKeyOutcome::Rerun
            } else {
                FilterKeyOutcome::Ignored
            };
        }
        // A search the user launched: ↑/↓ pick among its results, which is
        // what the list is for. An empty result list has nothing to pick.
        return if navigate_match(state, older) {
            FilterKeyOutcome::Navigate
        } else {
            FilterKeyOutcome::Ignored
        };
    }
    match key.ch {
        '\u{1B}' => FilterKeyOutcome::Close,
        '\r' => FilterKeyOutcome::Jump,
        '\u{7F}' | '\u{08}' => {
            state.query.pop();
            // Editing a recalled query makes it the draft again, so ↓ does
            // not throw the edit away.
            state.history_pos = None;
            // A new search starts on its first match.
            state.selected = 0;
            FilterKeyOutcome::Rerun
        }
        c if is_typed_char(c) && !key.shortcut => {
            if key.text.is_empty() {
                return FilterKeyOutcome::Ignored; // a dead key waiting for its second stroke
            }
            state.query.push_str(key.text);
            state.history_pos = None;
            state.selected = 0;
            FilterKeyOutcome::Rerun
        }
        _ => FilterKeyOutcome::Ignored,
    }
}

/// Move the pick through the listed matches: ↑ (older) is the row above,
/// clamped at the top; ↓ the row below, clamped at the bottom. Returns true
/// when it moved, so the list is redrawn.
fn navigate_match(state: &mut FilterState, older: bool) -> bool {
    let len = state.matches.len();
    if len == 0 {
        return false;
    }
    let next = if older {
        state.selected.saturating_sub(1)
    } else {
        (state.selected + 1).min(len - 1)
    };
    if next == state.selected {
        return false;
    }
    state.selected = next;
    true
}

/// Walk the filter's recall list (`history`, most recent first). `older` is ↑
/// (further back), otherwise ↓. Returns true if the query changed and the
/// matches need re-running.
fn recall_filter_query(state: &mut FilterState, older: bool, history: &[String]) -> bool {
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
    state.selected = 0;
    true
}

/// The listed match a click at `row` lands on, when the list is drawn from the
/// scroll that keeps `selected` in view. The window and a library pane both go
/// through here: the scroll, the row and the bounds are one computation, so the
/// two hosts cannot disagree and a test can pin the whole hit-testing.
pub(crate) fn filter_click_pick(
    selected: usize,
    max_visible: usize,
    row: usize,
    len: usize,
) -> Option<usize> {
    // A row past the ones the overlay drew is not a match: with ten results
    // and three rows on screen, a click on the fourth row of that pane lands
    // in the pane below, and must not jump the filtered one anywhere.
    if row >= max_visible {
        return None;
    }
    let idx = crate::renderer::filter_list_scroll(selected, max_visible) + row;
    (idx < len).then_some(idx)
}

/// The listed line a click at `py` pixels lands on, the overlay's top being at
/// `top` (one row of query bar, then the matches). `None` on the bar itself.
pub(crate) fn filter_click_row(py: f32, top: f32, cell_h: f32) -> Option<usize> {
    // The overlay starts with: 1 row search bar + matches below
    let match_start_y = top + cell_h;
    let click_row = ((py - match_start_y) / cell_h).floor() as i32;
    (click_row >= 0).then_some(click_row as usize)
}

/// A clipboard pasted into an open filter: its first line goes into the
/// query (`filter_paste_text`). False when nothing was added.
pub(crate) fn paste_into_filter_state(state: &mut FilterState, text: &str) -> bool {
    let pasted = filter_paste_text(text);
    if pasted.is_empty() {
        return false;
    }
    state.query.push_str(&pasted);
    state.history_pos = None;
    state.selected = 0;
    true
}

/// The matched lines, one per line: what Cmd+C copies while the filter is
/// open. `None` when nothing matches (the copy then does nothing).
pub(crate) fn filter_copy_text(state: &FilterState) -> Option<String> {
    if state.matches.is_empty() {
        return None;
    }
    let mut text = String::new();
    for (i, m) in state.matches.iter().enumerate() {
        if i > 0 {
            text.push('\n');
        }
        text.push_str(&m.text);
    }
    Some(text)
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
        *filter = Some(FilterState::new());
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
            state.record_query();
        }
        if let Some(pane) = self.focused_pane() {
            pane.terminal.read().dirty.store(true, std::sync::atomic::Ordering::Relaxed);
        }
        state
    }

    /// Cmd+V while the filter is open: the clipboard goes into the query, not
    /// into the pane's PTY behind it.
    pub(super) fn paste_into_filter(&self, text: &str) {
        let mut filter = self.ivars().filter.borrow_mut();
        let Some(state) = filter.as_mut() else { return };
        if !paste_into_filter_state(state, text) {
            return;
        }
        if let Some(pane) = self.focused_pane() {
            let term = pane.terminal.read();
            state.rerun(&term);
            term.dirty.store(true, std::sync::atomic::Ordering::Relaxed);
        }
    }

    pub(super) fn handle_filter_key(&self, event: &NSEvent) {
        let text = typed_text(event);
        let key = filter_key_from_event(event, &text);

        let outcome = match self.ivars().filter.borrow_mut().as_mut() {
            Some(state) => filter_key(state, &key, || {
                crate::search_history::list(crate::search_history::Scope::Filter)
            }),
            None => return,
        };
        match outcome {
            FilterKeyOutcome::Toggle => self.toggle_filter(),
            FilterKeyOutcome::Close => {
                self.close_filter();
            }
            FilterKeyOutcome::Jump => {
                if let Some(mut state) = self.close_filter() {
                    if let Some(pane) = self.focused_pane() {
                        let picked = state.selected;
                        state.jump_to_pick(picked, &mut pane.terminal.write());
                    }
                }
            }
            FilterKeyOutcome::Rerun => {
                let mut filter = self.ivars().filter.borrow_mut();
                if let (Some(state), Some(pane)) = (filter.as_mut(), self.focused_pane()) {
                    let term = pane.terminal.read();
                    state.rerun(&term);
                    term.dirty.store(true, std::sync::atomic::Ordering::Relaxed);
                }
            }
            FilterKeyOutcome::Navigate => {
                if let Some(pane) = self.focused_pane() {
                    pane.terminal
                        .read()
                        .dirty
                        .store(true, std::sync::atomic::Ordering::Relaxed);
                }
            }
            FilterKeyOutcome::Ignored => {}
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
                // Cmd or Ctrl with a letter is a shortcut, not typing; Option
                // composes, so the text comes from the layout, not the bare key.
                c if is_typed_char(c) && !is_shortcut(event) => {
                    let text = typed_text(event);
                    let byte_idx = state.input.char_indices()
                        .nth(state.cursor).map(|(i, _)| i)
                        .unwrap_or(state.input.len());
                    state.input.insert_str(byte_idx, &text);
                    state.cursor += text.chars().count();
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
                // Cmd or Ctrl with a letter is a shortcut, not typing; Option
                // composes, so the text comes from the layout, not the bare key.
                c if is_typed_char(c) && !is_shortcut(event) => {
                    let text = typed_text(event);
                    let byte_idx = state.input.char_indices()
                        .nth(state.cursor).map(|(i, _)| i)
                        .unwrap_or(state.input.len());
                    state.input.insert_str(byte_idx, &text);
                    state.cursor += text.chars().count();
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

        // The overlay is drawn over the focused pane: its top and height are
        // what the click has to be measured against.
        let panes_vp = self.panes_viewport();
        let focused_vp = {
            let tabs = self.ivars().tabs.borrow();
            let tab = match tabs.get(self.ivars().active_tab.get()) {
                Some(t) => t,
                None => return,
            };
            tab.viewport_for_pane(tab.focused_pane, panes_vp)
        };
        let Some(focused_vp) = focused_vp else { return };
        let Some(click_row) = filter_click_row(py, focused_vp.y, cell_h) else {
            return;
        };

        if self.ivars().filter.borrow().is_none() {
            return;
        }
        let Some(mut state) = self.close_filter() else {
            return;
        };
        // Rows are drawn from the scroll that keeps the pick in view: the
        // click lands on that window's row, not on the list's.
        let max_visible = ((focused_vp.height / cell_h).floor() as usize).saturating_sub(1);
        let Some(idx) =
            filter_click_pick(state.selected, max_visible, click_row, state.matches.len())
        else {
            return; // below the list the user was looking at
        };
        if let Some(pane) = self.focused_pane() {
            state.jump_to_pick(idx, &mut pane.terminal.write());
        }
    }

    /// Called once per tick while the filter is open: re-run the search if the
    /// focused terminal's content changed since the last one, so the listed
    /// matches (and the absolute lines Enter/click jump to) stay valid while
    /// output streams in or a resize reflows the scrollback, and new matching
    /// lines show up.
    pub(super) fn refresh_filter_matches(&self) {
        let mut filter = self.ivars().filter.borrow_mut();
        let Some(state) = filter.as_mut() else { return };
        if let Some(pane) = self.focused_pane() {
            state.refresh(&pane.terminal.read(), std::time::Instant::now());
        }
    }
}

/// A clipboard's first line. Unicode line and paragraph separators end it too:
/// they are not control chars, so the typed-char filter would let them through.
fn first_line(text: &str) -> &str {
    text.split(['\n', '\r', '\u{2028}', '\u{2029}']).next().unwrap_or("")
}

/// The part of a clipboard that can go into the single-line filter query: its
/// first line, without the control chars a line search could never match.
pub(crate) fn filter_paste_text(text: &str) -> String {
    first_line(text).chars().filter(|&c| is_typed_char(c)).collect()
}

/// Longest paste a single-line input keeps. A search term or a title has no
/// use for more, and every extra char is redrawn on each frame.
const MAX_LINE_PASTE_CHARS: usize = 500;

/// A clipboard pasted into a single-line input whose words matter one by one
/// (search palette terms, a tab or pane title): the first line, tabs turned
/// into spaces so the words around them stay apart, other control chars
/// dropped, and the length capped.
pub(super) fn line_paste_text(text: &str) -> String {
    first_line(text)
        .chars()
        .map(|c| if c == '\t' { ' ' } else { c })
        .filter(|&c| is_typed_char(c))
        .take(MAX_LINE_PASTE_CHARS)
        .collect()
}

#[cfg(test)]
mod filter_paste_tests {
    use super::{filter_paste_text, line_paste_text, MAX_LINE_PASTE_CHARS};

    #[test]
    fn keeps_first_line_only() {
        assert_eq!(filter_paste_text("foo bar\nbaz"), "foo bar");
        assert_eq!(filter_paste_text("foo\r\nbaz"), "foo");
    }

    #[test]
    fn drops_control_chars() {
        assert_eq!(filter_paste_text("a\tb\x1b[c"), "ab[c");
        assert_eq!(filter_paste_text(""), "");
    }

    #[test]
    fn unicode_line_separators_end_the_first_line() {
        assert_eq!(filter_paste_text("foo\u{2028}bar"), "foo");
        assert_eq!(line_paste_text("foo\u{2029}bar"), "foo");
    }

    #[test]
    fn line_paste_keeps_tab_separated_words_apart() {
        assert_eq!(line_paste_text("deploy\tfailed\x1b"), "deploy failed");
    }

    #[test]
    fn line_paste_is_capped() {
        let long = "x".repeat(MAX_LINE_PASTE_CHARS * 3);
        assert_eq!(line_paste_text(&long).chars().count(), MAX_LINE_PASTE_CHARS);
    }
}

#[cfg(test)]
mod filter_refresh_tests {
    use super::{remap_match_index, should_rerun, FilterState, FILTER_REFRESH_INTERVAL};
    use std::time::{Duration, Instant};
    use crate::terminal::{FilterMatch, TerminalState};

    fn m(abs_line: usize, text: &str) -> FilterMatch {
        FilterMatch { abs_line, text: text.to_string() }
    }

    fn state(query: &str) -> FilterState {
        FilterState {
            query: query.to_string(),
            matches: Vec::new(),
            selected: 0,
            history_pos: None,
            draft: String::new(),
            searched: None,
            last_run: None,
        }
    }

    /// 3 rows, scrollback of 5 lines, filled with "line 0".."line {n-1}".
    fn full_term(n: usize) -> TerminalState {
        let mut t = TerminalState::new(20, 3, 5, [200; 3], [0; 3]);
        push(&mut t, 0, n);
        t
    }

    fn push(t: &mut TerminalState, from: usize, n: usize) {
        for i in from..from + n {
            t.carriage_return();
            for c in format!("line {}", i).chars() {
                t.put_char(c);
            }
            t.newline();
        }
    }

    #[test]
    fn remap_follows_the_same_line_after_a_shift() {
        let old = vec![m(3, "a"), m(5, "b"), m(7, "c")];
        // Everything moved up by one, a new match appended at the end.
        let new = vec![m(2, "a"), m(4, "b"), m(6, "c"), m(8, "d")];
        assert_eq!(remap_match_index(&old, 1, &new), Some(1));
        assert_eq!(remap_match_index(&old, 2, &new), Some(2));
    }

    #[test]
    fn remap_follows_text_when_earlier_matches_were_trimmed() {
        let old = vec![m(0, "a"), m(5, "b"), m(7, "c")];
        let new = vec![m(4, "b"), m(6, "c")];
        assert_eq!(remap_match_index(&old, 2, &new), Some(1), "\"c\" moved from row 2 to row 1");
    }

    #[test]
    fn remap_picks_the_closest_duplicate() {
        let old = vec![m(2, "err"), m(9, "err")];
        let new = vec![m(1, "err"), m(8, "err"), m(12, "err")];
        assert_eq!(remap_match_index(&old, 1, &new), Some(1));
        assert_eq!(remap_match_index(&old, 0, &new), Some(0));
    }

    #[test]
    fn remap_clamps_when_the_line_is_gone() {
        let old = vec![m(0, "a"), m(1, "b"), m(2, "c")];
        let new = vec![m(0, "x")];
        assert_eq!(remap_match_index(&old, 2, &new), Some(0));
        assert_eq!(remap_match_index(&old, 0, &[]), None);
        assert_eq!(remap_match_index(&old, 5, &new), None, "out of the old list");
    }

    #[test]
    fn should_rerun_needs_stale_and_the_interval() {
        let t0 = Instant::now();
        let iv = Duration::from_millis(150);
        assert!(should_rerun(true, None, t0, iv), "never run");
        assert!(!should_rerun(false, None, t0, iv), "not stale");
        assert!(!should_rerun(true, Some(t0), t0 + Duration::from_millis(149), iv), "too soon");
        assert!(should_rerun(true, Some(t0), t0 + iv, iv), "interval elapsed");
        assert!(!should_rerun(false, Some(t0), t0 + Duration::from_secs(5), iv), "elapsed but fresh");
        // A clock that reads earlier than the last run must not panic or rerun.
        assert!(!should_rerun(true, Some(t0 + iv), t0, iv));
    }

    #[test]
    fn refresh_only_reruns_when_content_changed() {
        let mut t = full_term(10);
        let mut s = state("line");
        let now = Instant::now();
        assert!(s.refresh(&t, now), "never searched: stale");
        let later = s.last_run.unwrap() + FILTER_REFRESH_INTERVAL;
        assert!(!s.refresh(&t, later), "nothing changed");
        let count = s.matches.len();
        push(&mut t, 10, 1);
        assert!(!s.refresh(&t, s.last_run.unwrap()), "changed, but throttled");
        let later = s.last_run.unwrap() + FILTER_REFRESH_INTERVAL;
        assert!(s.refresh(&t, later), "output arrived and the interval elapsed");
        assert_eq!(s.matches.len(), count, "full buffer: one line in, one out");
    }

    /// Cost of one filter search over a full 10k-line scrollback, the worst
    /// case the per-frame refresh can hit. Run with:
    /// `cargo test --release --lib search_lines_full_scrollback_timing -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn search_lines_full_scrollback_timing() {
        let mut t = TerminalState::new(120, 40, 10_000, [200; 3], [0; 3]);
        for i in 0..10_100 {
            t.carriage_return();
            for c in format!("{i:>6} some typical log output with a path /usr/local/lib/thing.rs:{i}").chars() {
                t.put_char(c);
            }
            t.newline();
        }
        assert_eq!(t.scrollback_len(), 10_000);
        let runs = 20;
        let start = Instant::now();
        let mut hits = 0;
        for _ in 0..runs {
            hits += t.search_lines("thing.rs:99").len();
        }
        let per = start.elapsed() / runs;
        eprintln!("search_lines over 10k lines x120 cols: {per:?} per run ({} hits)", hits / runs as usize);
    }

    #[test]
    fn refresh_keeps_matches_valid_after_trim() {
        let mut t = full_term(10);
        let mut s = state("line 7");
        s.rerun(&t);
        push(&mut t, 10, 2); // trims two lines, shifting every index
        assert!(s.refresh(&t, s.last_run.unwrap() + FILTER_REFRESH_INTERVAL));
        let fresh = t.search_lines("line 7");
        assert_eq!(s.matches.len(), 1);
        assert_eq!(s.matches[0].abs_line, fresh[0].abs_line);
    }

    #[test]
    fn refresh_picks_up_new_matching_lines() {
        let mut t = full_term(6);
        let mut s = state("line 1");
        s.rerun(&t);
        assert_eq!(s.matches.len(), 1, "line 1");
        push(&mut t, 6, 6); // adds line 10, line 11
        assert!(s.refresh(&t, s.last_run.unwrap() + FILTER_REFRESH_INTERVAL));
        let texts: Vec<_> = s.matches.iter().map(|m| m.text.as_str()).collect();
        assert!(texts.contains(&"line 10") && texts.contains(&"line 11"), "{texts:?}");
    }

    #[test]
    fn resolve_pick_lands_on_the_line_the_user_saw() {
        let mut t = full_term(10);
        let mut s = state("line");
        s.rerun(&t);
        let idx = s.matches.iter().position(|m| m.text == "line 8").unwrap();
        // Output lands between the frame the user clicked on and the click.
        push(&mut t, 10, 2);
        let abs = s.resolve_pick(idx, &t).expect("line 8 is still there");
        let fresh = t.search_lines("line 8");
        assert_eq!(abs, fresh[0].abs_line);
    }

    #[test]
    fn resolve_pick_after_reflow() {
        let mut t = full_term(10);
        let mut s = state("line 9");
        s.rerun(&t);
        t.resize(10, 2); // reflow: narrower and shorter
        let abs = s.resolve_pick(0, &t).expect("line 9 survives the reflow");
        let fresh = t.search_lines("line 9");
        assert_eq!(abs, fresh[0].abs_line);
    }
}

#[cfg(test)]
mod filter_key_tests {
    use super::*;
    use crate::input::tests::key_event;
    use crate::terminal::FilterMatch;

    fn state(query: &str) -> FilterState {
        FilterState { query: query.to_string(), ..FilterState::new() }
    }

    fn key<'a>(ch: char, mods: &str, text: &'a str) -> FilterKey<'a> {
        let ctrl = mods.contains("ctrl");
        let cmd = mods.contains("cmd");
        FilterKey { arrow: None, ch, ctrl, cmd, shortcut: ctrl || cmd, text }
    }

    fn arrow(up: bool) -> FilterKey<'static> {
        FilterKey { arrow: Some(up), ch: if up { '\u{F700}' } else { '\u{F701}' }, ctrl: false, cmd: false, shortcut: false, text: "" }
    }

    fn no_history() -> Vec<String> {
        panic!("history read without an arrow")
    }

    #[test]
    fn typing_and_backspace_edit_the_query() {
        let mut s = state("er");
        assert_eq!(filter_key(&mut s, &key('r', "", "r"), no_history), FilterKeyOutcome::Rerun);
        assert_eq!(s.query, "err");
        assert_eq!(filter_key(&mut s, &key('e', "", "é"), no_history), FilterKeyOutcome::Rerun, "Option composes");
        assert_eq!(s.query, "erré");
        assert_eq!(filter_key(&mut s, &key('\u{7F}', "", ""), no_history), FilterKeyOutcome::Rerun);
        assert_eq!(s.query, "err", "a whole char, not a byte");
        assert_eq!(filter_key(&mut s, &key('e', "", ""), no_history), FilterKeyOutcome::Ignored, "dead key");
        assert_eq!(filter_key(&mut s, &key('a', "cmd", "a"), no_history), FilterKeyOutcome::Ignored, "shortcut");
        assert_eq!(filter_key(&mut s, &key('a', "ctrl", "\u{1}"), no_history), FilterKeyOutcome::Ignored);
        assert_eq!(s.query, "err");
    }

    #[test]
    fn escape_enter_and_ctrl_f_end_the_filter() {
        let mut s = state("x");
        assert_eq!(filter_key(&mut s, &key('\u{1B}', "", ""), no_history), FilterKeyOutcome::Close);
        assert_eq!(filter_key(&mut s, &key('\r', "", ""), no_history), FilterKeyOutcome::Jump);
        assert_eq!(filter_key(&mut s, &key('f', "ctrl", "\u{6}"), no_history), FilterKeyOutcome::Toggle);
        assert_eq!(filter_key(&mut s, &key('f', "ctrl cmd", "f"), no_history), FilterKeyOutcome::Ignored, "Cmd+Ctrl+F is not the toggle");
        assert_eq!(s.query, "x");
    }

    /// Once the search has rows, ↑/↓ move the pick through them — that is
    /// what the list is for. Clamped at both ends, and never touching the
    /// recall list, which is what ↑/↓ do while there is nothing to pick.
    #[test]
    fn arrows_pick_among_the_matches_once_the_search_has_rows() {
        let mut s = state("line");
        s.matches = vec![
            FilterMatch {
                abs_line: 1,
                text: "line 1".into(),
            },
            FilterMatch {
                abs_line: 4,
                text: "line 4".into(),
            },
            FilterMatch {
                abs_line: 9,
                text: "line 9".into(),
            },
        ];
        assert_eq!(
            filter_key(&mut s, &arrow(false), no_history),
            FilterKeyOutcome::Navigate
        );
        assert_eq!(s.selected, 1, "↓ moves down the list");
        assert_eq!(
            filter_key(&mut s, &arrow(false), no_history),
            FilterKeyOutcome::Navigate
        );
        assert_eq!(s.selected, 2);
        assert_eq!(
            filter_key(&mut s, &arrow(false), no_history),
            FilterKeyOutcome::Ignored,
            "already the last"
        );
        assert_eq!(s.selected, 2, "clamped at the bottom");
        assert_eq!(
            filter_key(&mut s, &arrow(true), no_history),
            FilterKeyOutcome::Navigate
        );
        assert_eq!(s.selected, 1, "↑ moves up");
        assert_eq!(
            filter_key(&mut s, &arrow(true), no_history),
            FilterKeyOutcome::Navigate
        );
        assert_eq!(s.selected, 0);
        assert_eq!(
            filter_key(&mut s, &arrow(true), no_history),
            FilterKeyOutcome::Ignored,
            "already the first"
        );
        assert_eq!(s.selected, 0, "clamped at the top");
    }

    /// A new search starts on its first match, so Enter keeps meaning what it
    /// always meant until the user picks something else.
    #[test]
    fn a_new_search_starts_on_its_first_match() {
        let mut s = state("lin");
        s.matches = vec![
            FilterMatch {
                abs_line: 1,
                text: "line 1".into(),
            },
            FilterMatch {
                abs_line: 4,
                text: "line 4".into(),
            },
        ];
        s.selected = 1;
        assert_eq!(
            filter_key(&mut s, &key('e', "", "e"), no_history),
            FilterKeyOutcome::Rerun
        );
        assert_eq!(s.selected, 0, "typing starts over on the first match");
        s.selected = 1;
        assert_eq!(
            filter_key(&mut s, &key('\u{7F}', "", ""), no_history),
            FilterKeyOutcome::Rerun
        );
        assert_eq!(s.selected, 0, "so does backspace");
        s.selected = 1;
        assert!(paste_into_filter_state(&mut s, "x"));
        assert_eq!(s.selected, 0, "and so does a paste");
    }

    /// A pane's worth of lines for `rerun` to search, the way the filter
    /// searches a live terminal.
    fn term_with_lines(lines: &[&str]) -> TerminalState {
        let mut t = TerminalState::new(20, 6, 5, [200; 3], [0; 3]);
        for l in lines {
            t.carriage_return();
            for c in l.chars() {
                t.put_char(c);
            }
            t.newline();
        }
        t
    }

    /// The pick is an index into a list the output can shorten under it: it
    /// never points past the last row.
    #[test]
    fn the_pick_stays_on_a_row_that_exists() {
        let mut s = state("line");
        s.selected = 2;
        s.rerun(&term_with_lines(&["line 1"]));
        assert_eq!(s.selected, 0, "one match left: the pick moved onto it");
        s.selected = 1;
        s.rerun(&term_with_lines(&["nothing here"]));
        assert_eq!(
            s.selected, 0,
            "no match left: the pick has nowhere else to go"
        );
    }

    /// With no search of the user's own on screen — the empty query Cmd+F
    /// opens on, or a query recalled and not edited since — ↑/↓ walk the
    /// queries filtered earlier in this run, the whole list and back. A query
    /// the user launched owns the arrows: they walk its results instead.
    #[test]
    fn arrows_walk_the_recall_list_until_a_search_is_launched() {
        let history = || vec!["newest".to_string(), "older".to_string()];
        let mut s = state("");
        assert_eq!(
            filter_key(&mut s, &arrow(false), history),
            FilterKeyOutcome::Ignored,
            "↓ on an empty query has nowhere to go"
        );
        assert_eq!(
            filter_key(&mut s, &arrow(true), Vec::new),
            FilterKeyOutcome::Ignored,
            "nothing to recall"
        );
        assert_eq!(
            filter_key(&mut s, &arrow(true), history),
            FilterKeyOutcome::Rerun
        );
        assert_eq!(s.query, "newest", "the most recent query comes back");
        assert_eq!(
            filter_key(&mut s, &arrow(true), history),
            FilterKeyOutcome::Rerun
        );
        assert_eq!(s.query, "older", "↑ keeps walking, one step further back");
        assert_eq!(
            filter_key(&mut s, &arrow(true), history),
            FilterKeyOutcome::Ignored,
            "already at the oldest"
        );
        assert_eq!(s.query, "older", "stopped on it");
        assert_eq!(
            filter_key(&mut s, &arrow(false), history),
            FilterKeyOutcome::Rerun
        );
        assert_eq!(s.query, "newest", "↓ comes back through the list");
        assert_eq!(
            filter_key(&mut s, &arrow(false), history),
            FilterKeyOutcome::Rerun
        );
        assert_eq!(s.query, "", "past the most recent one, the draft is back");
        // A query the user launches owns the arrows from then on.
        s.query = "line".to_string();
        s.matches = vec![
            FilterMatch {
                abs_line: 1,
                text: "line 1".into(),
            },
            FilterMatch {
                abs_line: 4,
                text: "line 4".into(),
            },
        ];
        assert_eq!(
            filter_key(&mut s, &arrow(false), history),
            FilterKeyOutcome::Navigate
        );
        assert_eq!(
            s.selected, 1,
            "the pick walks the results, not the recall list"
        );
        assert_eq!(
            s.query, "line",
            "the query the user launched stays on screen"
        );
    }

    /// A typed search owns ↑/↓: they move the pick through the results and
    /// never reach for the recall list, even when nothing matches. This is the
    /// case the user asked for — a NEW search, not the queries from before.
    #[test]
    fn a_typed_search_never_falls_back_to_the_recall_list() {
        let history = || vec!["newest".to_string()];
        let mut s = state("zzz");
        assert!(s.matches.is_empty());
        assert_eq!(
            filter_key(&mut s, &arrow(true), history),
            FilterKeyOutcome::Ignored
        );
        assert_eq!(s.query, "zzz", "the query the user typed stays on screen");
        s.matches = vec![
            FilterMatch {
                abs_line: 1,
                text: "zzz 1".into(),
            },
            FilterMatch {
                abs_line: 4,
                text: "zzz 2".into(),
            },
        ];
        assert_eq!(
            filter_key(&mut s, &arrow(false), history),
            FilterKeyOutcome::Navigate
        );
        assert_eq!(s.selected, 1, "the pick moved through the results");
        assert_eq!(s.query, "zzz", "still the search, not a recalled query");
    }

    /// The library reads a key from its `KeyEvent`, Kova.app from the
    /// `NSEvent`: both give the filter the same key.
    #[test]
    fn library_and_window_read_the_same_key() {
        let cases: &[(&str, &str, u16, &str)] = &[
            ("a", "a", 0x00, ""),
            ("A", "A", 0x00, "shift"),
            ("é", "e", 0x0E, "alt"),
            ("", "e", 0x0E, ""),
            ("\u{7f}", "\u{7f}", 0x33, ""),
            ("\r", "\r", 0x24, ""),
            ("\u{1b}", "\u{1b}", 0x35, ""),
            ("\u{F700}", "\u{F700}", 0x7E, ""),
            ("\u{F701}", "\u{F701}", 0x7D, ""),
            ("\u{F702}", "\u{F702}", 0x7B, ""),
            ("\u{6}", "f", 0x03, "ctrl"),
            ("a", "a", 0x00, "cmd"),
        ];
        for &(chars, unmod, code, mods) in cases {
            let ev = key_event(chars, unmod, code, mods);
            let window_text = typed_text(&ev);
            let window = filter_key_from_event(&ev, &window_text);
            let k = crate::appkit_input::key_event(&ev);
            let lib_text = filter_typed_text(&k);
            let lib = FilterKey::from_key_event(&k, &lib_text);
            assert_eq!(window, lib, "{chars:?} {mods}");
        }
    }

    #[test]
    fn click_rows_start_below_the_query_bar() {
        assert_eq!(filter_click_row(5.0, 0.0, 20.0), None, "on the bar");
        assert_eq!(filter_click_row(20.0, 0.0, 20.0), Some(0));
        assert_eq!(filter_click_row(59.0, 0.0, 20.0), Some(1));
        assert_eq!(filter_click_row(75.0, 50.0, 20.0), Some(0), "a pane lower in the window");
    }

    /// The list moves only as far as it must to keep the pick in view — one
    /// function, one answer, whichever host draws it.
    #[test]
    fn the_list_scrolls_just_enough_to_show_the_pick() {
        use crate::renderer::filter_list_scroll;
        assert_eq!(filter_list_scroll(0, 3), 0);
        assert_eq!(filter_list_scroll(2, 3), 0, "still fully visible");
        assert_eq!(filter_list_scroll(3, 3), 1, "one row down");
        assert_eq!(
            filter_list_scroll(9, 3),
            7,
            "the pick is the last row drawn"
        );
        assert_eq!(filter_list_scroll(5, 0), 0, "no room: nothing scrolls");
    }

    /// A click is resolved through that same scroll, so it lands on the row it
    /// was drawn over — the place a library pane once mixed points and drawable
    /// pixels, and where a scrolled list would otherwise jump a row off.
    #[test]
    fn a_click_lands_on_the_row_it_was_drawn_over() {
        assert_eq!(
            filter_click_pick(0, 3, 0, 10),
            Some(0),
            "no scroll at the top"
        );
        assert_eq!(
            filter_click_pick(3, 3, 0, 10),
            Some(1),
            "the list scrolled by one"
        );
        assert_eq!(
            filter_click_pick(3, 3, 2, 10),
            Some(3),
            "row 2 of a list scrolled by one"
        );
        assert_eq!(
            filter_click_pick(3, 3, 9, 10),
            None,
            "below the listed rows"
        );
        assert_eq!(
            filter_click_pick(0, 3, 5, 10),
            None,
            "a row the overlay never drew: the click landed in the pane below"
        );
        assert_eq!(
            filter_click_pick(0, 0, 0, 10),
            None,
            "a pane too short to list anything"
        );
        assert_eq!(filter_click_pick(0, 3, 0, 0), None, "nothing listed");
    }

    #[test]
    fn paste_and_copy_while_open() {
        let mut s = state("a");
        assert!(paste_into_filter_state(&mut s, "b c\nnext line"));
        assert_eq!(s.query, "ab c");
        assert!(!paste_into_filter_state(&mut s, "\n"), "nothing on the first line");
        assert_eq!(filter_copy_text(&s), None, "no match, nothing copied");
        s.matches = vec![
            FilterMatch { abs_line: 1, text: "one".into() },
            FilterMatch { abs_line: 4, text: "two".into() },
        ];
        assert_eq!(filter_copy_text(&s).as_deref(), Some("one\ntwo"));
    }
}
