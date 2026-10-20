//! Resizing: the tab's virtual width when the panes no longer fit the screen,
//! the reflow that follows a window or display change, and the delayed repaints
//! that clean up what a resize burst leaves behind.

use super::*;

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
        self.ivars().resize_repair.borrow_mut().prune_history(std::time::Instant::now());

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
                    // A real resize opens a fresh band-repair budget, nudges
                    // the child at once when the pane returns within 500ms to
                    // a recently-seen size (the SIGWINCHs may coalesce into a
                    // no-op while our reflow round-trip lost information), and
                    // re-arms a single robust repaint (soft_reset + nudge) for
                    // when the resize burst settles: rapid CTRL+OPTION+arrow
                    // resizes produce a storm of SIGWINCHs the child
                    // coalesces, and a differential TUI (Claude Code) can then
                    // skip rows and leave a stale blank band.
                    let fps = self.ivars().config.get().map(|c| c.terminal.fps).unwrap_or(60) as u32;
                    self.ivars().resize_repair.borrow_mut().on_resized(
                        pane,
                        old,
                        (cols, rows),
                        std::time::Instant::now(),
                        crate::resize_repair::settle_frames(fps),
                    );
                }
            });
        }
    }

    /// One frame of the resize repairs (`crate::resize_repair`): due winsize
    /// restores, then settle repaints, then post-restore hole checks, over
    /// the panes of every tab. Safe to call every frame.
    pub(super) fn tick_resize_repairs(&self) {
        let tabs = self.ivars().tabs.borrow();
        let mut lookup = |pane_id: PaneId, f: &mut dyn FnMut(&crate::surface::Surface, bool)| {
            if let Some(p) = tabs.iter().find_map(|tab| tab.pane(pane_id)) {
                f(&p.surface, p.minimized);
            }
        };
        self.ivars().resize_repair.borrow_mut().tick(1, &mut lookup);
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
