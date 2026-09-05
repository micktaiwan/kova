//! Where a pixel lands: the viewport a tab's panes are drawn into, the bars
//! that eat into it, and the conversions from a mouse position to a pane, a
//! cell, a separator or a tab.

use super::*;

impl KovaView {
    /// Convert pixel coords to (visible_row, col) within a pane viewport.
    pub(super) fn pixel_to_visible_row_col(&self, px: f32, py: f32, pane: &Pane, vp: &PaneViewport) -> Option<(usize, u16)> {
        let renderer = self.ivars().renderer.get()?;
        let renderer_r = renderer.read();
        let (cell_w, cell_h) = renderer_r.cell_size();
        drop(renderer_r);

        let rel_x = px - vp.x - self.h_padding();
        let rel_y = py - vp.y;

        let term = pane.terminal.read();
        let y_offset = term.y_offset_rows();
        let col = (rel_x / cell_w).floor() as i32;
        let visible_row = (rel_y / cell_h).floor() as i32 - y_offset as i32;

        if visible_row < 0 || col < 0 || visible_row >= term.rows as i32 {
            return None;
        }
        Some((visible_row as usize, (col as u16).min(term.cols.saturating_sub(1))))
    }

    /// Viewport for panes (below tab bar), reading scroll state from the active tab.
    /// WARNING: borrows tabs — do NOT call while tabs is already borrowed.
    pub(super) fn panes_viewport(&self) -> PaneViewport {
        let tabs = self.ivars().tabs.borrow();
        let idx = self.ivars().active_tab.get();
        if let Some(tab) = tabs.get(idx) {
            let screen_w = self.drawable_viewport().width;
            let vw = tab.virtual_width(screen_w, self.min_split_width_px());
            self.panes_viewport_inner(tab.scroll_offset_x, vw)
        } else {
            self.panes_viewport_inner(0.0, self.drawable_viewport().width)
        }
    }

    /// Viewport for panes using a tab reference (no extra borrow on tabs).
    pub(super) fn panes_viewport_for_tab(&self, tab: &crate::pane::Tab) -> PaneViewport {
        let screen_w = self.drawable_viewport().width;
        let vw = tab.virtual_width(screen_w, self.min_split_width_px());
        self.panes_viewport_inner(tab.scroll_offset_x, vw)
    }

    /// Scroll the tab so that the given pane is visible on screen.
    pub(super) fn scroll_to_reveal_pane(&self, tab: &mut Tab, pane_id: PaneId, screen_w: f32) {
        let panes_vp = self.panes_viewport_for_tab(tab);
        if let Some(vp) = tab.viewport_for_pane(pane_id, panes_vp) {
            tab.scroll_to_reveal(&vp, screen_w);
        }
    }

    pub(super) fn panes_viewport_inner(&self, scroll_offset_x: f32, virtual_width: f32) -> PaneViewport {
        let full = self.drawable_viewport();
        let tab_bar_h = self.tab_bar_height();
        let global_bar_h = self.global_bar_height();
        PaneViewport {
            x: -scroll_offset_x,
            y: full.y + tab_bar_h,
            width: virtual_width,
            height: full.height - tab_bar_h - global_bar_h,
        }
    }

    /// Global status bar height in pixels (1x cell height).
    pub(super) fn global_bar_height(&self) -> f32 {
        let renderer = match self.ivars().renderer.get() {
            Some(r) => r,
            None => return 0.0,
        };
        let r = renderer.read();
        r.cell_size().1
    }

    /// Width in logical points of the screen this window is on, 0.0 if unknown.
    pub(super) fn screen_logical_width(&self) -> f32 {
        self.window()
            .and_then(|w| w.screen())
            .map(|s| s.frame().size.width as f32)
            .unwrap_or(0.0)
    }

    pub(super) fn backing_scale(&self) -> f32 {
        self.window().map_or(2.0, |w| w.backingScaleFactor()) as f32
    }

    /// Horizontal pane padding in pixels for the current display scale. The
    /// constant is in logical points; using it raw would make the column count
    /// depend on the display (see `notes/screen-switch-resize.md`).
    pub(super) fn h_padding(&self) -> f32 {
        crate::renderer::PANE_H_PADDING * self.backing_scale()
    }

    /// Compute scaled min_split_width in pixels.
    pub(super) fn min_split_width_px(&self) -> f32 {
        let min_w = self.ivars().config.get()
            .map(|c| c.splits.min_width)
            .unwrap_or(300.0);
        min_w * self.backing_scale()
    }

    pub(super) fn get_tab_bar_left_inset(&self) -> f32 {
        let v = self.ivars().tab_bar_left_inset.get();
        if v > 0.0 { v } else { 136.0 } // fallback 68pt * 2x
    }

    /// Tab bar height in pixels (2.0x cell height).
    pub(super) fn tab_bar_height(&self) -> f32 {
        let renderer = match self.ivars().renderer.get() {
            Some(r) => r,
            None => return 0.0,
        };
        let r = renderer.read();
        let (_, cell_h) = r.cell_size();
        (cell_h * 2.0).round()
    }

    /// Hit-test separators in the active tab's tree.
    pub(super) fn hit_test_separator(&self, px: f32, py: f32) -> Option<SeparatorDrag> {
        let tabs = self.ivars().tabs.borrow();
        let idx = self.ivars().active_tab.get();
        let tab = tabs.get(idx)?;
        let vp = self.panes_viewport_for_tab(tab);
        let mut seps = Vec::new();
        tab.collect_separator_info(vp, &mut seps);

        let scale = self.backing_scale();
        let tolerance = 4.0 * scale;

        // Separators are in screen space (viewport uses x: -scroll_offset_x)
        for sep in &seps {
            if sep.is_column_sep {
                if (px - sep.pos).abs() < tolerance && py >= sep.cross_start && py <= sep.cross_end {
                    return Some(SeparatorDrag {
                        origin_pixel: px,
                        parent_dim: sep.parent_dim,
                        column_sep_index: sep.column_sep_index,
                        col_index: sep.col_index,
                        row_sep_index: sep.row_sep_index,
                    });
                }
            } else {
                if (py - sep.pos).abs() < tolerance && px >= sep.cross_start && px <= sep.cross_end {
                    return Some(SeparatorDrag {
                        origin_pixel: py,
                        parent_dim: sep.parent_dim,
                        column_sep_index: sep.column_sep_index,
                        col_index: sep.col_index,
                        row_sep_index: sep.row_sep_index,
                    });
                }
            }
        }
        None
    }

    /// Hit-test the tab bar. Returns true if click was in the tab bar (and handled).
    pub(super) fn hit_test_tab_bar(&self, px: f32, py: f32, event: &NSEvent) -> bool {
        let tab_bar_h = self.tab_bar_height();
        if py > tab_bar_h {
            return false;
        }
        if let Some(idx) = self.tab_index_at_x(px) {
            self.do_switch_tab(idx);
            self.ivars().drag_tab.set(Some(DragTabState {
                tab_index: idx,
                start_x: px,
                current_x: px,
                dragging: false,
            }));
        } else if let Some(win) = self.window() {
            if event.clickCount() == 2 {
                // Double-click in titlebar (not on a tab) → zoom, like native titlebars
                win.zoom(None);
            } else {
                // Click in titlebar but not on a tab — initiate window drag
                win.performWindowDragWithEvent(event);
            }
        }
        true
    }

    /// Returns the tab index at the given x pixel position, or None if outside tabs.
    pub(super) fn tab_index_at_x(&self, px: f32) -> Option<usize> {
        let tabs = self.ivars().tabs.borrow();
        let tab_count = tabs.len();
        if tab_count == 0 {
            return None;
        }
        let full = self.drawable_viewport();
        let left_inset = self.get_tab_bar_left_inset();
        let renderer = self.ivars().renderer.get()?;
        let cell_w = renderer.read().cell_size().0;
        let max_tab_w = cell_w * 20.0;
        // Reserve right inset for version label / drag handle
        let version_label = format!("Kova v{}", env!("CARGO_PKG_VERSION"));
        let version_chars = version_label.chars().count() as f32;
        let right_inset = cell_w * (version_chars + 3.5);
        let available_w = full.width - left_inset - right_inset;
        let tab_width = (available_w / tab_count as f32).max(cell_w * 4.0).min(max_tab_w);
        for i in 0..tab_count {
            let x = left_inset + i as f32 * tab_width;
            if px >= x && px <= x + tab_width {
                return Some(i);
            }
        }
        None
    }

    /// Total drawable viewport in pixels.
    pub(super) fn drawable_viewport(&self) -> PaneViewport {
        let frame = self.frame();
        let scale = self.window().map_or(2.0, |w| w.backingScaleFactor());
        PaneViewport {
            x: 0.0,
            y: 0.0,
            width: (frame.size.width * scale) as f32,
            height: (frame.size.height * scale) as f32,
        }
    }

    /// Convert an NSEvent location to Metal pixel coordinates (origin top-left).
    pub(super) fn event_to_pixel(&self, event: &NSEvent) -> (f32, f32) {
        let location = event.locationInWindow();
        let local: CGPoint = unsafe { msg_send![self, convertPoint: location, fromView: std::ptr::null::<objc2::runtime::AnyObject>()] };
        let frame = self.frame();
        let scale = self.backing_scale();
        let pixel_x = local.x as f32 * scale;
        let pixel_y = (frame.size.height as f32 - local.y as f32) * scale;
        (pixel_x, pixel_y)
    }

    /// Hit-test: find which pane is under the mouse event (in active tab).
    pub(super) fn pane_at_event(&self, event: &NSEvent) -> Option<(&Pane, PaneViewport)> {
        let tabs = self.ivars().tabs.borrow();
        let idx = self.ivars().active_tab.get();
        let tab = tabs.get(idx)?;
        let (px, py) = self.event_to_pixel(event);
        // Viewport is already in screen space (x: -scroll_offset_x), so use px directly
        let (pane, vp) = tab.hit_test(px, py, self.panes_viewport_for_tab(tab))?;
        Some((unsafe { &*(pane as *const Pane) }, vp))
    }

    /// Convert an NSEvent to a grid position within the given pane/viewport.
    pub(super) fn pixel_to_grid_in(&self, event: &NSEvent, pane: &Pane, vp: &PaneViewport) -> Option<GridPos> {
        let renderer = self.ivars().renderer.get()?;
        let (pixel_x, pixel_y) = self.event_to_pixel(event);

        let renderer_r = renderer.read();
        let (cell_w, cell_h) = renderer_r.cell_size();
        drop(renderer_r);

        let rel_x = pixel_x - vp.x - self.h_padding();
        let rel_y = pixel_y - vp.y;

        let term = pane.terminal.read();
        let y_offset = term.y_offset_rows();
        let col = (rel_x / cell_w).floor() as i32;
        let visible_row = (rel_y / cell_h).floor() as i32 - y_offset as i32;

        if visible_row < 0 || col < 0 {
            return None;
        }
        let col = (col as u16).min(term.cols.saturating_sub(1));
        let visible_row = visible_row as usize;
        if visible_row >= term.rows as usize {
            return None;
        }

        let abs_line = (term.scrollback_len() as i64 - term.scroll_offset() as i64 + visible_row as i64) as usize;
        Some(GridPos { line: abs_line, col })
    }

    /// Convert pixel coords to 1-based (col, row) within a pane viewport.
    /// Returns None if the pixel is outside the grid area.
    pub(super) fn pixel_to_cell_in(&self, event: &NSEvent, pane: &Pane, vp: &PaneViewport) -> Option<(u16, u16)> {
        let renderer = self.ivars().renderer.get()?;
        let (pixel_x, pixel_y) = self.event_to_pixel(event);
        let renderer_r = renderer.read();
        let (cell_w, cell_h) = renderer_r.cell_size();
        drop(renderer_r);

        let rel_x = pixel_x - vp.x - self.h_padding();
        let rel_y = pixel_y - vp.y;

        let term = pane.terminal.read();
        let y_offset = term.y_offset_rows();
        let col = (rel_x / cell_w).floor() as i32;
        let row = (rel_y / cell_h).floor() as i32 - y_offset as i32;

        if row < 0 || col < 0 {
            return None;
        }
        let col = (col as u16).min(term.cols.saturating_sub(1));
        let row = (row as u16).min(term.rows.saturating_sub(1));
        // SGR uses 1-based coordinates
        Some((col + 1, row + 1))
    }

    /// Compute cols/rows for a pane viewport.
    pub(super) fn viewport_to_grid(&self, vp: &PaneViewport) -> (u16, u16) {
        let renderer = self.ivars().renderer.get().unwrap();
        let renderer_r = renderer.read();
        let (cell_w, cell_h) = renderer_r.cell_size();
        let status_bar = renderer_r.status_bar_enabled();
        drop(renderer_r);

        let cols = ((vp.width - 2.0 * self.h_padding()) / cell_w).floor().max(1.0) as u16;
        let usable_h = if status_bar {
            vp.height - cell_h
        } else {
            vp.height
        };
        let rows = (usable_h / cell_h).floor().max(1.0) as u16;
        (cols, rows)
    }
}
