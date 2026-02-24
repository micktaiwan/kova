use block2::RcBlock;
use objc2::rc::Retained;
use objc2::{define_class, msg_send, DefinedClass, MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{NSApplication, NSBackingStoreType, NSEvent, NSEventModifierFlags, NSPasteboard, NSWindow, NSWindowStyleMask};
use objc2_core_foundation::{CGPoint, CGRect, CGSize};
use objc2_foundation::{NSObjectProtocol, NSRunLoop, NSRunLoopCommonModes, NSString, NSTimer};
use objc2_metal::MTLCreateSystemDefaultDevice;
use objc2_quartz_core::CAMetalLayer;
use std::cell::{Cell, OnceCell, RefCell};
use std::ptr::NonNull;
use std::sync::Arc;

use crate::config::Config;
use crate::input;
use crate::pane::{NavDirection, Pane, PaneId, SplitDirection, SplitTree};
use crate::renderer::{PaneViewport, Renderer};
use crate::terminal::{GridPos, Selection};

pub struct KovaViewIvars {
    renderer: OnceCell<Arc<parking_lot::RwLock<Renderer>>>,
    tree: RefCell<Option<SplitTree>>,
    focused: Cell<PaneId>,
    metal_layer: OnceCell<Retained<CAMetalLayer>>,
    last_scale: Cell<f64>,
    last_focused: Cell<bool>,
    config: OnceCell<Config>,
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

        #[unsafe(method(keyDown:))]
        fn key_down(&self, event: &NSEvent) {
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

            if has_cmd {
                let chars = event.charactersIgnoringModifiers();
                if let Some(chars) = chars {
                    let ch = chars.to_string();

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

                    // Cmd+W → close focused pane
                    if ch == "w" && !has_shift && !has_option {
                        self.do_close_pane();
                        return objc2::runtime::Bool::YES;
                    }

                    // Cmd+Option+arrows → navigate panes
                    if has_option {
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
                            if let Some(text) = unsafe { pasteboard.stringForType(objc2_app_kit::NSPasteboardTypeString) } {
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
            // Click sets focus to the pane under the cursor
            if let Some((pane, vp)) = self.pane_at_event(event) {
                let old_focused = self.ivars().focused.get();
                self.ivars().focused.set(pane.id);
                // Mark old focused pane dirty so its dim overlay updates
                if old_focused != pane.id {
                    let tree_ref = self.ivars().tree.borrow();
                    if let Some(tree) = tree_ref.as_ref() {
                        if let Some(old) = tree.pane(old_focused) {
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
            // Drag continues on the focused pane (set by mouseDown)
            if let Some(pane) = self.focused_pane() {
                let vp = {
                    let tree_ref = self.ivars().tree.borrow();
                    tree_ref.as_ref().and_then(|t| t.viewport_for_pane(pane.id, self.drawable_viewport()))
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
    }
);

impl KovaView {
    fn new(mtm: MainThreadMarker, frame: CGRect) -> Retained<Self> {
        let this = mtm.alloc::<Self>().set_ivars(KovaViewIvars {
            renderer: OnceCell::new(),
            tree: RefCell::new(None),
            focused: Cell::new(0),
            metal_layer: OnceCell::new(),
            last_scale: Cell::new(0.0),
            last_focused: Cell::new(true),
            config: OnceCell::new(),
        });
        unsafe { msg_send![super(this), initWithFrame: frame] }
    }

    /// Return the currently focused pane (keyboard input target).
    ///
    /// # Safety
    /// The returned reference borrows from the RefCell. This is safe because:
    /// - All access is on the main thread (MainThreadOnly)
    /// - We never hold the borrow across a mutation point
    fn focused_pane(&self) -> Option<&Pane> {
        let tree_ref = self.ivars().tree.borrow();
        let tree = tree_ref.as_ref()?;
        let id = self.ivars().focused.get();
        let pane = tree.pane(id)?;
        // SAFETY: The SplitTree lives in RefCell inside ivars, which is pinned in the
        // ObjC heap object. We only mutate the tree in the render timer (pane removal),
        // never while an event handler holds this reference.
        Some(unsafe { &*(pane as *const Pane) })
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

    /// Hit-test: find which pane is under the mouse event.
    fn pane_at_event(&self, event: &NSEvent) -> Option<(&Pane, PaneViewport)> {
        let tree_ref = self.ivars().tree.borrow();
        let tree = tree_ref.as_ref()?;
        let (px, py) = self.event_to_pixel(event);
        let (pane, vp) = tree.hit_test(px, py, self.drawable_viewport())?;
        // SAFETY: same reasoning as focused_pane — single-threaded, no mutation during event handling
        Some((unsafe { &*(pane as *const Pane) }, vp))
    }

    /// Convert an NSEvent to a grid position within the given pane/viewport.
    fn pixel_to_grid_in(&self, event: &NSEvent, pane: &Pane, vp: &PaneViewport) -> Option<GridPos> {
        let renderer = self.ivars().renderer.get()?;
        let (pixel_x, pixel_y) = self.event_to_pixel(event);

        let renderer_r = renderer.read();
        let (cell_w, cell_h) = renderer_r.cell_size();
        drop(renderer_r);

        // Coordinates relative to the pane's viewport
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

    /// Split the focused pane in the given direction.
    fn do_split(&self, direction: SplitDirection) {
        let config = match self.ivars().config.get() {
            Some(c) => c,
            None => return,
        };

        let focused_id = self.ivars().focused.get();

        // Compute the viewport the focused pane currently has + grab its cwd
        let (current_vp, focused_cwd) = {
            let tree_ref = self.ivars().tree.borrow();
            let tree = match tree_ref.as_ref() {
                Some(t) => t,
                None => return,
            };
            let vp = match tree.viewport_for_pane(focused_id, self.drawable_viewport()) {
                Some(vp) => vp,
                None => return,
            };
            let cwd = tree.pane(focused_id).and_then(|p| p.cwd());
            (vp, cwd)
        };

        // Compute cols/rows for the new (half-size) pane
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

        let new_pane = match Pane::spawn(cols, rows, config, focused_cwd.as_deref()) {
            Ok(p) => p,
            Err(e) => {
                log::error!("failed to spawn pane for split: {}", e);
                return;
            }
        };
        let new_id = new_pane.id;

        // Perform the split: take tree, transform, put back
        let mut tree_opt = self.ivars().tree.borrow_mut();
        if let Some(tree) = tree_opt.take() {
            *tree_opt = Some(tree.with_split(focused_id, new_pane, direction));
        }
        drop(tree_opt);

        // Focus the new pane
        self.ivars().focused.set(new_id);

        // Resize all panes to match their new viewports
        self.resize_all_panes();
    }

    /// Close the focused pane.
    fn do_close_pane(&self) {
        let focused_id = self.ivars().focused.get();
        let tree_opt = self.ivars().tree.borrow();
        // Don't close the last pane — use Cmd+Q to quit
        if let Some(tree) = tree_opt.as_ref() {
            if matches!(tree, SplitTree::Leaf(_)) {
                return;
            }
        }
        drop(tree_opt);
        let mut tree_opt = self.ivars().tree.borrow_mut();
        if let Some(tree) = tree_opt.take() {
            *tree_opt = tree.remove_pane(focused_id);
        }
        if let Some(tree) = tree_opt.as_ref() {
            self.ivars().focused.set(tree.first_pane().id);
            drop(tree_opt);
            self.resize_all_panes();
        }
    }

    /// Navigate focus to an adjacent pane.
    fn do_navigate(&self, dir: NavDirection) {
        let focused_id = self.ivars().focused.get();
        let tree_ref = self.ivars().tree.borrow();
        if let Some(tree) = tree_ref.as_ref() {
            if let Some(neighbor_id) = tree.neighbor(focused_id, dir, self.drawable_viewport()) {
                self.ivars().focused.set(neighbor_id);
                // Mark both old and new pane dirty so dim overlay updates instantly
                if let Some(old) = tree.pane(focused_id) {
                    old.terminal.read().dirty.store(true, std::sync::atomic::Ordering::Relaxed);
                }
                if let Some(new) = tree.pane(neighbor_id) {
                    new.terminal.read().dirty.store(true, std::sync::atomic::Ordering::Relaxed);
                }
            }
        }
    }

    /// Resize all panes to match their current viewports.
    fn resize_all_panes(&self) {
        let renderer = match self.ivars().renderer.get() {
            Some(r) => r,
            None => return,
        };
        let renderer_r = renderer.read();
        let (cell_w, cell_h) = renderer_r.cell_size();
        let status_bar = renderer_r.status_bar_enabled();
        drop(renderer_r);

        let drawable_vp = self.drawable_viewport();
        let tree_ref = self.ivars().tree.borrow();
        if let Some(tree) = tree_ref.as_ref() {
            tree.for_each_pane_with_viewport(drawable_vp, &mut |pane, vp| {
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
            self.ivars().last_scale.set(scale);
            renderer.write().rebuild_atlas(scale);
        }

        self.resize_all_panes();
    }

    pub fn setup_metal(&self, _mtm: MainThreadMarker, config: &Config) {
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

        let cols = config.terminal.columns;
        let rows = config.terminal.rows;
        let pane = Pane::spawn(cols, rows, config, None).expect("failed to spawn pane");
        let pane_id = pane.id;
        let renderer = Arc::new(parking_lot::RwLock::new(
            Renderer::new(&device, &layer, pane.terminal.clone(), scale, config),
        ));

        self.ivars().renderer.set(renderer).ok();
        self.ivars().config.set(config.clone()).ok();
        *self.ivars().tree.borrow_mut() = Some(SplitTree::Leaf(pane));
        self.ivars().focused.set(pane_id);

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

        let timer = unsafe {
            NSTimer::scheduledTimerWithTimeInterval_repeats_block(
                1.0 / fps as f64,
                true,
                &RcBlock::new(move |_timer: NonNull<NSTimer>| {
                    let ivars = &*ivars;

                    // --- Reap exited panes ---
                    let exited_ids = {
                        let tree_ref = ivars.tree.borrow();
                        match tree_ref.as_ref() {
                            Some(tree) => tree.exited_pane_ids(),
                            None => return,
                        }
                    };
                    if !exited_ids.is_empty() {
                        let mut tree_opt = ivars.tree.borrow_mut();
                        for id in &exited_ids {
                            if let Some(tree) = tree_opt.take() {
                                *tree_opt = tree.remove_pane(*id);
                            }
                        }
                        // If focused pane was removed, move focus to first remaining pane
                        let focused_id = ivars.focused.get();
                        if exited_ids.contains(&focused_id) {
                            if let Some(ref tree) = *tree_opt {
                                ivars.focused.set(tree.first_pane().id);
                            }
                        }
                        drop(tree_opt);
                    }

                    // If no panes left, terminate
                    {
                        let tree_ref = ivars.tree.borrow();
                        if tree_ref.is_none() {
                            let mtm = MainThreadMarker::new_unchecked();
                            let app = NSApplication::sharedApplication(mtm);
                            app.terminate(None);
                            return;
                        }
                    }

                    // Build pane render list for multi-pane rendering
                    let focused_id = ivars.focused.get();
                    let (pane_data, pty_ptr, focus_reporting) = {
                        let tree_ref = ivars.tree.borrow();
                        let tree = tree_ref.as_ref().unwrap();

                        // Collect pane render data
                        let mut pane_data: Vec<(Arc<parking_lot::RwLock<crate::terminal::TerminalState>>, PaneViewport, bool, bool)> = Vec::new();
                        let drawable_vp = PaneViewport {
                            x: 0.0,
                            y: 0.0,
                            width: layer.drawableSize().width as f32,
                            height: layer.drawableSize().height as f32,
                        };
                        tree.for_each_pane_with_viewport(drawable_vp, &mut |pane, vp| {
                            pane_data.push((
                                pane.terminal.clone(),
                                vp,
                                pane.is_ready(),
                                pane.id == focused_id,
                            ));
                        });

                        // Get focused pane PTY for focus reporting
                        let focused = tree.pane(focused_id);
                        let pty_ptr = focused.map(|p| &p.pty as *const crate::terminal::pty::Pty);
                        let focus_reporting = focused.map_or(false, |p| p.terminal.read().focus_reporting);

                        (pane_data, pty_ptr, focus_reporting)
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

                    // Collect split separators
                    let separators = {
                        let tree_ref = ivars.tree.borrow();
                        if let Some(tree) = tree_ref.as_ref() {
                            let drawable_vp = PaneViewport {
                                x: 0.0,
                                y: 0.0,
                                width: layer.drawableSize().width as f32,
                                height: layer.drawableSize().height as f32,
                            };
                            let mut seps = Vec::new();
                            tree.collect_separators(drawable_vp, &mut seps);
                            seps
                        } else {
                            Vec::new()
                        }
                    };

                    renderer.write().render_panes(&layer, &pane_data, &separators);
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
        | NSWindowStyleMask::Resizable;

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
    window.setMinSize(CGSize {
        width: 200.0,
        height: 150.0,
    });

    let view = KovaView::new(mtm, content_rect);
    view.setup_metal(mtm, config);
    window.setContentView(Some(&view));
    window.makeFirstResponder(Some(&view));

    window
}
