use block2::RcBlock;
use objc2::rc::Retained;
use objc2::{define_class, msg_send, DefinedClass, MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{NSApplication, NSBackingStoreType, NSEvent, NSEventModifierFlags, NSPasteboard, NSWindow, NSWindowStyleMask};
use objc2_core_foundation::{CGPoint, CGRect, CGSize};
use objc2_foundation::{NSObjectProtocol, NSRunLoop, NSRunLoopCommonModes, NSString, NSTimer};
use objc2_metal::MTLCreateSystemDefaultDevice;
use objc2_quartz_core::CAMetalLayer;
use std::cell::{Cell, OnceCell};
use std::ptr::NonNull;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use crate::config::Config;
use crate::input;
use crate::renderer::Renderer;
use crate::terminal::pty::Pty;
use crate::terminal::{GridPos, Selection, TerminalState};

pub struct KovaViewIvars {
    renderer: OnceCell<Arc<parking_lot::RwLock<Renderer>>>,
    terminal: OnceCell<Arc<parking_lot::RwLock<TerminalState>>>,
    pty: OnceCell<Pty>,
    metal_layer: OnceCell<Retained<CAMetalLayer>>,
    shell_exited: OnceCell<Arc<AtomicBool>>,
    shell_ready: OnceCell<Arc<AtomicBool>>,
    scroll_accumulator: Cell<f64>,
    last_scale: Cell<f64>,
    last_focused: Cell<bool>,
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
            if let Some(pty) = self.ivars().pty.get() {
                input::handle_key_event(event, pty);
            }
        }

        #[unsafe(method(performKeyEquivalent:))]
        fn perform_key_equivalent(&self, event: &NSEvent) -> objc2::runtime::Bool {
            let modifiers = event.modifierFlags();
            if modifiers.contains(NSEventModifierFlags::Command) {
                let chars = event.charactersIgnoringModifiers();
                if let Some(chars) = chars {
                    if chars.to_string() == "c" {
                        if let Some(terminal) = self.ivars().terminal.get() {
                            let mut term = terminal.write();
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
                    if chars.to_string() == "v" {
                        if let Some(pty) = self.ivars().pty.get() {
                            let pasteboard = NSPasteboard::generalPasteboard();
                            if let Some(text) = unsafe { pasteboard.stringForType(objc2_app_kit::NSPasteboardTypeString) } {
                                pty.write(text.to_string().as_bytes());
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
            if let Some(terminal) = self.ivars().terminal.get() {
                let dy = event.scrollingDeltaY();
                let lines = if event.hasPreciseScrollingDeltas() {
                    let acc = self.ivars().scroll_accumulator.get() + dy;
                    let discrete = acc as i32;
                    self.ivars().scroll_accumulator.set(acc - discrete as f64);
                    discrete
                } else {
                    dy as i32
                };
                if lines != 0 {
                    let mut term = terminal.write();
                    term.scroll(lines);
                    // Reset accumulator when hitting bounds to avoid residual drift
                    if term.scroll_offset() == 0 {
                        self.ivars().scroll_accumulator.set(0.0);
                    }
                }
            }
        }

        #[unsafe(method(mouseDown:))]
        fn mouse_down(&self, event: &NSEvent) {
            if let Some(pos) = self.pixel_to_grid(event) {
                if let Some(terminal) = self.ivars().terminal.get() {
                    let mut term = terminal.write();
                    term.selection = Some(Selection { anchor: pos, end: pos });
                    term.dirty.store(true, std::sync::atomic::Ordering::Relaxed);
                }
            }
        }

        #[unsafe(method(mouseDragged:))]
        fn mouse_dragged(&self, event: &NSEvent) {
            if let Some(pos) = self.pixel_to_grid(event) {
                if let Some(terminal) = self.ivars().terminal.get() {
                    let mut term = terminal.write();
                    if let Some(ref mut sel) = term.selection {
                        sel.end = pos;
                        term.dirty.store(true, std::sync::atomic::Ordering::Relaxed);
                    }
                }
            }
        }

        #[unsafe(method(mouseUp:))]
        fn mouse_up(&self, _event: &NSEvent) {
            if let Some(terminal) = self.ivars().terminal.get() {
                let term = terminal.read();
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
            terminal: OnceCell::new(),
            pty: OnceCell::new(),
            metal_layer: OnceCell::new(),
            shell_exited: OnceCell::new(),
            shell_ready: OnceCell::new(),
            scroll_accumulator: Cell::new(0.0),
            last_scale: Cell::new(0.0),
            last_focused: Cell::new(true),
        });
        unsafe { msg_send![super(this), initWithFrame: frame] }
    }

    fn pixel_to_grid(&self, event: &NSEvent) -> Option<GridPos> {
        let renderer = self.ivars().renderer.get()?;
        let terminal = self.ivars().terminal.get()?;

        let location = event.locationInWindow();
        let local: CGPoint = unsafe { msg_send![self, convertPoint: location, fromView: std::ptr::null::<objc2::runtime::AnyObject>()] };
        let frame = self.frame();
        let scale = self.window().map_or(2.0, |w| w.backingScaleFactor());

        let renderer_r = renderer.read();
        let (cell_w, cell_h) = renderer_r.cell_size();
        drop(renderer_r);

        // Flip Y (AppKit origin is bottom-left, Metal origin is top-left)
        let pixel_x = local.x as f32 * scale as f32;
        let pixel_y = (frame.size.height - local.y) as f32 * scale as f32;

        let term = terminal.read();
        let y_offset = term.y_offset_rows();
        let col = (pixel_x / cell_w).floor() as i32;
        let visible_row = (pixel_y / cell_h).floor() as i32 - y_offset as i32;

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

    fn handle_resize(&self) {
        let Some(layer) = self.ivars().metal_layer.get() else { return };
        let Some(renderer) = self.ivars().renderer.get() else { return };
        let Some(terminal) = self.ivars().terminal.get() else { return };

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

        let renderer_state = renderer.read();
        let (cell_w, cell_h) = renderer_state.cell_size();
        let status_bar = renderer_state.status_bar_enabled();
        drop(renderer_state);

        let pixel_w = (frame.size.width * scale) as f32;
        let pixel_h = (frame.size.height * scale) as f32;
        let cols = (pixel_w / cell_w).floor().max(1.0) as u16;
        let usable_h = if status_bar {
            pixel_h - cell_h // Reserve 1 row for the status bar
        } else {
            pixel_h
        };
        let rows = (usable_h / cell_h).floor().max(1.0) as u16;

        let mut term = terminal.write();
        if cols != term.cols || rows != term.rows {
            log::trace!("resize: {}x{} -> {}x{}", term.cols, term.rows, cols, rows);
            term.resize(cols, rows);
            drop(term);
            if let Some(pty) = self.ivars().pty.get() {
                pty.resize(cols, rows);
            }
        }
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
        let terminal = Arc::new(parking_lot::RwLock::new(
            TerminalState::new(cols, rows, config.terminal.scrollback, config.colors.foreground, config.colors.background),
        ));
        let renderer = Arc::new(parking_lot::RwLock::new(
            Renderer::new(&device, &layer, terminal.clone(), scale, config),
        ));
        let shell_exited = Arc::new(AtomicBool::new(false));
        let shell_ready = Arc::new(AtomicBool::new(false));
        let pty = Pty::spawn(cols, rows, terminal.clone(), shell_exited.clone(), shell_ready.clone())
            .expect("failed to spawn PTY");

        self.ivars().renderer.set(renderer).ok();
        self.ivars().terminal.set(terminal).ok();
        self.ivars().pty.set(pty).ok();
        self.ivars().shell_exited.set(shell_exited).ok();
        self.ivars().shell_ready.set(shell_ready).ok();

        self.start_render_timer(config.terminal.fps);
    }

    fn start_render_timer(&self, fps: u32) {
        let renderer = self.ivars().renderer.get().unwrap().clone();
        let terminal = self.ivars().terminal.get().unwrap().clone();
        let layer = self.ivars().metal_layer.get().expect("metal_layer not initialized").clone();
        let shell_exited = self.ivars().shell_exited.get().unwrap().clone();
        let shell_ready = self.ivars().shell_ready.get().unwrap().clone();
        let terminal_for_focus = terminal.clone();
        let pty_ptr = self.ivars().pty.get().unwrap() as *const Pty;
        let last_focused = &self.ivars().last_focused as *const Cell<bool>;

        let last_title: std::cell::RefCell<Option<String>> = std::cell::RefCell::new(None);
        let window_for_title: std::cell::OnceCell<Retained<NSWindow>> = std::cell::OnceCell::new();

        let timer = unsafe {
            NSTimer::scheduledTimerWithTimeInterval_repeats_block(
                1.0 / fps as f64,
                true,
                &RcBlock::new(move |_timer: NonNull<NSTimer>| {
                    if shell_exited.load(Ordering::Relaxed) {
                        let mtm = MainThreadMarker::new_unchecked();
                        let app = NSApplication::sharedApplication(mtm);
                        app.terminate(None);
                        return;
                    }

                    // Focus reporting (DEC mode 1004)
                    let mtm = MainThreadMarker::new_unchecked();
                    let app = NSApplication::sharedApplication(mtm);
                    let focused = app.isActive();
                    let prev = (*last_focused).get();
                    if focused != prev {
                        (*last_focused).set(focused);
                        let term = terminal_for_focus.read();
                        if term.focus_reporting {
                            drop(term);
                            let seq = if focused { b"\x1b[I" as &[u8] } else { b"\x1b[O" };
                            (*pty_ptr).write(seq);
                        }
                    }

                    // Update NSWindow title from OSC 0/2
                    {
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

                    renderer.write().render(&layer, &terminal, shell_ready.load(Ordering::Relaxed));
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
