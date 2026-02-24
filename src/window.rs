use block2::RcBlock;
use objc2::rc::Retained;
use objc2::{define_class, msg_send, DefinedClass, MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{NSApplication, NSBackingStoreType, NSEvent, NSEventModifierFlags, NSPasteboard, NSWindow, NSWindowButton, NSWindowStyleMask, NSWindowTitleVisibility};
use objc2_core_foundation::{CGPoint, CGRect, CGSize};
use objc2_foundation::{NSObjectProtocol, NSRunLoop, NSRunLoopCommonModes, NSString, NSTimer};
use objc2_metal::MTLCreateSystemDefaultDevice;
use objc2_quartz_core::CAMetalLayer;
use std::cell::{Cell, OnceCell, RefCell};
use std::ptr::NonNull;
use std::sync::Arc;

use crate::config::Config;
use crate::input;
use crate::pane::{NavDirection, Pane, SplitAxis, SplitDirection, SplitTree, Tab};
use crate::renderer::{FilterRenderData, PaneViewport, Renderer};
use crate::terminal::{FilterMatch, GridPos, Selection};

#[derive(Clone, Copy)]
struct SeparatorDrag {
    is_hsplit: bool,
    origin_pixel: f32,
    origin_ratio: f32,
    parent_dim: f32,
    node_ptr: usize,
}

#[derive(Clone, Copy)]
struct DragTabState {
    tab_index: usize,
    start_x: f32,
    current_x: f32,
    dragging: bool,
}

pub struct KovaViewIvars {
    renderer: OnceCell<Arc<parking_lot::RwLock<Renderer>>>,
    tabs: RefCell<Vec<Tab>>,
    active_tab: Cell<usize>,
    metal_layer: OnceCell<Retained<CAMetalLayer>>,
    last_scale: Cell<f64>,
    last_focused: Cell<bool>,
    config: OnceCell<Config>,
    drag_separator: Cell<Option<SeparatorDrag>>,
    filter: RefCell<Option<FilterState>>,
    rename_tab: RefCell<Option<RenameTabState>>,
    /// Left inset (pixels) for tab bar, cached from traffic light button positions.
    tab_bar_left_inset: Cell<f32>,
    /// Tab index targeted by right-click color menu.
    color_menu_tab: Cell<usize>,
    drag_tab: Cell<Option<DragTabState>>,
}

struct FilterState {
    query: String,
    matches: Vec<FilterMatch>,
}

struct RenameTabState {
    input: String,
}

define_class!(
    #[unsafe(super(objc2_app_kit::NSView))]
    #[thread_kind = MainThreadOnly]
    #[name = "KovaView"]
    #[ivars = KovaViewIvars]
    pub struct KovaView;

    unsafe impl NSObjectProtocol for KovaView {}

    impl KovaView {
        #[unsafe(method(acceptsFirstResponder))]
        fn accepts_first_responder(&self) -> bool {
            true
        }

        #[unsafe(method(mouseDownCanMoveWindow))]
        fn mouse_down_can_move_window(&self) -> bool {
            // Must be false so we get mouseDown events in the titlebar area.
            // We handle window dragging ourselves in hit_test_tab_bar when clicking
            // outside of tabs.
            false
        }

        #[unsafe(method(keyDown:))]
        fn key_down(&self, event: &NSEvent) {
            // If rename tab is active, route keys to rename
            if self.ivars().rename_tab.borrow().is_some() {
                self.handle_rename_tab_key(event);
                return;
            }

            // If filter is active, route keys to filter
            if self.ivars().filter.borrow().is_some() {
                self.handle_filter_key(event);
                return;
            }

            if let Some(pane) = self.focused_pane() {
                let cursor_keys_app = pane.terminal.read().cursor_keys_application;
                input::handle_key_event(event, &pane.pty, cursor_keys_app);
            }
        }

        #[unsafe(method(performKeyEquivalent:))]
        fn perform_key_equivalent(&self, event: &NSEvent) -> objc2::runtime::Bool {
            let modifiers = event.modifierFlags();
            let has_cmd = modifiers.contains(NSEventModifierFlags::Command);
            let has_shift = modifiers.contains(NSEventModifierFlags::Shift);
            let has_option = modifiers.contains(NSEventModifierFlags::Option);
            let has_ctrl = modifiers.contains(NSEventModifierFlags::Control);

            if has_cmd {
                let chars = event.charactersIgnoringModifiers();
                if let Some(chars) = chars {
                    let ch = chars.to_string();

                    // Cmd+F → toggle filter
                    if ch == "f" && !has_shift && !has_option {
                        self.toggle_filter();
                        return objc2::runtime::Bool::YES;
                    }

                    // Cmd+K → clear scrollback and screen
                    if ch == "k" && !has_shift && !has_option {
                        if let Some(pane) = self.focused_pane() {
                            pane.terminal.write().clear_scrollback_and_screen();
                            // Send Ctrl+L (form feed) so the shell redraws its prompt
                            pane.pty.write(b"\x0c");
                        }
                        return objc2::runtime::Bool::YES;
                    }

                    // Cmd+T → new tab
                    if ch == "t" && !has_shift && !has_option {
                        self.do_new_tab();
                        return objc2::runtime::Bool::YES;
                    }

                    // Cmd+D → vsplit
                    if ch == "d" && !has_shift && !has_option {
                        self.do_split(SplitDirection::Horizontal);
                        return objc2::runtime::Bool::YES;
                    }

                    // Cmd+Shift+D → hsplit
                    if ch == "D" && has_shift && !has_option {
                        self.do_split(SplitDirection::Vertical);
                        return objc2::runtime::Bool::YES;
                    }

                    // Cmd+W → close pane or tab
                    if ch == "w" && !has_shift && !has_option {
                        self.do_close_pane_or_tab();
                        return objc2::runtime::Bool::YES;
                    }

                    // Cmd+Shift+[ → previous tab
                    if ch == "{" && has_shift && !has_option {
                        self.do_switch_tab_relative(-1);
                        return objc2::runtime::Bool::YES;
                    }

                    // Cmd+Shift+] → next tab
                    if ch == "}" && has_shift && !has_option {
                        self.do_switch_tab_relative(1);
                        return objc2::runtime::Bool::YES;
                    }

                    // Cmd+Shift+R → rename tab
                    if ch == "R" && has_shift && !has_option {
                        self.start_rename_tab();
                        return objc2::runtime::Bool::YES;
                    }

                    // Cmd+1..9 → switch to tab N
                    if !has_shift && !has_option && !has_ctrl {
                        if let Some(digit) = ch.chars().next() {
                            if ('1'..='9').contains(&digit) {
                                let idx = (digit as usize) - ('1' as usize);
                                self.do_switch_tab(idx);
                                return objc2::runtime::Bool::YES;
                            }
                        }
                    }

                    // Cmd+Option+arrows → navigate panes
                    if has_option && !has_ctrl {
                        let nav = match ch.as_str() {
                            "\u{f702}" => Some(NavDirection::Left),   // left arrow
                            "\u{f703}" => Some(NavDirection::Right),  // right arrow
                            "\u{f700}" => Some(NavDirection::Up),     // up arrow
                            "\u{f701}" => Some(NavDirection::Down),   // down arrow
                            _ => None,
                        };
                        if let Some(dir) = nav {
                            self.do_navigate(dir);
                            return objc2::runtime::Bool::YES;
                        }
                    }

                    // Cmd+Ctrl+arrows → resize splits
                    if has_ctrl && !has_option {
                        let resize = match ch.as_str() {
                            "\u{f702}" => Some((SplitAxis::Horizontal, -0.05_f32)), // left
                            "\u{f703}" => Some((SplitAxis::Horizontal, 0.05)),       // right
                            "\u{f700}" => Some((SplitAxis::Vertical, -0.05)),        // up
                            "\u{f701}" => Some((SplitAxis::Vertical, 0.05)),         // down
                            _ => None,
                        };
                        if let Some((axis, delta)) = resize {
                            let mut tabs = self.ivars().tabs.borrow_mut();
                            let idx = self.ivars().active_tab.get();
                            if let Some(tab) = tabs.get_mut(idx) {
                                let focused_id = tab.focused_pane;
                                if tab.tree.adjust_ratio_for_pane(focused_id, delta, axis) {
                                    drop(tabs);
                                    self.resize_all_panes();
                                }
                            }
                            return objc2::runtime::Bool::YES;
                        }
                    }

                    // Cmd+C → copy
                    if ch == "c" && !has_shift && !has_option {
                        if let Some(pane) = self.focused_pane() {
                            let mut term = pane.terminal.write();
                            let text = term.selected_text();
                            if !text.is_empty() {
                                let pasteboard = NSPasteboard::generalPasteboard();
                                pasteboard.clearContents();
                                let ns_str = NSString::from_str(&text);
                                unsafe {
                                    pasteboard.setString_forType(&ns_str, objc2_app_kit::NSPasteboardTypeString);
                                }
                                term.clear_selection();
                                return objc2::runtime::Bool::YES;
                            }
                        }
                    }

                    // Cmd+V → paste
                    if ch == "v" && !has_shift && !has_option {
                        if let Some(pane) = self.focused_pane() {
                            let pasteboard = NSPasteboard::generalPasteboard();

                            // Try image paste first (PNG from clipboard)
                            let pasted_image = unsafe { pasteboard.dataForType(objc2_app_kit::NSPasteboardTypePNG) }
                                .and_then(|data| {
                                    if data.is_empty() { return None; }
                                    let bytes = data.to_vec();
                                    let timestamp = std::time::SystemTime::now()
                                        .duration_since(std::time::UNIX_EPOCH)
                                        .unwrap_or_default()
                                        .as_millis();
                                    let path = format!("/tmp/kova-paste-{timestamp}.png");
                                    std::fs::write(&path, bytes).ok().map(|_| path)
                                });

                            if let Some(path) = pasted_image {
                                let bracketed = pane.terminal.read().bracketed_paste;
                                if bracketed { pane.pty.write(b"\x1b[200~"); }
                                pane.pty.write(path.as_bytes());
                                if bracketed { pane.pty.write(b"\x1b[201~"); }
                            } else if let Some(text) = unsafe { pasteboard.stringForType(objc2_app_kit::NSPasteboardTypeString) } {
                                let text = text.to_string();
                                let bracketed = pane.terminal.read().bracketed_paste;
                                if bracketed {
                                    pane.pty.write(b"\x1b[200~");
                                }
                                pane.pty.write(text.as_bytes());
                                if bracketed {
                                    pane.pty.write(b"\x1b[201~");
                                }
                            }
                        }
                        return objc2::runtime::Bool::YES;
                    }
                }
            }
            objc2::runtime::Bool::NO
        }

        #[unsafe(method(flagsChanged:))]
        fn flags_changed(&self, _event: &NSEvent) {}

        #[unsafe(method(setFrameSize:))]
        fn set_frame_size(&self, new_size: CGSize) {
            let _: () = unsafe { msg_send![super(self), setFrameSize: new_size] };
            self.handle_resize();
        }

        #[unsafe(method(viewDidChangeBackingProperties))]
        fn view_did_change_backing_properties(&self) {
            self.handle_resize();
        }

        #[unsafe(method(scrollWheel:))]
        fn scroll_wheel(&self, event: &NSEvent) {
            // Scroll goes to the pane under the cursor, not necessarily the focused one
            if let Some((pane, _vp)) = self.pane_at_event(event) {
                let dy = event.scrollingDeltaY();
                let lines = if event.hasPreciseScrollingDeltas() {
                    let sensitivity = self.ivars().config.get()
                        .map(|c| c.terminal.scroll_sensitivity)
                        .unwrap_or(3.0);
                    let acc = pane.scroll_accumulator.get() + dy / sensitivity;
                    let discrete = acc as i32;
                    pane.scroll_accumulator.set(acc - discrete as f64);
                    discrete
                } else {
                    dy as i32
                };
                if lines != 0 {
                    let mut term = pane.terminal.write();
                    term.scroll(lines);
                    // Reset accumulator when hitting bounds to avoid residual drift
                    if term.scroll_offset() == 0 {
                        pane.scroll_accumulator.set(0.0);
                    }
                }
            }
        }

        #[unsafe(method(mouseDown:))]
        fn mouse_down(&self, event: &NSEvent) {
            let (px, py) = self.event_to_pixel(event);

            // Check filter click
            if self.ivars().filter.borrow().is_some() {
                self.handle_filter_click(px, py);
                return;
            }

            // Check tab bar click
            if self.hit_test_tab_bar(px, py, event) {
                return;
            }

            // Check separator hit
            if let Some(drag) = self.hit_test_separator(px, py) {
                self.ivars().drag_separator.set(Some(drag));
                return;
            }

            // Click sets focus to the pane under the cursor
            if let Some((pane, vp)) = self.pane_at_event(event) {
                let old_focused = {
                    let tabs = self.ivars().tabs.borrow();
                    let idx = self.ivars().active_tab.get();
                    tabs.get(idx).map(|t| t.focused_pane).unwrap_or(0)
                };
                {
                    let mut tabs = self.ivars().tabs.borrow_mut();
                    let idx = self.ivars().active_tab.get();
                    if let Some(tab) = tabs.get_mut(idx) {
                        tab.focused_pane = pane.id;
                    }
                }
                // Mark old focused pane dirty so its dim overlay updates
                if old_focused != pane.id {
                    let tabs = self.ivars().tabs.borrow();
                    let idx = self.ivars().active_tab.get();
                    if let Some(tab) = tabs.get(idx) {
                        if let Some(old) = tab.tree.pane(old_focused) {
                            old.terminal.read().dirty.store(true, std::sync::atomic::Ordering::Relaxed);
                        }
                    }
                }
                if let Some(pos) = self.pixel_to_grid_in(event, pane, &vp) {
                    let mut term = pane.terminal.write();
                    term.selection = Some(Selection { anchor: pos, end: pos });
                    term.dirty.store(true, std::sync::atomic::Ordering::Relaxed);
                }
            }
        }

        #[unsafe(method(mouseDragged:))]
        fn mouse_dragged(&self, event: &NSEvent) {
            // Handle tab drag
            if let Some(mut drag) = self.ivars().drag_tab.get() {
                let (px, _py) = self.event_to_pixel(event);
                drag.current_x = px;
                if !drag.dragging {
                    if (px - drag.start_x).abs() >= 3.0 {
                        drag.dragging = true;
                    } else {
                        self.ivars().drag_tab.set(Some(drag));
                        return;
                    }
                }
                if let Some(target) = self.tab_index_at_x(px) {
                    if target != drag.tab_index {
                        let mut tabs = self.ivars().tabs.borrow_mut();
                        tabs.swap(drag.tab_index, target);
                        drop(tabs);
                        self.ivars().active_tab.set(target);
                        drag.tab_index = target;
                        self.mark_dirty();
                    }
                }
                self.ivars().drag_tab.set(Some(drag));
                return;
            }

            // Handle separator drag
            if let Some(drag) = self.ivars().drag_separator.get() {
                let (px, py) = self.event_to_pixel(event);
                let current_pixel = if drag.is_hsplit { px } else { py };
                let new_ratio = drag.origin_ratio + (current_pixel - drag.origin_pixel) / drag.parent_dim;
                let mut tabs = self.ivars().tabs.borrow_mut();
                let idx = self.ivars().active_tab.get();
                if let Some(tab) = tabs.get_mut(idx) {
                    if tab.tree.set_ratio_by_ptr(drag.node_ptr, new_ratio) {
                        drop(tabs);
                        self.resize_all_panes();
                    }
                }
                return;
            }

            // Drag continues on the focused pane (set by mouseDown)
            if let Some(pane) = self.focused_pane() {
                let vp = {
                    let tabs = self.ivars().tabs.borrow();
                    let idx = self.ivars().active_tab.get();
                    tabs.get(idx).and_then(|t| t.tree.viewport_for_pane(pane.id, self.panes_viewport()))
                };
                if let Some(vp) = vp {
                    if let Some(pos) = self.pixel_to_grid_in(event, pane, &vp) {
                        let mut term = pane.terminal.write();
                        if let Some(ref mut sel) = term.selection {
                            sel.end = pos;
                            term.dirty.store(true, std::sync::atomic::Ordering::Relaxed);
                        }
                    }
                }
            }
        }

        #[unsafe(method(mouseUp:))]
        fn mouse_up(&self, _event: &NSEvent) {
            if self.ivars().drag_tab.get().is_some() {
                self.ivars().drag_tab.set(None);
                return;
            }
            if self.ivars().drag_separator.get().is_some() {
                self.ivars().drag_separator.set(None);
                return;
            }
            if let Some(pane) = self.focused_pane() {
                let mut term = pane.terminal.write();
                // Single click (no drag) — clear selection
                if let Some(ref sel) = term.selection {
                    if sel.anchor == sel.end {
                        term.selection = None;
                        term.dirty.store(true, std::sync::atomic::Ordering::Relaxed);
                        return;
                    }
                }
                let text = term.selected_text();
                if !text.is_empty() {
                    let pasteboard = NSPasteboard::generalPasteboard();
                    pasteboard.clearContents();
                    let ns_str = NSString::from_str(&text);
                    unsafe {
                        pasteboard.setString_forType(&ns_str, objc2_app_kit::NSPasteboardTypeString);
                    }
                }
            }
        }

        #[unsafe(method(tabColorSelected:))]
        fn tab_color_selected(&self, sender: &objc2_app_kit::NSMenuItem) {
            let tag = sender.tag();
            let tab_idx = self.ivars().color_menu_tab.get();
            let mut tabs = self.ivars().tabs.borrow_mut();
            if let Some(tab) = tabs.get_mut(tab_idx) {
                tab.color = if tag < 0 { None } else { Some(tag as usize) };
            }
            drop(tabs);
            self.mark_dirty();
        }

        #[unsafe(method(rightMouseDown:))]
        fn right_mouse_down(&self, event: &NSEvent) {
            let (px, py) = self.event_to_pixel(event);
            let tab_bar_h = self.tab_bar_height();
            if py <= tab_bar_h {
                if let Some(tab_idx) = self.tab_index_at_x(px) {
                    self.show_tab_color_menu(event, tab_idx);
                    return;
                }
            }
            // Default behavior for right-click outside tab bar
            unsafe { msg_send![super(self), rightMouseDown: event] }
        }
    }
);

impl KovaView {
    fn new(mtm: MainThreadMarker, frame: CGRect) -> Retained<Self> {
        let this = mtm.alloc::<Self>().set_ivars(KovaViewIvars {
            renderer: OnceCell::new(),
            tabs: RefCell::new(Vec::new()),
            active_tab: Cell::new(0),
            metal_layer: OnceCell::new(),
            last_scale: Cell::new(0.0),
            last_focused: Cell::new(true),
            config: OnceCell::new(),
            drag_separator: Cell::new(None),
            filter: RefCell::new(None),
            rename_tab: RefCell::new(None),
            tab_bar_left_inset: Cell::new(0.0),
            color_menu_tab: Cell::new(0),
            drag_tab: Cell::new(None),
        });
        unsafe { msg_send![super(this), initWithFrame: frame] }
    }

    /// Return the currently focused pane (keyboard input target).
    fn focused_pane(&self) -> Option<&Pane> {
        let tabs = self.ivars().tabs.borrow();
        let idx = self.ivars().active_tab.get();
        let tab = tabs.get(idx)?;
        let pane = tab.tree.pane(tab.focused_pane)?;
        // SAFETY: The Tab/SplitTree lives in RefCell inside ivars, pinned in ObjC heap.
        // We only mutate in the render timer (pane removal), never while an event handler holds this ref.
        Some(unsafe { &*(pane as *const Pane) })
    }

    /// Viewport for panes (below tab bar).
    fn panes_viewport(&self) -> PaneViewport {
        let full = self.drawable_viewport();
        let tab_bar_h = self.tab_bar_height();
        PaneViewport {
            x: full.x,
            y: full.y + tab_bar_h,
            width: full.width,
            height: full.height - tab_bar_h,
        }
    }

    fn get_tab_bar_left_inset(&self) -> f32 {
        let v = self.ivars().tab_bar_left_inset.get();
        if v > 0.0 { v } else { 136.0 } // fallback 68pt * 2x
    }

    /// Tab bar height in pixels (2.0x cell height).
    fn tab_bar_height(&self) -> f32 {
        let renderer = match self.ivars().renderer.get() {
            Some(r) => r,
            None => return 0.0,
        };
        let r = renderer.read();
        let (_, cell_h) = r.cell_size();
        (cell_h * 2.0).round()
    }

    /// Hit-test separators in the active tab's tree.
    fn hit_test_separator(&self, px: f32, py: f32) -> Option<SeparatorDrag> {
        let tabs = self.ivars().tabs.borrow();
        let idx = self.ivars().active_tab.get();
        let tab = tabs.get(idx)?;
        let vp = self.panes_viewport();
        let mut seps = Vec::new();
        tab.tree.collect_separator_info(vp, &mut seps);

        let scale = self.window().map_or(2.0, |w| w.backingScaleFactor()) as f32;
        let tolerance = 4.0 * scale;

        for sep in &seps {
            if sep.is_hsplit {
                if (px - sep.pos).abs() < tolerance && py >= sep.cross_start && py <= sep.cross_end {
                    return Some(SeparatorDrag {
                        is_hsplit: true,
                        origin_pixel: px,
                        origin_ratio: sep.origin_ratio,
                        parent_dim: sep.parent_dim,
                        node_ptr: sep.node_ptr,
                    });
                }
            } else {
                if (py - sep.pos).abs() < tolerance && px >= sep.cross_start && px <= sep.cross_end {
                    return Some(SeparatorDrag {
                        is_hsplit: false,
                        origin_pixel: py,
                        origin_ratio: sep.origin_ratio,
                        parent_dim: sep.parent_dim,
                        node_ptr: sep.node_ptr,
                    });
                }
            }
        }
        None
    }

    /// Hit-test the tab bar. Returns true if click was in the tab bar (and handled).
    fn hit_test_tab_bar(&self, px: f32, py: f32, event: &NSEvent) -> bool {
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
            // Click in titlebar but not on a tab — initiate window drag
            win.performWindowDragWithEvent(event);
        }
        true
    }

    /// Returns the tab index at the given x pixel position, or None if outside tabs.
    fn tab_index_at_x(&self, px: f32) -> Option<usize> {
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
        let available_w = full.width - left_inset;
        let tab_width = (available_w / tab_count as f32).min(max_tab_w);
        for i in 0..tab_count {
            let x = left_inset + i as f32 * tab_width;
            if px >= x && px <= x + tab_width {
                return Some(i);
            }
        }
        None
    }

    /// Total drawable viewport in pixels.
    fn drawable_viewport(&self) -> PaneViewport {
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
    fn event_to_pixel(&self, event: &NSEvent) -> (f32, f32) {
        let location = event.locationInWindow();
        let local: CGPoint = unsafe { msg_send![self, convertPoint: location, fromView: std::ptr::null::<objc2::runtime::AnyObject>()] };
        let frame = self.frame();
        let scale = self.window().map_or(2.0, |w| w.backingScaleFactor()) as f32;
        let pixel_x = local.x as f32 * scale;
        let pixel_y = (frame.size.height as f32 - local.y as f32) * scale;
        (pixel_x, pixel_y)
    }

    /// Hit-test: find which pane is under the mouse event (in active tab).
    fn pane_at_event(&self, event: &NSEvent) -> Option<(&Pane, PaneViewport)> {
        let tabs = self.ivars().tabs.borrow();
        let idx = self.ivars().active_tab.get();
        let tab = tabs.get(idx)?;
        let (px, py) = self.event_to_pixel(event);
        let (pane, vp) = tab.tree.hit_test(px, py, self.panes_viewport())?;
        Some((unsafe { &*(pane as *const Pane) }, vp))
    }

    /// Convert an NSEvent to a grid position within the given pane/viewport.
    fn pixel_to_grid_in(&self, event: &NSEvent, pane: &Pane, vp: &PaneViewport) -> Option<GridPos> {
        let renderer = self.ivars().renderer.get()?;
        let (pixel_x, pixel_y) = self.event_to_pixel(event);

        let renderer_r = renderer.read();
        let (cell_w, cell_h) = renderer_r.cell_size();
        drop(renderer_r);

        let rel_x = pixel_x - vp.x;
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

    /// Compute cols/rows for a pane viewport.
    fn viewport_to_grid(&self, vp: &PaneViewport) -> (u16, u16) {
        let renderer = self.ivars().renderer.get().unwrap();
        let renderer_r = renderer.read();
        let (cell_w, cell_h) = renderer_r.cell_size();
        let status_bar = renderer_r.status_bar_enabled();
        drop(renderer_r);

        let cols = ((vp.width - 2.0 * crate::renderer::PANE_H_PADDING) / cell_w).floor().max(1.0) as u16;
        let usable_h = if status_bar {
            vp.height - cell_h
        } else {
            vp.height
        };
        let rows = (usable_h / cell_h).floor().max(1.0) as u16;
        (cols, rows)
    }

    /// Create a new tab (Cmd+T).
    fn do_new_tab(&self) {
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
        tabs.push(tab);
        let new_idx = tabs.len() - 1;
        log::debug!("New tab created: index={}, total={}", new_idx, tabs.len());
        drop(tabs);
        self.ivars().active_tab.set(new_idx);
        self.resize_all_panes();
    }

    /// Switch to tab at index.
    fn do_switch_tab(&self, idx: usize) {
        let tabs = self.ivars().tabs.borrow();
        if idx >= tabs.len() || idx == self.ivars().active_tab.get() {
            return;
        }
        log::debug!("Switch to tab {}", idx);
        // Mark all panes of new tab dirty so the next render tick draws them
        tabs[idx].tree.for_each_pane(&mut |pane| {
            pane.terminal.read().dirty.store(true, std::sync::atomic::Ordering::Relaxed);
        });
        drop(tabs);
        self.ivars().active_tab.set(idx);
        // Lazy resize: resize panes when switching to them
        self.resize_all_panes();
    }

    /// Show a context menu to pick a color for a tab.
    fn show_tab_color_menu(&self, event: &NSEvent, tab_idx: usize) {
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
    fn do_switch_tab_relative(&self, delta: i32) {
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
    fn do_split(&self, direction: SplitDirection) {
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
            let vp = match tab.tree.viewport_for_pane(fid, self.panes_viewport()) {
                Some(vp) => vp,
                None => return,
            };
            let cwd = tab.tree.pane(fid).and_then(|p| p.cwd());
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

        let mut tabs = self.ivars().tabs.borrow_mut();
        let idx = self.ivars().active_tab.get();
        if let Some(tab) = tabs.get_mut(idx) {
            let tree = std::mem::replace(&mut tab.tree, SplitTree::Leaf(Pane::spawn(1, 1, config, None).unwrap()));
            tab.tree = tree.with_split(focused_id, new_pane, direction);
            tab.tree.equalize();
            tab.focused_pane = new_id;
        }
        drop(tabs);

        self.resize_all_panes();
    }

    /// Close focused pane. If it's the last pane in the tab, close the tab.
    fn do_close_pane_or_tab(&self) {
        let mut tabs = self.ivars().tabs.borrow_mut();
        let idx = self.ivars().active_tab.get();
        if idx >= tabs.len() {
            return;
        }

        let is_single_pane = matches!(tabs[idx].tree, SplitTree::Leaf(_));

        if is_single_pane {
            // Close the tab
            log::debug!("Closing tab {}", idx);
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
            return;
        }

        // Multiple panes → close focused pane
        let focused_id = tabs[idx].focused_pane;
        log::debug!("Closing pane {} in tab {}", focused_id, idx);
        let config = self.ivars().config.get().unwrap();
        let dummy = Pane::spawn(1, 1, config, None).unwrap();
        let tree = std::mem::replace(&mut tabs[idx].tree, SplitTree::Leaf(dummy));
        match tree.remove_pane(focused_id) {
            Some(new_tree) => {
                tabs[idx].focused_pane = new_tree.first_pane().id;
                let mut new_tree = new_tree;
                new_tree.equalize();
                tabs[idx].tree = new_tree;
            }
            None => {
                // Tab became empty (shouldn't happen given check above)
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
                self.ivars().active_tab.set(new_idx);
            }
        }
        drop(tabs);
        self.resize_all_panes();
    }

    /// Navigate focus to an adjacent pane.
    fn do_navigate(&self, dir: NavDirection) {
        let mut tabs = self.ivars().tabs.borrow_mut();
        let idx = self.ivars().active_tab.get();
        let tab = match tabs.get_mut(idx) {
            Some(t) => t,
            None => return,
        };
        let focused_id = tab.focused_pane;
        if let Some(neighbor_id) = tab.tree.neighbor(focused_id, dir, self.panes_viewport()) {
            tab.focused_pane = neighbor_id;
            if let Some(old) = tab.tree.pane(focused_id) {
                old.terminal.read().dirty.store(true, std::sync::atomic::Ordering::Relaxed);
            }
            if let Some(new) = tab.tree.pane(neighbor_id) {
                new.terminal.read().dirty.store(true, std::sync::atomic::Ordering::Relaxed);
            }
        } else {
            // No neighbor in this direction → overflow to adjacent tab
            let count = tabs.len();
            if count <= 1 {
                return;
            }
            drop(tabs);
            let delta: i32 = match dir {
                NavDirection::Left | NavDirection::Up => -1,
                NavDirection::Right | NavDirection::Down => 1,
            };
            self.do_switch_tab_relative(delta);
            // Focus the appropriate pane in the new tab:
            // going right/down → first pane, going left/up → last pane
            let mut tabs = self.ivars().tabs.borrow_mut();
            let new_idx = self.ivars().active_tab.get();
            if let Some(new_tab) = tabs.get_mut(new_idx) {
                let target_id = match dir {
                    NavDirection::Right | NavDirection::Down => new_tab.tree.first_pane().id,
                    NavDirection::Left | NavDirection::Up => new_tab.tree.last_pane().id,
                };
                new_tab.focused_pane = target_id;
            }
        }
    }

    /// Resize all panes in the active tab to match their current viewports.
    fn resize_all_panes(&self) {
        let renderer = match self.ivars().renderer.get() {
            Some(r) => r,
            None => return,
        };
        let renderer_r = renderer.read();
        let (cell_w, cell_h) = renderer_r.cell_size();
        let status_bar = renderer_r.status_bar_enabled();
        drop(renderer_r);

        let panes_vp = self.panes_viewport();
        let tabs = self.ivars().tabs.borrow();
        let idx = self.ivars().active_tab.get();
        if let Some(tab) = tabs.get(idx) {
            tab.tree.for_each_pane_with_viewport(panes_vp, &mut |pane, vp| {
                let cols = ((vp.width - 2.0 * crate::renderer::PANE_H_PADDING) / cell_w).floor().max(1.0) as u16;
                let usable_h = if status_bar { vp.height - cell_h } else { vp.height };
                let rows = (usable_h / cell_h).floor().max(1.0) as u16;
                let mut term = pane.terminal.write();
                if cols != term.cols || rows != term.rows {
                    term.resize(cols, rows);
                    drop(term);
                    pane.pty.resize(cols, rows);
                }
            });
        }
    }

    fn mark_dirty(&self) {
        if let Some(pane) = self.focused_pane() {
            pane.terminal.read().dirty.store(true, std::sync::atomic::Ordering::Relaxed);
        }
    }

    fn toggle_filter(&self) {
        let mut filter = self.ivars().filter.borrow_mut();
        if filter.is_some() {
            *filter = None;
        } else {
            *filter = Some(FilterState {
                query: String::new(),
                matches: Vec::new(),
            });
        }
        drop(filter);
        // Mark dirty to trigger redraw
        if let Some(pane) = self.focused_pane() {
            pane.terminal.read().dirty.store(true, std::sync::atomic::Ordering::Relaxed);
        }
    }

    fn handle_filter_key(&self, event: &NSEvent) {
        let chars = event.charactersIgnoringModifiers();
        let ch_str = chars.map(|s| s.to_string()).unwrap_or_default();
        let ch = ch_str.chars().next().unwrap_or('\0');

        let mut filter = self.ivars().filter.borrow_mut();
        let state = match filter.as_mut() {
            Some(s) => s,
            None => return,
        };

        match ch {
            '\u{1B}' => {
                // Escape → close filter without scrolling
                *filter = None;
                drop(filter);
                if let Some(pane) = self.focused_pane() {
                    pane.terminal.read().dirty.store(true, std::sync::atomic::Ordering::Relaxed);
                }
                return;
            }
            '\r' => {
                // Enter → close filter and scroll to first match
                let first_match = state.matches.first().map(|m| m.abs_line);
                *filter = None;
                drop(filter);
                if let Some(abs_line) = first_match {
                    if let Some(pane) = self.focused_pane() {
                        let mut term = pane.terminal.write();
                        term.scroll_to_abs_line(abs_line);
                    }
                }
                return;
            }
            '\u{7F}' | '\u{08}' => {
                // Backspace
                state.query.pop();
            }
            c if c >= ' ' && !c.is_control() => {
                state.query.push(c);
            }
            _ => return,
        }

        // Re-run search
        if let Some(pane) = self.focused_pane() {
            let term = pane.terminal.read();
            state.matches = term.search_lines(&state.query);
            term.dirty.store(true, std::sync::atomic::Ordering::Relaxed);
        }
    }

    fn start_rename_tab(&self) {
        // Pre-fill with current tab title
        let current_title = {
            let tabs = self.ivars().tabs.borrow();
            let idx = self.ivars().active_tab.get();
            tabs.get(idx).map(|t| t.title()).unwrap_or_default()
        };
        *self.ivars().rename_tab.borrow_mut() = Some(RenameTabState {
            input: current_title,
        });
        self.mark_dirty();
    }

    fn handle_rename_tab_key(&self, event: &NSEvent) {
        let chars = event.charactersIgnoringModifiers();
        let ch_str = chars.map(|s| s.to_string()).unwrap_or_default();
        let ch = ch_str.chars().next().unwrap_or('\0');

        let mut rename = self.ivars().rename_tab.borrow_mut();
        let state = match rename.as_mut() {
            Some(s) => s,
            None => return,
        };

        match ch {
            '\u{1B}' => {
                // Escape → cancel rename
                *rename = None;
                drop(rename);
                self.mark_dirty();
                return;
            }
            '\r' => {
                // Enter → apply rename (empty = reset to auto)
                let new_title = if state.input.trim().is_empty() {
                    None
                } else {
                    Some(state.input.clone())
                };
                *rename = None;
                drop(rename);
                let mut tabs = self.ivars().tabs.borrow_mut();
                let idx = self.ivars().active_tab.get();
                if let Some(tab) = tabs.get_mut(idx) {
                    tab.custom_title = new_title;
                }
                drop(tabs);
                self.mark_dirty();
                return;
            }
            '\u{7F}' | '\u{08}' => {
                // Backspace
                state.input.pop();
            }
            c if c >= ' ' && !c.is_control() => {
                state.input.push(c);
            }
            _ => return,
        }
        drop(rename);
        self.mark_dirty();
    }

    fn handle_filter_click(&self, _px: f32, py: f32) {
        let renderer = match self.ivars().renderer.get() {
            Some(r) => r,
            None => return,
        };
        let (_, cell_h) = renderer.read().cell_size();

        // The overlay starts with: 1 row search bar + matches below
        let match_start_y = {
            let panes_vp = self.panes_viewport();
            panes_vp.y + cell_h // search bar takes 1 row
        };

        let click_row = ((py - match_start_y) / cell_h).floor() as i32;
        if click_row < 0 {
            return;
        }

        let mut filter = self.ivars().filter.borrow_mut();
        let abs_line = match filter.as_ref() {
            Some(state) => {
                let idx = click_row as usize;
                state.matches.get(idx).map(|m| m.abs_line)
            }
            None => return,
        };

        *filter = None;
        drop(filter);

        if let Some(abs_line) = abs_line {
            if let Some(pane) = self.focused_pane() {
                let mut term = pane.terminal.write();
                term.scroll_to_abs_line(abs_line);
            }
        }
    }

    fn handle_resize(&self) {
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
        if (scale - self.ivars().last_scale.get()).abs() > 0.01 {
            log::debug!("Scale changed: {} -> {}", self.ivars().last_scale.get(), scale);
            self.ivars().last_scale.set(scale);
            renderer.write().rebuild_atlas(scale);
        }

        self.resize_all_panes();
    }

    pub fn save_session(&self) {
        let tabs = self.ivars().tabs.borrow();
        let active_tab = self.ivars().active_tab.get();
        crate::session::save(&tabs, active_tab);
    }

    pub fn setup_metal(&self, _mtm: MainThreadMarker, config: &Config) {
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

        // Try to restore previous session, fallback to fresh tab
        let (tabs, active_tab) = match crate::session::load().and_then(|s| crate::session::restore_session(s, config)) {
            Some((tabs, active)) => {
                log::info!("Restored session: {} tabs, active={}", tabs.len(), active);
                (tabs, active)
            }
            None => {
                let tab = Tab::new(config).expect("failed to create initial tab");
                (vec![tab], 0)
            }
        };

        let terminal_for_renderer = tabs[active_tab].tree.first_pane().terminal.clone();

        let renderer = Arc::new(parking_lot::RwLock::new(
            Renderer::new(&device, &layer, terminal_for_renderer, scale, config),
        ));

        self.ivars().renderer.set(renderer).ok();
        self.ivars().config.set(config.clone()).ok();
        *self.ivars().tabs.borrow_mut() = tabs;
        self.ivars().active_tab.set(active_tab);

        self.start_render_timer(config.terminal.fps);
    }

    /// # Safety
    /// The ivars pointer is stable for the lifetime of the ObjC view (heap-allocated,
    /// retained by the window). The timer is invalidated when the window closes,
    /// so the pointer remains valid for every tick.
    fn start_render_timer(&self, fps: u32) {
        let renderer = self.ivars().renderer.get().unwrap().clone();
        let layer = self.ivars().metal_layer.get().expect("metal_layer not initialized").clone();
        let ivars = self.ivars() as *const KovaViewIvars;

        let last_title: std::cell::RefCell<Option<String>> = std::cell::RefCell::new(None);
        let window_for_title: std::cell::OnceCell<Retained<NSWindow>> = std::cell::OnceCell::new();
        let git_poll_counter: Cell<u32> = Cell::new(0);
        let git_poll_interval: u32 = fps * 2; // poll git branch every ~2 seconds

        let timer = unsafe {
            NSTimer::scheduledTimerWithTimeInterval_repeats_block(
                1.0 / fps as f64,
                true,
                &RcBlock::new(move |_timer: NonNull<NSTimer>| {
                    let ivars = &*ivars;

                    // --- Poll git branch for all panes with a CWD ---
                    let count = git_poll_counter.get() + 1;
                    git_poll_counter.set(count);
                    if count >= git_poll_interval {
                        git_poll_counter.set(0);
                        let tabs = ivars.tabs.borrow();
                        for tab in tabs.iter() {
                            tab.tree.for_each_pane(&mut |pane| {
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
                            let exited = tab.tree.exited_pane_ids();
                            if exited.is_empty() {
                                continue;
                            }
                            any_removed = true;
                            log::debug!("Reaping exited panes in tab {}: {:?}", tab_idx, exited);
                            for id in &exited {
                                let tree = std::mem::replace(&mut tab.tree, SplitTree::Leaf(
                                    // We need a dummy — but remove_pane might return None
                                    // So we take the tree and put back the result
                                    Pane::spawn(1, 1, ivars.config.get().unwrap(), None).unwrap()
                                ));
                                match tree.remove_pane(*id) {
                                    Some(mut new_tree) => {
                                        new_tree.equalize();
                                        tab.tree = new_tree;
                                    }
                                    None => {
                                        // Tab is now empty
                                        tabs_to_remove.push(tab_idx);
                                        break;
                                    }
                                }
                            }
                            // Fix focused pane if it was removed
                            if exited.contains(&tab.focused_pane) {
                                if !tabs_to_remove.contains(&tab_idx) {
                                    tab.focused_pane = tab.tree.first_pane().id;
                                }
                            }
                        }
                        // Remove empty tabs (in reverse to preserve indices)
                        for &idx in tabs_to_remove.iter().rev() {
                            tabs.remove(idx);
                        }
                    }

                    // Adjust active_tab if needed
                    if any_removed {
                        let tabs = ivars.tabs.borrow();
                        if tabs.is_empty() {
                            drop(tabs);
                            let mtm = MainThreadMarker::new_unchecked();
                            let app = NSApplication::sharedApplication(mtm);
                            app.terminate(None);
                            return;
                        }
                        let active = ivars.active_tab.get();
                        if active >= tabs.len() {
                            ivars.active_tab.set(tabs.len() - 1);
                        }
                    }

                    // Build pane render list from active tab only
                    let active_idx = ivars.active_tab.get();
                    let (pane_data, pty_ptr, focus_reporting, tab_titles) = {
                        let tabs = ivars.tabs.borrow();
                        if tabs.is_empty() {
                            return;
                        }
                        let tab = &tabs[active_idx];
                        let focused_id = tab.focused_pane;

                        let mut pane_data: Vec<(Arc<parking_lot::RwLock<crate::terminal::TerminalState>>, PaneViewport, bool, bool)> = Vec::new();
                        let drawable_size = layer.drawableSize();
                        let cell_h = renderer.read().cell_size().1;
                        let tab_bar_h = (cell_h * 2.0).round();
                        let panes_vp = PaneViewport {
                            x: 0.0,
                            y: tab_bar_h,
                            width: drawable_size.width as f32,
                            height: drawable_size.height as f32 - tab_bar_h,
                        };
                        tab.tree.for_each_pane_with_viewport(panes_vp, &mut |pane, vp| {
                            pane_data.push((
                                pane.terminal.clone(),
                                vp,
                                pane.is_ready(),
                                pane.id == focused_id,
                            ));
                        });

                        let focused = tab.tree.pane(focused_id);
                        let pty_ptr = focused.map(|p| &p.pty as *const crate::terminal::pty::Pty);
                        let focus_reporting = focused.map_or(false, |p| p.terminal.read().focus_reporting);

                        let rename = ivars.rename_tab.borrow();
                        let tab_titles: Vec<(String, bool, Option<usize>, bool)> = tabs.iter().enumerate()
                            .map(|(i, t)| {
                                let is_renaming = i == active_idx && rename.is_some();
                                let title = if is_renaming {
                                    let rs = rename.as_ref().unwrap();
                                    format!("{}▏", rs.input)
                                } else {
                                    t.title()
                                };
                                (title, i == active_idx, t.color, is_renaming)
                            })
                            .collect();
                        drop(rename);
                        (pane_data, pty_ptr, focus_reporting, tab_titles)
                    };

                    // Focus reporting (DEC mode 1004) — send to focused pane only
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

                    // Update NSWindow title from focused pane's OSC 0/2
                    if let Some((terminal, _, _, _)) = pane_data.iter().find(|(_, _, _, f)| *f) {
                        let term = terminal.read();
                        let current = term.title.clone();
                        drop(term);
                        let mut prev = last_title.borrow_mut();
                        if current != *prev {
                            let mtm2 = MainThreadMarker::new_unchecked();
                            let app2 = NSApplication::sharedApplication(mtm2);
                            if let Some(win) = window_for_title.get().or_else(|| {
                                let w = app2.mainWindow()?;
                                let _ = window_for_title.set(w);
                                window_for_title.get()
                            }) {
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
                            let renderer_r = renderer.read();
                            let cell_h = renderer_r.cell_size().1;
                            let status_bar = renderer_r.status_bar_enabled();
                            drop(renderer_r);
                            let drawable_size = layer.drawableSize();
                            let status_bar_h = if status_bar { cell_h } else { 0.0 };
                            let panes_vp = PaneViewport {
                                x: 0.0,
                                y: cell_h,
                                width: drawable_size.width as f32,
                                height: drawable_size.height as f32 - cell_h - status_bar_h,
                            };
                            let mut seps = Vec::new();
                            tab.tree.collect_separators(panes_vp, &mut seps);
                            seps
                        } else {
                            Vec::new()
                        }
                    };

                    // Build filter render data if active
                    let filter_data = {
                        let filter = ivars.filter.borrow();
                        filter.as_ref().map(|f| FilterRenderData {
                            query: f.query.clone(),
                            matches: f.matches.clone(),
                        })
                    };

                    // Compute left_inset from traffic light buttons
                    let left_inset = {
                        let mtm2 = MainThreadMarker::new_unchecked();
                        let app2 = NSApplication::sharedApplication(mtm2);
                        let inset = app2.mainWindow()
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
                    renderer.write().render_panes(&layer, &pane_data, &separators, &tab_titles, filter_data.as_ref(), left_inset);
                }),
            )
        };
        let run_loop = NSRunLoop::currentRunLoop();
        unsafe { run_loop.addTimer_forMode(&timer, NSRunLoopCommonModes) };
    }

}

pub fn create_window(mtm: MainThreadMarker, config: &Config) -> Retained<NSWindow> {
    let content_rect = CGRect {
        origin: CGPoint { x: config.window.x, y: config.window.y },
        size: CGSize {
            width: config.window.width,
            height: config.window.height,
        },
    };

    let style = NSWindowStyleMask::Titled
        | NSWindowStyleMask::Closable
        | NSWindowStyleMask::Miniaturizable
        | NSWindowStyleMask::Resizable
        | NSWindowStyleMask::FullSizeContentView;

    let window = unsafe {
        NSWindow::initWithContentRect_styleMask_backing_defer(
            mtm.alloc(),
            content_rect,
            style,
            NSBackingStoreType::Buffered,
            false,
        )
    };

    let title = NSString::from_str("Kova");
    window.setTitle(&title);
    window.setTitlebarAppearsTransparent(true);
    window.setTitleVisibility(NSWindowTitleVisibility::Hidden);
    window.setMinSize(CGSize {
        width: 200.0,
        height: 150.0,
    });

    // Persist and restore window position/size automatically via NSUserDefaults
    window.setFrameAutosaveName(&NSString::from_str("KovaMainWindow"));

    let view = KovaView::new(mtm, content_rect);
    view.setup_metal(mtm, config);
    window.setContentView(Some(&view));
    window.makeFirstResponder(Some(&view));

    window
}
