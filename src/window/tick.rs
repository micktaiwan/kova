//! The per-frame pass. `tick` is what the run-loop timer calls: it drains the
//! PTYs, keeps the tab list and the session file in step, and builds the render
//! data for every overlay that is open, then hands the whole frame to the
//! renderer. `setup_metal` is what puts the layer under it in the first place.

use super::*;

/// Keep the same tab after removals before it. If it closed, prefer the next
/// surviving tab, falling back to the previous one at the end of the strip.
fn active_tab_after_removals(active: usize, removed: &[usize], remaining: usize) -> usize {
    let removed_before = removed.iter().filter(|&&idx| idx < active).count();
    active.saturating_sub(removed_before).min(remaining.saturating_sub(1))
}

/// Rendering a background window does not mean its selected pane was read.
/// Keep the completion flag itself sticky for IPC wait-for-completion.
fn pane_attention(term: &crate::terminal::TerminalState, seen: bool) -> (bool, bool) {
    use std::sync::atomic::Ordering::Relaxed;
    if seen {
        let had_bell = term.bell.swap(false, Relaxed);
        let had_completion = term.unread_completion();
        term.ack_completion();
        if had_bell || had_completion {
            term.dirty.store(true, Relaxed);
        }
    }
    (term.unread_completion(), term.bell.load(Relaxed))
}

/// How many panes sit entirely off-screen, left and right of the visible strip.
///
/// Fed `(x, width, minimized)` per pane, in the tab's own pixel space. Minimized
/// panes are skipped rather than counted: they are zero-sized by design, not
/// pushed out of sight by horizontal scroll, and the status bar's `‹N` / `N›`
/// hints are about what scrolling would bring back.
fn hidden_pane_counts(
    panes: impl Iterator<Item = (f32, f32, bool)>,
    screen_width: f32,
) -> (usize, usize) {
    let mut left = 0usize;
    let mut right = 0usize;
    for (x, width, minimized) in panes {
        if minimized {
            continue;
        }
        if x + width <= 0.0 {
            left += 1;
        } else if x >= screen_width {
            right += 1;
        }
    }
    (left, right)
}

/// The vertical line the boundary flash paints when a navigation ran into the
/// edge of the layout: the focused pane's right or left side, full height.
fn boundary_flash_line(vp: &PaneViewport, is_right: bool) -> (f32, f32, f32, f32, bool) {
    let edge_x = if is_right { vp.x + vp.width } else { vp.x };
    (edge_x, vp.y, vp.y + vp.height, 1.0, is_right)
}

/// The loading bar's `(ready, total)`, or `None` once the restore is over.
///
/// A tab still waiting its turn holds the bar up even when every pane already
/// spawned is ready, which is what keeps it from flashing away and back on a
/// session restored in batches.
fn loading_progress(ready: u32, fixed_total: u32, deferred_remaining: u32) -> Option<(u32, u32)> {
    (ready < fixed_total || deferred_remaining > 0).then_some((ready, fixed_total))
}

/// Everything the frame's panes contribute to `render_panes`, gathered in one
/// pass over the active tab so the borrow on the tab list is taken once.
struct FramePanes {
    pane_data: Vec<crate::renderer::PaneRenderData>,
    pty_ptr: Option<*const crate::terminal::pty::Pty>,
    focus_reporting: bool,
    tab_titles: Vec<(String, bool, Option<usize>, bool, bool, bool, bool)>,
    panes_vp: PaneViewport,
    screen_width: f32,
    total_columns: usize,
    focused_column: usize,
    active_tab: usize,
    total_tabs: usize,
    active_tab_name: String,
    working_claudes: usize,
    unread_panes: usize,
}

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

        self.sample_focused_pane();

        let renderer = match ivars.renderer.get() {
            Some(r) => r.clone(),
            None => return true, // not yet initialized
        };
        let layer = match ivars.metal_layer.get() {
            Some(l) => l.clone(),
            None => return true,
        };

        self.inject_pending_commands();

        self.restore_deferred_tabs();

        self.extend_drag_selection();

        self.fire_due_pty_restores();

        // --- Debounced robust repaint after a resize burst settles ---
        self.fire_resize_settle_repaints();

        // --- Post-restore hole check: repair clear+partial-repaint frames ---
        self.fire_post_restore_band_checks();

        self.poll_git_branches();

        if !self.reap_exited_panes() {
            return false;
        }

        // Build pane render list from active tab only
        let active_idx = ivars.active_tab.get();
        let split_min_w = ivars.config.get()
            .map(|c| c.splits.min_width)
            .unwrap_or(300.0)
            * ivars.last_scale.get().max(1.0) as f32;
        let FramePanes {
            pane_data,
            pty_ptr,
            focus_reporting,
            tab_titles,
            panes_vp: active_panes_vp,
            screen_width,
            total_columns,
            focused_column,
            active_tab,
            total_tabs,
            active_tab_name,
            working_claudes,
            unread_panes,
        } = match self.collect_frame_panes(&renderer, &layer, active_idx, split_min_w) {
            Some(f) => f,
            None => return false,
        };

        self.report_app_focus(focus_reporting, pty_ptr);

        self.sync_window_title(&pane_data);

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

        let minimized_counts = self.minimized_counts(active_idx);

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
        let (hidden_left, hidden_right) = hidden_pane_counts(
            pane_data.iter().map(|p| (p.viewport.x, p.viewport.width, p.minimized)),
            screen_width,
        );
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
                    let is_right = flash.edge == NavDirection::Right;
                    r.boundary_flash = Some(boundary_flash_line(&focused.viewport, is_right));
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
                r.loading_progress = loading_progress(ready, fixed_total, deferred_remaining);
                if r.loading_progress.is_none() {
                    // Clear so we don't keep checking
                    ivars.loading_total_panes.set(0);
                }
            }
        }

        r.render_panes(&layer, &pane_data, &separators, &tab_titles, filter_data.as_ref(), left_inset, hidden_left, hidden_right, focused_column, total_columns, active_tab, total_tabs, &active_tab_name, working_claudes, unread_panes, minimized_counts, show_help, show_mem_report, rp_data.as_ref(), stw_data.as_ref(), sp_data.as_ref(), ps_data.as_ref(), help_hint_remaining, keys_config);
        true
    }

    /// Hand every pane the command it was restored with, once its shell is up.
    fn inject_pending_commands(&self) {
        let ivars = self.ivars();
        {
            let tabs = ivars.tabs.borrow();
            for tab in tabs.iter() {
                tab.for_each_pane(&mut |pane| {
                    pane.inject_pending_command();
                });
            }
        }
    }

    /// Spawn the next batch of tabs a restored session still owes, keeping the
    /// number of shells starting at once bounded.
    fn restore_deferred_tabs(&self) {
        let ivars = self.ivars();
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
    }

    /// Keep a selection growing while the pointer is held past a pane's edge.
    fn extend_drag_selection(&self) {
        let ivars = self.ivars();
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
    }

    /// Re-send the winsize to the panes whose `Cmd+R` nudge has come due.
    fn fire_due_pty_restores(&self) {
        let ivars = self.ivars();
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
    }

    /// Refresh the git branch shown in every pane's status bar, on its own
    /// slower cadence than the frame.
    fn poll_git_branches(&self) {
        let ivars = self.ivars();
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
    }

    /// Once-a-frame sampling of the pane under the eye: it feeds the visit
    /// history, and marks what a Cmd+J tier has to stop offering because it
    /// has now been looked at.
    fn sample_focused_pane(&self) {
        let ivars = self.ivars();
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
    }

    /// Remove the panes whose shell has exited, and the tabs they empty.
    ///
    /// Returns false when the window has nothing left to show — the caller
    /// then ends the frame and lets the window close.
    fn reap_exited_panes(&self) -> bool {
        let ivars = self.ivars();
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
                let mut tabs = ivars.tabs.borrow_mut();
                if tabs.is_empty() {
                    drop(tabs);
                    return false;
                }
                let active = active_tab_after_removals(
                    ivars.active_tab.get(), &tabs_to_remove, tabs.len(),
                );
                ivars.active_tab.set(active);
                let tab = &mut tabs[active];
                let screen_w = self.drawable_viewport().width;
                tab.clamp_scroll(screen_w, self.min_split_width_px());
                self.scroll_to_reveal_pane(tab, tab.focused_pane, screen_w);
                tab.mark_all_dirty();
                drop(tabs);
                self.resize_all_panes();
            }
        true
    }

    /// Walk the active tab once and collect what the frame needs from its
    /// panes. `None` means the window has no tab left to draw.
    fn collect_frame_panes(
        &self,
        renderer: &Arc<parking_lot::RwLock<Renderer>>,
        layer: &CAMetalLayer,
        active_idx: usize,
        split_min_w: f32,
    ) -> Option<FramePanes> {
        let ivars = self.ivars();
        let window_focused = self.window().is_some_and(|w| w.isKeyWindow())
            && NSApplication::sharedApplication(MainThreadMarker::from(self)).isActive();
                let mut tabs = ivars.tabs.borrow_mut();
                if tabs.is_empty() {
                    return None;
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
                    let (completed, has_bell) = pane_attention(&term, is_focused && window_focused);
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
        Some(FramePanes {
            pane_data,
            pty_ptr,
            focus_reporting,
            tab_titles,
            panes_vp,
            screen_width,
            total_columns,
            focused_column,
            active_tab: active_tab_1based,
            total_tabs,
            active_tab_name,
            working_claudes,
            unread_panes,
        })
    }

    /// Tell the focused pane that the app gained or lost focus, when it asked
    /// to hear about it (DEC mode 1004). Only the transition is reported.
    fn report_app_focus(&self, focus_reporting: bool, pty_ptr: Option<*const crate::terminal::pty::Pty>) {
        let ivars = self.ivars();
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
    }

    /// Mirror the focused pane's OSC 0/2 title into the window title bar,
    /// only when it actually changed.
    fn sync_window_title(&self, pane_data: &[crate::renderer::PaneRenderData]) {
        let ivars = self.ivars();
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
    }

    /// Minimized panes, as the global status bar shows them: how many in the
    /// active tab of this window, and how many across every window.
    fn minimized_counts(&self, active_idx: usize) -> (usize, usize) {
        let ivars = self.ivars();
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
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn closing_background_tabs_preserves_the_selected_tab() {
        // A/B/C, B selected: removing A leaves B selected at index 0.
        assert_eq!(active_tab_after_removals(1, &[0], 2), 0);
        // Removal after the selection must not move it.
        assert_eq!(active_tab_after_removals(1, &[2], 2), 1);
        // Multiple shells can exit in the same frame, on either side.
        assert_eq!(active_tab_after_removals(3, &[0, 2, 4], 3), 1);
    }

    #[test]
    fn closing_the_selected_tab_prefers_next_then_previous() {
        assert_eq!(active_tab_after_removals(1, &[1], 2), 1);
        assert_eq!(active_tab_after_removals(2, &[2], 2), 1);
        assert_eq!(active_tab_after_removals(2, &[0, 2, 3], 1), 0);
    }

    #[test]
    fn background_frames_preserve_attention_until_the_pane_is_seen() {
        use std::sync::atomic::Ordering::Relaxed;
        let config = Config::default();
        let pane = Pane::placeholder(80, 24, &config).unwrap();
        let term = pane.terminal.read();
        term.bell.store(true, Relaxed);
        term.command_completed.store(true, Relaxed);
        term.dirty.store(false, Relaxed);

        for _ in 0..3 {
            assert_eq!(pane_attention(&term, false), (true, true));
        }
        assert_eq!(pane_attention(&term, true), (false, false));
        assert!(term.command_completed.load(Relaxed), "IPC completion must stay sticky");
        assert!(term.dirty.load(Relaxed), "the indicators must be repainted when read");
        assert_eq!(pane_attention(&term, false), (false, false));

        // A new command completion must become unread again.
        term.completion_seen.store(false, Relaxed);
        assert_eq!(pane_attention(&term, false), (true, false));
    }

    #[test]
    fn inactive_single_pane_tab_reports_completion_until_acknowledged() {
        use std::sync::atomic::Ordering::Relaxed;
        let mut tab = Tab::placeholder(&Config::default()).unwrap();
        assert!(!tab.check_completion());
        tab.first_pane().terminal.read().command_completed.store(true, Relaxed);
        assert!(tab.check_completion());
        tab.first_pane().terminal.read().ack_completion();
        assert!(!tab.check_completion());
        assert!(tab.first_pane().terminal.read().command_completed.load(Relaxed));
    }

    #[test]
    fn hidden_pane_counts_splits_the_panes_that_scrolled_out_of_sight() {
        // A 1000pt screen: one pane wholly left of it, one wholly right, two
        // visible (one of them straddling the right edge, so still visible).
        let panes = [
            (-600.0, 300.0, false),
            (0.0, 500.0, false),
            (900.0, 300.0, false),
            (1000.0, 300.0, false),
        ];
        assert_eq!(hidden_pane_counts(panes.into_iter(), 1000.0), (1, 1));
    }

    #[test]
    fn hidden_pane_counts_ignores_minimized_panes() {
        // Zero-sized by design and sitting at the far left: not "scrolled away".
        let panes = [(-600.0, 0.0, true), (-600.0, 300.0, false)];
        assert_eq!(hidden_pane_counts(panes.into_iter(), 1000.0), (1, 0));
    }

    #[test]
    fn hidden_pane_counts_treats_a_pane_touching_the_edge_as_hidden() {
        // Exactly flush with either edge counts as out: its last column is not
        // drawn, so scrolling is what brings it back.
        assert_eq!(hidden_pane_counts([(-300.0, 300.0, false)].into_iter(), 1000.0), (1, 0));
        assert_eq!(hidden_pane_counts([(1000.0, 300.0, false)].into_iter(), 1000.0), (0, 1));
    }

    #[test]
    fn boundary_flash_lands_on_the_side_the_navigation_ran_into() {
        let vp = PaneViewport { x: 100.0, y: 20.0, width: 400.0, height: 300.0 };
        // Rightward: the pane's right edge, from its top to its bottom.
        assert_eq!(boundary_flash_line(&vp, true), (500.0, 20.0, 320.0, 1.0, true));
        // Leftward: the left edge, same span.
        assert_eq!(boundary_flash_line(&vp, false), (100.0, 20.0, 320.0, 1.0, false));
    }

    #[test]
    fn loading_progress_holds_until_every_pane_and_tab_has_landed() {
        // Still spawning.
        assert_eq!(loading_progress(3, 8, 0), Some((3, 8)));
        // Every spawned pane is ready, but a tab has yet to be restored: the
        // bar stays up rather than flashing away and back.
        assert_eq!(loading_progress(8, 8, 2), Some((8, 8)));
        // Nothing left on either side.
        assert_eq!(loading_progress(8, 8, 0), None);
    }
}
