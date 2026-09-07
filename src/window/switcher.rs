//! The tab/pane switcher overlay (`Cmd+P`, and `Cmd+Shift+J` for the
//! attention-only list): the row model it is built from, and the keys, scroll
//! and clicks it answers while it is open.

use super::*;

/// One row of the tab/pane switcher overlay.
pub(super) enum SwitcherRow {
    /// A tab name — not selectable.
    TabHeader(String),
    /// A pane entry — selectable, focuses the pane on Enter/click.
    /// `minimized` panes are restored (unhidden) when selected.
    Pane {
        pane_id: PaneId,
        title: String,
        is_current: bool,
        has_bell: bool,
        has_completion: bool,
        minimized: bool,
        /// Claude Code is generating / running a tool in this pane (✳).
        working: bool,
        /// The binary running in the pane, with its version when known
        /// ("claude 2.1.226"). `None` at a bare shell prompt, and also when the
        /// title already *is* that name — no row should say "vim … vim".
        process: Option<String>,
        /// This pane holds a bookmarked conversation. The row is painted so it
        /// stands out, and the Bookmarks section leaves the conversation out:
        /// a bookmark that is already open is this row, not a second one.
        bookmarked: bool,
    },
    /// A bookmarked conversation — selectable. Enter jumps to the pane that
    /// still holds it, or reopens it where it belongs when nothing does.
    Bookmark {
        /// Index into the saved list, so the row can act on it without carrying
        /// the whole bookmark around.
        index: usize,
        title: String,
        /// Project name and agent, shown dim at the end of the row.
        detail: Option<String>,
    },
}

impl SwitcherRow {
    fn is_pane(&self) -> bool {
        matches!(self, SwitcherRow::Pane { .. })
    }

    /// Rows Enter and the arrow keys can land on: panes, and bookmarks.
    /// Headers are the only thing the selection skips.
    fn is_selectable(&self) -> bool {
        !matches!(self, SwitcherRow::TabHeader(_))
    }

    /// Is this row asking for something? A bell, or a command that finished
    /// while the eye was elsewhere — the two markers the switcher draws, and the
    /// ones the attention-only list keeps. `working` is deliberately not one of
    /// them: a session still chewing has nothing to hand over yet, exactly as in
    /// `Cmd+J`'s tiers.
    pub(super) fn needs_attention(&self) -> bool {
        match self {
            SwitcherRow::Pane { has_bell, has_completion, .. } => *has_bell || *has_completion,
            _ => false,
        }
    }
}

/// Keep only the rows that ask for something, tab by tab.
///
/// A tab header survives as long as one of its panes does — the filtered list
/// still has to say *where* the pane lives — and a tab whose panes all fall out
/// disappears whole, header included, rather than naming a tab the list has
/// nothing to say about. Groups are per-tab (one header followed by its panes),
/// which is what makes that decision local.
pub(super) fn retain_attention_rows(groups: Vec<Vec<SwitcherRow>>) -> Vec<Vec<SwitcherRow>> {
    groups
        .into_iter()
        .filter_map(|group| {
            let kept: Vec<SwitcherRow> =
                group.into_iter().filter(|r| !r.is_pane() || r.needs_attention()).collect();
            kept.iter().any(|r| r.is_pane()).then_some(kept)
        })
        .collect()
}

/// What a pane switcher row says about the binary running in the pane.
///
/// Titles come from the app itself (Claude Code names the session, an editor
/// names the file), so the program behind them is invisible in the list — this
/// is what puts it back. It stays out of the way when the title *is* the
/// program name, which is what a pane at a bare `vim` already shows.
pub(super) fn switcher_process_label(process: Option<&ProcessInfo>, title: &str) -> Option<String> {
    let label = process?.label();
    (!label.is_empty() && label != title).then_some(label)
}

/// Next pane row with unread output — a bell, or a command that finished while
/// the eye was elsewhere — scanning forward from just after `(col, row)` in
/// column-major order and wrapping around the whole grid. `None` when nothing is
/// unread: the caller then leaves the selection where it is, so pressing Tab in
/// a quiet switcher does nothing rather than jumping somewhere arbitrary.
pub(super) fn next_unread_row(
    columns: &[Vec<SwitcherRow>],
    col: usize,
    row: usize,
) -> Option<(usize, usize)> {
    let flat: Vec<(usize, usize)> = columns
        .iter()
        .enumerate()
        .flat_map(|(c, rows)| (0..rows.len()).map(move |r| (c, r)))
        .collect();
    let start = flat.iter().position(|&p| p == (col, row)).map_or(0, |i| i + 1);
    flat.iter()
        .cycle()
        .skip(start)
        .take(flat.len())
        .find(|&&(c, r)| {
            matches!(
                columns[c][r],
                SwitcherRow::Pane { has_bell: true, .. } | SwitcherRow::Pane { has_completion: true, .. }
            )
        })
        .copied()
}

/// Index of the pane row whose position is closest to `target` within `col`.
/// Every column holds at least one pane (each tab has ≥1 pane), so this always
/// returns a valid pane index; falls back to 0 only for a degenerate empty column.
fn nearest_pane_row(col: &[SwitcherRow], target: usize) -> usize {
    col.iter()
        .enumerate()
        .filter(|(_, r)| r.is_selectable())
        .min_by_key(|(i, _)| (*i as isize - target as isize).unsigned_abs())
        .map(|(i, _)| i)
        .unwrap_or(0)
}

/// What activating a switcher row does.
enum SwitcherTarget {
    Pane(PaneId),
    Bookmark(usize),
}

pub(super) struct PaneSwitcherState {
    /// Columns of rows. Each column holds whole tabs (a tab header followed by
    /// its pane rows); a tab is never split across two columns.
    pub(super) columns: Vec<Vec<SwitcherRow>>,
    /// Selected column index.
    pub(super) selected_col: usize,
    /// Selected row within `columns[selected_col]`; always points at a `Pane` row.
    pub(super) selected_row: usize,
    /// Per-column first-visible-row offset (vertical scroll), one entry per column.
    pub(super) scroll: Vec<usize>,
    /// Fractional accumulator for trackpad/wheel scroll (sub-row deltas).
    pub(super) scroll_acc: f64,
    /// Attention-only list: every row shown is a pane asking for something.
    /// The toggle rebuilds the overlay, so this is only what the current
    /// snapshot was built with — what the title says and what the toggle flips.
    pub(super) filtered: bool,
}

/// The dim right-hand half of a bookmark row: the project it belongs to, and
/// the agent holding the conversation. `None` when there is neither, which
/// keeps the row from ending in a stray separator.
pub(super) fn bookmark_detail(bm: &crate::bookmarks::Bookmark) -> Option<String> {
    let project = std::path::Path::new(&bm.cwd)
        .file_name()
        .and_then(|n| n.to_str())
        .filter(|n| !n.is_empty());
    let agent = bm.agent.map(|a| a.as_str());
    match (project, agent) {
        (Some(p), Some(a)) => Some(format!("{} · {}", p, a)),
        (Some(p), None) => Some(p.to_string()),
        (None, Some(a)) => Some(a.to_string()),
        (None, None) => None,
    }
}

/// The Bookmarks section of the list: one row per saved conversation that is
/// *not* already open, newest first, headed by its title. A bookmark whose pane
/// is open is up in its tab, painted as a bookmark — repeating it here would
/// offer two rows for one conversation. Empty when nothing is left to reopen,
/// and the section then disappears entirely.
pub(super) fn bookmark_rows(
    saved: &[crate::bookmarks::Bookmark],
    is_open: impl Fn(&crate::bookmarks::Bookmark) -> bool,
) -> Vec<SwitcherRow> {
    let mut rows: Vec<SwitcherRow> = Vec::new();
    for (index, bm) in saved.iter().enumerate() {
        if is_open(bm) {
            continue;
        }
        rows.push(SwitcherRow::Bookmark {
            index,
            title: bm.label.clone(),
            detail: bookmark_detail(bm),
        });
    }
    if !rows.is_empty() {
        rows.insert(0, SwitcherRow::TabHeader("Bookmarks".to_string()));
    }
    rows
}

impl KovaView {
    /// The pane of this window still running a bookmarked conversation, if one
    /// does — jumping to it beats opening a second pane on the same session.
    fn pane_holding_session(&self, bm: &crate::bookmarks::Bookmark) -> Option<PaneId> {
        let session_id = bm.session_id.as_deref()?;
        let tabs = self.ivars().tabs.borrow();
        let mut found = None;
        for tab in tabs.iter() {
            tab.for_each_pane(&mut |pane| {
                if found.is_none() && pane.agent_session_id().as_deref() == Some(session_id) {
                    found = Some(pane.id);
                }
            });
        }
        found
    }

    /// Act on a bookmark row: focus the pane that still holds the conversation,
    /// or put it back where it belongs.
    fn open_bookmark(&self, index: usize) {
        let saved = crate::bookmarks::load();
        let Some(bm) = saved.items.get(index) else { return };
        if let Some(pane_id) = self.pane_holding_session(bm) {
            self.ipc_focus_pane(pane_id);
            self.set_pane_flash(pane_id, 30, None);
            return;
        }
        self.open_conversation_in_project(bm.resume_command(), &bm.cwd);
    }

    /// Open the tab/pane switcher overlay: every tab with its panes, click or
    /// Enter to focus. Selection starts on the currently-focused pane.
    ///
    /// With `filtered`, the same list keeps only the panes asking for something
    /// — the unread and waiting ones. It is the sit-down counterpart of `Cmd+J`,
    /// which walks the same panes one jump at a time without ever showing how
    /// many there are; `u` flips between the two lists once open. Two things it
    /// does not share with `Cmd+J`: this list is one window's tabs (`Cmd+J`
    /// crosses windows), and its last tier — an idle Claude session — is not an
    /// unread pane, so it is not in here either.
    pub(super) fn open_pane_switcher(&self, filtered: bool) {
        // Read once, up front: the same list marks the open panes below and
        // fills the Bookmarks section at the end.
        let saved = crate::bookmarks::load();
        // Also the moment to refresh the cache the status bars read: another
        // window may have bookmarked something since this one last did.
        *self.ivars().bookmark_keys.borrow_mut() = crate::bookmarks::keys(&saved.items);
        let bookmarked_sessions: std::collections::HashSet<&str> = saved
            .items
            .iter()
            .filter_map(|b| b.session_id.as_deref())
            .collect();
        // Build one row group per tab (header followed by its pane rows).
        let mut groups: Vec<Vec<SwitcherRow>> = Vec::new();
        {
            let tabs = self.ivars().tabs.borrow();
            let active = self.ivars().active_tab.get();
            for (ti, tab) in tabs.iter().enumerate() {
                let mut rows: Vec<SwitcherRow> = Vec::new();
                rows.push(SwitcherRow::TabHeader(format!("{}  {}", ti + 1, tab.title())));
                let focused_pane = tab.focused_pane;
                tab.for_each_pane(&mut |pane| {
                    let is_current = ti == active && pane.id == focused_pane;
                    // Attention (unread) mirrors the per-pane status-bar dot: bell
                    // or a completed command, but never on the currently-focused pane.
                    let (has_bell, has_completion) = if is_current {
                        (false, false)
                    } else {
                        let term = pane.terminal.read();
                        (
                            term.bell.load(std::sync::atomic::Ordering::Relaxed),
                            term.unread_completion(),
                        )
                    };
                    let title = pane.display_title("shell");
                    let process = switcher_process_label(pane.fg_process().as_ref(), &title);
                    let bookmarked = pane
                        .agent_session_id()
                        .is_some_and(|id| bookmarked_sessions.contains(id.as_str()));
                    rows.push(SwitcherRow::Pane {
                        pane_id: pane.id,
                        title,
                        is_current,
                        has_bell,
                        has_completion,
                        minimized: pane.minimized,
                        working: pane.is_working(),
                        process,
                        bookmarked,
                    });
                });
                groups.push(rows);
            }
        }
        if !filtered && groups.iter().all(|g| g.iter().all(|r| !r.is_pane())) {
            return; // nothing to switch to
        }
        // Saved conversations, as their own group at the end of the list. Only
        // on the full list: the attention-only one answers "what is asking for
        // something", and a bookmark never asks for anything.
        if !filtered {
            let rows = bookmark_rows(&saved.items, |bm| self.pane_holding_session(bm).is_some());
            if !rows.is_empty() {
                groups.push(rows);
            }
        }
        // An empty filtered list still opens: the answer "nothing is unread" is
        // one the overlay has to give out loud, and `u` from there shows all the
        // panes again. Only the unfiltered list can refuse to open.
        let groups = if filtered { retain_attention_rows(groups) } else { groups };

        // Partition the tab groups into ≤3 contiguous columns, balanced by row
        // count. A group joins the current column unless closing the column now
        // (without it) lands closer to the per-column target than including it.
        let ncols = groups.len().min(3).max(1);
        let total: usize = groups.iter().map(|g| g.len()).sum();
        let mut columns: Vec<Vec<SwitcherRow>> = Vec::new();
        let mut cur: Vec<SwitcherRow> = Vec::new();
        let mut cur_w = 0usize;
        let mut placed_w = 0usize;
        for g in groups {
            let w = g.len();
            let cols_left = ncols - columns.len();
            if cols_left > 1 && !cur.is_empty() {
                let target = (total - placed_w) as f64 / cols_left as f64;
                if (cur_w as f64 - target).abs() <= ((cur_w + w) as f64 - target).abs() {
                    placed_w += cur_w;
                    columns.push(std::mem::take(&mut cur));
                    cur_w = 0;
                }
            }
            cur.extend(g);
            cur_w += w;
        }
        columns.push(cur);

        // Land on the currently-focused pane; otherwise the first pane row.
        let mut selected_col = 0usize;
        let mut selected_row = 0usize;
        let mut found = false;
        'outer: for (c, col) in columns.iter().enumerate() {
            for (r, row) in col.iter().enumerate() {
                if matches!(row, SwitcherRow::Pane { is_current: true, .. }) {
                    selected_col = c;
                    selected_row = r;
                    found = true;
                    break 'outer;
                }
            }
        }
        if !found {
            for (c, col) in columns.iter().enumerate() {
                if let Some(r) = col.iter().position(|x| x.is_selectable()) {
                    selected_col = c;
                    selected_row = r;
                    break;
                }
            }
        }

        let scroll = vec![0usize; columns.len()];
        *self.ivars().pane_switcher.borrow_mut() = Some(PaneSwitcherState {
            columns,
            selected_col,
            selected_row,
            scroll,
            scroll_acc: 0.0,
            filtered,
        });
        self.pane_switcher_clamp_scroll();
        self.mark_dirty();
    }

    /// Adjust the selected column's scroll offset so the selected row stays visible.
    fn pane_switcher_clamp_scroll(&self) {
        let max_visible = {
            let renderer = match self.ivars().renderer.get() { Some(r) => r, None => return };
            let vh = self.drawable_viewport().height;
            renderer.read().overlay_list_geometry(vh).max_visible.max(1)
        };
        let mut guard = self.ivars().pane_switcher.borrow_mut();
        if let Some(state) = guard.as_mut() {
            let col = state.selected_col;
            let sel = state.selected_row;
            if let Some(sc) = state.scroll.get_mut(col) {
                if sel < *sc {
                    *sc = sel;
                } else if sel >= *sc + max_visible {
                    *sc = sel + 1 - max_visible;
                }
            }
        }
    }

    /// Handle key events in the tab/pane switcher overlay.
    pub(super) fn handle_pane_switcher_key(&self, event: &NSEvent) {
        let keycode = event.keyCode();

        // Escape → close
        if keycode == 0x35 {
            *self.ivars().pane_switcher.borrow_mut() = None;
            self.mark_dirty();
            return;
        }

        // Enter → focus selected pane
        if keycode == 0x24 {
            self.pane_switcher_focus_selected();
            return;
        }

        // `u`, or the shortcut that opens the attention-only list, flips
        // between "every pane" and "only the ones asking for something".
        // Flipping rebuilds the overlay rather than hiding rows in place: the
        // list is a snapshot of the panes either way, and rebuilding is what
        // lands the selection back where each list wants it — the focused pane
        // on the full list, the first pane that wants something on the other.
        let opens_filtered = KeyCombo::from_event(event);
        let opens_filtered = self.ivars().keybindings.get().is_some_and(|kb| {
            matches!(kb.window_map.get(&opens_filtered), Some(Action::OpenUnreadSwitcher))
        });
        if keycode == 0x20 || opens_filtered {
            let filtered =
                self.ivars().pane_switcher.borrow().as_ref().is_some_and(|s| s.filtered);
            self.open_pane_switcher(!filtered);
            return;
        }

        // Cmd+↑ / Cmd+↓ → move the selected pane one step in its tab's order
        // instead of moving the selection. Minimized panes are steps like any
        // other, so the selected row travels exactly one line of the list per
        // press, and the selection follows the pane it moved. Only on the full
        // list: the attention-only one hides rows, so the pane the move steps
        // over is often not the row above or below, and the list would look
        // unchanged while the layout moved underneath.
        let full_list = self.ivars().pane_switcher.borrow().as_ref().is_some_and(|s| !s.filtered);
        if full_list
            && event.modifierFlags().contains(NSEventModifierFlags::Command)
            && (keycode == 0x7E || keycode == 0x7D)
        {
            self.pane_switcher_move_selected(keycode == 0x7D);
            return;
        }

        // Arrow keys: ↑↓ move within a column (headers skipped), ←→ between columns.
        {
            let mut guard = self.ivars().pane_switcher.borrow_mut();
            let state = match guard.as_mut() {
                Some(s) => s,
                None => return,
            };
            match keycode {
                0x7E => { // Up
                    let col = &state.columns[state.selected_col];
                    if let Some(i) = col[..state.selected_row].iter().rposition(|r| r.is_selectable()) {
                        state.selected_row = i;
                    }
                }
                0x7D => { // Down
                    let col = &state.columns[state.selected_col];
                    if let Some(off) = col.get(state.selected_row + 1..)
                        .and_then(|tail| tail.iter().position(|r| r.is_selectable()))
                    {
                        state.selected_row = state.selected_row + 1 + off;
                    }
                }
                0x7B => { // Left
                    if state.selected_col > 0 {
                        state.selected_col -= 1;
                        state.selected_row =
                            nearest_pane_row(&state.columns[state.selected_col], state.selected_row);
                    }
                }
                0x7C => { // Right
                    if state.selected_col + 1 < state.columns.len() {
                        state.selected_col += 1;
                        state.selected_row =
                            nearest_pane_row(&state.columns[state.selected_col], state.selected_row);
                    }
                }
                0x30 => { // Tab → jump to the next pane with unread output
                    if let Some((c, r)) = next_unread_row(
                        &state.columns,
                        state.selected_col,
                        state.selected_row,
                    ) {
                        state.selected_col = c;
                        state.selected_row = r;
                    }
                }
                _ => return,
            }
        }
        self.pane_switcher_clamp_scroll();
        self.mark_dirty();
    }

    /// Move the pane on the selected switcher row one step in its tab's order.
    ///
    /// The overlay is rebuilt afterwards rather than patched in place — the
    /// list is a snapshot of the panes, and the tab groups it packs into
    /// columns depend on the order — then the selection is put back on the pane
    /// that moved, wherever the rebuild landed it.
    fn pane_switcher_move_selected(&self, forward: bool) {
        let pane_id = {
            let guard = self.ivars().pane_switcher.borrow();
            match guard.as_ref().and_then(|s| {
                s.columns.get(s.selected_col).and_then(|c| c.get(s.selected_row))
            }) {
                Some(SwitcherRow::Pane { pane_id, .. }) => *pane_id,
                _ => return,
            }
        };
        let moved = {
            let mut tabs = self.ivars().tabs.borrow_mut();
            match tabs.iter_mut().find(|t| t.pane(pane_id).is_some()) {
                Some(tab) => tab.move_pane_in_order(pane_id, forward),
                None => false,
            }
        };
        if !moved {
            return;
        }
        self.resize_all_panes();

        let filtered = self.ivars().pane_switcher.borrow().as_ref().is_some_and(|s| s.filtered);
        self.open_pane_switcher(filtered);
        if let Some(state) = self.ivars().pane_switcher.borrow_mut().as_mut() {
            for (c, col) in state.columns.iter().enumerate() {
                if let Some(r) = col.iter().position(
                    |row| matches!(row, SwitcherRow::Pane { pane_id: id, .. } if *id == pane_id),
                ) {
                    state.selected_col = c;
                    state.selected_row = r;
                    break;
                }
            }
        }
        self.pane_switcher_clamp_scroll();
        self.mark_dirty();
    }

    /// Act on the currently-selected switcher row and close the overlay: focus
    /// a pane, or reopen a bookmarked conversation.
    fn pane_switcher_focus_selected(&self) {
        let target = {
            let guard = self.ivars().pane_switcher.borrow();
            guard.as_ref().and_then(|s| {
                match s.columns.get(s.selected_col).and_then(|c| c.get(s.selected_row)) {
                    Some(SwitcherRow::Pane { pane_id, .. }) => Some(SwitcherTarget::Pane(*pane_id)),
                    Some(SwitcherRow::Bookmark { index, .. }) => {
                        Some(SwitcherTarget::Bookmark(*index))
                    }
                    _ => None,
                }
            })
        };
        *self.ivars().pane_switcher.borrow_mut() = None;
        match target {
            Some(SwitcherTarget::Pane(pid)) => {
                self.ipc_focus_pane(pid);
            }
            Some(SwitcherTarget::Bookmark(index)) => self.open_bookmark(index),
            None => {}
        }
        self.mark_dirty();
    }

    /// Scroll the switcher column under the cursor with the mouse wheel / trackpad.
    /// Adjusts only the vertical row offset; selection is unchanged.
    pub(super) fn handle_pane_switcher_scroll(&self, event: &NSEvent, is_trackpad: bool) {
        let (px, _py) = self.event_to_pixel(event);
        let max_visible = {
            let renderer = match self.ivars().renderer.get() { Some(r) => r, None => return };
            let vh = self.drawable_viewport().height;
            renderer.read().overlay_list_geometry(vh).max_visible.max(1)
        };
        let vw = self.drawable_viewport().width;

        let mut guard = self.ivars().pane_switcher.borrow_mut();
        let state = match guard.as_mut() {
            Some(s) => s,
            None => return,
        };
        let ncols = state.columns.len().max(1);
        let col = ((px / (vw / ncols as f32)).floor() as usize).min(ncols - 1);

        // Natural scrolling: dragging content up (negative deltaY) moves the list down.
        let dy = event.scrollingDeltaY();
        let lines = if is_trackpad {
            let acc = state.scroll_acc - dy / 8.0;
            let discrete = acc.trunc();
            state.scroll_acc = acc - discrete;
            discrete as i32
        } else {
            state.scroll_acc = 0.0;
            -dy as i32
        };
        if lines == 0 {
            return;
        }

        let col_len = state.columns[col].len();
        let max_scroll = col_len.saturating_sub(max_visible);
        if let Some(sc) = state.scroll.get_mut(col) {
            let next = (*sc as i64 + lines as i64).clamp(0, max_scroll as i64) as usize;
            if next != *sc {
                *sc = next;
                drop(guard);
                self.mark_dirty();
            }
        }
    }

    /// Handle a click in the tab/pane switcher overlay. A click on a pane row
    /// focuses it; a click anywhere else dismisses the overlay.
    pub(super) fn handle_pane_switcher_click(&self, px: f32, py: f32) {
        let target = {
            let renderer = match self.ivars().renderer.get() { Some(r) => r, None => return };
            let vp = self.drawable_viewport();
            let geom = renderer.read().overlay_list_geometry(vp.height);
            let guard = self.ivars().pane_switcher.borrow();
            let state = match guard.as_ref() {
                Some(s) => s,
                None => return,
            };
            let ncols = state.columns.len().max(1);
            let col = ((px / (vp.width / ncols as f32)).floor() as usize).min(ncols - 1);
            if py < geom.content_top {
                None
            } else {
                let vis = ((py - geom.content_top) / geom.row_height).floor() as usize;
                if vis >= geom.max_visible {
                    None
                } else {
                    let idx = state.scroll.get(col).copied().unwrap_or(0) + vis;
                    match state.columns[col].get(idx) {
                        Some(SwitcherRow::Pane { pane_id, .. }) => {
                            Some(SwitcherTarget::Pane(*pane_id))
                        }
                        Some(SwitcherRow::Bookmark { index, .. }) => {
                            Some(SwitcherTarget::Bookmark(*index))
                        }
                        _ => None,
                    }
                }
            }
        };
        *self.ivars().pane_switcher.borrow_mut() = None;
        match target {
            Some(SwitcherTarget::Pane(pid)) => {
                self.ipc_focus_pane(pid);
            }
            Some(SwitcherTarget::Bookmark(index)) => self.open_bookmark(index),
            None => {}
        }
        self.mark_dirty();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a switcher grid from a compact spec: one string per column, one
    /// char per row — 'h' header, '.' plain pane, 'b' pane with a pending bell,
    /// 'c' pane with an unread completion.
    fn switcher_grid(spec: &[&str]) -> Vec<Vec<SwitcherRow>> {
        spec.iter()
            .map(|col| {
                col.chars()
                    .map(|c| match c {
                        'h' => SwitcherRow::TabHeader("tab".into()),
                        'k' => SwitcherRow::Bookmark {
                            index: 0,
                            title: "saved".into(),
                            detail: None,
                        },
                        _ => SwitcherRow::Pane {
                            pane_id: 0,
                            title: "p".into(),
                            is_current: false,
                            has_bell: c == 'b',
                            has_completion: c == 'c',
                            minimized: false,
                            working: false,
                            process: None,
                            bookmarked: false,
                        },
                    })
                    .collect()
            })
            .collect()
    }

    /// Render a grid back to the compact spec, so a filtered grid can be
    /// compared row by row: 'h' header, 'b' bell, 'c' completion, '.' a pane
    /// asking for nothing.
    fn switcher_spec(groups: &[Vec<SwitcherRow>]) -> Vec<String> {
        groups
            .iter()
            .map(|col| {
                col.iter()
                    .map(|r| match r {
                        SwitcherRow::TabHeader(_) => 'h',
                        SwitcherRow::Pane { has_bell: true, .. } => 'b',
                        SwitcherRow::Pane { has_completion: true, .. } => 'c',
                        SwitcherRow::Pane { .. } => '.',
                        SwitcherRow::Bookmark { .. } => 'k',
                    })
                    .collect()
            })
            .collect()
    }

    fn saved(id: &str) -> crate::bookmarks::Bookmark {
        crate::bookmarks::Bookmark {
            agent: Some(crate::agent_session::Agent::Claude),
            session_id: Some(id.into()),
            cwd: "/Users/x/projects/kova".into(),
            label: format!("conv {}", id),
        }
    }

    #[test]
    fn an_open_bookmark_leaves_the_bookmarks_section() {
        let items = vec![saved("a"), saved("b")];
        let rows = bookmark_rows(&items, |bm| bm.session_id.as_deref() == Some("a"));
        assert_eq!(switcher_spec(&[rows]), vec!["hk".to_string()]);
        // The row still points at the bookmark's own index in the saved list,
        // not at its position in the filtered rows.
        let rows = bookmark_rows(&items, |bm| bm.session_id.as_deref() == Some("a"));
        match &rows[1] {
            SwitcherRow::Bookmark { index, .. } => assert_eq!(*index, 1),
            _ => panic!("expected a bookmark row"),
        }
    }

    #[test]
    fn the_bookmarks_section_disappears_when_every_bookmark_is_open() {
        let items = vec![saved("a"), saved("b")];
        assert!(bookmark_rows(&items, |_| true).is_empty(), "no header without rows");
        assert!(bookmark_rows(&[], |_| false).is_empty());
    }

    #[test]
    fn the_attention_list_drops_the_bookmark_section_whole() {
        // Bookmarks never ask for anything, so the filtered list must not keep
        // them — header included.
        let grid = switcher_grid(&["hb.", "hkk"]);
        assert_eq!(switcher_spec(&retain_attention_rows(grid)), vec!["hb".to_string()]);
    }

    #[test]
    fn a_bookmark_row_is_selectable_but_is_not_a_pane() {
        let col = &switcher_grid(&["hk"])[0];
        assert!(col[1].is_selectable());
        assert!(!col[1].is_pane());
        assert!(!col[0].is_selectable(), "a header is never selected");
    }

    #[test]
    fn a_bookmark_row_names_its_project_and_agent() {
        let bm = crate::bookmarks::Bookmark {
            agent: Some(crate::agent_session::Agent::Codex),
            session_id: Some("id".into()),
            cwd: "/Users/x/projects/kova".into(),
            label: "the split refactor".into(),
        };
        assert_eq!(bookmark_detail(&bm).as_deref(), Some("kova · codex"));
        let shell = crate::bookmarks::Bookmark {
            agent: None,
            session_id: None,
            cwd: "/Users/x/projects/kova".into(),
            label: "shell".into(),
        };
        assert_eq!(bookmark_detail(&shell).as_deref(), Some("kova"));
    }

    #[test]
    fn nearest_pane_row_lands_on_the_closest_selectable_row() {
        // Column of one header then three panes. A target already on a pane
        // stays there; a target on the header slides down to the pane below.
        let col = &switcher_grid(&["h..."])[0];
        assert_eq!(nearest_pane_row(col, 2), 2);
        assert_eq!(nearest_pane_row(col, 0), 1);
    }

    #[test]
    fn nearest_pane_row_prefers_the_row_above_on_a_tie() {
        // Header sandwiched between two panes: rows 0 and 2 are both one step
        // from the target, and `min_by_key` keeps the first it meets.
        let col = vec![
            switcher_grid(&["."])[0].pop().unwrap(),
            SwitcherRow::TabHeader("tab".into()),
            switcher_grid(&["."])[0].pop().unwrap(),
        ];
        assert_eq!(nearest_pane_row(&col, 1), 0);
    }

    #[test]
    fn nearest_pane_row_falls_back_to_zero_without_a_pane() {
        // A column of headers only has nothing to select: row 0, not a panic.
        let col = &switcher_grid(&["hh"])[0];
        assert_eq!(nearest_pane_row(col, 1), 0);
    }

    #[test]
    fn nearest_pane_row_clamps_a_target_past_the_end() {
        // The selection can outlive the rebuild that shortened the column.
        let col = &switcher_grid(&["h.."])[0];
        assert_eq!(nearest_pane_row(col, 99), 2);
    }

    #[test]
    fn attention_filter_keeps_the_markers_and_drops_quiet_panes() {
        let groups = switcher_grid(&["h.b.b", "h..c"]);
        assert_eq!(switcher_spec(&retain_attention_rows(groups)), vec!["hbb", "hc"]);
    }

    #[test]
    fn attention_filter_drops_a_whole_tab_whose_panes_are_all_quiet() {
        // Second tab has nothing to say: its header goes with its panes rather
        // than standing alone over an empty group.
        let groups = switcher_grid(&["h.b", "h..", "hc"]);
        assert_eq!(switcher_spec(&retain_attention_rows(groups)), vec!["hb", "hc"]);
    }

    #[test]
    fn attention_filter_can_end_up_with_nothing_at_all() {
        // Every pane quiet: the caller opens an empty overlay that says so,
        // instead of a list that looks like the full one minus a few rows.
        let groups = switcher_grid(&["h..", "h."]);
        assert!(retain_attention_rows(groups).is_empty());
    }

    #[test]
    fn a_working_pane_is_not_asking_for_anything() {
        // Mirrors Cmd+J: a session still chewing has nothing to hand over, so
        // it stays out of the attention list even though the row draws a ✳.
        let working = SwitcherRow::Pane {
            pane_id: 0,
            title: "p".into(),
            is_current: false,
            has_bell: false,
            has_completion: false,
            minimized: false,
            working: true,
            process: None,
            bookmarked: false,
        };
        assert!(!working.needs_attention());
    }

    #[test]
    fn switcher_row_names_the_binary_and_its_version() {
        let claude = ProcessInfo { name: "claude".into(), version: Some("2.1.226".into()) };
        assert_eq!(
            switcher_process_label(Some(&claude), "Corriger child_processes"),
            Some("claude 2.1.226".to_string())
        );
    }

    #[test]
    fn switcher_row_stays_quiet_when_the_title_is_already_the_binary() {
        let vim = ProcessInfo { name: "vim".into(), version: None };
        assert_eq!(switcher_process_label(Some(&vim), "vim"), None);
        // A shell prompt has no foreground process at all.
        assert_eq!(switcher_process_label(None, "kova"), None);
    }

    #[test]
    fn tab_jumps_to_the_next_unread_pane_forward() {
        // Column 0: header, pane, unread pane. Column 1: header, unread pane.
        let g = switcher_grid(&["h.b", "hc"]);
        assert_eq!(next_unread_row(&g, 0, 1), Some((0, 2)));
        // From the last unread row of column 0, cross into the next column.
        assert_eq!(next_unread_row(&g, 0, 2), Some((1, 1)));
    }

    #[test]
    fn tab_wraps_around_to_the_first_unread_pane() {
        let g = switcher_grid(&["h.b", "h."]);
        // Past the only unread row, the search wraps back onto it.
        assert_eq!(next_unread_row(&g, 1, 1), Some((0, 2)));
        // Standing on the only unread row, Tab cycles back to itself rather
        // than reporting "nothing found".
        assert_eq!(next_unread_row(&g, 0, 2), Some((0, 2)));
    }

    #[test]
    fn tab_does_nothing_when_no_pane_is_unread() {
        let g = switcher_grid(&["h..", "h."]);
        assert_eq!(next_unread_row(&g, 0, 1), None);
        assert_eq!(next_unread_row(&[], 0, 0), None);
    }

    #[test]
    fn tab_never_lands_on_a_tab_header() {
        // Headers are not selectable; a header row must never be returned even
        // when it sits between the cursor and the unread pane.
        let g = switcher_grid(&["hb", "hc"]);
        let (c, r) = next_unread_row(&g, 0, 1).expect("an unread pane exists");
        assert!(matches!(g[c][r], SwitcherRow::Pane { .. }));
    }
}
