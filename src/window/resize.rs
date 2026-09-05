//! Resizing: the tab's virtual width when the panes no longer fit the screen,
//! the reflow that follows a window or display change, and the delayed repaints
//! that clean up what a resize burst leaves behind.

use super::*;

/// One debounce step for the per-pane resize-settle countdowns: decrement
/// every entry and return the pane ids whose countdown reached 0 (removing
/// them from the map). A fresh resize re-arms a pane by re-inserting its
/// countdown, so the burst only fires once, after it stops.
fn step_resize_settle(map: &mut std::collections::HashMap<PaneId, u32>) -> Vec<PaneId> {
    let mut fire: Vec<PaneId> = Vec::new();
    map.retain(|&pane_id, frames| {
        *frames = frames.saturating_sub(1);
        if *frames == 0 {
            fire.push(pane_id);
            false
        } else {
            true
        }
    });
    fire
}

/// Whether the post-resize settle repaint (winsize nudge) can be skipped.
/// Skip only when the app already repainted broadly AND the grid shows no
/// hole — both conditions observable, so this is strictly safer than always
/// nudging (the round-4 behavior) which can itself re-trigger the hole.
fn should_skip_settle_nudge(row_coverage: f32, has_interior_band: bool) -> bool {
    !has_interior_band && row_coverage >= SETTLE_SKIP_COVERAGE
}

impl KovaView {
    /// Mode 2: adjust the virtual width override of the active tab (all panes scale proportionally).
    pub(super) fn adjust_virtual_width(&self, dir: f32) {
        let screen_w = self.drawable_viewport().width;
        let step = (0.33 * screen_w).max(200.0 * self.backing_scale());
        let min_w = self.min_split_width_px();
        let mut tabs = self.ivars().tabs.borrow_mut();
        let idx = self.ivars().active_tab.get();
        if let Some(tab) = tabs.get_mut(idx) {
            let current_vw = tab.virtual_width(screen_w, min_w);
            let new_vw = (current_vw + dir * step).max(screen_w);
            tab.virtual_width_override = if new_vw > screen_w { new_vw } else { 0.0 };
            self.enforce_max_pane_width(tab, screen_w, min_w);
            tab.clamp_scroll(screen_w, min_w);
            self.scroll_to_reveal_pane(tab, tab.focused_pane, screen_w);
            self.set_resize_feedback("Virtual", tab, screen_w, min_w);
        }
        drop(tabs);
        self.resize_all_panes();
    }

    /// Mode 1 post-validation: reduce virtual_width so no pane exceeds screen_width.
    /// Does NOT touch ratios (the user just set them).
    pub(super) fn cap_virtual_width(&self, tab: &mut Tab, screen_w: f32, min_w: f32) {
        let vw = tab.virtual_width(screen_w, min_w);
        if vw <= screen_w { return; }
        let max_frac = tab.max_leaf_width_fraction();
        if max_frac <= 0.0 { return; }
        let max_vw = screen_w / max_frac;
        if vw > max_vw {
            tab.virtual_width_override = if max_vw > screen_w { max_vw } else { 0.0 };
            tab.clamp_scroll(screen_w, min_w);
        }
    }

    /// Modes 2 & 3 post-validation: adjust ratios of oversized panes first,
    /// then reduce virtual_width as last resort.
    pub(super) fn enforce_max_pane_width(&self, tab: &mut Tab, screen_w: f32, min_w: f32) {
        let vw = tab.virtual_width(screen_w, min_w);
        if vw <= screen_w { return; }
        // Step 1: adjust ratios to cap oversized panes
        tab.clamp_pane_widths(vw, screen_w);
        // Step 2: if still oversized (total too large), reduce virtual_width
        let max_frac = tab.max_leaf_width_fraction();
        if max_frac > 0.0 {
            let max_vw = screen_w / max_frac;
            let current_vw = tab.virtual_width(screen_w, min_w);
            if current_vw > max_vw {
                tab.virtual_width_override = if max_vw > screen_w { max_vw } else { 0.0 };
            }
        }
        tab.clamp_scroll(screen_w, min_w);
    }

    /// Store resize feedback info to display in the global status bar for ~2 seconds.
    pub(super) fn set_resize_feedback(&self, mode: &str, tab: &Tab, screen_w: f32, min_w: f32) {
        let fps = self.ivars().config.get().map(|c| c.terminal.fps).unwrap_or(60) as u32;
        let resize_mode = match mode {
            "Virtual" => ResizeMode::Virtual,
            "Right Edge" => ResizeMode::Edge,
            _ => ResizeMode::Ratio,
        };
        self.ivars().resize_feedback.set(Some(ResizeFeedback {
            mode: resize_mode,
            screen_w: screen_w as u32,
            virtual_w: tab.virtual_width(screen_w, min_w) as u32,
            remaining_frames: fps * 2,
        }));
    }

    /// Resize all panes in the active tab to match their current viewports.
    pub(super) fn resize_all_panes(&self) {
        let renderer = match self.ivars().renderer.get() {
            Some(r) => r,
            None => return,
        };
        let renderer_r = renderer.read();
        let (cell_w, cell_h) = renderer_r.cell_size();
        let status_bar = renderer_r.status_bar_enabled();
        drop(renderer_r);
        let h_pad = self.h_padding();

        // Drop expired resize histories (also clears entries of closed panes)
        self.ivars().recent_resizes.borrow_mut().retain(|_, h| {
            h.iter().any(|&(_, t)| t.elapsed().as_millis() < 500)
        });

        let panes_vp = self.panes_viewport();
        let tabs = self.ivars().tabs.borrow();
        let idx = self.ivars().active_tab.get();
        if let Some(tab) = tabs.get(idx) {
            tab.cell_h.set(cell_h);
            tab.for_each_pane_with_viewport(panes_vp, &mut |pane, vp| {
                // Skip PTY resize for minimized panes (keep old dimensions)
                if pane.minimized {
                    return;
                }
                let cols = ((vp.width - 2.0 * h_pad) / cell_w).floor().max(1.0) as u16;
                let usable_h = if status_bar { vp.height - cell_h } else { vp.height };
                let rows = (usable_h / cell_h).floor().max(1.0) as u16;
                let mut term = pane.terminal.write();
                if cols != term.cols || rows != term.rows {
                    let old = (term.cols, term.rows);
                    term.resize(cols, rows);
                    drop(term);
                    pane.pty.resize(cols, rows);
                    // A real resize opens a fresh band-repair budget (see
                    // MAX_BAND_REPAIRS) and coverage window (reset by
                    // term.resize above).
                    self.ivars().band_repair_attempts.borrow_mut().remove(&pane.id);

                    // Round-trip detection: returning within 500ms to ANY
                    // recently-seen size means the child may coalesce the
                    // SIGWINCHs into one no-op and skip its repaint while our
                    // reflow round-trip lost information. Nudge it.
                    let now = std::time::Instant::now();
                    let mut recent = self.ivars().recent_resizes.borrow_mut();
                    let history = recent.entry(pane.id).or_default();
                    history.retain(|&(_, t)| now.duration_since(t).as_millis() < 500);
                    let round_trip = history.iter().any(|&(sz, _)| sz == (cols, rows));
                    if round_trip {
                        history.clear();
                        drop(recent);
                        pane.pty.resize(cols, if rows > 1 { rows - 1 } else { rows + 1 });
                        let mut restores = self.ivars().pty_restore.borrow_mut();
                        restores.retain(|r| r.pane_id != pane.id);
                        restores.push(PtyRestore { pane_id: pane.id, remaining_frames: 3 });
                    } else {
                        history.push((old, now));
                        if history.len() > 8 {
                            history.remove(0);
                        }
                    }

                    // Debounce a single robust repaint to run once the resize
                    // burst settles: rapid CTRL+OPTION+arrow resizes produce a
                    // storm of SIGWINCHs the child coalesces, and a differential
                    // TUI (Claude Code) can then skip rows and leave a stale
                    // blank band. Re-arming on every resize collapses the burst
                    // into one Cmd+R-style repaint (soft_reset + nudge) at the
                    // end. See `resize_settle` and `fire_resize_settle_repaints`.
                    let fps = self.ivars().config.get().map(|c| c.terminal.fps).unwrap_or(60) as u32;
                    let settle = (fps / 6).max(4); // ~150ms after the last resize
                    self.ivars().resize_settle.borrow_mut().insert(pane.id, settle);
                }
            });
        }
    }

    /// Tick the per-pane resize-settle debounce. When a pane's countdown
    /// reaches 0 (no resize for ~150ms), run the same robust repaint as Cmd+R
    /// so the foreground program fully repaints against a clean grid. Returns
    /// nothing; safe to call every frame.
    pub(super) fn fire_resize_settle_repaints(&self) {
        let fire = {
            let mut settle = self.ivars().resize_settle.borrow_mut();
            step_resize_settle(&mut settle)
        };
        for pane_id in fire {
            self.repaint_pane_settle(pane_id);
        }
    }

    /// Tick the post-restore hole checks. ~0.5s after each winsize restore,
    /// scan the pane's grid: if the app answered the restore with a
    /// clear-screen + partial repaint (interior blank band in alt-screen),
    /// force another robust repaint — bounded by MAX_BAND_REPAIRS per pane
    /// between real resizes so a misbehaving app can't loop us.
    pub(super) fn fire_post_restore_band_checks(&self) {
        let due: Vec<PaneId> = {
            let mut checks = self.ivars().post_restore_checks.borrow_mut();
            let mut due = Vec::new();
            for c in checks.iter_mut() {
                c.remaining_frames = c.remaining_frames.saturating_sub(1);
                if c.remaining_frames == 0 {
                    due.push(c.pane_id);
                }
            }
            checks.retain(|c| c.remaining_frames > 0);
            due
        };
        for pane_id in due {
            // None = pane gone; Some(band) = pane found, band presence known.
            let holed: Option<Option<(usize, usize)>> = {
                let tabs = self.ivars().tabs.borrow();
                let mut found = None;
                for tab in tabs.iter() {
                    if let Some(p) = tab.pane(pane_id) {
                        let t = p.terminal.read();
                        let band = if !p.minimized && t.in_alt_screen {
                            t.interior_blank_band(BAND_MIN_ROWS)
                        } else {
                            None
                        };
                        found = Some(band);
                        break;
                    }
                }
                found
            };
            match holed {
                Some(Some((start, end))) => {
                    let mut attempts = self.ivars().band_repair_attempts.borrow_mut();
                    let n = attempts.entry(pane_id).or_insert(0);
                    if *n < MAX_BAND_REPAIRS {
                        *n += 1;
                        let attempt = *n;
                        drop(attempts);
                        log::info!(
                            "post-restore check: pane {} has a {}-row blank band (rows {}..{}) — forcing repaint (attempt {}/{})",
                            pane_id, end - start + 1, start, end, attempt, MAX_BAND_REPAIRS
                        );
                        self.repaint_pane_settle(pane_id);
                    } else {
                        log::warn!(
                            "post-restore check: pane {} still holed after {} repaints — giving up until next resize",
                            pane_id, MAX_BAND_REPAIRS
                        );
                    }
                }
                _ => {
                    // Clean grid or pane closed: reset the repair budget.
                    self.ivars().band_repair_attempts.borrow_mut().remove(&pane_id);
                }
            }
        }
    }

    /// Robust repaint for a specific pane (by id), mirroring `do_repaint_pane`
    /// but without the border flash — used by the automatic post-resize settle.
    pub(super) fn repaint_pane_settle(&self, pane_id: PaneId) {
        let pane = {
            let tabs = self.ivars().tabs.borrow();
            let mut found: Option<*const Pane> = None;
            for tab in tabs.iter() {
                if let Some(p) = tab.pane(pane_id) {
                    found = Some(p as *const Pane);
                    break;
                }
            }
            // SAFETY: same invariant as `focused_pane` — Tab mutations happen
            // only in the render tick, and this runs from the render tick.
            match found {
                Some(ptr) => unsafe { &*ptr },
                None => return,
            }
        };
        if pane.minimized {
            return;
        }
        let (cols, rows) = {
            let t = pane.terminal.read();
            if should_skip_settle_nudge(t.row_coverage(), t.interior_blank_band(BAND_MIN_ROWS).is_some()) {
                log::debug!(
                    "settle repaint skipped for pane {}: app repainted {:.0}% of rows since resize, no hole",
                    pane_id,
                    t.row_coverage() * 100.0
                );
                return;
            }
            (t.cols, t.rows)
        };
        {
            let mut t = pane.terminal.write();
            t.soft_reset();
            // Fresh coverage window: measure what the app repaints in
            // response to this nudge, not what came before it.
            t.reset_rows_touched();
        }
        let nudged = if rows > 1 { rows - 1 } else { rows + 1 };
        pane.pty.resize(cols, nudged);
        let mut restores = self.ivars().pty_restore.borrow_mut();
        restores.retain(|r| r.pane_id != pane_id);
        restores.push(PtyRestore { pane_id, remaining_frames: 3 });
    }

    pub(super) fn handle_resize(&self) {
        let Some(layer) = self.ivars().metal_layer.get() else { return };
        let Some(renderer) = self.ivars().renderer.get() else { return };

        let scale = self.window().map_or(2.0, |w| w.backingScaleFactor());
        let frame = self.frame();
        layer.setContentsScale(scale);
        layer.setDrawableSize(CGSize {
            width: frame.size.width * scale,
            height: frame.size.height * scale,
        });

        // Rebuild glyph atlas if scale changed (e.g. moved to different display)
        // and convert every tab's pixel geometry to the new scale, so a pane keeps
        // the same APPARENT width across displays (the pixel count changes, the
        // physical size does not). Column weights are relative and need nothing.
        let old_scale = self.ivars().last_scale.get();
        if (scale - old_scale).abs() > 0.01 {
            log::debug!("Scale changed: {} -> {}", old_scale, scale);
            self.ivars().last_scale.set(scale);
            renderer.write().rebuild_atlas(scale);
            let mut tabs = self.ivars().tabs.borrow_mut();
            for tab in tabs.iter_mut() {
                tab.adopt_geometry_scale(scale as f32);
            }
        }

        // Moved to another screen: a tab that never had a manual width takes its
        // total from the screen it is on, so a narrower one shrinks every pane —
        // the "my panes came back small after unplugging the monitor" half of the
        // symptom, which the scale conversion above does nothing about. Pin the
        // width the tab was laid out at and let it scroll instead. Done in
        // logical points, and keyed on the screen rather than the scale: two
        // displays can share a scale, and AppKit can move the window on one event
        // and change the backing scale on the next.
        let screen_logical_w = self.screen_logical_width();
        let old_screen_logical_w = self.ivars().last_screen_w.get();
        if screen_logical_w > 0.0 {
            self.ivars().last_screen_w.set(screen_logical_w);
        }
        if old_screen_logical_w > 0.0
            && screen_logical_w > 0.0
            && (screen_logical_w - old_screen_logical_w).abs() > 1.0
        {
            let min_w = self.ivars().config.get()
                .map(|c| c.splits.min_width)
                .unwrap_or(300.0);
            let mut tabs = self.ivars().tabs.borrow_mut();
            for tab in tabs.iter_mut() {
                if tab.virtual_width_override > 0.0 {
                    continue;
                }
                let old_vw = tab.virtual_width(old_screen_logical_w, min_w);
                if let Some(pinned) =
                    crate::pane::pinned_virtual_width(old_vw, screen_logical_w, scale as f32)
                {
                    tab.virtual_width_override = pinned;
                }
            }
        }

        // Only clamp the scroll offset. Pane widths are deliberately NOT capped
        // to the new screen: on a narrower display the panes keep their size and
        // the tab scrolls horizontally instead. Capping them here mutated
        // column_weights and dropped virtual_width_override for good, so coming
        // back to the wide display never restored the layout.
        let screen_w = self.drawable_viewport().width;
        let min_w = self.min_split_width_px();
        {
            let mut tabs = self.ivars().tabs.borrow_mut();
            for tab in tabs.iter_mut() {
                tab.clamp_scroll(screen_w, min_w);
            }
        }

        self.resize_all_panes();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn resize_settle_fires_once_when_burst_ends() {
        let mut m: HashMap<PaneId, u32> = HashMap::new();
        m.insert(7, 3);
        assert!(step_resize_settle(&mut m).is_empty()); // 3 -> 2
        assert!(step_resize_settle(&mut m).is_empty()); // 2 -> 1
        assert_eq!(step_resize_settle(&mut m), vec![7]); // 1 -> 0, fires
        assert!(m.is_empty(), "fired pane is removed");
        assert!(step_resize_settle(&mut m).is_empty(), "no re-fire");
    }

    #[test]
    fn resize_settle_rearms_and_defers_fire() {
        let mut m: HashMap<PaneId, u32> = HashMap::new();
        m.insert(1, 2);
        assert!(step_resize_settle(&mut m).is_empty()); // 2 -> 1
        // A new resize re-arms the same pane before it could fire.
        m.insert(1, 2);
        assert!(step_resize_settle(&mut m).is_empty()); // 2 -> 1 (not 0)
        assert_eq!(step_resize_settle(&mut m), vec![1]); // 1 -> 0, fires now
    }

    #[test]
    fn resize_settle_handles_multiple_panes_independently() {
        let mut m: HashMap<PaneId, u32> = HashMap::new();
        m.insert(1, 1);
        m.insert(2, 3);
        assert_eq!(step_resize_settle(&mut m), vec![1]); // pane 1 fires; pane 2: 3 -> 2
        assert!(m.contains_key(&2) && !m.contains_key(&1));
        assert!(step_resize_settle(&mut m).is_empty()); // pane 2: 2 -> 1
        assert_eq!(step_resize_settle(&mut m), vec![2]); // pane 2: 1 -> 0, fires
    }

    #[test]
    fn settle_nudge_skipped_only_on_broad_repaint_without_hole() {
        // App fully repainted after the real resize: nudging again is the
        // exact trigger of the clear+partial-repaint hole — skip.
        assert!(should_skip_settle_nudge(1.0, false));
        assert!(should_skip_settle_nudge(SETTLE_SKIP_COVERAGE, false));
        // App barely repainted (possibly coalesced the SIGWINCH): nudge.
        assert!(!should_skip_settle_nudge(0.1, false));
        // A hole is visible: always nudge, whatever the coverage says
        // (scroll frames can accumulate coverage while the band persists).
        assert!(!should_skip_settle_nudge(1.0, true));
        assert!(!should_skip_settle_nudge(0.1, true));
    }
}
