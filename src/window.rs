use block2::RcBlock;
use objc2::rc::Retained;
use objc2::{define_class, msg_send, DefinedClass, MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{NSBackingStoreType, NSEvent, NSEventModifierFlags, NSPasteboard, NSWindow, NSWindowStyleMask};
use objc2_core_foundation::{CGPoint, CGRect, CGSize};
use objc2_foundation::{NSObjectProtocol, NSRunLoop, NSRunLoopCommonModes, NSString, NSTimer};
use objc2_metal::MTLCreateSystemDefaultDevice;
use objc2_quartz_core::CAMetalLayer;
use std::cell::{Cell, OnceCell};
use std::ptr::NonNull;
use std::sync::Arc;

use crate::input;
use crate::renderer::Renderer;
use crate::terminal::pty::Pty;
use crate::terminal::TerminalState;

pub struct KovaViewIvars {
    renderer: OnceCell<Arc<parking_lot::RwLock<Renderer>>>,
    terminal: OnceCell<Arc<parking_lot::RwLock<TerminalState>>>,
    pty: OnceCell<Pty>,
    metal_layer: OnceCell<Retained<CAMetalLayer>>,
    scroll_accumulator: Cell<f64>,
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
    }
);

impl KovaView {
    fn new(mtm: MainThreadMarker, frame: CGRect) -> Retained<Self> {
        let this = mtm.alloc::<Self>().set_ivars(KovaViewIvars {
            renderer: OnceCell::new(),
            terminal: OnceCell::new(),
            pty: OnceCell::new(),
            metal_layer: OnceCell::new(),
            scroll_accumulator: Cell::new(0.0),
        });
        unsafe { msg_send![super(this), initWithFrame: frame] }
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

        let (cell_w, cell_h) = renderer.read().cell_size();
        let pixel_w = (frame.size.width * scale) as f32;
        let pixel_h = (frame.size.height * scale) as f32;
        let cols = (pixel_w / cell_w).floor().max(1.0) as u16;
        let rows = (pixel_h / cell_h).floor().max(1.0) as u16;

        let mut term = terminal.write();
        if cols != term.cols || rows != term.rows {
            term.resize(cols, rows);
            drop(term);
            if let Some(pty) = self.ivars().pty.get() {
                pty.resize(cols, rows);
            }
        }
    }

    pub fn setup_metal(&self, _mtm: MainThreadMarker) {
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

        let terminal = Arc::new(parking_lot::RwLock::new(TerminalState::new(80, 24)));
        let renderer = Arc::new(parking_lot::RwLock::new(Renderer::new(&device, &layer, terminal.clone())));
        let pty = Pty::spawn(80, 24, terminal.clone()).expect("failed to spawn PTY");

        self.ivars().renderer.set(renderer).ok();
        self.ivars().terminal.set(terminal).ok();
        self.ivars().pty.set(pty).ok();

        self.start_render_timer();
    }

    fn start_render_timer(&self) {
        let renderer = self.ivars().renderer.get().unwrap().clone();
        let terminal = self.ivars().terminal.get().unwrap().clone();
        let layer = self.ivars().metal_layer.get().expect("metal_layer not initialized").clone();

        let timer = unsafe {
            NSTimer::scheduledTimerWithTimeInterval_repeats_block(
                1.0 / 60.0,
                true,
                &RcBlock::new(move |_timer: NonNull<NSTimer>| {
                    renderer.write().render(&layer, &terminal);
                }),
            )
        };
        let run_loop = NSRunLoop::currentRunLoop();
        unsafe { run_loop.addTimer_forMode(&timer, NSRunLoopCommonModes) };
    }

}

pub fn create_window(mtm: MainThreadMarker) -> Retained<NSWindow> {
    let content_rect = CGRect {
        origin: CGPoint { x: 200.0, y: 200.0 },
        size: CGSize {
            width: 800.0,
            height: 600.0,
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
    view.setup_metal(mtm);
    window.setContentView(Some(&view));
    window.makeFirstResponder(Some(&view));

    window
}
