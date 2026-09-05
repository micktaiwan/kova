//! The per-frame pass. `tick` is what the run-loop timer calls: it drains the
//! PTYs, keeps the tab list and the session file in step, and builds the render
//! data for every overlay that is open, then hands the whole frame to the
//! renderer. `setup_metal` is what puts the layer under it in the first place.

use super::*;

impl KovaView {
    /// Initialize Metal rendering with the given tabs.
    pub fn setup_metal(&self, _mtm: MainThreadMarker, config: &Config, tabs: Vec<Tab>, active_tab: usize) {
        log::info!("Setting up Metal");
        let device = MTLCreateSystemDefaultDevice()
            .expect("no Metal device");

        let layer = CAMetalLayer::new();
        layer.setDevice(Some(&device));
        layer.setPixelFormat(objc2_metal::MTLPixelFormat::BGRA8Unorm);
        layer.setFramebufferOnly(true);

        let frame = self.frame();
        let scale = if let Some(window) = self.window() {
            window.backingScaleFactor()
        } else {
            2.0
        };
        layer.setContentsScale(scale);
        layer.setDrawableSize(CGSize {
            width: frame.size.width * scale,
            height: frame.size.height * scale,
        });

        self.setWantsLayer(true);
        self.setLayer(Some(&layer));
        self.ivars().metal_layer.set(layer.clone()).ok();

        self.ivars().last_scale.set(scale);

        let terminal_for_renderer = tabs[active_tab].first_pane().terminal.clone();

        let renderer = Arc::new(parking_lot::RwLock::new(
            Renderer::new(&device, &layer, terminal_for_renderer, scale, config),
        ));

        self.ivars().renderer.set(renderer).ok();
        self.ivars().config.set(config.clone()).ok();
        self.ivars().keybindings.set(Keybindings::from_config(&config.keys)).ok();
        self.ivars().git_poll_interval.set(config.terminal.fps * 2);
        self.ivars().help_hint_frames.set(config.terminal.fps * 3);
        // Tabs restored from a session carry the pixel geometry of the display
        // they were saved on: convert it to this window's display.
        let mut tabs = tabs;
        for tab in tabs.iter_mut() {
            tab.adopt_geometry_scale(scale as f32);
        }
        *self.ivars().tabs.borrow_mut() = tabs;
        self.ivars().active_tab.set(active_tab);
    }

    /// Called by the global render timer in AppDelegate for each window.
    /// Handles all per-frame work: command injection, auto-scroll, git polling,
    /// pane reaping, rendering, focus reporting, and window title updates.
    /// Returns `false` if the window has no tabs left and should be closed.
    pub fn tick(&self) -> bool {
        let ivars = self.ivars();
        if ivars.closing.get() {
            return false;
        }

        // --- Feed the pane visit history ---
        // Sampling the focused pane of the key window once per frame catches
        // every way of landing on a pane (keyboard, mouse, IPC, tab switch)
        // without each of those paths having to record anything itself.
        if self.window().is_some_and(|w| w.isKeyWindow()) {
            let focused = {
                let tabs = ivars.tabs.borrow();
                tabs.get(ivars.active_tab.get()).map(|tab| tab.focused_pane)
            };
            if let Some(id) = focused {
                crate::pane_history::record(id, &pane_history_state);
            }
        }

        // --- A read pane stops pulling Cmd+J back to itself ---
        // Same sampling rule as the visit history: the focused pane of the key
        // window is the one under the eye. The waiting flag itself stays up
        // (retracting it is for answering, see `Pane::clear_awaiting`): nothing
        // in the UI draws it any more, but IPC clients still read it, and
        // `awaiting_seen` is what tells them the pane has been looked at.
        if self.window().is_some_and(|w| w.isKeyWindow()) {
            let tabs = ivars.tabs.borrow();
            if let Some(tab) = tabs.get(ivars.active_tab.get()) {
                if let Some(pane) = tab.pane(tab.focused_pane) {
                    pane.mark_awaiting_seen();
                    pane.mark_idle_claude_seen();
                }
            }
        }

        let renderer = match ivars.renderer.get() {
            Some(r) => r.clone(),
            None => return true, // not yet initialized
        };
        let layer = match ivars.metal_layer.get() {
            Some(l) => l.clone(),
            None => return true,
        };

        // --- Inject pending commands for restored panes ---
        {
            let tabs = ivars.tabs.borrow();
            for tab in tabs.iter() {
                tab.for_each_pane(&mut |pane| {
                    pane.inject_pending_command();
                });
            }
        }

        // --- Progressive restore of deferred tabs (batched) ---
        // Allow up to MAX_CONCURRENT_SHELLS non-ready shells at once.
        // This gives parallelism without the 30+ shell stampede.
        {
            const MAX_CONCURRENT_SHELLS: u32 = 4;

            let mut deferred = ivars.deferred_tabs.borrow_mut();
            if !deferred.is_empty() {
                // Count shells currently loading (live PTY, not yet ready)
                let tabs = ivars.tabs.borrow();
                let mut loading: u32 = 0;
                for tab in tabs.iter() {
                    tab.for_each_pane(&mut |pane| {
                        if !pane.is_ready() && pane.pty.is_live() {
                            loading += 1;
                        }
                    });
                }
                drop(tabs);

                // Restore tabs until we hit the concurrency limit
                while !deferred.is_empty() && loading < MAX_CONCURRENT_SHELLS {
                    let (tab_id, saved_tab) = deferred.pop().unwrap();
                    let pane_count = crate::session::count_panes_in_saved_tab(&saved_tab);
                    // The placeholder may have been closed by the user while
                    // waiting — skip the entry instead of restoring it.
                    if !ivars.tabs.borrow().iter().any(|t| t.id == tab_id) {
                        log::info!("Deferred-restore: placeholder tab {:?} was closed; skipping", tab_id);
                        ivars.tab_backup.borrow_mut().remove(&tab_id);
                        let cur = ivars.loading_total_panes.get();
                        ivars.loading_total_panes.set(cur.saturating_sub(pane_count as u32));
                        continue;
                    }
                    let config = ivars.config.get().unwrap();
                    let cols = config.terminal.columns;
                    let rows = config.terminal.rows;
                    match crate::session::restore_saved_tab(&saved_tab, cols, rows, config) {
                        Some(mut tab) => {
                            tab.adopt_geometry_scale(self.backing_scale());
                            loading += pane_count as u32;
                            let mut tabs = ivars.tabs.borrow_mut();
                            if let Some(pos) = tabs.iter().position(|t| t.id == tab_id) {
                                tabs[pos] = tab;
                                // Drop the placeholder's backup entry — its data
                                // has been replaced by the live restored tab.
                                ivars.tab_backup.borrow_mut().remove(&tab_id);
                            } else {
                                log::warn!(
                                    "Deferred-restore: placeholder tab {:?} disappeared during restore; dropping restored tab",
                                    tab_id
                                );
                                let cur = ivars.loading_total_panes.get();
                                ivars.loading_total_panes.set(cur.saturating_sub(pane_count as u32));
                            }
                            drop(tabs);
                            self.resize_all_panes();
                        }
                        None => {
                            log::warn!("Failed to restore deferred tab {:?}", tab_id);
                            // The placeholder stays put — its tab_backup entry
                            // already preserves the original SavedTab, so save
                            // won't overwrite the user's data. But the loading
                            // counter would otherwise hang forever, so drop
                            // these panes from the expected total.
                            let cur = ivars.loading_total_panes.get();
                            ivars.loading_total_panes.set(cur.saturating_sub(pane_count as u32));
                        }
                    }
                }
            }
        }

        // --- Auto-scroll during drag selection ---
        {
            let speed = ivars.auto_scroll_speed.get();
            if speed != 0 {
                let tabs = ivars.tabs.borrow();
                let idx = ivars.active_tab.get();
                if let Some(tab) = tabs.get(idx) {
                    if let Some(pane) = tab.pane(tab.focused_pane) {
                        let mut term = pane.terminal.write();
                        if term.selection.is_some() {
                            term.scroll(-speed);
                            let sb_len = term.scrollback_len();
                            let scroll_off = term.scroll_offset();
                            if speed < 0 {
                                let first_visible = (sb_len as i64 - scroll_off as i64) as usize;
                                if let Some(ref mut sel) = term.selection {
                                    sel.end = crate::terminal::GridPos { line: first_visible, col: 0 };
                                }
                            } else {
                                let last_visible = (sb_len as i64 - scroll_off as i64 + term.rows as i64 - 1) as usize;
                                let last_col = term.cols.saturating_sub(1);
                                if let Some(ref mut sel) = term.selection {
                                    sel.end = crate::terminal::GridPos { line: last_visible, col: last_col };
                                }
                            }
                            term.dirty.store(true, std::sync::atomic::Ordering::Relaxed);
                        }
                    }
                }
            }
        }

        // --- Deferred PTY winsize restores after Cmd+R nudges ---
        {
            let mut due: Vec<PaneId> = Vec::new();
            {
                let mut restores = ivars.pty_restore.borrow_mut();
                for r in restores.iter_mut() {
                    r.remaining_frames = r.remaining_frames.saturating_sub(1);
                    if r.remaining_frames == 0 {
                        due.push(r.pane_id);
                    }
                }
                restores.retain(|r| r.remaining_frames > 0);
            }
            if !due.is_empty() {
                // Dims are re-read at fire time: a window resize in between
                // already set the PTY to the new size, and the terminal grid
                // is the source of truth for what the winsize should be.
                let tabs = ivars.tabs.borrow();
                for tab in tabs.iter() {
                    tab.for_each_pane(&mut |pane| {
                        if due.contains(&pane.id) {
                            let (cols, rows) = {
                                let t = pane.terminal.read();
                                (t.cols, t.rows)
                            };
                            pane.pty.resize(cols, rows);
                            // Measure the app's response to this restore
                            // SIGWINCH from a clean slate.
                            pane.terminal.write().reset_rows_touched();
                        }
                    });
                }
                drop(tabs);
                // The app's answer to a winsize restore is the frame that can
                // carry the hole (clear + partial repaint). Schedule a grid
                // scan once that frame has certainly landed.
                let mut checks = ivars.post_restore_checks.borrow_mut();
                for &pane_id in &due {
                    checks.retain(|c| c.pane_id != pane_id);
                    checks.push(PtyRestore { pane_id, remaining_frames: POST_RESTORE_CHECK_FRAMES });
                }
            }
        }

        // --- Debounced robust repaint after a resize burst settles ---
        self.fire_resize_settle_repaints();

        // --- Post-restore hole check: repair clear+partial-repaint frames ---
        self.fire_post_restore_band_checks();

        // --- Poll git branch for all panes with a CWD ---
        let git_poll_interval = ivars.git_poll_interval.get();
        let count = ivars.git_poll_counter.get() + 1;
        ivars.git_poll_counter.set(count);
        if count >= git_poll_interval {
            ivars.git_poll_counter.set(0);
            let tabs = ivars.tabs.borrow();
            for tab in tabs.iter() {
                tab.for_each_pane(&mut |pane| {
                    let term = pane.terminal.read();
                    let cwd = term.cwd.clone();
                    let old_branch = term.git_branch.clone();
                    drop(term);
                    if let Some(ref cwd) = cwd {
                        let new_branch = crate::terminal::parser::resolve_git_branch(cwd);
                        if new_branch != old_branch {
                            let mut term = pane.terminal.write();
                            term.git_branch = new_branch;
                            term.dirty.store(true, std::sync::atomic::Ordering::Relaxed);
                        }
                    }
                });
            }
        }

        // --- Reap exited panes across ALL tabs ---
        let mut any_removed = false;
        let mut tabs_to_remove: Vec<usize> = Vec::new();
        {
            let mut tabs = ivars.tabs.borrow_mut();
            for (tab_idx, tab) in tabs.iter_mut().enumerate() {
                let exited = tab.exited_pane_ids();
                if exited.is_empty() {
                    continue;
                }
                any_removed = true;
                log::debug!("Reaping exited panes in tab {}: {:?}", tab_idx, exited);
                for id in &exited {
                    let old_cols = tab.num_visible_columns();
                    if !tab.remove_pane(*id) {
                        tabs_to_remove.push(tab_idx);
                        break;
                    }
                    let new_cols = tab.num_visible_columns();
                    tab.scale_virtual_width(old_cols, new_cols);
                    tab.minimized_stack.retain(|&pid| pid != *id);
                }
                if !tabs_to_remove.contains(&tab_idx) {
                    // If only minimized panes remain, restore the last minimized one
                    if let Some(restored) = tab.ensure_visible_pane() {
                        tab.focused_pane = restored;
                    } else if exited.contains(&tab.focused_pane) {
                        tab.focused_pane = tab
                            .first_visible_pane()
                            .unwrap_or_else(|| tab.first_pane().id);
                    }
                }
            }
            for &idx in tabs_to_remove.iter().rev() {
                tabs.remove(idx);
            }
        }

        // Adjust active_tab if needed; signal close if no tabs left
        if any_removed {
            let tabs = ivars.tabs.borrow();
            if tabs.is_empty() {
                drop(tabs);
                return false;
            }
            let active = ivars.active_tab.get();
            if active >= tabs.len() {
                ivars.active_tab.set(tabs.len() - 1);
            }
        }

        // Build pane render list from active tab only
        let active_idx = ivars.active_tab.get();
        let split_min_w = ivars.config.get()
            .map(|c| c.splits.min_width)
            .unwrap_or(300.0)
            * ivars.last_scale.get().max(1.0) as f32;
        let (pane_data, pty_ptr, focus_reporting, tab_titles, active_panes_vp, screen_width, total_columns, focused_column, active_tab, total_tabs, active_tab_name, working_claudes, unread_panes) = {
            let mut tabs = ivars.tabs.borrow_mut();
            if tabs.is_empty() {
                return false;
            }
            let tab = &mut tabs[active_idx];
            let focused_id = tab.focused_pane;

            let mut pane_data: Vec<crate::renderer::PaneRenderData> = Vec::new();
            let cell_h = renderer.read().cell_size().1;
            tab.cell_h.set(cell_h);
            let tab_bar_h = (cell_h * 2.0).round();
            let drawable_size = layer.drawableSize();
            let screen_width = drawable_size.width as f32;
            let virtual_width = tab.virtual_width(screen_width, split_min_w);
            let global_bar_h = cell_h;
            let panes_vp = PaneViewport {
                x: -tab.scroll_offset_x,
                y: tab_bar_h,
                width: virtual_width,
                height: drawable_size.height as f32 - tab_bar_h - global_bar_h,
            };
            tab.for_each_pane_with_viewport(panes_vp, &mut |pane, vp| {
                // First frame this pane is submitted to the renderer = it becomes
                // visible (loading overlay or content). "time to rectangle".
                pane.open_timer.mark_first_paint(pane.id);
                let is_focused = pane.id == focused_id;
                let term = pane.terminal.read();
                // The focused pane is "seen": acknowledge its bell and its
                // completion every frame so neither reappears stale once focus
                // moves away. The ack is a separate flag — command_completed
                // stays sticky until the next OSC 133;C for the IPC
                // wait-for-completion contract.
                if is_focused {
                    term.bell.store(false, std::sync::atomic::Ordering::Relaxed);
                    term.ack_completion();
                }
                let completed = !is_focused && term.unread_completion();
                let has_bell = !is_focused
                    && term.bell.load(std::sync::atomic::Ordering::Relaxed);
                drop(term);
                pane_data.push(crate::renderer::PaneRenderData {
                    terminal: pane.terminal.clone(),
                    viewport: vp,
                    shell_ready: pane.is_ready(),
                    is_focused,
                    pane_id: pane.id,
                    custom_title: pane.custom_title.clone(),
                    has_completion: completed,
                    has_bell,
                    minimized: pane.minimized,
                    input_chars: pane.pty.input_chars.clone(),
                    // Name only: the status bar is the densest line in the app,
                    // and the version has room in the pane switcher instead.
                    fg_process: pane.fg_process().map(|p| p.name),
                });
            });

            // Propagate OSC 1 sticky titles to pane custom_title
            for entry in &mut pane_data {
                let has_osc1 = entry.terminal.read().osc1_title.is_some();
                if has_osc1 {
                    let sticky = entry.terminal.write().osc1_title.take().unwrap();
                    let title = if sticky.is_empty() { None } else { Some(sticky) };
                    if let Some(pane) = tab.pane_mut(entry.pane_id) {
                        pane.custom_title = title.clone();
                    }
                    entry.custom_title = title;
                }
            }

            // Override custom_title for focused pane when rename_pane is active
            {
                let rename_pane = ivars.rename_pane.borrow();
                if let Some(ref rs) = *rename_pane {
                    for entry in &mut pane_data {
                        if entry.is_focused {
                            let before: String = rs.input.chars().take(rs.cursor).collect();
                            let after: String = rs.input.chars().skip(rs.cursor).collect();
                            entry.custom_title = Some(format!("{}▏{}", before, after));
                        }
                    }
                }
            }

            let focused = tab.pane(focused_id);
            let pty_ptr = focused.map(|p| &p.pty as *const crate::terminal::pty::Pty);
            let focus_reporting = focused.map_or(false, |p| p.terminal.read().focus_reporting);

            // Probe foreground process groups every ~0.5s (30 ticks @60fps);
            // OSC-based running state is still refreshed every tick.
            let fg_count = ivars.fg_poll_counter.get() + 1;
            let refresh_fg = fg_count >= 30;
            ivars.fg_poll_counter.set(if refresh_fg { 0 } else { fg_count });
            for (i, t) in tabs.iter_mut().enumerate() {
                t.check_bell();
                t.check_running(refresh_fg);
                // Skip active tab: completion already read into pane_data
                if i != active_idx {
                    t.check_completion();
                }
            }
            tabs[active_idx].clear_bell();
            // Derive active tab's completion from pane_data (avoids double atomic read)
            tabs[active_idx].has_completion = pane_data.iter().any(|p| p.has_completion);

            let rename = ivars.rename_tab.borrow();
            let tab_titles: Vec<(String, bool, Option<usize>, bool, bool, bool, bool)> = tabs.iter().enumerate()
                .map(|(i, t)| {
                    let is_renaming = i == active_idx && rename.is_some();
                    let title = if is_renaming {
                        let rs = rename.as_ref().unwrap();
                        let before: String = rs.input.chars().take(rs.cursor).collect();
                        let after: String = rs.input.chars().skip(rs.cursor).collect();
                        format!("{}▏{}", before, after)
                    } else {
                        t.title()
                    };
                    (title, i == active_idx, t.color, is_renaming, t.has_bell, t.has_completion, t.has_running)
                })
                .collect();
            drop(rename);
            // Status-bar column indicator: count only columns that occupy
            // layout space (fully-minimized columns are zero-width, invisible).
            let total_columns = tabs[active_idx].num_visible_columns();
            let focused_column = tabs[active_idx].visible_column_index(tabs[active_idx].focused_pane).unwrap_or(1);
            let active_tab_1based = active_idx + 1;
            let total_tabs = tabs.len();
            let active_tab_name = tabs[active_idx].title();
            // Count panes across every tab of this window whose app is signalling
            // activity via the OSC-title marker (Claude Code busy).
            let mut working_claudes = 0usize;
            // Panes carrying output nobody has looked at: a bell, or a command
            // that finished while the eye was elsewhere — the same signal as the
            // per-pane dot and as Cmd+J's first tier.
            let mut unread_panes = 0usize;
            for t in tabs.iter() {
                t.for_each_pane(&mut |p| {
                    if p.is_working() { working_claudes += 1; }
                    let term = p.terminal.read();
                    if term.bell.load(std::sync::atomic::Ordering::Relaxed) || term.unread_completion() {
                        unread_panes += 1;
                    }
                });
            }
            (pane_data, pty_ptr, focus_reporting, tab_titles, panes_vp, screen_width, total_columns, focused_column, active_tab_1based, total_tabs, active_tab_name, working_claudes, unread_panes)
        };

        // Focus reporting (DEC mode 1004) — send to focused pane only
        unsafe {
            let mtm = MainThreadMarker::new_unchecked();
            let app = NSApplication::sharedApplication(mtm);
            let focused = app.isActive();
            let prev = ivars.last_focused.get();
            if focused != prev {
                ivars.last_focused.set(focused);
                if focus_reporting {
                    if let Some(pty_ptr) = pty_ptr {
                        let seq = if focused { b"\x1b[I" as &[u8] } else { b"\x1b[O" };
                        (*pty_ptr).write(seq);
                    }
                }
            }
        }

        // Update NSWindow title from focused pane's OSC 0/2
        if let Some(focused_pane) = pane_data.iter().find(|p| p.is_focused) {
            let term = focused_pane.terminal.read();
            let current = term.title.clone();
            drop(term);
            let mut prev = ivars.last_title.borrow_mut();
            if current != *prev {
                if let Some(win) = self.window() {
                    let title_str = match current {
                        Some(ref t) => format!("Kova — {}", t),
                        None => "Kova".to_string(),
                    };
                    win.setTitle(&NSString::from_str(&title_str));
                }
                *prev = current;
            }
        }

        // Collect split separators from active tab
        let separators = {
            let tabs = ivars.tabs.borrow();
            if let Some(tab) = tabs.get(active_idx) {
                let mut seps = Vec::new();
                tab.collect_separators(active_panes_vp, &mut seps);
                seps
            } else {
                Vec::new()
            }
        };

        // Minimized-pane counters for the global status bar:
        // current = active tab of this window; total = every tab of every window.
        let minimized_counts = {
            let current = ivars.tabs.borrow()[active_idx].count_minimized();
            let mut total = 0usize;
            let mtm = unsafe { MainThreadMarker::new_unchecked() };
            let ad = crate::app::app_delegate(mtm);
            for win in ad.ivars().windows.borrow().iter() {
                if let Some(view) = crate::app::kova_view(win) {
                    for tab in view.ivars().tabs.borrow().iter() {
                        total += tab.count_minimized();
                    }
                }
            }
            (current, total)
        };

        // Decrement help hint countdown
        let help_hint_remaining = ivars.help_hint_frames.get();
        if help_hint_remaining > 0 {
            ivars.help_hint_frames.set(help_hint_remaining - 1);
        }
        let show_help = ivars.show_help.get();
        let show_mem_report = ivars.show_mem_report.get();

        // Build filter render data if active
        let filter_data = {
            let filter = ivars.filter.borrow();
            filter.as_ref().map(|f| FilterRenderData {
                query: f.query.clone(),
                matches: f.matches.clone(),
                // Only while the input is empty: once the user types, the match
                // count is the useful number.
                hint: (f.query.is_empty()
                    && !crate::search_history::is_empty(crate::search_history::Scope::Filter))
                .then(|| "↑ recent".to_string()),
            })
        };

        // Compute left_inset from traffic light buttons
        let left_inset = {
            let inset = self.window()
                .and_then(|win| {
                    let scale = win.backingScaleFactor() as f32;
                    win.standardWindowButton(NSWindowButton::ZoomButton)
                        .map(|btn| {
                            let frame = btn.frame();
                            let right_edge = (frame.origin.x + frame.size.width) as f32;
                            (right_edge + 8.0) * scale
                        })
                })
                .unwrap_or(140.0);
            ivars.tab_bar_left_inset.set(inset);
            inset
        };
        let (hover_segments, hover_text, hover_pane_id) = {
            let h = ivars.hovered_url.borrow();
            (
                h.as_ref().map(|(_, segs, _)| segs.clone()),
                h.as_ref().map(|(_, _, url)| url.clone()),
                h.as_ref().map(|(pid, _, _)| *pid),
            )
        };
        let mut r = renderer.write();
        r.hovered_url = hover_segments;
        r.hovered_url_text = hover_text;
        r.hovered_url_pane_id = hover_pane_id;
        // Count hidden panes (fully off-screen). Minimized panes are excluded:
        // they are zero-sized by design, not hidden by horizontal scroll.
        let mut hidden_left = 0usize;
        let mut hidden_right = 0usize;
        for p in &pane_data {
            if p.minimized {
                continue;
            }
            if p.viewport.x + p.viewport.width <= 0.0 {
                hidden_left += 1;
            } else if p.viewport.x >= screen_width {
                hidden_right += 1;
            }
        }
        let keys_config = ivars.config.get().map(|c| &c.keys);

        // Build recent projects render data if overlay is active (uses cached data)
        let rp_guard = ivars.recent_projects.borrow();
        let rp_entries: Vec<&crate::renderer::RecentProjectEntry> = rp_guard.as_ref()
            .map(|state| state.items.iter().map(|item| &item.render).collect())
            .unwrap_or_default();
        let rp_data = rp_guard.as_ref().map(|state| {
            crate::renderer::RecentProjectsRenderData {
                entries: &rp_entries,
                selected: state.selected,
                scroll: state.scroll,
            }
        });

        // Build search palette render data + decrement pane flash counter
        let sp_guard = ivars.search_palette.borrow();
        let sp_rows: Vec<crate::renderer::SearchRowRender> = sp_guard.as_ref()
            .map(|state| state.rows.iter().map(|r| match r {
                SearchRow::Header(t) => crate::renderer::SearchRowRender { text: t.as_str(), is_header: true },
                SearchRow::Hit(h) => crate::renderer::SearchRowRender { text: h.label.as_str(), is_header: false },
            }).collect())
            .unwrap_or_default();
        let sp_data = sp_guard.as_ref().map(|state| {
            crate::renderer::SearchPaletteRenderData {
                query: &state.query,
                cursor: state.cursor,
                submitted_query: &state.submitted_query,
                searching: state.searching,
                rows: &sp_rows,
                selected: state.selected,
                scroll: state.scroll,
            }
        });

        // Pane flash for search-palette jumps: pulse the matching pane's border
        // for the configured number of frames, then clear.
        r.pane_flash = None;
        r.pane_flash_label = None;
        let mut flash_slot = ivars.pane_flash.borrow_mut();
        if let Some(flash) = flash_slot.as_mut() {
            if flash.remaining_frames > 0 {
                flash.remaining_frames -= 1;
                if let Some(target) = pane_data.iter().find(|p| p.pane_id == flash.pane_id) {
                    let vp = &target.viewport;
                    // Linear fade from 1.0 → 0.0 over the last 30 frames; a
                    // longer flash simply holds at full opacity before it.
                    let alpha = (flash.remaining_frames as f32 / 30.0).clamp(0.0, 1.0);
                    r.pane_flash = Some((vp.x, vp.y, vp.width, vp.height, alpha));
                    r.pane_flash_label = flash
                        .label
                        .as_ref()
                        .map(|l| (l.name.clone(), l.parent.clone()));
                } else {
                    // Pane disappeared (e.g. closed) — drop the flash.
                    *flash_slot = None;
                }
            } else {
                *flash_slot = None;
            }
        }
        drop(flash_slot);

        // Build list-overlay render data (send-to-window or merge-tab)
        let stw_guard = ivars.send_to_window.borrow();
        let mt_guard = ivars.merge_tab.borrow();
        let overlay_labels: Vec<String> = if stw_guard.is_some() {
            stw_guard.as_ref().unwrap().entries.iter().map(|e| e.label.clone()).collect()
        } else if mt_guard.is_some() {
            mt_guard.as_ref().unwrap().entries.iter().map(|e| e.label.clone()).collect()
        } else {
            Vec::new()
        };
        let stw_data = if let Some(state) = stw_guard.as_ref() {
            Some(crate::renderer::SendToWindowRenderData {
                title: "Send Tab to Window",
                entries: &overlay_labels,
                selected: state.selected,
                has_new_entry: state.entries.last().map_or(false, |e| e.window_index.is_none()),
            })
        } else if let Some(state) = mt_guard.as_ref() {
            Some(crate::renderer::SendToWindowRenderData {
                title: "Merge Tab Into",
                entries: &overlay_labels,
                selected: state.selected,
                has_new_entry: false,
            })
        } else {
            None
        };

        // Build tab/pane switcher render data (one entry per column)
        let ps_guard = ivars.pane_switcher.borrow();
        let ps_cols_rows: Vec<Vec<crate::renderer::PaneSwitcherRowRender>> = ps_guard.as_ref()
            .map(|state| state.columns.iter().map(|col| col.iter().map(|r| match r {
                SwitcherRow::TabHeader(t) => crate::renderer::PaneSwitcherRowRender { text: t.as_str(), is_header: true, has_bell: false, has_completion: false, minimized: false, working: false, process: None },
                SwitcherRow::Pane { title, has_bell, has_completion, minimized, working, process, .. } => crate::renderer::PaneSwitcherRowRender { text: title.as_str(), is_header: false, has_bell: *has_bell, has_completion: *has_completion, minimized: *minimized, working: *working, process: process.as_deref() },
            }).collect()).collect())
            .unwrap_or_default();
        let ps_columns: Vec<crate::renderer::PaneSwitcherColumnRender> = ps_guard.as_ref()
            .map(|state| ps_cols_rows.iter().enumerate().map(|(i, rows)| crate::renderer::PaneSwitcherColumnRender {
                rows,
                scroll: state.scroll.get(i).copied().unwrap_or(0),
            }).collect())
            .unwrap_or_default();
        let ps_data = ps_guard.as_ref().map(|state| crate::renderer::PaneSwitcherRenderData {
            columns: &ps_columns,
            selected_col: state.selected_col,
            selected_row: state.selected_row,
            filtered: state.filtered,
        });

        // Update resize feedback (decrement frames, build text)
        if let Some(mut fb) = ivars.resize_feedback.get() {
            if fb.remaining_frames > 0 {
                fb.remaining_frames -= 1;
                ivars.resize_feedback.set(Some(fb));
                let mode_str = match fb.mode {
                    ResizeMode::Ratio => "Ratio",
                    ResizeMode::Virtual => "Virtual",
                    ResizeMode::Edge => "Right Edge",
                };
                r.resize_feedback_text = Some(format!("{} — screen {}px — virtual {}px", mode_str, fb.screen_w, fb.virtual_w));
            } else {
                ivars.resize_feedback.set(None);
                r.resize_feedback_text = None;
            }
        } else {
            r.resize_feedback_text = None;
        }

        // Transient status message (e.g. Break Pane no-op) — takes priority over
        // the resize feedback slot in the global status bar while it lasts.
        {
            let mut ts = ivars.transient_status.borrow_mut();
            if let Some((msg, frames)) = ts.as_mut() {
                if *frames > 0 {
                    *frames -= 1;
                    r.resize_feedback_text = Some(msg.clone());
                } else {
                    *ts = None;
                }
            }
        }

        // Attention banner: the tier the last Cmd+J landed in, painted across the
        // focused pane's own status bar (the renderer places it there).
        {
            let mut banner = ivars.attention_banner.borrow_mut();
            match banner.as_mut() {
                Some((text, color, frames)) if *frames > 0 => {
                    *frames -= 1;
                    r.pane_banner = Some((text.clone(), *color));
                }
                Some(_) => {
                    *banner = None;
                    r.pane_banner = None;
                }
                None => r.pane_banner = None,
            }
        }

        // Update boundary flash (decrement frames, compute edge position)
        if let Some(mut flash) = ivars.boundary_flash.get() {
            if flash.remaining_frames > 0 {
                flash.remaining_frames -= 1;
                ivars.boundary_flash.set(Some(flash));
                // Find focused pane viewport to position the flash line
                if let Some(focused) = pane_data.iter().find(|p| p.is_focused) {
                    let vp = &focused.viewport;
                    let is_right = flash.edge == NavDirection::Right;
                    let edge_x = if is_right { vp.x + vp.width } else { vp.x };
                    r.boundary_flash = Some((edge_x, vp.y, vp.y + vp.height, 1.0, is_right));
                } else {
                    r.boundary_flash = None;
                }
            } else {
                ivars.boundary_flash.set(None);
                r.boundary_flash = None;
            }
        } else {
            r.boundary_flash = None;
        }

        // Update loading progress: count shell_ready (live PTYs only) against fixed total
        {
            let fixed_total = ivars.loading_total_panes.get();
            if fixed_total > 0 {
                let tabs = ivars.tabs.borrow();
                let deferred_remaining = ivars.deferred_tabs.borrow().len() as u32;
                let mut ready: u32 = 0;
                for tab in tabs.iter() {
                    tab.for_each_pane(&mut |pane| {
                        if pane.is_ready() && pane.pty.is_live() {
                            ready += 1;
                        }
                    });
                }
                if ready < fixed_total || deferred_remaining > 0 {
                    r.loading_progress = Some((ready, fixed_total));
                } else {
                    r.loading_progress = None;
                    // Clear so we don't keep checking
                    ivars.loading_total_panes.set(0);
                }
            }
        }

        r.render_panes(&layer, &pane_data, &separators, &tab_titles, filter_data.as_ref(), left_inset, hidden_left, hidden_right, focused_column, total_columns, active_tab, total_tabs, &active_tab_name, working_claudes, unread_panes, minimized_counts, show_help, show_mem_report, rp_data.as_ref(), stw_data.as_ref(), sp_data.as_ref(), ps_data.as_ref(), help_hint_remaining, keys_config);
        true
    }
}
