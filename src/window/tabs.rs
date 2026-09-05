//! Tabs and splits over their whole life: opening one, closing it, and moving
//! it around — split off a pane, break it out to its own tab, detach a tab to
//! its own window, or merge two of them back together.

use super::*;

pub(super) struct MergeTabState {
    pub(super) entries: Vec<MergeTabEntry>,
    pub(super) selected: usize,
}

pub(super) struct SendToWindowState {
    pub(super) entries: Vec<SendToWindowEntry>,
    pub(super) selected: usize,
    /// When true, confirming the overlay merges *all* of this window's tabs into
    /// the chosen window and closes this one (whole-window merge). When false,
    /// only the active tab is sent (detach-tab flow).
    pub(super) merge_all: bool,
}

impl KovaView {
    /// Create a new tab (Cmd+T).
    pub(super) fn do_new_tab(&self) {
        let config = match self.ivars().config.get() {
            Some(c) => c,
            None => return,
        };

        // Get CWD from currently focused pane
        let cwd = self.focused_pane().and_then(|p| p.cwd());

        let tab = match Tab::new_with_cwd(config, cwd.as_deref()) {
            Ok(t) => t,
            Err(e) => {
                log::error!("failed to create tab: {}", e);
                return;
            }
        };

        let mut tabs = self.ivars().tabs.borrow_mut();
        let new_idx = self.ivars().active_tab.get() + 1;
        tabs.insert(new_idx, tab);
        log::debug!("New tab created: index={}, total={}", new_idx, tabs.len());
        drop(tabs);
        self.ivars().active_tab.set(new_idx);
        self.resize_all_panes();
    }

    /// Switch to tab at index.
    pub(super) fn do_switch_tab(&self, idx: usize) {
        let tabs = self.ivars().tabs.borrow();
        if idx >= tabs.len() || idx == self.ivars().active_tab.get() {
            return;
        }
        log::debug!("Switch to tab {}", idx);
        // Mark all panes of new tab dirty so the next render tick draws them
        tabs[idx].for_each_pane(&mut |pane| {
            pane.terminal.read().dirty.store(true, std::sync::atomic::Ordering::Relaxed);
        });
        drop(tabs);
        self.ivars().active_tab.set(idx);
        // Clear bell/attention indicator on the newly focused tab
        {
            let mut tabs = self.ivars().tabs.borrow_mut();
            tabs[idx].clear_bell();
            tabs[idx].clear_completion();
        }
        // Lazy resize: resize panes when switching to them
        self.resize_all_panes();
    }

    /// Show a context menu to pick a color for a tab.
    pub(super) fn show_tab_color_menu(&self, event: &NSEvent, tab_idx: usize) {
        use objc2_app_kit::{NSMenu, NSMenuItem};

        self.ivars().color_menu_tab.set(tab_idx);
        let mtm = unsafe { MainThreadMarker::new_unchecked() };
        let pastilles = ["🔴", "🟠", "🟡", "🟢", "🔵", "🟣"];
        let menu = NSMenu::new(mtm);
        let action = objc2::sel!(tabColorSelected:);
        let empty_ke = NSString::from_str("");

        for (i, emoji) in pastilles.iter().enumerate() {
            let title = NSString::from_str(emoji);
            let item = unsafe {
                NSMenuItem::initWithTitle_action_keyEquivalent(
                    NSMenuItem::alloc(mtm),
                    &title,
                    Some(action),
                    &empty_ke,
                )
            };
            item.setTag(i as isize);
            unsafe { item.setTarget(Some(&*self)) };
            menu.addItem(&item);
        }

        // Separator + "Aucune" item
        menu.addItem(&NSMenuItem::separatorItem(mtm));
        let none_title = NSString::from_str("Aucune");
        let none_item = unsafe {
            NSMenuItem::initWithTitle_action_keyEquivalent(
                NSMenuItem::alloc(mtm),
                &none_title,
                Some(action),
                &empty_ke,
            )
        };
        none_item.setTag(-1);
        unsafe { none_item.setTarget(Some(&*self)) };
        menu.addItem(&none_item);

        // Show menu at click location (synchronous, blocks until user picks or dismisses)
        let location = event.locationInWindow();
        let _ok: bool = unsafe {
            objc2::msg_send![&menu, popUpMenuPositioningItem: std::ptr::null::<NSMenuItem>(), atLocation: location, inView: self]
        };
    }

    /// Switch to relative tab (delta = -1 for prev, +1 for next).
    pub(super) fn do_switch_tab_relative(&self, delta: i32) {
        let tabs = self.ivars().tabs.borrow();
        let count = tabs.len();
        if count <= 1 {
            return;
        }
        drop(tabs);
        let current = self.ivars().active_tab.get() as i32;
        let new_idx = ((current + delta) % count as i32 + count as i32) as usize % count;
        self.do_switch_tab(new_idx);
    }

    /// Split the focused pane in the given direction.
    pub(super) fn do_split(&self, direction: SplitDirection) {
        let config = match self.ivars().config.get() {
            Some(c) => c,
            None => return,
        };

        let (focused_id, current_vp, focused_cwd) = {
            let tabs = self.ivars().tabs.borrow();
            let idx = self.ivars().active_tab.get();
            let tab = match tabs.get(idx) {
                Some(t) => t,
                None => return,
            };
            let fid = tab.focused_pane;
            let vp = match tab.viewport_for_pane(fid, self.panes_viewport_for_tab(tab)) {
                Some(vp) => vp,
                None => return,
            };
            let cwd = tab.pane(fid).and_then(|p| p.cwd());
            (fid, vp, cwd)
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

        let dir_name = match direction {
            SplitDirection::Horizontal => "horizontal",
            SplitDirection::Vertical => "vertical",
        };
        log::debug!("Split pane {}: direction={}, new size={}x{}", focused_id, dir_name, cols, rows);

        let new_pane = match Pane::spawn(cols, rows, config, focused_cwd.as_deref()) {
            Ok(p) => p,
            Err(e) => {
                log::error!("failed to spawn pane for split: {}", e);
                return;
            }
        };
        let new_id = new_pane.id;
        let open_timer = new_pane.open_timer.clone();

        let mut tabs = self.ivars().tabs.borrow_mut();
        let idx = self.ivars().active_tab.get();
        if let Some(tab) = tabs.get_mut(idx) {
            match direction {
                SplitDirection::Horizontal => {
                    // Insert new column after the focused pane's column. If we
                    // are already scrolling (virtual width > screen), grow the
                    // virtual space by the new column's width instead of
                    // shrinking the existing panes.
                    let screen = self.drawable_viewport().width;
                    let min_w = self.min_split_width_px();
                    let old_virtual = tab.virtual_width(screen, min_w);
                    tab.insert_column_after_focused(new_pane);
                    if old_virtual > screen {
                        if let Some(new_col_idx) = tab.column_index_of(new_id) {
                            // New pane is born at the same width as the pane it
                            // was split from, so it doesn't look shrunk.
                            let new_col_px = current_vp.width.max(min_w);
                            tab.grow_virtual_for_scrolled_split(new_col_idx, old_virtual, new_col_px, screen);
                        }
                    }
                }
                SplitDirection::Vertical => {
                    // Split focused pane vertically within its column
                    tab.vsplit_at_pane(focused_id, new_pane);
                }
            }
            tab.focused_pane = new_id;
            // Auto-scroll to reveal the new pane
            self.scroll_to_reveal_pane(tab, new_id, self.drawable_viewport().width);
        }
        drop(tabs);

        open_timer.mark_inserted(new_id);
        self.resize_all_panes();
    }

    /// Split at the root level: the new pane spans the full width/height.
    pub(super) fn do_split_root(&self, direction: SplitDirection) {
        let config = match self.ivars().config.get() {
            Some(c) => c,
            None => return,
        };

        let focused_cwd = {
            let tabs = self.ivars().tabs.borrow();
            let idx = self.ivars().active_tab.get();
            tabs.get(idx).and_then(|tab| {
                tab.pane(tab.focused_pane).and_then(|p| p.cwd())
            })
        };

        let panes_vp = self.panes_viewport();
        let half_vp = match direction {
            SplitDirection::Horizontal => PaneViewport {
                x: panes_vp.x,
                y: panes_vp.y,
                width: panes_vp.width / 2.0,
                height: panes_vp.height,
            },
            SplitDirection::Vertical => PaneViewport {
                x: panes_vp.x,
                y: panes_vp.y,
                width: panes_vp.width,
                height: panes_vp.height / 2.0,
            },
        };
        let (cols, rows) = self.viewport_to_grid(&half_vp);

        let dir_name = match direction {
            SplitDirection::Horizontal => "horizontal",
            SplitDirection::Vertical => "vertical",
        };
        log::debug!("Split root: direction={}, new size={}x{}", dir_name, cols, rows);

        let new_pane = match Pane::spawn(cols, rows, config, focused_cwd.as_deref()) {
            Ok(p) => p,
            Err(e) => {
                log::error!("failed to spawn pane for root split: {}", e);
                return;
            }
        };
        let new_id = new_pane.id;
        let open_timer = new_pane.open_timer.clone();

        let mut tabs = self.ivars().tabs.borrow_mut();
        let idx = self.ivars().active_tab.get();
        if let Some(tab) = tabs.get_mut(idx) {
            let screen = self.drawable_viewport().width;
            let min_w = self.min_split_width_px();
            let old_virtual = tab.virtual_width(screen, min_w);
            // Width of the pane we split from, to size the new column like a sibling.
            let focused_w = tab.viewport_for_pane(tab.focused_pane, self.panes_viewport_for_tab(tab))
                .map(|vp| vp.width)
                .unwrap_or(min_w);
            match direction {
                SplitDirection::Horizontal => {
                    // Append new column at the end
                    tab.append_column(new_pane);
                    // Already scrolling → grow the virtual space by the new
                    // column's width instead of shrinking the existing panes.
                    if old_virtual > screen {
                        if let Some(new_col_idx) = tab.column_index_of(new_id) {
                            let new_col_px = focused_w.max(min_w);
                            tab.grow_virtual_for_scrolled_split(new_col_idx, old_virtual, new_col_px, screen);
                        }
                    }
                }
                SplitDirection::Vertical => {
                    // Wrap column at bottom
                    tab.vsplit_root_at_column(new_pane);
                }
            }
            tab.focused_pane = new_id;
            if direction == SplitDirection::Horizontal {
                // Auto-scroll to reveal the new pane (rightmost)
                let vw = tab.virtual_width(screen, min_w);
                if vw > screen {
                    tab.scroll_offset_x = (vw - screen).max(0.0);
                }
            }
        }
        drop(tabs);

        open_timer.mark_inserted(new_id);
        self.resize_all_panes();
    }

    /// Close focused pane. If it's the last pane in the tab, close the tab.
    pub(super) fn do_close_pane_or_tab(&self) {
        // Collect info for confirmation dialog BEFORE holding the borrow,
        // because NSAlert runs a modal run loop that can dispatch events
        // which access tabs → would panic on double borrow.
        let proc = {
            let tabs = self.ivars().tabs.borrow();
            let idx = self.ivars().active_tab.get();
            if idx >= tabs.len() {
                return;
            }
            tabs[idx].pane(tabs[idx].focused_pane)
                .and_then(|p| p.foreground_process_name().map(|name| (tabs[idx].title(), name)))
        };
        if let Some(proc) = proc {
            let mtm = unsafe { MainThreadMarker::new_unchecked() };
            if !confirm_running_processes(mtm, &[proc], "Close this pane?", "Close") {
                return;
            }
        }

        let mut tabs = self.ivars().tabs.borrow_mut();
        let idx = self.ivars().active_tab.get();
        if idx >= tabs.len() {
            return;
        }

        if tabs[idx].is_single_pane() {
            log::debug!("Closing tab {}", idx);
            drop(tabs);
            self.remove_tab(idx);
            return;
        }

        // Multiple panes → close focused pane
        let focused_id = tabs[idx].focused_pane;
        log::debug!("Closing pane {} in tab {}", focused_id, idx);

        // Find a neighbor to focus before removing (prefer right, then left, then any)
        let panes_vp = self.panes_viewport_for_tab(&tabs[idx]);
        let next_focus = tabs[idx].neighbor(focused_id, NavDirection::Right, panes_vp)
            .or_else(|| tabs[idx].neighbor(focused_id, NavDirection::Left, panes_vp))
            .or_else(|| tabs[idx].neighbor(focused_id, NavDirection::Down, panes_vp))
            .or_else(|| tabs[idx].neighbor(focused_id, NavDirection::Up, panes_vp));

        let old_columns = tabs[idx].num_visible_columns();
        if !tabs[idx].remove_pane(focused_id) {
            // Tab became empty
            drop(tabs);
            self.remove_tab(idx);
            return;
        }
        // Clean up minimized_stack (closed pane may have been minimized)
        tabs[idx].minimized_stack.retain(|&pid| pid != focused_id);
        // If only minimized panes remain, restore the last minimized one
        let restored = tabs[idx].ensure_visible_pane();
        let new_focus = restored
            .or(next_focus.filter(|id| tabs[idx].contains(*id)))
            .or_else(|| tabs[idx].first_visible_pane())
            .unwrap_or_else(|| tabs[idx].first_pane().id);
        tabs[idx].focused_pane = new_focus;
        let new_columns = tabs[idx].num_visible_columns();
        tabs[idx].scale_virtual_width(old_columns, new_columns);
        // Clamp scroll and auto-scroll to reveal focused pane
        let full = self.drawable_viewport();
        let min_w = self.min_split_width_px();
        tabs[idx].clamp_scroll(full.width, min_w);
        let tab = &mut tabs[idx];
        self.scroll_to_reveal_pane(tab, new_focus, full.width);
        drop(tabs);
        self.resize_all_panes();
    }

    /// Remove a tab by index: save to recent projects, remove from list,
    /// update active_tab, terminate if empty, then resize.
    /// Caller must NOT hold `tabs` borrow when calling this.
    pub(super) fn remove_tab(&self, idx: usize) {
        let mut tabs = self.ivars().tabs.borrow_mut();
        if idx >= tabs.len() { return; }
        crate::recent_projects::add(&tabs[idx]);
        tabs.remove(idx);
        if tabs.is_empty() {
            drop(tabs);
            unsafe {
                let mtm = MainThreadMarker::new_unchecked();
                let app = NSApplication::sharedApplication(mtm);
                app.terminate(None);
            }
            return;
        }
        let new_idx = if idx >= tabs.len() { tabs.len() - 1 } else { idx };
        drop(tabs);
        self.ivars().active_tab.set(new_idx);
        self.resize_all_panes();
    }

    /// Close the entire active tab (all its panes), with confirmation.
    /// Saves to recent projects before closing.
    pub(super) fn do_close_tab(&self) {
        // Capture the target tab index once — the confirmation modal pumps events,
        // so `active_tab` could drift before we call remove_tab.
        let target_idx = self.ivars().active_tab.get();
        let procs = {
            let tabs = self.ivars().tabs.borrow();
            if target_idx >= tabs.len() {
                return;
            }
            let title = tabs[target_idx].title();
            let mut result = Vec::new();
            tabs[target_idx].for_each_pane(&mut |pane| {
                if let Some(name) = pane.foreground_process_name() {
                    result.push((title.clone(), name));
                }
            });
            result
        };

        let mtm = unsafe { MainThreadMarker::new_unchecked() };
        if !confirm_running_processes(mtm, &procs, "Close this tab?", "Close") {
            return;
        }

        log::debug!("Closing entire tab {}", target_idx);
        self.remove_tab(target_idx);
    }

    /// Handle key events in the "Send Tab to Window" overlay.
    pub(super) fn handle_send_to_window_key(&self, event: &NSEvent) {
        let keycode = event.keyCode();

        // Escape → close
        if keycode == 0x35 {
            *self.ivars().send_to_window.borrow_mut() = None;
            self.mark_dirty();
            return;
        }

        // Enter → confirm selection
        if keycode == 0x24 {
            let selection = {
                let state = self.ivars().send_to_window.borrow();
                state.as_ref().map(|s| (s.entries[s.selected].window_index, s.merge_all))
            };
            if let Some((window_index, merge_all)) = selection {
                *self.ivars().send_to_window.borrow_mut() = None;
                if merge_all {
                    // Whole-window merge only targets existing windows, so
                    // window_index is always Some here.
                    if let Some(idx) = window_index {
                        self.merge_window_into(idx);
                    }
                } else {
                    self.send_active_tab_to(window_index);
                }
            }
            return;
        }

        // Arrow keys
        {
            let mut guard = self.ivars().send_to_window.borrow_mut();
            let state = match guard.as_mut() {
                Some(s) => s,
                None => return,
            };
            match keycode {
                0x7E => { // Up
                    if state.selected > 0 {
                        state.selected -= 1;
                    }
                }
                0x7D => { // Down
                    if state.selected + 1 < state.entries.len() {
                        state.selected += 1;
                    }
                }
                _ => {}
            }
        }
        self.mark_dirty();
    }

    /// Send the active tab to another window.
    /// - 1 tab + no other window → no-op (would leave nothing)
    /// - 1 tab + other windows → overlay (no "New Window" option)
    /// - 2+ tabs + no other window → detach to new window directly
    /// - 2+ tabs + other windows → overlay with "New Window" option
    pub(super) fn do_detach_tab(&self) {
        let tab_count = self.ivars().tabs.borrow().len();
        let mtm = unsafe { MainThreadMarker::new_unchecked() };
        let source = self.window().unwrap();
        let others = crate::app::list_other_windows(mtm, &source);
        let is_last_tab = tab_count <= 1;

        if is_last_tab && others.is_empty() {
            log::debug!("do_detach_tab: single tab, single window, ignoring");
            return;
        }

        if others.is_empty() {
            // Multiple tabs, no other window — detach directly
            self.detach_active_tab_to_new_window();
        } else if others.len() == 1 && is_last_tab {
            // Last tab, single other window — send directly
            self.send_active_tab_to(Some(others[0].index));
        } else {
            // Show overlay
            let mut entries: Vec<SendToWindowEntry> = others.into_iter()
                .map(|info| SendToWindowEntry {
                    label: info.label,
                    window_index: Some(info.index),
                })
                .collect();
            // Only offer "New Window" if this isn't the last tab
            if !is_last_tab {
                entries.push(SendToWindowEntry {
                    label: "New Window".to_string(),
                    window_index: None,
                });
            }
            *self.ivars().send_to_window.borrow_mut() = Some(SendToWindowState {
                entries,
                selected: 0,
                merge_all: false,
            });
            self.mark_dirty();
        }
    }

    /// Merge this whole window (all its tabs) into another window.
    /// - no other window → no-op (nothing to merge into)
    /// - exactly one other window → merge directly
    /// - several other windows → overlay picker (no "New Window" option)
    pub(super) fn do_merge_window(&self) {
        let mtm = unsafe { MainThreadMarker::new_unchecked() };
        let source = self.window().unwrap();
        let others = crate::app::list_other_windows(mtm, &source);

        if others.is_empty() {
            log::debug!("do_merge_window: no other window to merge into, ignoring");
            return;
        }

        if others.len() == 1 {
            self.merge_window_into(others[0].index);
        } else {
            let entries: Vec<SendToWindowEntry> = others.into_iter()
                .map(|info| SendToWindowEntry {
                    label: info.label,
                    window_index: Some(info.index),
                })
                .collect();
            *self.ivars().send_to_window.borrow_mut() = Some(SendToWindowState {
                entries,
                selected: 0,
                merge_all: true,
            });
            self.mark_dirty();
        }
    }

    /// Move every tab of this window into the window at `target_index`
    /// (app-delegate window-list index), then close this now-empty window.
    /// Shared by the merge-window overlay and the IPC `merge-window` command.
    pub fn merge_window_into(&self, target_index: usize) {
        let tabs: Vec<crate::pane::Tab> = self.ivars().tabs.borrow_mut().drain(..).collect();
        if tabs.is_empty() {
            return;
        }
        let mtm = unsafe { MainThreadMarker::new_unchecked() };
        crate::app::send_tabs_to_window(mtm, tabs, target_index);
        // This window is now empty — close it without re-saving (the tabs live
        // on in the target window's session).
        self.ivars().skip_session_save.set(true);
        self.ivars().closing.set(true);
    }

    /// Detach the active tab to a new window (no overlay).
    pub(super) fn detach_active_tab_to_new_window(&self) {
        let mut tabs = self.ivars().tabs.borrow_mut();
        let idx = self.ivars().active_tab.get();
        if idx >= tabs.len() || tabs.len() <= 1 {
            return;
        }
        let tab = tabs.remove(idx);
        let new_idx = if idx >= tabs.len() { tabs.len() - 1 } else { idx };
        self.ivars().active_tab.set(new_idx);
        drop(tabs);
        self.resize_all_panes();

        let source_frame = self.window().map(|w| w.frame());
        let mtm = unsafe { MainThreadMarker::new_unchecked() };
        crate::app::detach_tab_to_new_window(mtm, tab, source_frame);
    }

    /// Send the active tab to a specific window (by index) or a new window.
    pub(super) fn send_active_tab_to(&self, window_index: Option<usize>) {
        let mut tabs = self.ivars().tabs.borrow_mut();
        let idx = self.ivars().active_tab.get();
        if idx >= tabs.len() {
            return;
        }
        let is_last = tabs.len() == 1;
        let tab = tabs.remove(idx);
        if !is_last {
            let new_idx = if idx >= tabs.len() { tabs.len() - 1 } else { idx };
            self.ivars().active_tab.set(new_idx);
        }
        drop(tabs);

        let mtm = unsafe { MainThreadMarker::new_unchecked() };
        match window_index {
            Some(wi) => crate::app::send_tab_to_window(mtm, tab, wi),
            None => {
                let source_frame = self.window().map(|w| w.frame());
                crate::app::detach_tab_to_new_window(mtm, tab, source_frame);
            }
        }

        if is_last {
            // Close this window — it's now empty
            self.ivars().skip_session_save.set(true);
            self.ivars().closing.set(true);
        } else {
            self.resize_all_panes();
        }
    }

    /// Break the focused pane out of its split into a new tab.
    /// No-op if the pane is already alone (single leaf tab).
    pub(super) fn do_break_pane(&self) {
        let mut tabs = self.ivars().tabs.borrow_mut();
        let idx = self.ivars().active_tab.get();
        if idx >= tabs.len() {
            return;
        }

        // No-op if already a single pane
        if tabs[idx].is_single_pane() {
            log::debug!("do_break_pane: pane is already alone, ignoring");
            drop(tabs);
            self.set_transient_status("Break Pane needs 2+ panes in this tab");
            return;
        }

        let focused_id = tabs[idx].focused_pane;
        log::debug!("do_break_pane: extracting pane {} from tab {}", focused_id, idx);

        // Find a neighbor to focus in the remaining tree
        let panes_vp = self.panes_viewport_for_tab(&tabs[idx]);
        let next_focus = tabs[idx].neighbor(focused_id, NavDirection::Right, panes_vp)
            .or_else(|| tabs[idx].neighbor(focused_id, NavDirection::Left, panes_vp))
            .or_else(|| tabs[idx].neighbor(focused_id, NavDirection::Down, panes_vp))
            .or_else(|| tabs[idx].neighbor(focused_id, NavDirection::Up, panes_vp));

        let old_columns = tabs[idx].num_visible_columns();

        // Extract the pane from the tab
        match tabs[idx].extract_pane(focused_id) {
            Some(extracted) => {
                // Update the source tab
                let new_focus = next_focus
                    .filter(|id| tabs[idx].contains(*id))
                    .unwrap_or_else(|| tabs[idx].first_pane().id);
                tabs[idx].focused_pane = new_focus;
                let new_columns = tabs[idx].num_visible_columns();
                tabs[idx].scale_virtual_width(old_columns, new_columns);
                tabs[idx].minimized_stack.retain(|&pid| pid != focused_id);

                let full = self.drawable_viewport();
                let min_w = self.min_split_width_px();
                tabs[idx].clamp_scroll(full.width, min_w);
                let tab = &mut tabs[idx];
                self.scroll_to_reveal_pane(tab, new_focus, full.width);

                // Create a new tab from the extracted pane
                let new_tab = Tab {
                    id: alloc_tab_id(),
                    columns: vec![crate::pane::Column::new(extracted)],
                    column_weights: vec![1.0],
                    custom_weights: vec![false],
                    focused_pane: focused_id,
                    custom_title: None,
                    color: None,
                    has_bell: false,
                    has_completion: false,
                    has_running: false,
                    fg_running_cache: false,
                    minimized_stack: Vec::new(),
                    scroll_offset_x: 0.0,
                    virtual_width_override: 0.0,
                    geometry_scale: self.backing_scale(),
                    cell_h: std::cell::Cell::new(0.0),
                };

                // Resize the source tab's remaining panes while it's still active
                drop(tabs);
                self.resize_all_panes();

                // Insert the new tab right after the current one and switch to it
                let mut tabs = self.ivars().tabs.borrow_mut();
                let new_idx = idx + 1;
                tabs.insert(new_idx, new_tab);
                self.ivars().active_tab.set(new_idx);
                drop(tabs);
                self.resize_all_panes();
            }
            None => {
                log::error!("do_break_pane: extract_pane returned None unexpectedly");
            }
        }
    }

    /// Merge the current tab into another tab (show overlay to pick target).
    /// No-op if there's only one tab.
    pub(super) fn do_merge_tab(&self) {
        let tabs = self.ivars().tabs.borrow();
        if tabs.len() <= 1 {
            log::debug!("do_merge_tab: only one tab, ignoring");
            return;
        }
        let active = self.ivars().active_tab.get();
        let entries: Vec<MergeTabEntry> = tabs.iter().enumerate()
            .filter(|(i, _)| *i != active)
            .map(|(i, t)| MergeTabEntry {
                label: t.title(),
                tab_index: i,
            })
            .collect();
        drop(tabs);

        if entries.len() == 1 {
            // Only one possible target — merge directly
            let target = entries[0].tab_index;
            self.merge_active_tab_into(target);
        } else {
            *self.ivars().merge_tab.borrow_mut() = Some(MergeTabState {
                entries,
                selected: 0,
            });
            self.mark_dirty();
        }
    }

    /// Handle key events in the "Merge Tab" overlay.
    pub(super) fn handle_merge_tab_key(&self, event: &NSEvent) {
        let keycode = event.keyCode();

        // Escape → close
        if keycode == 0x35 {
            *self.ivars().merge_tab.borrow_mut() = None;
            self.mark_dirty();
            return;
        }

        // Enter → confirm selection
        if keycode == 0x24 {
            let target = {
                let state = self.ivars().merge_tab.borrow();
                state.as_ref().map(|s| s.entries[s.selected].tab_index)
            };
            if let Some(target_idx) = target {
                *self.ivars().merge_tab.borrow_mut() = None;
                self.merge_active_tab_into(target_idx);
            }
            return;
        }

        // Arrow keys
        {
            let mut guard = self.ivars().merge_tab.borrow_mut();
            let state = match guard.as_mut() {
                Some(s) => s,
                None => return,
            };
            match keycode {
                0x7E => { // Up
                    if state.selected > 0 {
                        state.selected -= 1;
                    }
                }
                0x7D => { // Down
                    if state.selected + 1 < state.entries.len() {
                        state.selected += 1;
                    }
                }
                _ => {}
            }
        }
        self.mark_dirty();
    }

    /// Merge the active tab's columns into the target tab (appended to the right).
    /// The active tab is removed. Focus moves to the leftmost pane of the merged columns.
    pub(super) fn merge_active_tab_into(&self, target_idx: usize) {
        let mut tabs = self.ivars().tabs.borrow_mut();
        let active = self.ivars().active_tab.get();
        if active >= tabs.len() || target_idx >= tabs.len() || active == target_idx {
            return;
        }

        // Remove the source tab first
        let source = tabs.remove(active);

        // Adjust target index after removal
        let target = if target_idx > active { target_idx - 1 } else { target_idx };

        // The leftmost pane in the source becomes the new focus
        let new_focus = source.columns.first()
            .and_then(|col| col.panes.first())
            .map(|p| p.id)
            .unwrap_or(source.focused_pane);

        // Append source columns to target tab, normalizing weights
        let target_avg: f32 = tabs[target].column_weights.iter().sum::<f32>()
            / tabs[target].columns.len() as f32;
        let source_avg: f32 = source.column_weights.iter().sum::<f32>()
            / source.columns.len().max(1) as f32;
        let scale = if source_avg > 0.0 { target_avg / source_avg } else { 1.0 };
        for (i, (col, weight)) in source.columns.into_iter().zip(source.column_weights.into_iter()).enumerate() {
            tabs[target].columns.push(col);
            tabs[target].column_weights.push(weight * scale);
            tabs[target].custom_weights.push(
                source.custom_weights.get(i).copied().unwrap_or(false)
            );
        }

        // Merge minimized stacks
        tabs[target].minimized_stack.extend(source.minimized_stack);

        // Focus the leftmost pane from the merged columns
        tabs[target].focused_pane = new_focus;

        // Switch to the target tab
        self.ivars().active_tab.set(target);

        drop(tabs);
        self.resize_all_panes();
    }

    /// Get tab titles for this window (used by "Send Tab to Window" overlay).
    pub fn tab_titles(&self) -> Vec<String> {
        let tabs = self.ivars().tabs.borrow();
        tabs.iter().map(|t| t.title()).collect()
    }

    /// Append external tabs (used by send-tab-to-window).
    pub fn append_tabs(&self, new_tabs: Vec<crate::pane::Tab>) {
        let mut tabs = self.ivars().tabs.borrow_mut();
        let first_new = tabs.len();
        tabs.extend(new_tabs);
        drop(tabs);
        self.ivars().active_tab.set(first_new);
        self.resize_all_panes();
    }
}
