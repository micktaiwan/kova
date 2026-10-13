//! The AppKit half of terminal input, written once: turns macOS events —
//! `keyDown:`, the `NSTextInputClient` calls (`insertText:`,
//! `setMarkedText:`, `doCommandBySelector:`…), mouse and scroll-wheel
//! events — into the typed API of `crate::input` on a `Surface`.
//!
//! A view owns one `AppKitInput` and, once it has handled whatever belongs to
//! the window (overlays, tab shortcuts, separators…), hands each event to it
//! with the surface it targets. AppKit still requires the responder and
//! text-input methods on the view class itself; each of them is a one-line
//! delegation to this component.

use std::cell::{Cell, RefCell};

use objc2::rc::Retained;
use objc2::{msg_send, Message};
use objc2_app_kit::{NSEvent, NSEventModifierFlags, NSEventPhase, NSPasteboard, NSView};
use objc2_core_foundation::{CGPoint, CGRect, CGSize};
use objc2_foundation::{NSArray, NSRange, NSString};

use crate::input::{KeyEvent, KeyResult, Mods, MouseAction, MouseEvent, MouseResult, ScrollEvent};
use crate::keybindings::{KeyCombo, Keybindings};
use crate::surface::Surface;

/// Axis a trackpad gesture is locked to, so a vertical swipe does not drift
/// sideways (and the reverse).
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum ScrollAxisLock {
    None,
    Vertical,
    Horizontal,
}

/// Where a surface sits in the view, in pixels from the view's top-left: its
/// left and top edges, and the horizontal padding before its first column.
/// Kept apart (not pre-added) so the point → cell arithmetic stays the exact
/// f32 expression it has always been: `px - x - pad_x`.
#[derive(Clone, Copy, Debug)]
pub struct PaneOrigin {
    pub x: f32,
    pub y: f32,
    pub pad_x: f32,
}

/// Per-view input state: the text being composed, the key event macOS text
/// input is working on, the selection auto-scroll and the scroll axis lock.
pub struct AppKitInput {
    /// Marked text from IME composition (dead keys, etc.)
    marked_text: RefCell<Option<String>>,
    /// Current NSEvent being processed by interpretKeyEvents, so doCommandBySelector can access it.
    /// SAFETY: pointer is only live during the synchronous keyDown → interpretKeyEvents → doCommandBySelector
    /// call chain, and cleared immediately after. Never accessed outside that stack frame.
    current_event: Cell<Option<*const NSEvent>>,
    /// Auto-scroll speed during drag selection (lines/tick, positive = down, negative = up, 0 = inactive)
    autoscroll_speed: Cell<i32>,
    /// Axis lock for trackpad scroll gestures (prevents cross-axis drift).
    scroll_axis_lock: Cell<ScrollAxisLock>,
}

impl Default for AppKitInput {
    fn default() -> Self {
        Self::new()
    }
}

impl AppKitInput {
    pub fn new() -> Self {
        AppKitInput {
            marked_text: RefCell::new(None),
            current_event: Cell::new(None),
            autoscroll_speed: Cell::new(0),
            scroll_axis_lock: Cell::new(ScrollAxisLock::None),
        }
    }

    // --- Keys and text input ---

    /// `keyDown:` for `surface`, once the host has taken its own keys. A key
    /// the surface hands back for composing goes through macOS text input
    /// (`interpretKeyEvents:` on `view`), which calls back into the view's
    /// `insertText:` / `setMarkedText:` / `doCommandBySelector:`.
    pub fn key_down(&self, view: &NSView, event: &NSEvent, key: &KeyEvent, surface: &Surface, keybindings: &Keybindings) -> KeyResult {
        let result = surface.key(key, keybindings);
        if result == KeyResult::Compose {
            // Normal path: macOS text input (dead keys, IME)
            self.current_event.set(Some(event as *const NSEvent));
            let event_retained: Retained<NSEvent> = event.retain();
            let events = NSArray::from_retained_slice(&[event_retained]);
            view.interpretKeyEvents(&events);
            self.current_event.set(None);
        }
        result
    }

    /// `insertText:replacementRange:` — committed text goes to `surface`.
    ///
    /// # Safety
    /// `string` must be the NSString or NSAttributedString AppKit passed.
    pub unsafe fn insert_text(&self, string: &objc2::runtime::AnyObject, surface: Option<&Surface>) {
        let text = unsafe { nsstring_from_input(string) };
        // Clear marked text
        *self.marked_text.borrow_mut() = None;
        // Write to PTY
        if let Some(surface) = surface {
            surface.text(&text);
        }
    }

    /// `doCommandBySelector:` — the key text input did not turn into text is
    /// encoded for the surface. `surface` is only asked for while a key is
    /// being interpreted.
    pub fn do_command_by_selector<'a>(&self, surface: impl FnOnce() -> Option<&'a Surface>, keybindings: Option<&Keybindings>) {
        if let Some(event_ptr) = self.current_event.get() {
            let event = unsafe { &*event_ptr };
            if let Some(surface) = surface() {
                if let Some(kb) = keybindings {
                    surface.send_key(&key_event(event), kb);
                }
            }
        }
    }

    /// `setMarkedText:selectedRange:replacementRange:`.
    ///
    /// # Safety
    /// `string` must be the NSString or NSAttributedString AppKit passed.
    pub unsafe fn set_marked_text(&self, string: &objc2::runtime::AnyObject) {
        let text = unsafe { nsstring_from_input(string) };
        self.set_preedit(&text);
    }

    /// The text being composed, as plain text: empty ends the composition.
    /// What `setMarkedText:` records, and a host's typed `preedit` call.
    pub fn set_preedit(&self, text: &str) {
        *self.marked_text.borrow_mut() = if text.is_empty() { None } else { Some(text.to_string()) };
    }

    /// `unmarkText`.
    pub fn unmark_text(&self) {
        *self.marked_text.borrow_mut() = None;
    }

    /// `hasMarkedText`.
    pub fn has_marked_text(&self) -> bool {
        self.marked_text.borrow().is_some()
    }

    /// `markedRange`.
    pub fn marked_range(&self) -> NSRange {
        if self.marked_text.borrow().is_some() {
            NSRange { location: 0, length: 1 }
        } else {
            NSRange { location: objc2_foundation::NSNotFound as usize, length: 0 }
        }
    }

    /// `selectedRange`: no selection is exposed to text input.
    pub fn selected_range(&self) -> NSRange {
        NSRange { location: objc2_foundation::NSNotFound as usize, length: 0 }
    }

    /// `firstRectForCharacterRange:actualRange:` — where macOS opens the
    /// accent window: the view's origin in screen coordinates.
    pub fn first_rect_for_character_range(&self, view: &NSView) -> CGRect {
        let frame = view.frame();
        let window_frame = if let Some(window) = view.window() {
            window.frame()
        } else {
            return CGRect::ZERO;
        };
        CGRect {
            origin: CGPoint {
                x: window_frame.origin.x + frame.origin.x,
                y: window_frame.origin.y + frame.origin.y,
            },
            size: CGSize { width: 0.0, height: 0.0 },
        }
    }

    /// `characterIndexForPoint:`.
    pub fn character_index_for_point(&self) -> usize {
        objc2_foundation::NSNotFound as usize
    }

    // --- Mouse ---

    /// A mouse event over `surface`, whose position in the view is `origin`;
    /// `cell` is the cell size in pixels. A drag records the selection
    /// auto-scroll speed the host's tick reads back (`autoscroll_speed`).
    pub fn mouse(&self, view: &NSView, event: &NSEvent, action: MouseAction, surface: &Surface, origin: PaneOrigin, cell: (f32, f32)) -> MouseResult {
        let (px, py) = event_to_pixel(view, event);
        // button 0=left, 1=middle, 2=right. Read only on the events that
        // always carried it: a move reports "no button".
        let button = match action {
            MouseAction::Move => 0,
            _ => (event.buttonNumber() as u8).min(2),
        };
        let click_count = if action == MouseAction::Press { event.clickCount() } else { 0 };
        let ev = MouseEvent {
            action,
            button,
            mods: mods(event),
            x: px - origin.x - origin.pad_x,
            y: py - origin.y,
            click_count,
        };
        self.typed_mouse(&ev, surface, cell)
    }

    /// A mouse event already in the typed form, for a host that has no
    /// `NSEvent` to hand over (the C API). Same recording of the drag
    /// auto-scroll speed as `mouse`.
    pub fn typed_mouse(&self, ev: &MouseEvent, surface: &Surface, cell: (f32, f32)) -> MouseResult {
        let result = surface.mouse(ev, cell);
        if let MouseResult::Autoscroll(speed) = result {
            self.autoscroll_speed.set(speed);
        }
        result
    }

    /// Mouse up: whatever the release lands on, the selection stops scrolling.
    pub fn end_drag(&self) {
        self.autoscroll_speed.set(0);
    }

    /// Lines per tick to auto-scroll the dragged selection, 0 when idle.
    pub fn autoscroll_speed(&self) -> i32 {
        self.autoscroll_speed.get()
    }

    /// End of a selection on `surface`: copy what it selected.
    pub fn finish_selection(&self, surface: &Surface) {
        let text = surface.finish_selection();
        if let Some(text) = text {
            copy_to_pasteboard(&text);
        }
    }

    // --- Scroll ---

    /// Update the trackpad axis lock from a scroll event's phases and return it.
    pub fn scroll_axis_lock(&self, event: &NSEvent) -> ScrollAxisLock {
        // Phase-based axis lock (trackpad only)
        if event.hasPreciseScrollingDeltas() {
            let phase = event.phase();
            let momentum = event.momentumPhase();

            if phase == NSEventPhase::Began {
                let dy = event.scrollingDeltaY().abs();
                let dx = event.scrollingDeltaX().abs();
                self.scroll_axis_lock.set(if dy >= dx {
                    ScrollAxisLock::Vertical
                } else {
                    ScrollAxisLock::Horizontal
                });
            } else if phase.intersects(NSEventPhase::Ended | NSEventPhase::Cancelled)
                && momentum == NSEventPhase::None
            {
                self.scroll_axis_lock.set(ScrollAxisLock::None);
            } else if momentum.intersects(NSEventPhase::Ended | NSEventPhase::Cancelled) {
                self.scroll_axis_lock.set(ScrollAxisLock::None);
            }
        }
        self.scroll_axis_lock.get()
    }

    /// The vertical part of a scroll-wheel event over `surface`.
    #[allow(clippy::too_many_arguments)]
    pub fn scroll(&self, view: &NSView, event: &NSEvent, surface: &Surface, origin: PaneOrigin, cell: (f32, f32), sensitivity: f64, log_tab: usize) {
        let (px, py) = event_to_pixel(view, event);
        let ev = ScrollEvent {
            dy: event.scrollingDeltaY(),
            precise: event.hasPreciseScrollingDeltas(),
            mods: mods(event),
            x: px - origin.x - origin.pad_x,
            y: py - origin.y,
        };
        surface.scroll(&ev, cell, sensitivity, log_tab);
    }
}

/// The typed form of a macOS key event.
pub fn key_event(event: &NSEvent) -> KeyEvent {
    KeyEvent {
        combo: KeyCombo::from_event(event),
        chars: event.characters().map(|s| s.to_string()),
        unmod: event.charactersIgnoringModifiers().map(|s| s.to_string()),
    }
}

/// The modifier keys of a macOS event.
fn mods(event: &NSEvent) -> Mods {
    let flags = event.modifierFlags();
    Mods {
        shift: flags.contains(NSEventModifierFlags::Shift),
        ctrl: flags.contains(NSEventModifierFlags::Control),
        option: flags.contains(NSEventModifierFlags::Option),
    }
}

fn backing_scale(view: &NSView) -> f32 {
    view.window().map_or(2.0, |w| w.backingScaleFactor()) as f32
}

/// Convert an NSEvent location to Metal pixel coordinates (origin top-left).
pub fn event_to_pixel(view: &NSView, event: &NSEvent) -> (f32, f32) {
    let location = event.locationInWindow();
    let local: CGPoint = unsafe { msg_send![view, convertPoint: location, fromView: std::ptr::null::<objc2::runtime::AnyObject>()] };
    let frame = view.frame();
    let scale = backing_scale(view);
    let pixel_x = local.x as f32 * scale;
    let pixel_y = (frame.size.height as f32 - local.y as f32) * scale;
    (pixel_x, pixel_y)
}

/// Extract a String from an NSTextInputClient input object (NSString or NSAttributedString).
unsafe fn nsstring_from_input(obj: &objc2::runtime::AnyObject) -> String {
    let responds: bool = unsafe { msg_send![obj, respondsToSelector: objc2::sel!(string)] };
    if responds {
        let ns_str: *const NSString = unsafe { msg_send![obj, string] };
        unsafe { &*ns_str }.to_string()
    } else {
        let ns_str: &NSString = unsafe { &*(obj as *const objc2::runtime::AnyObject as *const NSString) };
        ns_str.to_string()
    }
}

/// Copy text to the system pasteboard.
pub fn copy_to_pasteboard(text: &str) {
    let pasteboard = NSPasteboard::generalPasteboard();
    pasteboard.clearContents();
    let ns_str = NSString::from_str(text);
    unsafe {
        pasteboard.setString_forType(&ns_str, objc2_app_kit::NSPasteboardTypeString);
    }
}
