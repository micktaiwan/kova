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

    /// `firstRectForCharacterRange:actualRange:` for a view that shows one
    /// surface alone, at any depth in its window and flipped or not (a
    /// library pane, `embed::KovaPaneView`): the terminal cursor's cell in
    /// screen coordinates, so the accent window opens on the caret. `cell` is
    /// the cell size and `pad_x` the left padding, in pixels.
    pub fn cursor_rect_on_screen(&self, view: &NSView, surface: &Surface, cell: (f32, f32), pad_x: f32) -> CGRect {
        let Some(window) = view.window() else { return CGRect::ZERO };
        let bounds = view.bounds();
        let geometry = CellGeometry {
            scale: window.backingScaleFactor(),
            cell_w: cell.0,
            cell_h: cell.1,
            pad_x,
            view_height: bounds.size.height,
            flipped: view.isFlipped(),
        };
        let in_view = cursor_rect_in_view(surface, &geometry);
        let in_window: CGRect =
            unsafe { msg_send![view, convertRect: in_view, toView: std::ptr::null::<objc2::runtime::AnyObject>()] };
        window.convertRectToScreen(in_window)
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

/// What `cursor_cell_rect` needs to know about the view a surface fills.
#[derive(Clone, Copy, Debug)]
pub struct CellGeometry {
    /// Backing scale: pixels per point.
    pub scale: f64,
    /// Cell size and left padding, in pixels.
    pub cell_w: f32,
    pub cell_h: f32,
    pub pad_x: f32,
    /// The view's height in points, and whether its y axis points down.
    pub view_height: f64,
    pub flipped: bool,
}

/// The cell at column `col` of on-screen row `screen_row` (the cursor's row
/// plus the scrollback offset, as the renderer draws it), in the view's own
/// coordinates (points). Rows are placed as the renderer places them: from
/// the top, `y_offset` pixels down (`content_y_offset`). A cursor scrolled
/// out of sight is clamped to the nearest visible row, so the accent window
/// stays on the pane.
pub fn cursor_cell_rect(col: u16, screen_row: i32, rows: u16, y_offset: f32, g: &CellGeometry) -> CGRect {
    let row = screen_row.clamp(0, rows.max(1) as i32 - 1);
    let scale = if g.scale > 0.0 { g.scale } else { 1.0 };
    let x_px = g.pad_x as f64 + col as f64 * g.cell_w as f64;
    let top_px = y_offset as f64 + row as f64 * g.cell_h as f64;
    let (w, h) = (g.cell_w as f64 / scale, g.cell_h as f64 / scale);
    let top = top_px / scale;
    let y = if g.flipped { top } else { g.view_height - top - h };
    CGRect { origin: CGPoint { x: x_px / scale, y }, size: CGSize { width: w, height: h } }
}

/// The terminal cursor's cell in the view's own coordinates (points), where
/// the renderer draws it: rows from the top, shell output pushed down by the
/// bottom gravity (`TerminalState::y_offset_rows`, `content_y_offset`), the
/// scrollback offset applied.
pub fn cursor_rect_in_view(surface: &Surface, g: &CellGeometry) -> CGRect {
    let t = surface.terminal.read();
    let y_offset = crate::renderer::pane_renderer::content_y_offset(
        t.rows,
        t.y_offset_rows(),
        g.cell_h,
        (g.view_height * g.scale) as f32,
    );
    cursor_cell_rect(t.cursor_x, t.scroll_offset() + t.cursor_y as i32, t.rows, y_offset, g)
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

/// The pasteboard's PNG image written to /tmp, and its path: pasted into a
/// terminal, the path is how Claude Code takes an image. None without an image.
pub fn pasted_image_path(pasteboard: &NSPasteboard) -> Option<String> {
    let data = unsafe { pasteboard.dataForType(objc2_app_kit::NSPasteboardTypePNG) }?;
    if data.is_empty() {
        return None;
    }
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let path = format!("/tmp/kova-paste-{timestamp}.png");
    std::fs::write(&path, data.to_vec()).ok().map(|_| path)
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A 200-point-high view at 2x, 10×20 px cells, 8 px padding, no offset.
    fn geometry(flipped: bool) -> CellGeometry {
        CellGeometry { scale: 2.0, cell_w: 10.0, cell_h: 20.0, pad_x: 8.0, view_height: 200.0, flipped }
    }

    fn rect(x: f64, y: f64, w: f64, h: f64) -> CGRect {
        CGRect { origin: CGPoint { x, y }, size: CGSize { width: w, height: h } }
    }

    #[test]
    fn the_cursor_cell_in_a_view_whose_origin_is_bottom_left() {
        // Column 3, row 2: x = (8 + 30) / 2, top = 40 / 2 = 20 pt from the
        // top, so the cell's bottom edge sits 200 - 20 - 10 = 170 pt up.
        assert_eq!(cursor_cell_rect(3, 2, 24, 0.0, &geometry(false)), rect(19.0, 170.0, 5.0, 10.0));
        assert_eq!(cursor_cell_rect(0, 0, 24, 0.0, &geometry(false)), rect(4.0, 190.0, 5.0, 10.0));
    }

    #[test]
    fn the_cursor_cell_in_a_flipped_view() {
        assert_eq!(cursor_cell_rect(3, 2, 24, 0.0, &geometry(true)), rect(19.0, 20.0, 5.0, 10.0));
    }

    #[test]
    fn the_cursor_cell_follows_the_content_offset() {
        // Content pushed 40 px (20 pt) down, as the renderer draws it.
        assert_eq!(cursor_cell_rect(0, 0, 24, 40.0, &geometry(true)), rect(4.0, 20.0, 5.0, 10.0));
    }

    #[test]
    fn a_cursor_scrolled_out_of_sight_stays_on_the_pane() {
        // Scrolled back so far the cursor's row is below the last one.
        assert_eq!(cursor_cell_rect(0, 40, 24, 0.0, &geometry(true)), cursor_cell_rect(0, 23, 24, 0.0, &geometry(true)));
        assert_eq!(cursor_cell_rect(0, -3, 24, 0.0, &geometry(true)), cursor_cell_rect(0, 0, 24, 0.0, &geometry(true)));
    }

    /// A host app's measured case (14/10/2026): a 650×231 pt pane at 2x, 16×32 px
    /// cells, 13 rows plus the status bar, a shell that printed two prompts
    /// and sits on the third line (logical row 2). Shell output is pushed to
    /// the bottom (10 blank rows above it), so the cursor is drawn on the
    /// last grid row, just above the status bar: 23 pt up from the view's
    /// bottom edge — the y the host app measured (pane at y 200, rect at y 223).
    #[test]
    fn the_cursor_rect_follows_the_bottom_gravity_of_shell_output() {
        let (surface, _reader) = Surface::capture(40, 13);
        {
            let mut t = surface.terminal.write();
            for (i, line) in ["$ true", "$ true", "$ "].iter().enumerate() {
                if i > 0 {
                    t.newline();
                    t.carriage_return();
                }
                line.chars().for_each(|c| t.put_char(c));
            }
            assert_eq!((t.cursor_x, t.cursor_y), (2, 2));
            assert_eq!(t.y_offset_rows(), 10, "three lines of shell flow in 13 rows");
        }
        let g = CellGeometry { scale: 2.0, cell_w: 16.0, cell_h: 32.0, pad_x: 8.0, view_height: 231.0, flipped: false };
        // Top of the drawn row: 10 + 2 = 12 rows down = 384 px = 192 pt.
        assert_eq!(cursor_rect_in_view(&surface, &g), rect(20.0, 231.0 - 192.0 - 16.0, 8.0, 16.0));
        // The same pane in a flipped view: measured from the top instead.
        assert_eq!(cursor_rect_in_view(&surface, &CellGeometry { flipped: true, ..g }), rect(20.0, 192.0, 8.0, 16.0));
        // A full-screen program (alt screen) has no gravity: logical row 2.
        surface.terminal.write().in_alt_screen = true;
        assert_eq!(cursor_rect_in_view(&surface, &g), rect(20.0, 231.0 - 32.0 - 16.0, 8.0, 16.0));
    }
}
