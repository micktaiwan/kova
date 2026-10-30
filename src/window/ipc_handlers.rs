//! What a pane, tab or window looks like to an IPC client, and every command
//! the socket can run against this window. The parsing and the socket itself
//! live in `crate::ipc`, the pane commands' replies in `crate::ipc::host`;
//! this is the half that touches the view.

use super::*;
use crate::ipc::host::{IpcCloseTabResult, PaneSignal};
use crate::ipc::host::TabInfo;

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
/// and the event payloads — a client parses the same object everywhere. The
/// shape itself is `crate::ipc::host::surface_json`, which a library host
/// shares.
///
/// Costs a `proc_pidinfo` (the CWD) and a process-table walk (the children), so
/// callers on a polling path must build it only for panes they actually report.
pub fn pane_json(
    pane: &Pane,
    win_idx: usize,
    tab_idx: usize,
    focused: bool,
) -> serde_json::Value {
    let mut json = crate::ipc::host::surface_json(pane, &pane.display_title("shell"), pane.minimized, win_idx, tab_idx, focused);
    // Kova.app's own: a library host has no subjects, and its panes carry no
    // such field.
    json["subject"] = serde_json::json!(pane.subject);
    json
}

/// `list-subjects` for one window's tabs: one entry per subject, tab by tab,
/// each tab's in the order Cmd+P draws them (`subject_listing`).
pub fn collect_subjects_in(tabs: &[Tab], win_idx: usize, out: &mut Vec<serde_json::Value>) {
    for (tab_idx, tab) in tabs.iter().enumerate() {
        for (name, pane_ids) in super::switcher::subject_listing(&tab.subjects, &tab.pane_subjects()) {
            out.push(serde_json::json!({
                "tab_id": tab.id,
                "window": win_idx,
                "tab_index": tab_idx,
                "color": tab.subject_color(&name),
                "name": name,
                "pane_ids": pane_ids,
            }));
        }
    }
}

/// Why a subject command against a named tab failed.
pub enum SubjectError {
    /// This window has no tab with that id: keep scanning.
    NoTab,
    /// Final answer: the tab is here, the subject is not.
    NoSubject,
}

/// `list-panes` for one window's tabs: every pane in tab then column then row
/// order. A pane is `focused` only if it is its tab's focused pane, in the active
/// tab, in the key window.
pub fn collect_panes_in(
    tabs: &[Tab],
    active_tab: usize,
    win_idx: usize,
    is_key_window: bool,
    out: &mut Vec<serde_json::Value>,
) {
    for (tab_idx, tab) in tabs.iter().enumerate() {
        let focused_id = tab.focused_pane;
        let is_active_tab = tab_idx == active_tab;
        tab.for_each_pane(&mut |pane| {
            let is_focused = pane.id == focused_id && is_active_tab && is_key_window;
            out.push(pane_json(pane, win_idx, tab_idx, is_focused));
        });
    }
}

/// `send-keys`: write `text` to the pane's PTY as is. Returns false when no tab
/// holds the pane.
pub fn send_keys_in(tabs: &[Tab], pane_id: PaneId, text: &str) -> bool {
    for tab in tabs.iter() {
        if let Some(pane) = tab.pane(pane_id) {
            crate::ipc::host::send_keys_to(pane, text);
            return true;
        }
    }
    false
}

/// `list-tabs` for one window's tabs, in tab order. A tab is `active` only if it
/// is the active tab of the key window.
pub fn collect_tabs_in(
    tabs: &[Tab],
    active_tab: usize,
    win_idx: usize,
    is_key_window: bool,
    out: &mut Vec<TabInfo>,
) {
    for (tab_idx, tab) in tabs.iter().enumerate() {
        let mut pane_count = 0;
        tab.for_each_pane(&mut |_| { pane_count += 1; });
        let is_active = tab_idx == active_tab && is_key_window;
        out.push(TabInfo {
            id: tab.id,
            window: win_idx,
            tab_index: tab_idx,
            title: tab.title(),
            pane_count,
            focused_pane_id: tab.focused_pane,
            active: is_active,
            has_bell: tab.has_bell,
            has_completion: tab.has_completion,
            has_running: tab.has_running,
        });
    }
}

/// Every pane id of these tabs, in `list-panes` order (expands `panes: "all"`).
pub fn collect_pane_ids_in(tabs: &[Tab], out: &mut Vec<PaneId>) {
    for tab in tabs.iter() {
        tab.for_each_pane(&mut |p| out.push(p.id));
    }
}

/// One `get-pane-content` entry, or None when no tab holds the pane.
pub fn dump_pane_text_in(
    tabs: &[Tab],
    pane_id: PaneId,
    mode: crate::terminal::DumpMode,
    trim: bool,
) -> Option<serde_json::Value> {
    for tab in tabs.iter() {
        if let Some(pane) = tab.pane(pane_id) {
            return Some(crate::ipc::host::pane_text_entry(pane_id, pane, mode, trim));
        }
    }
    None
}

/// The OSC 133;D flag of a pane, or None when no tab holds it.
pub fn check_completion_in(tabs: &[Tab], pane_id: PaneId) -> Option<bool> {
    for tab in tabs.iter() {
        if let Some(pane) = tab.pane(pane_id) {
            return Some(crate::ipc::host::completion_flag(pane));
        }
    }
    None
}

/// `(chars, bytes)` of what `get-pane-content` would return for a pane, or None
/// when no tab holds it.
pub fn measure_pane_text_in(
    tabs: &[Tab],
    pane_id: PaneId,
    mode: crate::terminal::DumpMode,
    trim: bool,
) -> Option<(usize, usize)> {
    for tab in tabs.iter() {
        if let Some(pane) = tab.pane(pane_id) {
            return Some(crate::ipc::host::measure_pane_text(pane, mode, trim));
        }
    }
    None
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

        // Recorded while the pane is still alive: the undo needs its
        // conversation id, which an agent forgets on exit.
        if let Some(entry) = tabs[tab_idx].closed_pane_record(pane_id) {
            crate::closed_pane::push(entry);
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
        collect_panes_in(&tabs, self.ivars().active_tab.get(), win_idx, is_key_window, out);
    }

    /// The pane this window would hand the keyboard to: the focused pane of the
    /// active tab, its tab index, and the agent conversation running in it.
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
        let session = pane.and_then(|p| p.agent_session_id());
        // The name too, not just the id: `/rename` is how the user says the conversation is about
        // something else now, and a client that never hears about it keeps filing the work under
        // the old subject.
        let name = pane.and_then(|p| p.agent_session_name());
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

    /// IPC: `list-subjects` for this window.
    pub fn ipc_collect_subjects(&self, win_idx: usize, out: &mut Vec<serde_json::Value>) {
        collect_subjects_in(&self.ivars().tabs.borrow(), win_idx, out);
    }

    /// IPC: file `pane_id` under `subject`, or take it out of its subject with
    /// `None` — what Cmd+Right and Cmd+Left do. `None` back when this window
    /// does not hold the pane; otherwise the reply's data: the tab, and
    /// whether the subject was created. Taking out a pane in no subject does
    /// nothing and is not an error: the pane ends where it was asked to be.
    pub fn ipc_set_pane_subject(&self, pane_id: PaneId, subject: Option<String>) -> Option<serde_json::Value> {
        let (tab_id, created, moved) = {
            let mut tabs = self.ivars().tabs.borrow_mut();
            let tab = tabs.iter_mut().find(|t| t.pane(pane_id).is_some())?;
            let (created, moved) = match &subject {
                Some(name) => (super::switcher::file_under_subject(tab, pane_id, name)?, false),
                None => (false, super::switcher::take_out_of_subject(tab, pane_id).unwrap_or(false)),
            };
            (tab.id, created, moved)
        };
        log::info!("IPC: pane {} filed under {:?}", pane_id, subject);
        self.after_subject_change(moved);
        Some(serde_json::json!({ "tab_id": tab_id, "subject": subject, "created": created }))
    }

    /// IPC: rename a subject of tab `tab_id`; a name the tab already has merges
    /// the two (`Tab::rename_subject`). Ok(true) when it merged.
    pub fn ipc_rename_subject(&self, tab_id: TabId, name: &str, new_name: &str) -> Result<bool, SubjectError> {
        let merged = {
            let mut tabs = self.ivars().tabs.borrow_mut();
            let tab = tabs.iter_mut().find(|t| t.id == tab_id).ok_or(SubjectError::NoTab)?;
            if !tab.subjects.iter().any(|s| s == name) {
                return Err(SubjectError::NoSubject);
            }
            let merged = name != new_name && tab.subjects.iter().any(|s| s == new_name);
            tab.rename_subject(name, new_name);
            merged
        };
        log::info!("IPC: renamed subject {} to {} in tab {}", name, new_name, tab_id);
        self.after_subject_change(false);
        Ok(merged)
    }

    /// IPC: delete a subject of tab `tab_id`. Its panes stay open, out of any
    /// box. Ok(n): how many panes it held.
    pub fn ipc_delete_subject(&self, tab_id: TabId, name: &str) -> Result<usize, SubjectError> {
        let released = {
            let mut tabs = self.ivars().tabs.borrow_mut();
            let tab = tabs.iter_mut().find(|t| t.id == tab_id).ok_or(SubjectError::NoTab)?;
            if !tab.subjects.iter().any(|s| s == name) {
                return Err(SubjectError::NoSubject);
            }
            tab.delete_subject(name)
        };
        log::info!("IPC: deleted subject {} in tab {} ({} panes released)", name, tab_id, released);
        self.after_subject_change(false);
        Ok(released)
    }

    /// IPC: give a subject of tab `tab_id` a palette color.
    pub fn ipc_set_subject_color(&self, tab_id: TabId, name: &str, color: usize) -> Result<(), SubjectError> {
        {
            let mut tabs = self.ivars().tabs.borrow_mut();
            let tab = tabs.iter_mut().find(|t| t.id == tab_id).ok_or(SubjectError::NoTab)?;
            if !tab.set_subject_color(name, color) {
                return Err(SubjectError::NoSubject);
            }
        }
        log::info!("IPC: subject {} in tab {} colored {}", name, tab_id, color);
        self.after_subject_change(false);
        Ok(())
    }

    /// What follows a subject change made from outside: the layout catches up
    /// when a pane moved, and an open Cmd+P is rebuilt, since its rows are a
    /// snapshot that would keep showing the old boxes.
    fn after_subject_change(&self, moved: bool) {
        if moved {
            self.resize_all_panes();
        }
        self.refresh_open_pane_switcher();
        self.mark_dirty();
    }

    /// IPC: write text to a pane's PTY. Returns true if the pane was found.
    pub fn ipc_send_keys(&self, pane_id: PaneId, text: &str) -> bool {
        send_keys_in(&self.ivars().tabs.borrow(), pane_id, text)
    }

    /// IPC: take a new anchor list into this window. The status bars repaint,
    /// and an open Cmd+P is rebuilt around it: the overlay is a snapshot, so a
    /// list pushed while it is up would otherwise stay on screen out of date.
    pub fn ipc_refresh_anchors(&self, anchors: &crate::anchors::Anchors) {
        self.refresh_anchor_keys(anchors);
        self.refresh_open_pane_switcher();
    }


    /// IPC: what `pane_id` would be saved as, if this window holds it. Lets
    /// `set-anchor` name a live pane instead of repeating what Kova knows.
    pub fn ipc_pane_as_saved(&self, pane_id: PaneId) -> Option<crate::bookmarks::Bookmark> {
        self.pane_as_saved(pane_id)
    }

    /// IPC: apply a freshly read config to this window's renderer. Only the
    /// appearance half travels (see `Renderer::apply_config`); the config this
    /// window was built with keeps driving font, keys and geometry.
    pub fn ipc_apply_config(&self, config: &Config) {
        if let Some(renderer) = self.ivars().renderer.get() {
            renderer.write().apply_config(config);
        }
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

    /// IPC: wake the agent asleep in `pane_id` (`Surface::wake`). `None` when
    /// the pane is not in this window.
    pub fn ipc_wake_pane(&self, pane_id: PaneId) -> Option<bool> {
        let tabs = self.ivars().tabs.borrow();
        tabs.iter().find_map(|t| t.pane(pane_id)).map(|pane| pane.wake())
    }

    /// IPC: collect all pane IDs in this window (used to expand `panes: "all"`).
    pub fn ipc_collect_pane_ids(&self, out: &mut Vec<PaneId>) {
        collect_pane_ids_in(&self.ivars().tabs.borrow(), out);
    }

    /// IPC: build a JSON entry with the rendered text of a pane.
    /// Returns `None` if the pane is not in this window.
    pub fn ipc_dump_pane_text(
        &self,
        pane_id: PaneId,
        mode: crate::terminal::DumpMode,
        trim: bool,
    ) -> Option<serde_json::Value> {
        dump_pane_text_in(&self.ivars().tabs.borrow(), pane_id, mode, trim)
    }

    /// IPC: report whether the OSC 133;D flag is set for a pane.
    /// `None` means the pane is not in this window; `Some(b)` is the flag's value.
    pub fn ipc_check_completion(&self, pane_id: PaneId) -> Option<bool> {
        check_completion_in(&self.ivars().tabs.borrow(), pane_id)
    }

    /// IPC: measure how big the rendered text of a pane would be.
    /// Returns `(chars, bytes)`, or `None` if the pane is not in this window.
    pub fn ipc_measure_pane_text(
        &self,
        pane_id: PaneId,
        mode: crate::terminal::DumpMode,
        trim: bool,
    ) -> Option<(usize, usize)> {
        measure_pane_text_in(&self.ivars().tabs.borrow(), pane_id, mode, trim)
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

    /// IPC: collect this window's tabs as `list-tabs` entries.
    pub fn ipc_collect_tabs(&self, win_idx: usize, is_key_window: bool, out: &mut Vec<TabInfo>) {
        let tabs = self.ivars().tabs.borrow();
        collect_tabs_in(&tabs, self.ivars().active_tab.get(), win_idx, is_key_window, out);
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

    pub fn ipc_pane_signal(&self, pane_id: PaneId, signal: PaneSignal) -> bool {
        let found = {
            let tabs = self.ivars().tabs.borrow();
            let pane = tabs.iter().find_map(|tab| tab.pane(pane_id));
            if let Some(pane) = pane {
                signal.apply(pane);
                log::info!("IPC: pane {} {:?}", pane_id, signal);
            }
            pane.is_some()
        };
        if found {
            self.mark_dirty();
            // An open switcher would otherwise keep this pane's row as it was.
            self.refresh_open_pane_switcher();
        }
        found
    }
}

#[cfg(test)]
mod tests {
    //! Characterization of the pane-level IPC commands as of 13/10/2026, before the
    //! embedding refactor (docs/embedding.md), on panes whose PTY input is captured.
    use super::*;
    use crate::input::tests::drain;
    use crate::terminal::DumpMode;
    use std::os::fd::OwnedFd;

    fn pane(text: &str) -> (Pane, OwnedFd) {
        let (p, fd) = Pane::capture(20, 4);
        {
            let mut t = p.terminal.write();
            for c in text.chars() {
                t.put_char(c);
            }
        }
        (p, fd)
    }

    #[test]
    fn send_keys_writes_the_text_verbatim_to_the_pane() {
        let (a, _fa) = pane("");
        let (b, fb) = pane("");
        let b_id = b.id;
        let tabs = vec![Tab::with_columns(vec![vec![a]]), Tab::with_columns(vec![vec![b]])];
        assert!(send_keys_in(&tabs, b_id, "ls -la\r"));
        assert_eq!(drain(&fb), b"ls -la\r");
        assert!(send_keys_in(&tabs, b_id, "\x1b[A\u{e9}"));
        assert_eq!(drain(&fb), "\x1b[A\u{e9}".as_bytes(), "escapes and UTF-8 untouched");
    }

    #[test]
    fn send_keys_to_an_unknown_pane_is_false_and_writes_nothing() {
        let (a, fa) = pane("");
        let tabs = vec![Tab::with_columns(vec![vec![a]])];
        assert!(!send_keys_in(&tabs, 999_999, "x"));
        assert_eq!(drain(&fa), b"");
    }

    #[test]
    fn send_keys_answers_a_waiting_pane() {
        let (a, _fa) = pane("");
        let id = a.id;
        a.set_awaiting();
        assert!(a.is_awaiting());
        let tabs = vec![Tab::with_columns(vec![vec![a]])];
        send_keys_in(&tabs, id, "y");
        assert!(!tabs[0].pane(id).unwrap().is_awaiting());
    }

    #[test]
    fn only_a_submitted_line_resets_the_completion_flag() {
        let (a, _fa) = pane("");
        let id = a.id;
        let tabs = vec![Tab::with_columns(vec![vec![a]])];
        let term = || tabs[0].pane(id).unwrap().terminal.clone();
        term().read().mark_command_completed();
        send_keys_in(&tabs, id, "partial");
        assert_eq!(check_completion_in(&tabs, id), Some(true), "no newline: still finished");
        send_keys_in(&tabs, id, "go\n");
        assert_eq!(check_completion_in(&tabs, id), Some(false));
        term().read().mark_command_completed();
        send_keys_in(&tabs, id, "\r");
        assert_eq!(check_completion_in(&tabs, id), Some(false));
    }

    #[test]
    fn check_completion_of_an_unknown_pane_is_none() {
        let (a, _fa) = pane("");
        let tabs = vec![Tab::with_columns(vec![vec![a]])];
        assert_eq!(check_completion_in(&tabs, 999_999), None);
    }

    #[test]
    fn pane_ids_come_in_tab_column_row_order() {
        let (a, _1) = pane("");
        let (b, _2) = pane("");
        let (c, _3) = pane("");
        let (d, _4) = pane("");
        let ids = [a.id, b.id, c.id, d.id];
        let tabs = vec![
            Tab::with_columns(vec![vec![a, b], vec![c]]),
            Tab::with_columns(vec![vec![d]]),
        ];
        let mut out = Vec::new();
        collect_pane_ids_in(&tabs, &mut out);
        assert_eq!(out, ids);
    }

    #[test]
    fn dump_pane_text_reports_text_size_and_cursor() {
        let (a, _fa) = pane("hello");
        let id = a.id;
        let tabs = vec![Tab::with_columns(vec![vec![a]])];
        let v = dump_pane_text_in(&tabs, id, DumpMode::Visible, true).unwrap();
        assert_eq!(v["id"], id);
        assert_eq!(v["text"], "hello\n", "each line ends with a newline");
        assert_eq!(v["cols"], 20);
        assert_eq!(v["rows"], 4);
        assert_eq!(v["cursor"]["row"], 0);
        assert_eq!(v["cursor"]["col"], 5);
        assert!(dump_pane_text_in(&tabs, 999_999, DumpMode::Visible, true).is_none());
    }

    #[test]
    fn dump_and_measure_agree_on_every_mode() {
        let (a, _fa) = Pane::capture(10, 2);
        let id = a.id;
        {
            let mut t = a.terminal.write();
            for (i, line) in ["one", "two", "thré", "four"].iter().enumerate() {
                if i > 0 {
                    t.newline();
                    t.carriage_return();
                }
                for c in line.chars() {
                    t.put_char(c);
                }
            }
        }
        let tabs = vec![Tab::with_columns(vec![vec![a]])];
        for mode in [DumpMode::Visible, DumpMode::Scrollback, DumpMode::All] {
            for trim in [true, false] {
                let text = dump_pane_text_in(&tabs, id, mode, trim).unwrap()["text"]
                    .as_str()
                    .unwrap()
                    .to_string();
                let (chars, bytes) = measure_pane_text_in(&tabs, id, mode, trim).unwrap();
                assert_eq!((chars, bytes), (text.chars().count(), text.len()), "{mode:?} trim={trim}");
            }
        }
        let all = dump_pane_text_in(&tabs, id, DumpMode::All, true).unwrap();
        assert_eq!(all["text"], "one\ntwo\nthré\nfour\n");
        let sb = dump_pane_text_in(&tabs, id, DumpMode::Scrollback, true).unwrap();
        assert_eq!(sb["text"], "one\ntwo\n");
        assert!(measure_pane_text_in(&tabs, 999_999, DumpMode::All, true).is_none());
    }

    #[test]
    fn list_panes_marks_only_the_focused_pane_of_the_active_tab_of_the_key_window() {
        let (a, _1) = pane("");
        let (b, _2) = pane("");
        let (c, _3) = pane("");
        let (a_id, b_id, c_id) = (a.id, b.id, c.id);
        let mut t0 = Tab::with_columns(vec![vec![a, b]]);
        t0.focused_pane = b_id;
        let tabs = vec![t0, Tab::with_columns(vec![vec![c]])];

        let mut out = Vec::new();
        collect_panes_in(&tabs, 0, 3, true, &mut out);
        let row = |id: u32| out.iter().find(|v| v["id"] == id).unwrap().clone();
        assert_eq!(out.len(), 3);
        assert_eq!(row(a_id)["focused"], false);
        assert_eq!(row(b_id)["focused"], true);
        assert_eq!(row(c_id)["focused"], false, "focused in its tab, but the tab is not active");
        assert_eq!(row(c_id)["tab"], 1);
        assert_eq!(row(a_id)["window"], 3);

        let mut out = Vec::new();
        collect_panes_in(&tabs, 0, 0, false, &mut out);
        assert!(out.iter().all(|v| v["focused"] == false), "not the key window");
    }

    #[test]
    fn pane_json_shape() {
        let (a, _fa) = pane("");
        a.set_awaiting();
        let v = pane_json(&a, 1, 2, true);
        let keys: Vec<&str> = v.as_object().unwrap().keys().map(|k| k.as_str()).collect();
        let mut keys = keys;
        keys.sort();
        assert_eq!(keys, [
            "agent", "agent_session_id", "agent_session_name", "awaiting", "awaiting_seen",
            "awaiting_since", "child_processes", "claude_config_dir", "claude_session_id", "claude_session_name",
            "cwd", "dormant_session_id", "focused", "id", "is_idle", "minimized", "peer_name", "pid", "subject", "tab",
            "title", "window", "working",
        ]);
        assert_eq!(v["subject"], serde_json::Value::Null, "a pane in no subject");
        let (mut b, _fb) = pane("");
        b.subject = Some("auth".into());
        assert_eq!(pane_json(&b, 0, 0, false)["subject"], "auth");
        assert_eq!(v["id"], a.id);
        assert_eq!(v["window"], 1);
        assert_eq!(v["tab"], 2);
        assert_eq!(v["focused"], true);
        assert_eq!(v["awaiting"], true);
        assert_eq!(v["awaiting_seen"], false);
        assert!(v["awaiting_since"].is_u64());
        assert_eq!(v["working"], false);
        assert_eq!(v["minimized"], false);
        assert!(v["agent"].is_null(), "a bare shell has no agent");
        assert!(v["claude_session_id"].is_null());
        a.mark_awaiting_seen();
        assert_eq!(pane_json(&a, 1, 2, true)["awaiting_seen"], true);
    }

    // --- The same per-window functions behind `crate::ipc::host::dispatch` ---

    /// One key window of real panes, answering the host interface with the very
    /// functions `KovaView`'s `ipc_*` methods call, as `KovaAppHost` does per window.
    struct TabsHost {
        tabs: Vec<Tab>,
    }

    impl crate::ipc::host::IpcHost for TabsHost {
        fn collect_panes(&self, out: &mut Vec<serde_json::Value>) {
            collect_panes_in(&self.tabs, 0, 0, true, out);
        }
        fn collect_pane_ids(&self, out: &mut Vec<PaneId>) {
            collect_pane_ids_in(&self.tabs, out);
        }
        fn send_keys(&self, pane_id: PaneId, text: &str) -> bool {
            send_keys_in(&self.tabs, pane_id, text)
        }
        fn dump_pane_text(&self, pane_id: PaneId, mode: DumpMode, trim: bool) -> Option<serde_json::Value> {
            dump_pane_text_in(&self.tabs, pane_id, mode, trim)
        }
        fn measure_pane_text(&self, pane_id: PaneId, mode: DumpMode, trim: bool) -> Option<(usize, usize)> {
            measure_pane_text_in(&self.tabs, pane_id, mode, trim)
        }
        fn check_completion(&self, pane_id: PaneId) -> Option<bool> {
            check_completion_in(&self.tabs, pane_id)
        }
        fn focus_pane(&self, _: PaneId) -> bool {
            unimplemented!("needs a KovaView")
        }
        fn close_pane(&self, _: PaneId) -> Option<bool> {
            unimplemented!("needs a KovaView")
        }
        fn set_pane_status(&self, _: PaneId, _: bool) -> bool {
            unimplemented!("needs a KovaView")
        }
        fn pane_signal(&self, _: PaneId, _: PaneSignal) -> bool {
            unimplemented!("needs a KovaView")
        }
        fn notify(&self, _: Option<PaneId>, _: &str, _: &str, _: bool) -> Result<(), String> {
            unimplemented!("posts a real notification")
        }
        fn collect_tabs(&self, out: &mut Vec<TabInfo>) {
            collect_tabs_in(&self.tabs, 0, 0, true, out);
        }
        fn close_tab(&self, _: u32) -> IpcCloseTabResult {
            unimplemented!("needs a KovaView")
        }
    }

    fn reply(host: &TabsHost, request: &str) -> serde_json::Value {
        use crate::ipc::host::{dispatch, Disposition};
        let cmd = crate::ipc::parse_command(request).unwrap();
        match dispatch(host, cmd) {
            Ok(Disposition::Reply(r)) => r.to_json(),
            _ => panic!("{request}: expected an immediate reply"),
        }
    }

    #[test]
    fn list_panes_through_the_host_is_the_window_listing() {
        let (a, _1) = pane("");
        let (b, _2) = pane("");
        let (c, _3) = pane("");
        let mut t0 = Tab::with_columns(vec![vec![a, b]]);
        t0.focused_pane = t0.first_pane().id;
        let host = TabsHost { tabs: vec![t0, Tab::with_columns(vec![vec![c]])] };
        let mut direct = Vec::new();
        collect_panes_in(&host.tabs, 0, 0, true, &mut direct);
        let r = reply(&host, r#"{"cmd":"list-panes"}"#);
        assert_eq!(r["ok"], true);
        assert_eq!(r["data"], serde_json::Value::Array(direct));
        assert_eq!(r["data"].as_array().unwrap().len(), 3);
    }

    #[test]
    fn send_keys_and_content_through_the_host() {
        let (a, fa) = pane("hello");
        let (b, _fb) = pane("bye");
        let (a_id, b_id) = (a.id, b.id);
        let host = TabsHost { tabs: vec![Tab::with_columns(vec![vec![a], vec![b]])] };

        let r = reply(&host, &format!(r#"{{"cmd":"send-keys","pane_id":{a_id},"text":"ls\r"}}"#));
        assert_eq!(r.to_string(), r#"{"ok":true}"#);
        assert_eq!(drain(&fa), b"ls\r");

        let r = reply(&host, r#"{"cmd":"get-pane-content"}"#);
        let entry = |id: PaneId| dump_pane_text_in(&host.tabs, id, DumpMode::Visible, true).unwrap();
        assert_eq!(r["data"], serde_json::json!({"panes": [entry(a_id), entry(b_id)]}));
        assert_eq!(r["data"]["panes"][0]["text"], "hello\n");

        let r = reply(&host, &format!(r#"{{"cmd":"count-pane-content","panes":[{b_id},999999]}}"#));
        assert_eq!(
            r.to_string(),
            format!(r#"{{"data":{{"panes":[{{"bytes":4,"chars":4,"id":{b_id}}},{{"error":"not found","id":999999}}],"total_bytes":4,"total_chars":4}},"ok":true}}"#)
        );
    }

    #[test]
    fn wait_for_completion_through_the_host_reads_the_pane_flag() {
        use crate::ipc::host::{dispatch, Disposition};
        let (a, _fa) = pane("");
        let id = a.id;
        let host = TabsHost { tabs: vec![Tab::with_columns(vec![vec![a]])] };
        let wait = |host: &TabsHost| {
            dispatch(host, crate::ipc::parse_command(&format!(r#"{{"cmd":"wait-for-completion","pane_id":{id}}}"#)).unwrap())
        };
        assert!(matches!(wait(&host), Ok(Disposition::Pending(_))));
        host.tabs[0].pane(id).unwrap().terminal.read().mark_command_completed();
        match wait(&host) {
            Ok(Disposition::Reply(r)) => assert_eq!(
                r.to_json().to_string(),
                format!(r#"{{"data":{{"completed":true,"pane_id":{id},"timed_out":false}},"ok":true}}"#)
            ),
            _ => panic!("a finished pane answers at once"),
        }
    }

    #[test]
    fn list_tabs_through_the_host() {
        let (a, _1) = pane("");
        let (b, _2) = pane("");
        let (c, _3) = pane("");
        let c_id = c.id;
        let mut t1 = Tab::with_columns(vec![vec![c]]);
        t1.has_bell = true;
        let t0 = Tab::with_columns(vec![vec![a], vec![b]]);
        let (t0_id, t1_id, t0_focus) = (t0.id, t1.id, t0.focused_pane);
        let (t0_title, t1_title) = (t0.title(), t1.title());
        let host = TabsHost { tabs: vec![t0, t1] };
        let r = reply(&host, r#"{"cmd":"list-tabs"}"#);
        assert_eq!(
            r,
            serde_json::json!({"ok": true, "data": [
                {"id": t0_id, "window": 0, "tab_index": 0, "title": t0_title, "pane_count": 2,
                 "focused_pane_id": t0_focus, "active": true, "has_bell": false,
                 "has_completion": false, "has_running": false},
                {"id": t1_id, "window": 0, "tab_index": 1, "title": t1_title, "pane_count": 1,
                 "focused_pane_id": c_id, "active": false, "has_bell": true,
                 "has_completion": false, "has_running": false},
            ]})
        );
    }

}
