//! What a pane, tab or window looks like to an IPC client, and every command
//! the socket can run against this window. The parsing and the socket itself
//! live in `crate::ipc`; this is the half that touches the view.

use super::*;

/// Outcome of `KovaView::ipc_close_tab`. Lets the caller distinguish "not in this window"
/// (keep scanning) from "last tab — refuse" (final answer).
pub enum IpcCloseTabResult {
    Closed,
    WouldTerminate,
    NotFound,
}

/// Outcome of `KovaView::ipc_merge_tab`. `SourceMissing` lets the caller keep scanning
/// across windows; `TargetMissing` is a final answer because we found the source here.
pub enum IpcMergeTabResult {
    Merged,
    SourceMissing,
    TargetMissing,
}

/// Outcome of `KovaView::ipc_swap_pane`. Same pattern as merge: only `AMissing` keeps
/// scanning across windows.
pub enum IpcSwapPaneResult {
    Swapped,
    AMissing,
    BMissing,
    Failed,
}

/// The JSON shape of one pane, shared by `list-panes`, the `subscribe` snapshot
/// and the event payloads — a client parses the same object everywhere.
///
/// Costs a `proc_pidinfo` (the CWD) and a process-table walk (the children), so
/// callers on a polling path must build it only for panes they actually report.
pub fn pane_json(
    pane: &Pane,
    win_idx: usize,
    tab_idx: usize,
    focused: bool,
) -> serde_json::Value {
    let pid = pane.pty.pid();
    let children = pane.pty.child_processes();
    let is_idle = children.is_empty();
    let child_json: Vec<serde_json::Value> = children
        .into_iter()
        .map(|(cpid, info)| {
            serde_json::json!({
                "pid": cpid,
                "name": info.name,
                "version": info.version,
            })
        })
        .collect();
    serde_json::json!({
        "id": pane.id,
        "window": win_idx,
        "tab": tab_idx,
        "cwd": pane.cwd().unwrap_or_default(),
        "title": pane.display_title("shell"),
        "focused": focused,
        "pid": pid,
        "child_processes": child_json,
        "is_idle": is_idle,
        "working": pane.is_working(),
        "awaiting": pane.is_awaiting(),
        "awaiting_since": pane.awaiting_since(),
        // Whether the waiting pane has been looked at here since it started waiting. A remote
        // client walking waiting panes needs that halt, or it hands back panes already read on
        // this Mac. False on a pane that is not waiting at all, where the bit means nothing.
        "awaiting_seen": pane.is_awaiting() && !pane.is_awaiting_unseen(),
        "minimized": pane.minimized,
        "claude_session_id": pane.claude_session_id(),
        "claude_session_name": pane.claude_session_name(),
    })
}

impl KovaView {
    /// IPC: create a split in the active tab's focused pane.
    /// Returns the new pane's ID on success.
    pub fn ipc_split(
        &self,
        config: &crate::config::Config,
        direction: SplitDirection,
        cwd: Option<&str>,
        command: Option<String>,
    ) -> Option<PaneId> {
        let (focused_id, current_vp) = {
            let tabs = self.ivars().tabs.borrow();
            let idx = self.ivars().active_tab.get();
            let tab = tabs.get(idx)?;
            let fid = tab.focused_pane;
            let vp = tab.viewport_for_pane(fid, self.panes_viewport_for_tab(tab))?;
            (fid, vp)
        };

        let half_vp = match direction {
            SplitDirection::Horizontal => PaneViewport {
                x: current_vp.x,
                y: current_vp.y,
                width: current_vp.width / 2.0,
                height: current_vp.height,
            },
            SplitDirection::Vertical => PaneViewport {
                x: current_vp.x,
                y: current_vp.y,
                width: current_vp.width,
                height: current_vp.height / 2.0,
            },
        };
        let (cols, rows) = self.viewport_to_grid(&half_vp);

        let new_pane = match Pane::spawn(cols, rows, config, cwd) {
            Ok(p) => p,
            Err(e) => {
                log::error!("IPC split: failed to spawn pane: {}", e);
                return None;
            }
        };

        // If a command was provided, set it as pending (will be injected once shell is ready)
        if let Some(cmd) = command {
            new_pane.pending_command.set(Some(cmd));
        }

        let new_id = new_pane.id;
        let open_timer = new_pane.open_timer.clone();

        let mut tabs = self.ivars().tabs.borrow_mut();
        let idx = self.ivars().active_tab.get();
        if let Some(tab) = tabs.get_mut(idx) {
            match direction {
                SplitDirection::Horizontal => {
                    let screen = self.drawable_viewport().width;
                    let min_w = self.min_split_width_px();
                    let old_virtual = tab.virtual_width(screen, min_w);
                    tab.insert_column_after_focused(new_pane);
                    if old_virtual > screen {
                        if let Some(new_col_idx) = tab.column_index_of(new_id) {
                            let new_col_px = current_vp.width.max(min_w);
                            tab.grow_virtual_for_scrolled_split(new_col_idx, old_virtual, new_col_px, screen);
                        }
                    }
                }
                SplitDirection::Vertical => {
                    tab.vsplit_at_pane(focused_id, new_pane);
                }
            }
            tab.focused_pane = new_id;
            self.scroll_to_reveal_pane(tab, new_id, self.drawable_viewport().width);
        }
        drop(tabs);

        open_timer.mark_inserted(new_id);
        self.resize_all_panes();
        log::info!("IPC: split created pane {}", new_id);
        Some(new_id)
    }

    /// IPC: close a specific pane by ID. Returns true if found and closed.
    /// Try to close a pane by ID. Returns:
    /// - Some(true) = closed successfully
    /// - Some(false) = found but refused (last pane in last tab)
    /// - None = pane not in this window
    pub fn ipc_close_pane(&self, pane_id: PaneId) -> Option<bool> {
        let mut tabs = self.ivars().tabs.borrow_mut();

        // Find which tab contains this pane
        let tab_idx = match tabs.iter().position(|tab| tab.contains(pane_id)) {
            Some(i) => i,
            None => return None,
        };

        // If it's the sole pane in the sole tab, refuse (would close the window)
        if tabs.len() == 1 && tabs[0].is_single_pane() {
            return Some(false);
        }

        if tabs[tab_idx].is_single_pane() {
            // Close the entire tab
            crate::recent_projects::add(&tabs[tab_idx]);
            tabs.remove(tab_idx);
            if tabs.is_empty() {
                drop(tabs);
                self.ivars().closing.set(true);
                return Some(true);
            }
            let new_idx = if tab_idx >= tabs.len() { tabs.len() - 1 } else { tab_idx };
            drop(tabs);
            self.ivars().active_tab.set(new_idx);
            self.resize_all_panes();
            log::info!("IPC: closed tab containing pane {}", pane_id);
            return Some(true);
        }

        // Multiple panes — close just this pane
        let panes_vp = self.panes_viewport_for_tab(&tabs[tab_idx]);
        let next_focus = tabs[tab_idx].neighbor(pane_id, crate::pane::NavDirection::Right, panes_vp)
            .or_else(|| tabs[tab_idx].neighbor(pane_id, crate::pane::NavDirection::Left, panes_vp))
            .or_else(|| tabs[tab_idx].neighbor(pane_id, crate::pane::NavDirection::Down, panes_vp))
            .or_else(|| tabs[tab_idx].neighbor(pane_id, crate::pane::NavDirection::Up, panes_vp));

        let old_columns = tabs[tab_idx].num_visible_columns();
        if !tabs[tab_idx].remove_pane(pane_id) {
            drop(tabs);
            return Some(false);
        }
        tabs[tab_idx].minimized_stack.retain(|&pid| pid != pane_id);
        // If only minimized panes remain, restore the last minimized one
        let restored = tabs[tab_idx].ensure_visible_pane();
        let new_focus = restored
            .or(next_focus.filter(|id| tabs[tab_idx].contains(*id)))
            .or_else(|| tabs[tab_idx].first_visible_pane())
            .unwrap_or_else(|| tabs[tab_idx].first_pane().id);
        tabs[tab_idx].focused_pane = new_focus;
        let new_columns = tabs[tab_idx].num_visible_columns();
        tabs[tab_idx].scale_virtual_width(old_columns, new_columns);
        let full = self.drawable_viewport();
        let min_w = self.min_split_width_px();
        tabs[tab_idx].clamp_scroll(full.width, min_w);
        let tab = &mut tabs[tab_idx];
        self.scroll_to_reveal_pane(tab, new_focus, full.width);
        drop(tabs);
        self.resize_all_panes();
        log::info!("IPC: closed pane {}", pane_id);
        Some(true)
    }

    /// IPC: get the CWD of the focused pane (for split fallback).
    pub fn ipc_focused_cwd(&self) -> Option<String> {
        let tabs = self.ivars().tabs.borrow();
        let idx = self.ivars().active_tab.get();
        tabs.get(idx).and_then(|tab| {
            tab.pane(tab.focused_pane).and_then(|p| p.cwd())
        })
    }

    /// IPC: collect pane info as JSON values for the list-panes command.
    pub fn ipc_collect_panes(&self, win_idx: usize, is_key_window: bool, out: &mut Vec<serde_json::Value>) {
        let tabs = self.ivars().tabs.borrow();
        let active_tab = self.ivars().active_tab.get();
        for (tab_idx, tab) in tabs.iter().enumerate() {
            let focused_id = tab.focused_pane;
            let is_active_tab = tab_idx == active_tab;
            tab.for_each_pane(&mut |pane| {
                let is_focused = pane.id == focused_id && is_active_tab && is_key_window;
                out.push(pane_json(pane, win_idx, tab_idx, is_focused));
            });
        }
    }

    /// The pane this window would hand the keyboard to: the focused pane of the
    /// active tab, its tab index, and the Claude conversation running in it.
    ///
    /// The conversation is part of the identity, not a detail of the payload: a
    /// pane where you have just launched `claude` is not the same work surface it
    /// was a second earlier, and without this the event only ever fired when the
    /// *pane* changed — so a session started in the pane you were already in went
    /// unannounced until you left and came back.
    ///
    /// Still cheap enough for every tick: two `RefCell` reads and a short string
    /// clone, no process introspection (the session is cached by the throttled probe).
    pub fn events_focus_key(&self) -> Option<(usize, PaneId, Option<String>, Option<String>)> {
        let tabs = self.ivars().tabs.borrow();
        let idx = self.ivars().active_tab.get();
        let tab = tabs.get(idx)?;
        let pane_id = tab.focused_pane;
        let pane = tab.pane(pane_id);
        let session = pane.and_then(|p| p.claude_session_id());
        // The name too, not just the id: `/rename` is how the user says the conversation is about
        // something else now, and a client that never hears about it keeps filing the work under
        // the old subject.
        let name = pane.and_then(|p| p.claude_session_name());
        Some((idx, pane_id, session, name))
    }

    /// Full JSON for one pane, if this window holds it. Building it costs a
    /// `proc_pidinfo` for the CWD, so it happens on an edge only, never on the
    /// polling path.
    pub fn events_pane_json(
        &self,
        win_idx: usize,
        pane_id: PaneId,
        is_key_window: bool,
    ) -> Option<serde_json::Value> {
        let tabs = self.ivars().tabs.borrow();
        let active_tab = self.ivars().active_tab.get();
        for (tab_idx, tab) in tabs.iter().enumerate() {
            if let Some(pane) = tab.pane(pane_id) {
                let focused =
                    pane.id == tab.focused_pane && tab_idx == active_tab && is_key_window;
                return Some(pane_json(pane, win_idx, tab_idx, focused));
            }
        }
        None
    }

    /// Collect the per-pane flags the event diff watches. Reads nothing that
    /// costs a syscall — this runs on a timer, across every pane of every window.
    pub fn events_collect_flags(
        &self,
        win_idx: usize,
        out: &mut std::collections::HashMap<PaneId, crate::events::PaneFlags>,
    ) {
        let tabs = self.ivars().tabs.borrow();
        for (tab_idx, tab) in tabs.iter().enumerate() {
            tab.for_each_pane(&mut |pane| {
                out.insert(
                    pane.id,
                    crate::events::PaneFlags {
                        window: win_idx,
                        tab: tab_idx,
                        working: pane.is_working(),
                        awaiting: pane.is_awaiting(),
                        awaiting_since: pane.awaiting_since(),
                    },
                );
            });
        }
    }

    /// IPC: write text to a pane's PTY. Returns true if the pane was found.
    pub fn ipc_send_keys(&self, pane_id: PaneId, text: &str) -> bool {
        let tabs = self.ivars().tabs.borrow();
        for tab in tabs.iter() {
            if let Some(pane) = tab.pane(pane_id) {
                // Same rule as a keystroke: someone answered this pane.
                pane.clear_awaiting();
                pane.pty.write(text.as_bytes());
                return true;
            }
        }
        false
    }

    /// IPC: focus a pane by ID (switch tab if needed). Returns true if found.
    pub fn ipc_focus_pane(&self, pane_id: PaneId) -> bool {
        let mut tabs = self.ivars().tabs.borrow_mut();
        let tab_idx = match tabs.iter().position(|tab| tab.contains(pane_id)) {
            Some(i) => i,
            None => return false,
        };

        // Focusing a minimized (hidden) pane restores it first — it has no
        // layout footprint, so focus alone would land on an invisible pane.
        let full = self.drawable_viewport();
        let min_w = self.min_split_width_px();
        if tabs[tab_idx].pane(pane_id).is_some_and(|p| p.minimized) {
            tabs[tab_idx].restore_pane_adjust_virtual(pane_id, full.width, min_w);
            tabs[tab_idx].mark_all_dirty();
        }
        tabs[tab_idx].focused_pane = pane_id;
        tabs[tab_idx].clamp_scroll(full.width, min_w);
        let tab = &mut tabs[tab_idx];
        self.scroll_to_reveal_pane(tab, pane_id, full.width);
        drop(tabs);

        self.ivars().active_tab.set(tab_idx);
        self.resize_all_panes();
        log::info!("IPC: focused pane {}", pane_id);
        true
    }

    /// IPC: collect all pane IDs in this window (used to expand `panes: "all"`).
    pub fn ipc_collect_pane_ids(&self, out: &mut Vec<PaneId>) {
        let tabs = self.ivars().tabs.borrow();
        for tab in tabs.iter() {
            tab.for_each_pane(&mut |p| out.push(p.id));
        }
    }

    /// IPC: build a JSON entry with the rendered text of a pane.
    /// Returns `None` if the pane is not in this window.
    pub fn ipc_dump_pane_text(
        &self,
        pane_id: PaneId,
        mode: crate::terminal::DumpMode,
        trim: bool,
    ) -> Option<serde_json::Value> {
        let tabs = self.ivars().tabs.borrow();
        for tab in tabs.iter() {
            if let Some(pane) = tab.pane(pane_id) {
                let term = pane.terminal.read();
                let dump = term.dump_text(mode, trim);
                return Some(serde_json::json!({
                    "id": pane_id,
                    "text": dump.text,
                    "cols": dump.cols,
                    "rows": dump.rows,
                    "cursor": { "row": dump.cursor_row, "col": dump.cursor_col },
                }));
            }
        }
        None
    }

    /// IPC: report whether the OSC 133;D flag is set for a pane.
    /// `None` means the pane is not in this window; `Some(b)` is the flag's value.
    pub fn ipc_check_completion(&self, pane_id: PaneId) -> Option<bool> {
        let tabs = self.ivars().tabs.borrow();
        for tab in tabs.iter() {
            if let Some(pane) = tab.pane(pane_id) {
                let flag = pane
                    .terminal
                    .read()
                    .command_completed
                    .load(std::sync::atomic::Ordering::Relaxed);
                return Some(flag);
            }
        }
        None
    }

    /// IPC: measure how big the rendered text of a pane would be.
    /// Returns `(chars, bytes)`, or `None` if the pane is not in this window.
    pub fn ipc_measure_pane_text(
        &self,
        pane_id: PaneId,
        mode: crate::terminal::DumpMode,
        trim: bool,
    ) -> Option<(usize, usize)> {
        let tabs = self.ivars().tabs.borrow();
        for tab in tabs.iter() {
            if let Some(pane) = tab.pane(pane_id) {
                let term = pane.terminal.read();
                return Some(term.measure_text(mode, trim));
            }
        }
        None
    }

    /// IPC: set the custom title of the tab containing `pane_id`.
    /// `title: None` clears the custom title (tab falls back to auto-derived title).
    /// Returns true if the pane was found in this window.
    pub fn ipc_set_tab_title(&self, pane_id: PaneId, title: Option<String>) -> bool {
        let mut tabs = self.ivars().tabs.borrow_mut();
        let tab_idx = match tabs.iter().position(|tab| tab.contains(pane_id)) {
            Some(i) => i,
            None => return false,
        };
        tabs[tab_idx].custom_title = title;
        log::info!("IPC: set tab title for pane {}", pane_id);
        true
    }

    /// IPC: color the tab holding `pane_id`, or clear its color with `None`.
    ///
    /// Same palette as the tab bar's right-click menu, and the redraw is asked for the
    /// same way: nothing else observes this field, so without it the tab keeps its old
    /// color until something unrelated makes the window dirty.
    pub fn ipc_set_tab_color(&self, pane_id: PaneId, color: Option<usize>) -> bool {
        let mut tabs = self.ivars().tabs.borrow_mut();
        let tab_idx = match tabs.iter().position(|tab| tab.contains(pane_id)) {
            Some(i) => i,
            None => return false,
        };
        tabs[tab_idx].color = color;
        drop(tabs);
        self.mark_dirty();
        log::info!("IPC: set tab color for pane {}", pane_id);
        true
    }

    /// IPC: create a new tab. Returns (tab_id, pane_id) on success.
    pub fn ipc_new_tab(
        &self,
        config: &crate::config::Config,
        cwd: Option<&str>,
        command: Option<String>,
    ) -> Option<(u32, u32)> {
        // Determine CWD: explicit param > focused pane's CWD
        let effective_cwd: Option<String> = cwd.map(String::from).or_else(|| self.ipc_focused_cwd());

        let tab = match Tab::new_with_cwd(config, effective_cwd.as_deref()) {
            Ok(t) => t,
            Err(e) => {
                log::error!("IPC new-tab: failed to create tab: {}", e);
                return None;
            }
        };

        let tab_id = tab.id;
        let pane_id = tab.first_pane().id;

        // If a command was provided, set it as pending
        if let Some(cmd) = command {
            tab.first_pane().pending_command.set(Some(cmd));
        }

        let mut tabs = self.ivars().tabs.borrow_mut();
        let new_idx = self.ivars().active_tab.get() + 1;
        tabs.insert(new_idx, tab);
        drop(tabs);
        self.ivars().active_tab.set(new_idx);
        self.resize_all_panes();
        log::info!("IPC: new tab created: tab_id={}, pane_id={}", tab_id, pane_id);
        Some((tab_id, pane_id))
    }

    /// IPC: collect this window's tabs as JSON entries.
    pub fn ipc_collect_tabs(&self, win_idx: usize, is_key_window: bool, out: &mut Vec<serde_json::Value>) {
        let tabs = self.ivars().tabs.borrow();
        let active_tab = self.ivars().active_tab.get();
        for (tab_idx, tab) in tabs.iter().enumerate() {
            let mut pane_count = 0;
            tab.for_each_pane(&mut |_| { pane_count += 1; });
            let is_active = tab_idx == active_tab && is_key_window;
            out.push(serde_json::json!({
                "id": tab.id,
                "window": win_idx,
                "tab_index": tab_idx,
                "title": tab.title(),
                "pane_count": pane_count,
                "focused_pane_id": tab.focused_pane,
                "active": is_active,
                "has_bell": tab.has_bell,
                "has_completion": tab.has_completion,
                "has_running": tab.has_running,
            }));
        }
    }

    /// IPC: close a tab by ID. Refuses to close the very last tab — that would
    /// terminate the app, which is too surprising for a remote caller.
    pub fn ipc_close_tab(&self, tab_id: u32) -> IpcCloseTabResult {
        let idx = {
            let tabs = self.ivars().tabs.borrow();
            match tabs.iter().position(|t| t.id == tab_id) {
                Some(i) => i,
                None => return IpcCloseTabResult::NotFound,
            }
        };
        // Refuse if this would empty the window (remove_tab terminates the app in that case).
        if self.ivars().tabs.borrow().len() <= 1 {
            return IpcCloseTabResult::WouldTerminate;
        }
        self.remove_tab(idx);
        log::info!("IPC: closed tab {}", tab_id);
        IpcCloseTabResult::Closed
    }

    /// IPC: merge `source_tab_id` into `target_tab_id` (both must be in this window).
    pub fn ipc_merge_tab(&self, source_tab_id: u32, target_tab_id: u32) -> IpcMergeTabResult {
        let (source_idx, target_idx) = {
            let tabs = self.ivars().tabs.borrow();
            let s = match tabs.iter().position(|t| t.id == source_tab_id) {
                Some(i) => i,
                None => return IpcMergeTabResult::SourceMissing,
            };
            let t = match tabs.iter().position(|t| t.id == target_tab_id) {
                Some(i) => i,
                None => return IpcMergeTabResult::TargetMissing,
            };
            (s, t)
        };

        // `merge_active_tab_into` operates on the active tab, so move the active
        // pointer to the source first, then call it. The function adjusts the
        // target index internally to account for the source removal.
        self.ivars().active_tab.set(source_idx);
        self.merge_active_tab_into(target_idx);
        log::info!("IPC: merged tab {} into tab {}", source_tab_id, target_tab_id);
        IpcMergeTabResult::Merged
    }

    /// IPC: swap two panes. Both must live in the same tab.
    pub fn ipc_swap_pane(&self, pane_id_a: PaneId, pane_id_b: PaneId) -> IpcSwapPaneResult {
        if pane_id_a == pane_id_b {
            return IpcSwapPaneResult::Failed;
        }
        let mut tabs = self.ivars().tabs.borrow_mut();
        let tab_idx = match tabs.iter().position(|t| t.contains(pane_id_a)) {
            Some(i) => i,
            None => return IpcSwapPaneResult::AMissing,
        };
        if !tabs[tab_idx].contains(pane_id_b) {
            return IpcSwapPaneResult::BMissing;
        }
        // Pick a synthetic direction based on layout: same column → Up (in-column),
        // different columns → Right (swap whole columns). This reuses the existing
        // direction-aware logic without forcing the caller to know layout details.
        let tab = &mut tabs[tab_idx];
        let col_a = tab.column_index_of(pane_id_a);
        let col_b = tab.column_index_of(pane_id_b);
        let dir = match (col_a, col_b) {
            (Some(a), Some(b)) if a == b => crate::pane::NavDirection::Up,
            (Some(_), Some(_)) => crate::pane::NavDirection::Right,
            _ => return IpcSwapPaneResult::Failed,
        };
        let ok = tab.swap_panes(pane_id_a, pane_id_b, dir);
        if !ok {
            return IpcSwapPaneResult::Failed;
        }
        // Mark both panes dirty so they redraw in their new positions.
        if let Some(p) = tab.pane(pane_id_a) {
            p.terminal.read().dirty.store(true, std::sync::atomic::Ordering::Relaxed);
        }
        if let Some(p) = tab.pane(pane_id_b) {
            p.terminal.read().dirty.store(true, std::sync::atomic::Ordering::Relaxed);
        }
        drop(tabs);
        self.resize_all_panes();
        log::info!("IPC: swapped panes {} and {}", pane_id_a, pane_id_b);
        IpcSwapPaneResult::Swapped
    }

    /// IPC: resize the split containing `pane_id`. `grow=true` makes that pane's
    /// column/row larger; `grow=false` makes it smaller. Returns:
    /// - `None` if the pane isn't in this window (caller should keep scanning).
    /// - `Some(false)` if there's no neighbor to resize against (single-pane axis).
    /// - `Some(true)` on success.
    pub fn ipc_resize_pane(
        &self,
        pane_id: PaneId,
        axis: crate::pane::SplitAxis,
        grow: bool,
        amount_pct: f32,
    ) -> Option<bool> {
        use crate::pane::SplitAxis;
        let mut tabs = self.ivars().tabs.borrow_mut();
        let tab_idx = tabs.iter().position(|t| t.contains(pane_id))?;
        let tab = &mut tabs[tab_idx];

        // Translate grow/shrink into the signed delta the internal API expects.
        // For non-last position, delta>0 = grow. For last position, delta<0 = grow.
        // (Mirrors adjust_column_weight_directional / adjust_row_weight_directional.)
        let is_last = match axis {
            SplitAxis::Horizontal => {
                let col_idx = tab.column_index_of(pane_id);
                if tab.columns.len() < 2 { return Some(false); }
                col_idx.map(|i| i == tab.columns.len() - 1).unwrap_or(false)
            }
            SplitAxis::Vertical => {
                let col_idx = match tab.column_index_of(pane_id) {
                    Some(i) => i,
                    None => return Some(false),
                };
                let col = &tab.columns[col_idx];
                if col.panes.len() < 2 { return Some(false); }
                col.panes.iter().position(|p| p.id == pane_id)
                    .map(|i| i == col.panes.len() - 1)
                    .unwrap_or(false)
            }
        };
        let mag = amount_pct / 100.0;
        let delta = match (grow, is_last) {
            (true, false) => mag,
            (true, true) => -mag,
            (false, false) => -mag,
            (false, true) => mag,
        };

        let changed = tab.adjust_ratio_directional(pane_id, delta, axis);
        if !changed {
            return Some(false);
        }
        let full = self.drawable_viewport();
        let min_w = self.min_split_width_px();
        self.cap_virtual_width(tab, full.width, min_w);
        tab.clamp_scroll(full.width, min_w);
        drop(tabs);
        self.resize_all_panes();
        self.mark_dirty();
        log::info!("IPC: resized pane {} ({:?} {}{}%)", pane_id, axis, if grow {"+"} else {"-"}, amount_pct);
        Some(true)
    }

    /// IPC: set/clear a pane's sticky custom title (equivalent to OSC 1 / Cmd-Option-R).
    /// Returns true if the pane was found.
    pub fn ipc_rename_pane(&self, pane_id: PaneId, title: Option<String>) -> bool {
        let mut tabs = self.ivars().tabs.borrow_mut();
        for tab in tabs.iter_mut() {
            // for_each_pane is read-only; we need mutable access to set custom_title.
            // Walk columns directly for that.
            for col in tab.columns.iter_mut() {
                for pane in col.panes.iter_mut() {
                    if pane.id == pane_id {
                        pane.custom_title = title.clone();
                        // Also clear any pending OSC 1 sticky from the terminal so a stale
                        // OSC 1 doesn't immediately overwrite the IPC title on next frame.
                        pane.terminal.write().osc1_title = None;
                        pane.terminal.read().dirty.store(true, std::sync::atomic::Ordering::Relaxed);
                        log::info!("IPC: renamed pane {}", pane_id);
                        return true;
                    }
                }
            }
        }
        false
    }

    /// IPC: set/clear a pane's "waiting for the user" flag. Returns true if the
    /// pane was found. Nothing in the UI draws this flag — it exists for IPC
    /// clients (`list-panes`, the `pane-status` event), which is why it is
    /// stored as-is and never filtered on the focused pane.
    pub fn ipc_set_pane_status(&self, pane_id: PaneId, waiting: bool) -> bool {
        let tabs = self.ivars().tabs.borrow();
        for tab in tabs.iter() {
            let Some(pane) = tab.pane(pane_id) else { continue };
            if waiting {
                pane.set_awaiting();
            } else {
                pane.clear_awaiting();
            }
            log::info!("IPC: pane {} waiting={}", pane_id, waiting);
            self.mark_dirty();
            return true;
        }
        false
    }
}
