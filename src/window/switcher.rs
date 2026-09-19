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
        /// stands out, and the bookmark groups leave the conversation out:
        /// a bookmark that is already open is this row, not a second one.
        bookmarked: bool,
    },
    /// An anchored conversation — selectable, and always listed, even when a
    /// pane still holds it: the anchors are what today is for, so the section
    /// stays the same whether or not the work is currently open.
    Anchor {
        /// Index into the saved anchors.
        index: usize,
        title: String,
        /// Agent holding the conversation, shown dim at the end of the row.
        detail: Option<String>,
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
    Anchor(usize),
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

/// The dim right-hand half of a bookmark row: the agent holding the
/// conversation. The directory is not repeated here — it is the header of the
/// group the row sits in. `None` for a bookmarked shell.
pub(super) fn bookmark_detail(bm: &crate::bookmarks::Bookmark) -> Option<String> {
    bm.agent.map(|a| a.as_str().to_string())
}

/// The header naming a directory of bookmarks: its last component ("cto"),
/// or the whole path with `~` for home when another saved directory ends the
/// same way, so two `app` folders never share one name.
pub(super) fn bookmark_dir_header(cwd: &str, all_cwds: &[&str], home: Option<&str>) -> String {
    let base = |p: &str| {
        std::path::Path::new(p)
            .file_name()
            .and_then(|n| n.to_str())
            .filter(|n| !n.is_empty())
            .map(str::to_string)
    };
    let name = base(cwd);
    let clashes = name.is_some() && all_cwds.iter().any(|&other| other != cwd && base(other) == name);
    match name {
        Some(n) if !clashes => n,
        _ => match home.filter(|h| !h.is_empty()).and_then(|h| cwd.strip_prefix(h)) {
            Some(rest) if rest.is_empty() || rest.starts_with('/') => format!("~{}", rest),
            _ => cwd.to_string(),
        },
    }
}

/// The anchors, as the one group that opens the list. Unlike the bookmarks
/// below they are not split by directory and an open one is not left out: a
/// short, stable section is the point — you should find today's work in the
/// same place whatever else is running.
///
/// Empty list, no group: a section headed "Anchors" with nothing under it would
/// take a line to say nothing.
pub(super) fn anchor_groups(saved: &[crate::anchors::Anchor]) -> Vec<Vec<SwitcherRow>> {
    if saved.is_empty() {
        return Vec::new();
    }
    let mut rows = vec![SwitcherRow::TabHeader("\u{2693} Anchors".to_string())];
    for (index, anchor) in saved.iter().enumerate() {
        rows.push(SwitcherRow::Anchor {
            index,
            title: anchor.label.clone(),
            detail: bookmark_detail(anchor),
        });
    }
    vec![rows]
}

/// The whole list, in the order it is read: the anchors, then the tabs with
/// their panes, then the bookmarks. Today's work opens the list — whatever is
/// running below is what happened to the day, not what it was for.
///
/// `hidden` is the bookmark filter (open, or anchored); the anchors are not
/// filtered at all, which is the difference between the two sections.
pub(super) fn saved_sections(
    anchors: &[crate::anchors::Anchor],
    bookmarks: &[crate::bookmarks::Bookmark],
    tabs: Vec<Vec<SwitcherRow>>,
    hidden: impl Fn(&crate::bookmarks::Bookmark) -> bool,
) -> Vec<Vec<SwitcherRow>> {
    let mut groups = anchor_groups(anchors);
    groups.extend(tabs);
    groups.extend(bookmark_groups(bookmarks, hidden));
    groups
}

/// The bookmark part of the list: one group per directory, each headed by that
/// directory, holding the saved conversations that are *not* already open. A
/// bookmark whose pane is open is up in its tab, painted as a bookmark —
/// repeating it here would offer two rows for one conversation.
///
/// Groups come in the order of their newest bookmark, and rows keep the saved
/// order (newest first) inside a group. A directory whose bookmarks are all
/// open has no group; with nothing left to reopen the result is empty.
/// `hidden` also covers the anchored conversations: an anchor is already a row
/// up in its own section, and one conversation never gets two rows.
pub(super) fn bookmark_groups(
    saved: &[crate::bookmarks::Bookmark],
    hidden: impl Fn(&crate::bookmarks::Bookmark) -> bool,
) -> Vec<Vec<SwitcherRow>> {
    let home = std::env::var("HOME").ok();
    let mut dirs: Vec<&str> = Vec::new();
    let mut groups: Vec<Vec<SwitcherRow>> = Vec::new();
    for (index, bm) in saved.iter().enumerate() {
        if hidden(bm) {
            continue;
        }
        let row = SwitcherRow::Bookmark {
            index,
            title: bm.label.clone(),
            detail: bookmark_detail(bm),
        };
        match dirs.iter().position(|d| *d == bm.cwd) {
            Some(g) => groups[g].push(row),
            None => {
                dirs.push(&bm.cwd);
                groups.push(vec![row]);
            }
        }
    }
    // Only the directories that end up on screen: a name is ambiguous when two
    // *visible* groups share it, and spelling out a path to avoid a clash with a
    // row nobody sees reads as a bug.
    let all_cwds: &[&str] = &dirs;
    for (dir, group) in dirs.iter().zip(groups.iter_mut()) {
        let header = bookmark_dir_header(dir, &all_cwds, home.as_deref());
        group.insert(0, SwitcherRow::TabHeader(format!("★ {}", header)));
    }
    groups
}

/// Where the selection lands once the bookmark at `removed` (its index in the
/// saved list) is gone and the list rebuilt: on the row that took its place,
/// else the last bookmark row, so a run of Cmd+Backspace walks down the section.
/// `None` when no bookmark row is left — the caller keeps the default selection.
pub(super) fn bookmark_row_after_removal(
    columns: &[Vec<SwitcherRow>],
    removed: usize,
) -> Option<(usize, usize)> {
    let rows: Vec<(usize, usize, usize)> = columns
        .iter()
        .enumerate()
        .flat_map(|(c, col)| {
            col.iter().enumerate().filter_map(move |(r, row)| match row {
                SwitcherRow::Bookmark { index, .. } => Some((c, r, *index)),
                _ => None,
            })
        })
        .collect();
    rows.iter()
        .find(|&&(_, _, i)| i >= removed)
        .or(rows.last())
        .map(|&(c, r, _)| (c, r))
}

impl KovaView {
    /// The pane of this window still running a bookmarked conversation, if one
    /// does — jumping to it beats opening a second pane on the same session.
    pub(super) fn pane_holding_session(&self, bm: &crate::bookmarks::Bookmark) -> Option<PaneId> {
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
    /// or put it back where it belongs and run its resume line — picking the
    /// bookmark already said which conversation to bring back.
    fn open_bookmark(&self, index: usize) {
        let saved = crate::bookmarks::load();
        let Some(bm) = saved.items.get(index) else { return };
        self.open_saved_conversation(bm);
    }

    /// Act on an anchor row: same gesture as a bookmark row, on the other list.
    fn open_anchor(&self, index: usize) {
        let saved = crate::anchors::load();
        let Some(anchor) = saved.items.get(index) else { return };
        self.open_saved_conversation(anchor);
    }

    /// The pane of this window this saved conversation is already in: the one
    /// running its session, or — for a shell, which has no session to match on —
    /// a bare shell sitting in its directory. Without the second case, going
    /// back to a shell anchor would split a new pane every single time.
    pub(super) fn pane_holding_saved(&self, bm: &crate::bookmarks::Bookmark) -> Option<PaneId> {
        if bm.session_id.is_some() {
            return self.pane_holding_session(bm);
        }
        let tabs = self.ivars().tabs.borrow();
        let mut found = None;
        for tab in tabs.iter() {
            tab.for_each_pane(&mut |pane| {
                if found.is_none()
                    && pane.agent_session_id().is_none()
                    && pane.cwd().as_deref() == Some(bm.cwd.as_str())
                {
                    found = Some(pane.id);
                }
            });
        }
        found
    }

    /// Bring a saved conversation back: focus the pane that still holds it,
    /// wherever it lives, or put it where it belongs and run its resume line.
    /// Shared by the bookmark rows, the anchor rows and Cmd+A — picking any of
    /// them already said which conversation to bring back.
    ///
    /// Every window is searched, not just this one: a conversation open in
    /// another window is open, and reopening it would run a second `resume` on
    /// a live session.
    pub(super) fn open_saved_conversation(&self, bm: &crate::bookmarks::Bookmark) {
        let mtm = unsafe { MainThreadMarker::new_unchecked() };
        let app = objc2_app_kit::NSApplication::sharedApplication(mtm);
        let windows = app.windows();
        for i in 0..windows.count() {
            let win = windows.objectAtIndex(i);
            let Some(view) = crate::app::kova_view(&win) else { continue };
            let Some(pane_id) = view.pane_holding_saved(bm) else { continue };
            view.ipc_focus_pane(pane_id);
            win.makeKeyAndOrderFront(None);
            view.set_pane_flash(pane_id, 30, None);
            return;
        }
        self.open_conversation_in_project(bm.resume_command(), &bm.cwd);
    }

    /// Cmd+Backspace on a bookmark row: drop it from the saved list once an
    /// alert naming it is confirmed. The list is rebuilt in place, so several
    /// dead bookmarks can go in a row without reopening Cmd+P.
    fn pane_switcher_remove_selected_bookmark(&self) {
        let index = {
            let guard = self.ivars().pane_switcher.borrow();
            match guard
                .as_ref()
                .and_then(|s| s.columns.get(s.selected_col).and_then(|c| c.get(s.selected_row)))
            {
                Some(SwitcherRow::Bookmark { index, .. }) => *index,
                _ => return,
            }
        };
        let Some(bm) = crate::bookmarks::load().items.get(index).cloned() else { return };
        // No borrow is held past this point: the alert pumps events.
        let mtm = unsafe { MainThreadMarker::new_unchecked() };
        let informative = format!(
            "\u{ab}{}\u{bb} will no longer appear in Cmd+P. The conversation itself is not deleted.",
            bm.label
        );
        if !confirm_action(mtm, "Remove this bookmark?", &informative, "Remove") {
            return;
        }
        self.pane_switcher_drop_bookmark(index, &bm);
    }

    /// Drop a bookmark and rebuild the open switcher around the gap. Shared by
    /// Cmd+Backspace (after its alert) and Cmd+B (no alert, like on a pane).
    fn pane_switcher_drop_bookmark(&self, index: usize, bm: &crate::bookmarks::Bookmark) {
        // Read the file again rather than reuse the copy from before the alert:
        // another window may have changed the list meanwhile, which is also why
        // the bookmark is found by its key and not by its old index.
        let mut saved = crate::bookmarks::load();
        crate::bookmarks::remove(&mut saved.items, bm.key());
        crate::bookmarks::save(&saved);
        *self.ivars().bookmark_keys.borrow_mut() = crate::bookmarks::keys(&saved.items);
        self.set_transient_status(&format!("Removed bookmark {}", bm.label));

        if self.ivars().pane_switcher.borrow().is_none() {
            return;
        }
        self.open_pane_switcher(false);
        if let Some(state) = self.ivars().pane_switcher.borrow_mut().as_mut() {
            if let Some((c, r)) = bookmark_row_after_removal(&state.columns, index) {
                state.selected_col = c;
                state.selected_row = r;
            }
        }
        self.pane_switcher_clamp_scroll();
        self.mark_dirty();
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
        // fills the bookmark groups at the end.
        let saved = crate::bookmarks::load();
        let anchors = crate::anchors::load();
        // Also the moment to refresh both caches the status bars read: another
        // window — or track, through `set-anchor` — may have changed either list
        // since this one last did.
        *self.ivars().bookmark_keys.borrow_mut() = crate::bookmarks::keys(&saved.items);
        *self.ivars().anchor_keys.borrow_mut() = crate::anchors::keys(&anchors.items);
        let anchored = crate::anchors::keys(&anchors.items);
        // Anchored conversations count as saved here too: the pane's own status
        // bar paints them, and a row that contradicted the pane it points at
        // would be the worse of the two answers.
        let bookmarked_sessions: std::collections::HashSet<&str> = saved
            .items
            .iter()
            .filter_map(|b| b.session_id.as_deref())
            .chain(anchors.items.iter().filter_map(|a| a.session_id.as_deref()))
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
        // Saved conversations, one group per directory at the end of the list.
        // Only on the full list: the attention-only one answers "what is asking
        // for something", and a bookmark never asks for anything. Neither does
        // an anchor — same reason it is left out of the filtered list.
        if !filtered {
            groups = saved_sections(&anchors.items, &saved.items, groups, |bm| {
                anchored.contains(bm.key()) || self.pane_holding_session(bm).is_some()
            });
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

        // Cmd+Backspace on a bookmark row → remove it, after confirmation. Same
        // key as the closed-tabs list (Cmd+O). Pane rows ignore it.
        if keycode == 0x33 && event.modifierFlags().contains(NSEventModifierFlags::Command) {
            self.pane_switcher_remove_selected_bookmark();
            return;
        }

        // Cmd+B (the toggle-bookmark binding) → same toggle as in a pane: a pane
        // row gains or loses its bookmark, a bookmark row is dropped.
        let toggles_bookmark = KeyCombo::from_event(event);
        let toggles_bookmark = self.ivars().keybindings.get().is_some_and(|kb| {
            matches!(kb.window_map.get(&toggles_bookmark), Some(Action::ToggleBookmark))
        });
        if toggles_bookmark {
            self.pane_switcher_toggle_selected_bookmark();
            return;
        }

        // Cmd+Shift+A (the toggle-anchor binding) → the same gesture on the
        // other list: a pane row is anchored or un-anchored, an anchor row is
        // dropped. No alert on either: an anchor is a claim about today, and
        // taking it back costs one keystroke, unlike a bookmark you may have
        // been keeping for weeks.
        let toggles_anchor = KeyCombo::from_event(event);
        let toggles_anchor = self.ivars().keybindings.get().is_some_and(|kb| {
            matches!(kb.window_map.get(&toggles_anchor), Some(Action::ToggleAnchor))
        });
        if toggles_anchor {
            self.pane_switcher_toggle_selected_anchor();
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

    /// Cmd+B inside the switcher. A pane row toggles its bookmark and the list
    /// is rebuilt with the selection kept on that pane, so the band appears or
    /// goes away under the eye; a bookmark row is removed without an alert.
    fn pane_switcher_toggle_selected_bookmark(&self) {
        let target = {
            let guard = self.ivars().pane_switcher.borrow();
            match guard.as_ref().and_then(|s| s.columns.get(s.selected_col).and_then(|c| c.get(s.selected_row))) {
                Some(SwitcherRow::Pane { pane_id, .. }) => SwitcherTarget::Pane(*pane_id),
                Some(SwitcherRow::Bookmark { index, .. }) => SwitcherTarget::Bookmark(*index),
                _ => return,
            }
        };
        match target {
            SwitcherTarget::Bookmark(index) => {
                let Some(bm) = crate::bookmarks::load().items.get(index).cloned() else { return };
                self.pane_switcher_drop_bookmark(index, &bm);
            }
            // An anchor row has its own key (Cmd+Shift+A); Cmd+B on it would
            // bookmark what is already saved under a stronger promise.
            SwitcherTarget::Anchor(_) => {}
            SwitcherTarget::Pane(pane_id) => {
                self.toggle_pane_bookmark(pane_id);
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
        }
    }

    /// Cmd+Shift+A inside the switcher: anchor or un-anchor the pane on the
    /// selected row, or drop the selected anchor. The list is rebuilt either
    /// way, so the Anchors section grows or shrinks under the eye.
    fn pane_switcher_toggle_selected_anchor(&self) {
        let target = {
            let guard = self.ivars().pane_switcher.borrow();
            match guard.as_ref().and_then(|s| s.columns.get(s.selected_col).and_then(|c| c.get(s.selected_row))) {
                Some(SwitcherRow::Pane { pane_id, .. }) => SwitcherTarget::Pane(*pane_id),
                Some(SwitcherRow::Anchor { index, .. }) => SwitcherTarget::Anchor(*index),
                Some(SwitcherRow::Bookmark { index, .. }) => SwitcherTarget::Bookmark(*index),
                _ => return,
            }
        };
        // The conversation this ran on, by the key both lists store it under:
        // the rebuild below looks the row up again, and it may have changed
        // section in between.
        let mut acted_key: Option<String> = None;
        match target {
            SwitcherTarget::Anchor(index) => {
                let mut anchors = crate::anchors::load();
                let Some(anchor) = anchors.items.get(index).cloned() else { return };
                crate::anchors::remove(&mut anchors.items, anchor.key());
                crate::anchors::save(&anchors);
                self.refresh_anchor_keys(&anchors);
                acted_key = Some(anchor.key().to_string());
                self.set_transient_status(&format!("Removed anchor {}", anchor.label));
            }
            SwitcherTarget::Pane(pane_id) => self.toggle_pane_anchor(pane_id),
            // A bookmark row is promoted rather than ignored: it is the row you
            // are most likely to be pointing at when you decide what today is
            // for. It stays bookmarked, and moves up under the anchors.
            SwitcherTarget::Bookmark(index) => {
                let Some(bm) = crate::bookmarks::load().items.get(index).cloned() else { return };
                let mut anchors = crate::anchors::load();
                let label = bm.label.clone();
                acted_key = Some(bm.key().to_string());
                // The row on screen can be one push behind the file — another
                // window, or track, may have anchored this conversation since
                // Cmd+P was opened — so the message follows what the toggle did,
                // not what the row led us to expect.
                match crate::anchors::toggle(&mut anchors.items, bm) {
                    crate::anchors::Toggled::Full => {
                        self.set_transient_status(&format!(
                            "Already {} anchors — drop one first",
                            crate::anchors::MAX_ANCHORS
                        ));
                        return;
                    }
                    outcome => {
                        crate::anchors::save(&anchors);
                        self.refresh_anchor_keys(&anchors);
                        self.set_transient_status(&if outcome == crate::anchors::Toggled::Added {
                            format!("Anchored {}", label)
                        } else {
                            format!("Removed anchor {}", label)
                        });
                    }
                }
            }
        }
        if self.ivars().pane_switcher.borrow().is_none() {
            return;
        }
        let filtered = self.ivars().pane_switcher.borrow().as_ref().is_some_and(|s| s.filtered);
        self.open_pane_switcher(filtered);
        // Put the selection back where the eye is: the rebuild moved every row
        // below the Anchors section, which just gained or lost a line. A pane is
        // found again by its id; an anchored or un-anchored conversation by the
        // key it is saved under, since its row changed section on the way.
        if let Some(state) = self.ivars().pane_switcher.borrow_mut().as_mut() {
            let anchors = crate::anchors::load();
            let bookmarks = crate::bookmarks::load();
            let row_matches = |row: &SwitcherRow| match (row, &target) {
                (SwitcherRow::Pane { pane_id, .. }, SwitcherTarget::Pane(id)) => pane_id == id,
                // Un-anchoring a conversation a pane still runs: its row is that
                // pane's, up in its tab, since the bookmark section leaves out
                // what is open.
                (SwitcherRow::Pane { pane_id, .. }, _) => {
                    acted_key.is_some()
                        && self.pane_as_saved(*pane_id).map(|s| s.key().to_string()) == acted_key
                }
                (SwitcherRow::Anchor { index, .. }, _) => {
                    anchors.items.get(*index).map(|a| a.key()) == acted_key.as_deref()
                }
                (SwitcherRow::Bookmark { index, .. }, _) => {
                    bookmarks.items.get(*index).map(|b| b.key()) == acted_key.as_deref()
                }
                _ => false,
            };
            for (c, col) in state.columns.iter().enumerate() {
                if let Some(r) = col.iter().position(row_matches) {
                    state.selected_col = c;
                    state.selected_row = r;
                    break;
                }
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
                    Some(SwitcherRow::Anchor { index, .. }) => {
                        Some(SwitcherTarget::Anchor(*index))
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
            Some(SwitcherTarget::Anchor(index)) => self.open_anchor(index),
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
                        Some(SwitcherRow::Anchor { index, .. }) => {
                            Some(SwitcherTarget::Anchor(*index))
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
            Some(SwitcherTarget::Anchor(index)) => self.open_anchor(index),
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
                        SwitcherRow::Anchor { .. } => 'a',
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
    fn the_list_reads_anchors_then_panes_then_bookmarks() {
        // The order is the whole point of the section: today's work first, what
        // is actually running second, the rest after.
        let tabs = switcher_grid(&["h.."]);
        let groups = saved_sections(&[saved("a")], &[saved("b")], tabs, |_| false);
        assert_eq!(switcher_spec(&groups), vec!["ha".to_string(), "h..".into(), "hk".into()]);
    }

    #[test]
    fn an_anchor_stays_listed_while_its_pane_is_open_and_its_bookmark_does_not() {
        // Same conversation in both lists, and open in a pane: one row, under
        // the anchors. This is the difference between the two sections.
        let hidden = |bm: &crate::bookmarks::Bookmark| bm.session_id.as_deref() == Some("a");
        let groups = saved_sections(&[saved("a")], &[saved("a")], Vec::new(), hidden);
        assert_eq!(switcher_spec(&groups), vec!["ha".to_string()]);
    }

    #[test]
    fn the_anchor_section_lists_every_anchor_it_is_given() {
        // The counterpart of the bookmark rule right below: a bookmark whose
        // pane is open leaves its group, an anchor never does — the section is
        // what today is for, not what is missing from the screen.
        let groups = anchor_groups(&[saved("a"), saved("b")]);
        assert_eq!(switcher_spec(&groups), vec!["haa".to_string()]);
        match &groups[0][0] {
            SwitcherRow::TabHeader(h) => assert!(h.starts_with('\u{2693}'), "headed by the anchor"),
            _ => panic!("the section opens with its header"),
        }
    }

    #[test]
    fn no_anchor_means_no_section_at_all() {
        assert!(anchor_groups(&[]).is_empty(), "an empty section would cost a line to say nothing");
    }

    #[test]
    fn an_anchored_conversation_is_not_offered_a_second_time_as_a_bookmark() {
        let items = vec![saved("a"), saved("b")];
        let anchored = crate::anchors::keys(&[saved("a")]);
        let groups = bookmark_groups(&items, |bm| anchored.contains(bm.key()));
        assert_eq!(switcher_spec(&groups), vec!["hk".to_string()]);
        match &groups[0][1] {
            SwitcherRow::Bookmark { index, .. } => assert_eq!(*index, 1, "the row left is the other one"),
            _ => panic!("expected the bookmark row"),
        }
    }

    #[test]
    fn an_anchor_row_is_selectable_but_is_not_a_pane() {
        let row = SwitcherRow::Anchor { index: 0, title: "conv".into(), detail: None };
        assert!(row.is_selectable());
        assert!(!row.is_pane());
        assert!(!row.needs_attention(), "an anchor asks for nothing, like a bookmark");
    }

    #[test]
    fn an_open_bookmark_leaves_its_directory_group() {
        let items = vec![saved("a"), saved("b")];
        let groups = bookmark_groups(&items, |bm| bm.session_id.as_deref() == Some("a"));
        assert_eq!(switcher_spec(&groups), vec!["hk".to_string()]);
        // The row still points at the bookmark's own index in the saved list,
        // not at its position in the filtered rows.
        match &groups[0][1] {
            SwitcherRow::Bookmark { index, .. } => assert_eq!(*index, 1),
            _ => panic!("expected a bookmark row"),
        }
    }

    #[test]
    fn bookmarks_are_grouped_by_directory_in_order_of_their_newest() {
        let at = |id: &str, cwd: &str| crate::bookmarks::Bookmark { cwd: cwd.into(), ..saved(id) };
        let items = vec![at("a", "/h/cto"), at("b", "/h/self"), at("c", "/h/cto")];
        let groups = bookmark_groups(&items, |_| false);
        assert_eq!(switcher_spec(&groups), vec!["hkk".to_string(), "hk".to_string()]);
        let indexes: Vec<usize> = groups[0]
            .iter()
            .filter_map(|r| match r {
                SwitcherRow::Bookmark { index, .. } => Some(*index),
                _ => None,
            })
            .collect();
        assert_eq!(indexes, vec![0, 2]);
        match (&groups[0][0], &groups[1][0]) {
            (SwitcherRow::TabHeader(first), SwitcherRow::TabHeader(second)) => {
                assert_eq!(first, "★ cto");
                assert_eq!(second, "★ self");
            }
            _ => panic!("each group starts with its directory"),
        }
    }

    #[test]
    fn a_directory_header_shows_the_path_only_when_names_clash() {
        let cwds = ["/Users/x/a/app", "/Users/x/b/app", "/Users/x/cto"];
        assert_eq!(bookmark_dir_header("/Users/x/cto", &cwds, Some("/Users/x")), "cto");
        assert_eq!(bookmark_dir_header("/Users/x/a/app", &cwds, Some("/Users/x")), "~/a/app");
        assert_eq!(bookmark_dir_header("/opt/app", &["/opt/app", "/srv/app"], Some("/Users/x")), "/opt/app");
        // "/Users/xy" is not under "/Users/x".
        assert_eq!(bookmark_dir_header("/Users/xy/app", &["/Users/xy/app", "/app"], Some("/Users/x")), "/Users/xy/app");
    }

    #[test]
    fn after_a_removal_the_selection_takes_the_next_bookmark_then_the_last() {
        let bookmark = |index: usize| SwitcherRow::Bookmark { index, title: "b".into(), detail: None };
        // Saved list after removing index 1: what was 2 and 3 are now 1 and 2,
        // and the bookmark at 0 is open in a pane, so it has no row.
        let columns = vec![
            switcher_grid(&["h."]).remove(0),
            vec![SwitcherRow::TabHeader("Bookmarks".into()), bookmark(1), bookmark(2)],
        ];
        assert_eq!(bookmark_row_after_removal(&columns, 1), Some((1, 1)));
        // The last one removed: fall back on the row above it.
        assert_eq!(bookmark_row_after_removal(&columns, 3), Some((1, 2)));
        // No bookmark left: the caller keeps its own selection.
        assert_eq!(bookmark_row_after_removal(&switcher_grid(&["h."]), 0), None);
    }

    #[test]
    fn bookmark_groups_disappear_when_every_bookmark_is_open() {
        let items = vec![saved("a"), saved("b")];
        assert!(bookmark_groups(&items, |_| true).is_empty(), "no header without rows");
        assert!(bookmark_groups(&[], |_| false).is_empty());
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
    fn a_bookmark_row_names_its_agent_only() {
        let bm = crate::bookmarks::Bookmark {
            agent: Some(crate::agent_session::Agent::Codex),
            session_id: Some("id".into()),
            cwd: "/Users/x/projects/kova".into(),
            label: "the split refactor".into(),
        };
        assert_eq!(bookmark_detail(&bm).as_deref(), Some("codex"));
        let shell = crate::bookmarks::Bookmark {
            agent: None,
            session_id: None,
            cwd: "/Users/x/projects/kova".into(),
            label: "shell".into(),
        };
        assert_eq!(bookmark_detail(&shell), None);
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
