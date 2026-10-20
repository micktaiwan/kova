//! The C API of the library: what `include/kova.h` declares, for a host that
//! is not Rust (a Swift app, say). Thin: every function converts its C values,
//! calls `crate::embed`, and converts back. No Rust type crosses the boundary;
//! a pane is named by its id (`KovaPaneId`, the same number its shell sees as
//! `KOVA_PANE_ID`), strings are NUL-terminated UTF-8, and a string handed out
//! is freed with `kova_string_free`.
//!
//! Threading: the pane table is per thread, and a pane with a view is AppKit's,
//! so the contract stated in the header is "one thread, the main thread",
//! except `kova_version` and `kova_string_free`.
//!
//! A panic never unwinds into the host: each entry point catches it, logs it
//! and returns its failure value.

use std::ffi::{c_char, c_int, c_void, CStr, CString};
use std::panic::AssertUnwindSafe;

use objc2::rc::Retained;
use objc2_app_kit::NSView;
use objc2_core_foundation::{CGPoint, CGRect, CGSize};

use crate::embed::{self, PaneOptions};
use crate::input::{KeyEvent, KeyResult, Mods, MouseAction, MouseResult};
use crate::keybindings::KeyCombo;
use crate::terminal::DumpMode;

/// Modifier bits of `KovaKeyEvent::mods` and of the mouse and scroll calls.
pub const KOVA_MOD_SHIFT: u32 = 1 << 0;
pub const KOVA_MOD_CONTROL: u32 = 1 << 1;
pub const KOVA_MOD_OPTION: u32 = 1 << 2;
pub const KOVA_MOD_COMMAND: u32 = 1 << 3;

/// `KovaPaneOptions` of `include/kova.h`.
#[repr(C)]
pub struct KovaPaneOptions {
    pub parent_view: *mut c_void,
    pub cwd: *const c_char,
    pub command: *const c_char,
    pub env: *const *const c_char,
    pub cols: u16,
    pub rows: u16,
}

/// `KovaKeyEvent` of `include/kova.h`: the fields of an `NSEvent` key press.
#[repr(C)]
pub struct KovaKeyEvent {
    pub keycode: u16,
    pub mods: u32,
    pub characters: *const c_char,
    pub characters_ignoring_modifiers: *const c_char,
}

/// Run `f`, turning a panic into `fallback`: unwinding out of an
/// `extern "C"` function would abort the host.
fn guard<R>(name: &str, fallback: R, f: impl FnOnce() -> R) -> R {
    match std::panic::catch_unwind(AssertUnwindSafe(f)) {
        Ok(r) => r,
        Err(_) => {
            log::error!("kova C API: {} panicked", name);
            fallback
        }
    }
}

/// A C string as Rust text; `None` for NULL.
///
/// # Safety
/// `p` is NULL or a NUL-terminated string valid for the call.
unsafe fn text(p: *const c_char) -> Option<String> {
    (!p.is_null()).then(|| unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned())
}

/// Rust text as a C string the host frees with `kova_string_free`. An
/// interior NUL (never produced by the terminal) ends the string there.
fn out(s: String) -> *mut c_char {
    let s = match s.find('\0') {
        Some(end) => s[..end].to_string(),
        None => s,
    };
    CString::new(s).map_or(std::ptr::null_mut(), CString::into_raw)
}

/// The pointer modifiers of `kova_pane_mouse` / `kova_pane_scroll`. The typed
/// mouse input has no Command (`input::Mods`): `KOVA_MOD_COMMAND` is dropped,
/// as the header says.
fn mods(bits: u32) -> Mods {
    Mods {
        shift: bits & KOVA_MOD_SHIFT != 0,
        ctrl: bits & KOVA_MOD_CONTROL != 0,
        option: bits & KOVA_MOD_OPTION != 0,
    }
}

/// The typed key event of a `KovaKeyEvent`, built as `appkit_input::key_event`
/// builds it from an `NSEvent`.
///
/// # Safety
/// The strings of `ev` are NULL or valid NUL-terminated strings.
unsafe fn key_event(ev: &KovaKeyEvent) -> KeyEvent {
    let chars = unsafe { text(ev.characters) };
    let unmod = unsafe { text(ev.characters_ignoring_modifiers) };
    let combo = KeyCombo::from_parts(
        ev.mods & KOVA_MOD_COMMAND != 0,
        ev.mods & KOVA_MOD_CONTROL != 0,
        ev.mods & KOVA_MOD_OPTION != 0,
        ev.mods & KOVA_MOD_SHIFT != 0,
        ev.keycode,
        || unmod.clone(),
    );
    KeyEvent { combo, chars, unmod }
}

/// 1/0 for a yes/no, -1 when the pane is unknown.
fn yes_no(answer: Option<bool>) -> c_int {
    match answer {
        Some(true) => 1,
        Some(false) => 0,
        None => -1,
    }
}

/// 0 when done, -1 when not (unknown pane, or the call needs a view).
fn done(ok: bool) -> c_int {
    if ok { 0 } else { -1 }
}

// ─── Library ────────────────────────────────────────────────────────────────

/// The library version, a static string.
#[unsafe(no_mangle)]
pub extern "C" fn kova_version() -> *const c_char {
    concat!(env!("CARGO_PKG_VERSION"), "\0").as_ptr() as *const c_char
}

/// Free a string returned by a `kova_*_copy_*` function. NULL is ignored.
///
/// # Safety
/// `s` is NULL or a pointer this library returned and not yet freed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn kova_string_free(s: *mut c_char) {
    if !s.is_null() {
        drop(unsafe { CString::from_raw(s) });
    }
}

// ─── Panes ──────────────────────────────────────────────────────────────────

/// Create a pane; 0 on failure.
///
/// # Safety
/// `options` is NULL or valid; its pointers are NULL or valid for the call,
/// `parent_view` an `NSView`, `env` NULL-terminated.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn kova_pane_new(options: *const KovaPaneOptions) -> u32 {
    guard("kova_pane_new", 0, || {
        let mut opts = PaneOptions::default();
        let mut parent: Option<&NSView> = None;
        if let Some(o) = unsafe { options.as_ref() } {
            opts.cwd = unsafe { text(o.cwd) };
            opts.command = unsafe { text(o.command) };
            opts.cols = o.cols;
            opts.rows = o.rows;
            if !o.env.is_null() {
                let mut p = o.env;
                while let Some(entry) = unsafe { text(*p) } {
                    match entry.split_once('=') {
                        Some((k, v)) if !k.is_empty() => opts.env.push((k.to_string(), v.to_string())),
                        _ => log::warn!("kova_pane_new: ignoring env entry without NAME=: {:?}", entry),
                    }
                    p = unsafe { p.add(1) };
                }
            }
            parent = unsafe { (o.parent_view as *const NSView).as_ref() };
        }
        match embed::create(opts, parent) {
            Ok(id) => id,
            Err(e) => {
                log::error!("kova_pane_new: {}", e);
                0
            }
        }
    })
}

/// Close a pane and hang up its shell; reaped on a background thread.
#[unsafe(no_mangle)]
pub extern "C" fn kova_pane_free(pane: u32) -> c_int {
    guard("kova_pane_free", -1, || done(embed::close(pane)))
}

/// Close every pane of this thread, stop the socket, and return once every
/// closed pane's shell and foreground job are gone. How many panes were open.
#[unsafe(no_mangle)]
pub extern "C" fn kova_shutdown() -> c_int {
    guard("kova_shutdown", -1, || embed::shutdown() as c_int)
}

/// The pane's view (an unretained `NSView *`), NULL without one.
#[unsafe(no_mangle)]
pub extern "C" fn kova_pane_view(pane: u32) -> *mut c_void {
    guard("kova_pane_view", std::ptr::null_mut(), || {
        embed::find(pane)
            .and_then(|p| p.view())
            .map_or(std::ptr::null_mut(), |v| Retained::as_ptr(&v) as *mut c_void)
    })
}

/// Set the view's frame, in points, in its parent's coordinates.
#[unsafe(no_mangle)]
pub extern "C" fn kova_pane_set_frame(pane: u32, x: f64, y: f64, width: f64, height: f64) -> c_int {
    guard("kova_pane_set_frame", -1, || {
        let frame = CGRect { origin: CGPoint { x, y }, size: CGSize { width, height } };
        done(embed::find(pane).is_some_and(|p| p.set_frame(frame)))
    })
}

/// Set the grid of a pane without a view.
#[unsafe(no_mangle)]
pub extern "C" fn kova_pane_resize(pane: u32, cols: u16, rows: u16) -> c_int {
    guard("kova_pane_resize", -1, || {
        done(embed::find(pane).filter(|p| p.view().is_none()).map(|p| p.resize(cols, rows)).is_some())
    })
}

/// The pane's grid size.
///
/// # Safety
/// `cols` and `rows` are NULL or valid.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn kova_pane_get_size(pane: u32, cols: *mut u16, rows: *mut u16) -> c_int {
    guard("kova_pane_get_size", -1, || {
        let Some(p) = embed::find(pane) else { return -1 };
        let term = p.surface.terminal.read();
        unsafe {
            if let Some(c) = cols.as_mut() {
                *c = term.cols;
            }
            if let Some(r) = rows.as_mut() {
                *r = term.rows;
            }
        }
        0
    })
}

/// The per-frame work, then what changed (`KOVA_EVENT_*`); 0 for an unknown pane.
#[unsafe(no_mangle)]
pub extern "C" fn kova_pane_tick(pane: u32) -> u32 {
    guard("kova_pane_tick", 0, || embed::find(pane).map_or(0, |p| p.tick()))
}

/// 1 when `kova_pane_draw` would present a frame.
#[unsafe(no_mangle)]
pub extern "C" fn kova_pane_needs_draw(pane: u32) -> c_int {
    guard("kova_pane_needs_draw", -1, || yes_no(embed::find(pane).map(|p| p.needs_draw())))
}

/// Draw the pane if it needs it (always with `force`); 1 when a frame was presented.
#[unsafe(no_mangle)]
pub extern "C" fn kova_pane_draw(pane: u32, force: c_int) -> c_int {
    guard("kova_pane_draw", -1, || yes_no(embed::find(pane).map(|p| p.draw(force != 0))))
}

// ─── Input ──────────────────────────────────────────────────────────────────

/// A key press: `KOVA_KEY_SENT` (0), `KOVA_KEY_COMPOSE` (1), `KOVA_KEY_HOST`
/// (2: taken by the pane's filter, or a Kova.app window key; nothing sent).
///
/// # Safety
/// `event` is valid, its strings NULL or NUL-terminated.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn kova_pane_key(pane: u32, event: *const KovaKeyEvent) -> c_int {
    guard("kova_pane_key", -1, || {
        let (Some(p), Some(ev)) = (embed::find(pane), unsafe { event.as_ref() }) else { return -1 };
        let key = unsafe { key_event(ev) };
        // The view's order: the filter (or Ctrl+F opening it) first.
        if p.filter_key(&key) {
            return 2;
        }
        match p.key(&key) {
            KeyResult::Sent => 0,
            KeyResult::Compose => 1,
            KeyResult::Host(_) => 2,
        }
    })
}

/// A key the host's text input did not turn into text: encode it for the PTY.
///
/// # Safety
/// As `kova_pane_key`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn kova_pane_send_key(pane: u32, event: *const KovaKeyEvent) -> c_int {
    guard("kova_pane_send_key", -1, || {
        let (Some(p), Some(ev)) = (embed::find(pane), unsafe { event.as_ref() }) else { return -1 };
        p.send_key(&unsafe { key_event(ev) });
        0
    })
}

/// Committed text (typed characters, a composed accent, an IME result).
///
/// # Safety
/// `utf8` is NULL or NUL-terminated.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn kova_pane_text(pane: u32, utf8: *const c_char) -> c_int {
    guard("kova_pane_text", -1, || {
        let (Some(p), Some(t)) = (embed::find(pane), unsafe { text(utf8) }) else { return -1 };
        p.surface.text(&t);
        0
    })
}

/// A paste, bracketed when the app asked for it.
///
/// # Safety
/// `utf8` is NULL or NUL-terminated.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn kova_pane_paste(pane: u32, utf8: *const c_char) -> c_int {
    guard("kova_pane_paste", -1, || {
        let (Some(p), Some(t)) = (embed::find(pane), unsafe { text(utf8) }) else { return -1 };
        // The open filter owns the paste, as with the view's Edit > Paste.
        if !p.paste_into_filter(&t) {
            p.surface.paste(&t);
        }
        0
    })
}

/// The text being composed; NULL or "" ends the composition.
///
/// # Safety
/// `utf8` is NULL or NUL-terminated.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn kova_pane_preedit(pane: u32, utf8: *const c_char) -> c_int {
    guard("kova_pane_preedit", -1, || {
        let Some(p) = embed::find(pane) else { return -1 };
        p.preedit(&unsafe { text(utf8) }.unwrap_or_default());
        0
    })
}

/// A mouse event at `(x, y)` points from the view's top-left corner:
/// `KOVA_MOUSE_LOCAL` (0), `KOVA_MOUSE_REPORTED` (1), `KOVA_MOUSE_AUTOSCROLL` (2).
#[unsafe(no_mangle)]
pub extern "C" fn kova_pane_mouse(pane: u32, action: c_int, button: c_int, mods_bits: u32, x: f64, y: f64, click_count: c_int) -> c_int {
    guard("kova_pane_mouse", -1, || {
        let action = match action {
            0 => MouseAction::Press,
            1 => MouseAction::Drag,
            2 => MouseAction::Release,
            3 => MouseAction::Move,
            _ => return -1,
        };
        let Some(p) = embed::find(pane) else { return -1 };
        match p.mouse(action, button.clamp(0, 2) as u8, mods(mods_bits), x, y, click_count as isize) {
            Some(MouseResult::Local) => 0,
            Some(MouseResult::Reported) => 1,
            Some(MouseResult::Autoscroll(_)) => 2,
            None => -1,
        }
    })
}

/// A vertical scroll at `(x, y)` points from the view's top-left corner.
#[unsafe(no_mangle)]
pub extern "C" fn kova_pane_scroll(pane: u32, dy: f64, precise: c_int, mods_bits: u32, x: f64, y: f64) -> c_int {
    guard("kova_pane_scroll", -1, || {
        done(embed::find(pane).is_some_and(|p| p.scroll(dy, precise != 0, mods(mods_bits), x, y)))
    })
}

// ─── Reading ────────────────────────────────────────────────────────────────

/// The pane's text, as `get-pane-content` returns it; NULL for an unknown pane.
#[unsafe(no_mangle)]
pub extern "C" fn kova_pane_copy_text(pane: u32, mode: c_int, trim_trailing_blank_lines: c_int) -> *mut c_char {
    guard("kova_pane_copy_text", std::ptr::null_mut(), || {
        let mode = match mode {
            1 => DumpMode::Scrollback,
            2 => DumpMode::All,
            _ => DumpMode::Visible,
        };
        embed::find(pane).map_or(std::ptr::null_mut(), |p| out(p.text(mode, trim_trailing_blank_lines != 0)))
    })
}

/// The pane's display title.
#[unsafe(no_mangle)]
pub extern "C" fn kova_pane_copy_title(pane: u32) -> *mut c_char {
    guard("kova_pane_copy_title", std::ptr::null_mut(), || {
        embed::find(pane).map_or(std::ptr::null_mut(), |p| out(p.title()))
    })
}

/// The id of the Claude conversation running in the pane, NULL when none.
#[unsafe(no_mangle)]
pub extern "C" fn kova_pane_copy_claude_session_id(pane: u32) -> *mut c_char {
    guard("kova_pane_copy_claude_session_id", std::ptr::null_mut(), || {
        embed::find(pane).and_then(|p| p.surface.claude_session_id()).map_or(std::ptr::null_mut(), out)
    })
}

/// The current selection as text, then cleared; NULL when nothing is selected.
#[unsafe(no_mangle)]
pub extern "C" fn kova_pane_copy_selection(pane: u32) -> *mut c_char {
    guard("kova_pane_copy_selection", std::ptr::null_mut(), || {
        embed::find(pane).and_then(|p| p.take_selection()).map_or(std::ptr::null_mut(), out)
    })
}

/// 1 while the shell runs, 0 once it has exited.
#[unsafe(no_mangle)]
pub extern "C" fn kova_pane_is_alive(pane: u32) -> c_int {
    guard("kova_pane_is_alive", -1, || yes_no(embed::find(pane).map(|p| p.surface.is_alive())))
}

/// 1 while the app in the pane says it waits for the user.
#[unsafe(no_mangle)]
pub extern "C" fn kova_pane_is_awaiting(pane: u32) -> c_int {
    guard("kova_pane_is_awaiting", -1, || yes_no(embed::find(pane).map(|p| p.surface.is_awaiting())))
}

/// 1 while the app in the pane is working: Claude Code's spinner leads its
/// title. What Kova.app counts in its status bar's `✳N`.
#[unsafe(no_mangle)]
pub extern "C" fn kova_pane_is_working(pane: u32) -> c_int {
    guard("kova_pane_is_working", -1, || yes_no(embed::find(pane).map(|p| p.surface.is_working())))
}

// ─── Socket ─────────────────────────────────────────────────────────────────

/// Serve the socket protocol at `path` (NULL: `/tmp/kova-<pid>.sock`).
///
/// # Safety
/// `path` is NULL or NUL-terminated.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn kova_ipc_start(path: *const c_char) -> c_int {
    guard("kova_ipc_start", -1, || {
        let path = unsafe { text(path) }.map_or_else(crate::ipc::socket_path, std::path::PathBuf::from);
        match embed::ipc_start(path) {
            Ok(()) => 0,
            Err(e) => {
                log::error!("kova_ipc_start: {}", e);
                -1
            }
        }
    })
}

/// Answer the requests received since the last call; how many were received
/// (answered, or parked as a wait-for-completion).
#[unsafe(no_mangle)]
pub extern "C" fn kova_ipc_poll() -> c_int {
    guard("kova_ipc_poll", -1, || embed::ipc_poll() as c_int)
}

/// Stop serving and remove the socket file.
#[unsafe(no_mangle)]
pub extern "C" fn kova_ipc_stop() -> c_int {
    guard("kova_ipc_stop", -1, || done(embed::ipc_stop()))
}

/// `KovaNotifyHandler` of `include/kova.h`: `(ctx, pane or 0, title,
/// message, sound)`, the strings valid for the call only.
pub type KovaNotifyHandler = extern "C" fn(*mut c_void, u32, *const c_char, *const c_char, bool);

/// A Rust string for the length of a C call; an interior NUL (never sent by
/// the socket's JSON parser, but possible in principle) ends it there.
fn c_string(s: &str) -> CString {
    let end = s.find('\0').unwrap_or(s.len());
    CString::new(&s[..end]).unwrap_or_default()
}

/// Register the handler the socket's `notify` requests go to (NULL removes
/// it). Called from `kova_ipc_poll`, on this thread, once per notification.
#[unsafe(no_mangle)]
pub extern "C" fn kova_set_notify_handler(handler: Option<KovaNotifyHandler>, ctx: *mut c_void) {
    guard("kova_set_notify_handler", (), || {
        embed::set_notify_handler(handler.map(|f| -> embed::NotifyHandler {
            std::rc::Rc::new(move |pane: Option<u32>, title: &str, message: &str, sound: bool| {
                let (title, message) = (c_string(title), c_string(message));
                f(ctx, pane.unwrap_or(0), title.as_ptr(), message.as_ptr(), sound);
            })
        }))
    })
}

#[cfg(test)]
mod tests {
    //! The C API driven through its `extern "C"` functions, as a host calls
    //! them, against panes whose PTY input is readable (`embed::create_capture`).
    use super::*;
    use crate::input::tests::{drain, key_event as ns_key_event, TABLE};
    use std::io::{BufRead, BufReader, Write};
    use std::sync::atomic::Ordering;

    fn c(s: &str) -> CString {
        CString::new(s).unwrap()
    }

    /// Take a string the library returned, freeing it as a host must.
    fn take(p: *mut c_char) -> Option<String> {
        if p.is_null() {
            return None;
        }
        let s = unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned();
        unsafe { kova_string_free(p) };
        Some(s)
    }

    fn mod_bits(mods: &str) -> u32 {
        mods.split_whitespace()
            .map(|m| match m {
                "cmd" => KOVA_MOD_COMMAND,
                "ctrl" => KOVA_MOD_CONTROL,
                "alt" => KOVA_MOD_OPTION,
                "shift" => KOVA_MOD_SHIFT,
                other => panic!("unknown modifier {other}"),
            })
            .fold(0, |a, b| a | b)
    }

    fn type_text(pane: u32, lines: &[&str]) {
        let p = embed::find(pane).unwrap();
        let mut t = p.surface.terminal.write();
        for (i, l) in lines.iter().enumerate() {
            if i > 0 {
                t.newline();
                t.carriage_return();
            }
            for ch in l.chars() {
                t.put_char(ch);
            }
        }
    }

    #[test]
    fn version_is_the_crate_version() {
        let v = unsafe { CStr::from_ptr(kova_version()) }.to_str().unwrap();
        assert_eq!(v, env!("CARGO_PKG_VERSION"));
        unsafe { kova_string_free(std::ptr::null_mut()) };
    }

    #[test]
    fn an_unknown_pane_fails_everywhere() {
        let ghost = 0xFFFF_FFF0;
        assert_eq!(kova_pane_free(ghost), -1);
        assert!(kova_pane_view(ghost).is_null());
        assert_eq!(kova_pane_set_frame(ghost, 0.0, 0.0, 10.0, 10.0), -1);
        assert_eq!(kova_pane_resize(ghost, 10, 10), -1);
        assert_eq!(unsafe { kova_pane_get_size(ghost, std::ptr::null_mut(), std::ptr::null_mut()) }, -1);
        assert_eq!(kova_pane_tick(ghost), 0);
        assert_eq!(kova_pane_needs_draw(ghost), -1);
        assert_eq!(kova_pane_draw(ghost, 1), -1);
        let t = c("x");
        assert_eq!(unsafe { kova_pane_text(ghost, t.as_ptr()) }, -1);
        assert_eq!(unsafe { kova_pane_paste(ghost, t.as_ptr()) }, -1);
        assert_eq!(unsafe { kova_pane_preedit(ghost, t.as_ptr()) }, -1);
        let ev = KovaKeyEvent { keycode: 0, mods: 0, characters: t.as_ptr(), characters_ignoring_modifiers: t.as_ptr() };
        assert_eq!(unsafe { kova_pane_key(ghost, &ev) }, -1);
        assert_eq!(unsafe { kova_pane_send_key(ghost, &ev) }, -1);
        assert_eq!(kova_pane_mouse(ghost, 0, 0, 0, 1.0, 1.0, 1), -1);
        assert_eq!(kova_pane_scroll(ghost, 1.0, 1, 0, 1.0, 1.0), -1);
        assert!(kova_pane_copy_text(ghost, 0, 1).is_null());
        assert!(kova_pane_copy_title(ghost).is_null());
        assert!(kova_pane_copy_claude_session_id(ghost).is_null());
        assert!(kova_pane_copy_selection(ghost).is_null());
        assert_eq!(kova_pane_is_alive(ghost), -1);
        assert_eq!(kova_pane_is_awaiting(ghost), -1);
        assert_eq!(kova_pane_is_working(ghost), -1);
    }

    /// Every case of the frozen key table (`input::tests::TABLE`) sent through
    /// `kova_pane_key` / `kova_pane_send_key` gives the bytes Kova.app sends
    /// for the same NSEvent, and the C fields build the very `KeyEvent` the
    /// AppKit path builds.
    #[test]
    fn the_frozen_key_table_through_the_c_api() {
        let (pane, reader) = embed::create_capture(80, 24);
        let p = embed::find(pane).unwrap();
        let mut failures = Vec::new();
        for k in TABLE {
            {
                let mut t = p.surface.terminal.write();
                t.cursor_keys_application = k.app_cursor;
                t.kitty_keyboard_flags = if k.kitty > 0 { vec![k.kitty] } else { Vec::new() };
            }
            let chars = c(k.chars);
            let unmod = c(k.unmod);
            let ev = KovaKeyEvent {
                keycode: k.keycode,
                mods: mod_bits(k.mods),
                characters: chars.as_ptr(),
                characters_ignoring_modifiers: unmod.as_ptr(),
            };
            let from_c = unsafe { key_event(&ev) };
            let from_appkit = crate::appkit_input::key_event(&ns_key_event(k.chars, k.unmod, k.keycode, k.mods));
            if from_c != from_appkit {
                failures.push(format!("{}: C {:?} vs AppKit {:?}", k.name, from_c, from_appkit));
                continue;
            }
            // What the view does: the key first, and when it is handed back
            // for composing, the text input's `doCommandBySelector:` path.
            let got = match unsafe { kova_pane_key(pane, &ev) } {
                0 => drain(&reader),
                1 => {
                    assert_eq!(unsafe { kova_pane_send_key(pane, &ev) }, 0);
                    drain(&reader)
                }
                2 => Vec::new(),
                other => panic!("{}: kova_pane_key returned {other}", k.name),
            };
            if got != k.out {
                failures.push(format!("{}: expected {:?}, got {:?}", k.name, String::from_utf8_lossy(k.out), String::from_utf8_lossy(&got)));
            }
        }
        assert_eq!(kova_pane_free(pane), 0);
        assert!(failures.is_empty(), "{} case(s) differ:\n{}", failures.len(), failures.join("\n"));
    }

    #[test]
    fn host_keys_and_text_paste_preedit() {
        let (pane, reader) = embed::create_capture(80, 24);
        // Ctrl+F opens (then closes) the pane's filter: nothing reaches the PTY.
        let (f, ctrl_f) = (c("\u{6}"), c("f"));
        let ev = KovaKeyEvent { keycode: 0x03, mods: KOVA_MOD_CONTROL, characters: f.as_ptr(), characters_ignoring_modifiers: ctrl_f.as_ptr() };
        assert_eq!(unsafe { kova_pane_key(pane, &ev) }, 2);
        assert!(embed::find(pane).unwrap().filter_open());
        assert_eq!(unsafe { kova_pane_key(pane, &ev) }, 2);
        assert!(!embed::find(pane).unwrap().filter_open());
        assert!(drain(&reader).is_empty());

        let typed = c("ls -la é");
        assert_eq!(unsafe { kova_pane_text(pane, typed.as_ptr()) }, 0);
        assert_eq!(drain(&reader), "ls -la é".as_bytes());

        let pasted = c("two\nlines");
        assert_eq!(unsafe { kova_pane_paste(pane, pasted.as_ptr()) }, 0);
        assert_eq!(drain(&reader), b"two\nlines");
        embed::find(pane).unwrap().surface.terminal.write().bracketed_paste = true;
        let sneaky = c("a\x1b[201~b");
        assert_eq!(unsafe { kova_pane_paste(pane, sneaky.as_ptr()) }, 0);
        assert_eq!(drain(&reader), b"\x1b[200~ab\x1b[201~", "the terminator cannot break out");

        let p = embed::find(pane).unwrap();
        let accent = c("^");
        assert_eq!(unsafe { kova_pane_preedit(pane, accent.as_ptr()) }, 0);
        assert!(p.has_preedit());
        assert_eq!(unsafe { kova_pane_preedit(pane, std::ptr::null()) }, 0);
        assert!(!p.has_preedit());
        assert!(drain(&reader).is_empty(), "composition never reaches the PTY");
        assert_eq!(unsafe { kova_pane_text(pane, std::ptr::null()) }, -1, "NULL text is refused");
        assert_eq!(kova_pane_free(pane), 0);
    }

    #[test]
    fn a_pane_without_view_reads_resizes_and_has_no_drawing_nor_pointer() {
        let (pane, _reader) = embed::create_capture(20, 4);
        type_text(pane, &["hello", "world"]);
        assert_eq!(take(kova_pane_copy_text(pane, 0, 1)).as_deref(), Some("hello\nworld\n"));
        assert_eq!(
            take(kova_pane_copy_text(pane, 0, 1)),
            Some(embed::find(pane).unwrap().surface.terminal.read().dump_text(DumpMode::Visible, true).text),
            "the get-pane-content text"
        );
        let (mut cols, mut rows) = (0u16, 0u16);
        assert_eq!(unsafe { kova_pane_get_size(pane, &mut cols, &mut rows) }, 0);
        assert_eq!((cols, rows), (20, 4));
        assert_eq!(kova_pane_resize(pane, 100, 30), 0);
        assert_eq!(unsafe { kova_pane_get_size(pane, &mut cols, &mut rows) }, 0);
        assert_eq!((cols, rows), (100, 30));
        assert_eq!(take(kova_pane_copy_text(pane, 2, 1)).as_deref(), Some("hello\nworld\n"), "a resize keeps the text");

        assert!(kova_pane_view(pane).is_null());
        assert_eq!(kova_pane_set_frame(pane, 0.0, 0.0, 100.0, 100.0), -1, "no view to frame");
        assert_eq!(kova_pane_needs_draw(pane), 0);
        assert_eq!(kova_pane_draw(pane, 1), 0, "nothing to draw into");
        assert_eq!(kova_pane_mouse(pane, 0, 0, 0, 5.0, 5.0, 1), -1, "no cell size without a view");
        assert_eq!(kova_pane_scroll(pane, 3.0, 0, 0, 5.0, 5.0), -1);
        assert_eq!(kova_pane_mouse(pane, 9, 0, 0, 5.0, 5.0, 1), -1, "unknown action");

        assert_eq!(kova_pane_is_alive(pane), 1);
        assert_eq!(kova_pane_is_awaiting(pane), 0);
        assert!(kova_pane_copy_claude_session_id(pane).is_null());
        assert!(kova_pane_copy_selection(pane).is_null(), "nothing selected");
        assert_eq!(kova_pane_free(pane), 0);
        assert_eq!(kova_pane_free(pane), -1, "already closed");
        assert!(kova_pane_copy_text(pane, 0, 1).is_null());
    }

    #[test]
    fn tick_reports_each_change_once() {
        let (pane, _reader) = embed::create_capture(20, 4);
        // The first tick probes the pane (a childless PTY: nothing runs, so
        // any waiting flag is dropped) and sees its first title.
        assert_ne!(kova_pane_tick(pane) & embed::EVENT_TITLE_CHANGED, 0);
        assert_eq!(kova_pane_tick(pane) & embed::EVENT_TITLE_CHANGED, 0, "unchanged title, no event");
        assert_eq!(take(kova_pane_copy_title(pane)).as_deref(), Some("shell"));

        let p = embed::find(pane).unwrap();
        p.surface.terminal.write().title = Some("vim notes.md".into());
        assert_ne!(kova_pane_tick(pane) & embed::EVENT_TITLE_CHANGED, 0);
        assert_eq!(take(kova_pane_copy_title(pane)).as_deref(), Some("vim notes.md"));
        assert_eq!(kova_pane_is_working(pane), 0);
        p.surface.terminal.write().title = Some("\u{2810} Claude Code".into());
        assert_eq!(kova_pane_is_working(pane), 1, "Claude's spinner: working");
        p.surface.terminal.write().title = Some("\u{2733} Claude Code".into());
        assert_eq!(kova_pane_is_working(pane), 0, "the idle asterisk is not work");
        p.surface.terminal.write().title = Some("vim notes.md".into());
        kova_pane_tick(pane);

        p.surface.set_awaiting();
        assert_eq!(kova_pane_is_awaiting(pane), 1);
        assert_ne!(kova_pane_tick(pane) & embed::EVENT_AWAITING_CHANGED, 0);
        assert_eq!(kova_pane_tick(pane) & embed::EVENT_AWAITING_CHANGED, 0);

        p.surface.shell_exited.store(true, Ordering::Relaxed);
        let ev = kova_pane_tick(pane);
        assert_ne!(ev & embed::EVENT_EXITED, 0);
        assert_ne!(ev & embed::EVENT_AWAITING_CHANGED, 0, "an exited pane waits for nobody");
        assert_eq!(kova_pane_is_alive(pane), 0);
        assert_eq!(kova_pane_tick(pane) & embed::EVENT_EXITED, 0, "reported once");
        assert_eq!(kova_pane_tick(pane) & embed::EVENT_NEEDS_DRAW, 0, "no view, never drawn");
        assert_eq!(kova_pane_free(pane), 0);
    }

    /// A real `/bin/sh` pane whose shell and foreground job both ignore
    /// SIGHUP (as Claude Code does), and the pids of the two once the job
    /// printed its own.
    fn hup_proof_pane(tag: &str) -> (u32, i32, i32) {
        let options = PaneOptions {
            shell: Some("/bin/sh".into()),
            command: Some(format!("trap '' HUP; sh -c 'trap \"\" HUP; echo {tag}:$$; exec sleep 600'")),
            cols: 80,
            rows: 24,
            ..Default::default()
        };
        let pane = embed::create(options, None).expect("spawn /bin/sh");
        let shell = embed::find(pane).unwrap().surface.pty.pid() as i32;
        let marker = format!("{tag}:");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            kova_pane_tick(pane);
            let text = take(kova_pane_copy_text(pane, 2, 1)).unwrap_or_default();
            let job = text.lines().find_map(|l| l.trim().strip_prefix(marker.as_str())?.parse::<i32>().ok());
            if let Some(job) = job {
                return (pane, shell, job);
            }
            assert!(std::time::Instant::now() < deadline, "the job never started:\n{text}");
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    fn alive(pid: i32) -> bool {
        unsafe { libc::kill(pid, 0) == 0 }
    }

    /// True once `pid` is gone, waiting up to a second for an orphan to be
    /// reaped by launchd after its SIGKILL.
    fn gone_soon(pid: i32) -> bool {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(1);
        while alive(pid) {
            if std::time::Instant::now() > deadline {
                return false;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        true
    }

    /// `kova_shutdown` returns once the shell and the foreground job of every
    /// pane are gone, those it closes and one freed just before, even when
    /// both ignore SIGHUP.
    #[test]
    fn shutdown_leaves_no_process_behind() {
        let (freed, freed_shell, freed_job) = hup_proof_pane("FREED");
        let (open, open_shell, open_job) = hup_proof_pane("OPEN");
        assert!(alive(freed_shell) && alive(freed_job) && alive(open_shell) && alive(open_job));
        assert_eq!(kova_pane_free(freed), 0);
        assert_eq!(kova_shutdown(), 1, "one pane was still open");
        assert_eq!(kova_pane_is_alive(open), -1, "shutdown closed it");
        assert!(!alive(freed_shell), "the freed pane's shell is reaped");
        assert!(!alive(open_shell), "the open pane's shell is reaped");
        assert!(gone_soon(freed_job), "the freed pane's job is gone");
        assert!(gone_soon(open_job), "the open pane's job is gone");
        assert_eq!(kova_shutdown(), 0, "nothing left; the library stays usable");
    }

    /// Unique socket path in the temp directory for this test.
    fn socket_path(tag: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("capi-{}-{}.sock", tag, std::process::id()))
    }

    /// The socket answers the generic pane commands over this thread's panes,
    /// one tab per pane; app commands are refused; close and focus are
    /// requests the host sees on its next tick.
    #[test]
    fn the_socket_serves_the_panes_one_tab_each() {
        let path = socket_path("serve");
        let cpath = c(path.to_str().unwrap());
        assert_eq!(unsafe { kova_ipc_start(cpath.as_ptr()) }, 0);
        assert_eq!(unsafe { kova_ipc_start(cpath.as_ptr()) }, -1, "one socket per thread");
        let (a, reader_a) = embed::create_capture(20, 4);
        let (b, _reader_b) = embed::create_capture(20, 4);
        type_text(b, &["from b"]);
        embed::find(b).unwrap().surface.terminal.read().mark_command_completed();

        let requests = vec![
            r#"{"cmd":"list-panes"}"#.to_string(),
            format!(r#"{{"cmd":"send-keys","pane_id":{a},"text":"echo hi\r"}}"#),
            format!(r#"{{"cmd":"get-pane-content","panes":[{b}]}}"#),
            r#"{"cmd":"list-tabs"}"#.to_string(),
            format!(r#"{{"cmd":"wait-for-completion","pane_id":{b},"timeout_ms":1000}}"#),
            format!(r#"{{"cmd":"set-pane-status","pane_id":{a},"status":"waiting"}}"#),
            format!(r#"{{"cmd":"focus-pane","pane_id":{a}}}"#),
            format!(r#"{{"cmd":"close-tab","tab_id":{b}}}"#),
            r#"{"cmd":"split"}"#.to_string(),
            r#"{"cmd":"notify","message":"m"}"#.to_string(),
            r#"{"cmd":"focus-pane","pane_id":999999}"#.to_string(),
            r#"{"cmd":"list-asks"}"#.to_string(),
            r#"{"cmd":"answer-ask","id":"0f8fad5b-d9cb-469f-a165-70867728950e","action":"OK"}"#.to_string(),
            r#"{"cmd":"withdraw-asks","session_id":"abc-1"}"#.to_string(),
        ];
        let client_path = path.clone();
        let client = std::thread::spawn(move || {
            let stream = std::os::unix::net::UnixStream::connect(&client_path).unwrap();
            let mut writer = stream.try_clone().unwrap();
            let mut reader = BufReader::new(stream);
            let mut answers = Vec::new();
            for r in requests {
                writeln!(writer, "{r}").unwrap();
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                answers.push(serde_json::from_str::<serde_json::Value>(&line).unwrap());
            }
            answers
        });
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !client.is_finished() {
            assert!(std::time::Instant::now() < deadline, "client never finished");
            assert!(kova_ipc_poll() >= 0);
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        let answers = client.join().unwrap();

        let panes = answers[0]["data"].as_array().unwrap();
        assert_eq!(panes.iter().map(|p| p["id"].as_u64().unwrap() as u32).collect::<Vec<_>>(), [a, b]);
        assert_eq!(panes[1]["tab"], 1, "one tab per pane, in creation order");
        assert_eq!(panes[0]["window"], 0);
        assert_eq!(answers[1]["ok"], true);
        assert_eq!(drain(&reader_a), b"echo hi\r", "send-keys reached pane a's PTY");
        assert_eq!(answers[2]["data"]["panes"][0]["text"], "from b\n");
        let tabs = answers[3]["data"].as_array().unwrap();
        assert_eq!(tabs[1]["id"].as_u64(), Some(b as u64), "a tab is named by its pane id");
        assert_eq!(tabs[1]["focused_pane_id"].as_u64(), Some(b as u64));
        assert_eq!(tabs[1]["pane_count"], 1);
        assert_eq!(answers[4]["data"]["completed"], true);
        assert_eq!(answers[5]["ok"], true);
        assert_eq!(kova_pane_is_awaiting(a), 1);
        assert_eq!(answers[6]["ok"], true);
        assert_eq!(answers[7]["ok"], true);
        assert_eq!(answers[8]["error"], "command not supported by this host");
        assert_eq!(answers[9]["error"], "notify is not supported by this host");
        assert_eq!(answers[10]["error"], "pane 999999 not found");
        assert_eq!(answers[11]["error"], "command not supported by this host", "no question card here");
        assert_eq!(answers[12]["error"], "command not supported by this host");
        assert_eq!(answers[13]["error"], "command not supported by this host");

        assert_ne!(kova_pane_tick(a) & embed::EVENT_FOCUS_REQUESTED, 0);
        assert_ne!(kova_pane_tick(b) & embed::EVENT_CLOSE_REQUESTED, 0);
        assert_eq!(kova_pane_tick(b) & embed::EVENT_CLOSE_REQUESTED, 0, "raised once");
        assert_eq!(embed::count(), 2, "the host closes the pane, not the socket");

        assert_eq!(kova_pane_free(a), 0);
        assert_eq!(kova_pane_free(b), 0);
        assert_eq!(kova_ipc_stop(), 0);
        assert!(!path.exists(), "the socket file is removed");
        assert_eq!(kova_ipc_stop(), -1);
    }

    /// A socket that cannot be bound is reported to the caller, and leaves the
    /// thread free to serve another one.
    #[test]
    fn a_socket_that_cannot_be_bound_fails_the_start() {
        let missing_dir = std::env::temp_dir().join(format!("capi-no-such-dir-{}", std::process::id()));
        assert!(!missing_dir.exists());
        let bad = c(missing_dir.join("kova.sock").to_str().unwrap());
        assert_eq!(unsafe { kova_ipc_start(bad.as_ptr()) }, -1, "bind fails in a missing directory");
        assert_eq!(kova_ipc_poll(), 0, "nothing is served");
        assert_eq!(kova_ipc_stop(), -1, "nothing to stop");
        let path = socket_path("after-bad");
        let good = c(path.to_str().unwrap());
        assert_eq!(unsafe { kova_ipc_start(good.as_ptr()) }, 0);
        assert!(path.exists(), "bound before kova_ipc_start returned");
        assert_eq!(kova_ipc_stop(), 0);
        assert!(!path.exists());
    }

    /// A `wait-for-completion` on a pane that is not finished stays open
    /// across polls and resolves when the flag rises.
    #[test]
    fn a_pending_wait_resolves_on_a_later_poll() {
        let path = socket_path("wait");
        let cpath = c(path.to_str().unwrap());
        assert_eq!(unsafe { kova_ipc_start(cpath.as_ptr()) }, 0);
        let (a, _reader) = embed::create_capture(20, 4);
        let request = format!(r#"{{"cmd":"wait-for-completion","pane_id":{a},"timeout_ms":5000}}"#);
        let client_path = path.clone();
        let client = std::thread::spawn(move || {
            let stream = std::os::unix::net::UnixStream::connect(&client_path).unwrap();
            let mut writer = stream.try_clone().unwrap();
            writeln!(writer, "{request}").unwrap();
            let mut line = String::new();
            BufReader::new(stream).read_line(&mut line).unwrap();
            serde_json::from_str::<serde_json::Value>(&line).unwrap()
        });
        // Let the request arrive and be parked.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while kova_ipc_poll() == 0 {
            assert!(std::time::Instant::now() < deadline, "request never arrived");
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        assert!(!client.is_finished(), "still waiting");
        embed::find(a).unwrap().surface.terminal.read().mark_command_completed();
        while !client.is_finished() {
            assert!(std::time::Instant::now() < deadline, "the wait never resolved");
            kova_ipc_poll();
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        let answer = client.join().unwrap();
        assert_eq!(answer["data"]["completed"], true);
        assert_eq!(answer["data"]["timed_out"], false);
        assert_eq!(kova_pane_free(a), 0);
        assert_eq!(kova_ipc_stop(), 0);
    }

    /// What the notify handler below was called with.
    type NotifyLog = std::cell::RefCell<Vec<(usize, u32, String, String, bool)>>;

    extern "C" fn record_notify(ctx: *mut c_void, pane: u32, title: *const c_char, message: *const c_char, sound: bool) {
        let log = unsafe { &*(ctx as *const NotifyLog) };
        let s = |p: *const c_char| unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned();
        log.borrow_mut().push((ctx as usize, pane, s(title), s(message), sound));
    }

    /// `subscribe` and `notify` over the socket of a library host: the
    /// snapshot of this thread's panes, then a `pane-open` when one is
    /// created; `notify` refused without a handler, handed to the handler
    /// registered with `kova_set_notify_handler` (pane 0 = none) once
    /// `kova_ipc_poll` answered it.
    #[test]
    fn the_socket_streams_events_and_hands_notifications_to_the_host() {
        let path = socket_path("events");
        let cpath = c(path.to_str().unwrap());
        assert_eq!(unsafe { kova_ipc_start(cpath.as_ptr()) }, 0);
        let (a, _reader_a) = embed::create_capture(20, 4);

        let log: NotifyLog = std::cell::RefCell::new(Vec::new());
        let ctx = &log as *const NotifyLog as *mut c_void;
        // Commands on one connection, the event stream on another.
        let run = |requests: Vec<String>| {
            let client_path = path.clone();
            let client = std::thread::spawn(move || {
                let stream = std::os::unix::net::UnixStream::connect(&client_path).unwrap();
                let mut writer = stream.try_clone().unwrap();
                let mut reader = BufReader::new(stream);
                let mut answers = Vec::new();
                for r in requests {
                    writeln!(writer, "{r}").unwrap();
                    let mut line = String::new();
                    reader.read_line(&mut line).unwrap();
                    answers.push(serde_json::from_str::<serde_json::Value>(&line).unwrap());
                }
                answers
            });
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            while !client.is_finished() {
                assert!(std::time::Instant::now() < deadline, "client never finished");
                kova_ipc_poll();
                std::thread::sleep(std::time::Duration::from_millis(2));
            }
            client.join().unwrap()
        };

        let notify = format!(r#"{{"cmd":"notify","pane_id":{a},"title":"Claude Code","message":"done","sound":true}}"#);
        assert_eq!(run(vec![notify.clone()])[0]["error"], "notify is not supported by this host");
        kova_set_notify_handler(Some(record_notify), ctx);
        let answers = run(vec![notify, r#"{"cmd":"notify","message":"bare"}"#.to_string()]);
        assert_eq!(answers[0]["ok"], true);
        assert_eq!(answers[1]["ok"], true);
        kova_ipc_poll();
        assert_eq!(
            *log.borrow(),
            [
                (ctx as usize, a, "Claude Code".to_string(), "done".to_string(), true),
                (ctx as usize, 0, "Kova".to_string(), "bare".to_string(), false),
            ]
        );
        kova_set_notify_handler(None, std::ptr::null_mut());
        assert_eq!(run(vec![r#"{"cmd":"notify","message":"x"}"#.to_string()])[0]["error"], "notify is not supported by this host");

        // The subscriber: snapshot first, then events, one JSON per line.
        let client_path = path.clone();
        let (tx, rx) = std::sync::mpsc::channel::<serde_json::Value>();
        std::thread::spawn(move || {
            let stream = std::os::unix::net::UnixStream::connect(&client_path).unwrap();
            let mut writer = stream.try_clone().unwrap();
            writeln!(writer, r#"{{"cmd":"subscribe","events":["pane-open","pane-close"]}}"#).unwrap();
            for line in BufReader::new(stream).lines() {
                let Ok(line) = line else { break };
                if tx.send(serde_json::from_str(&line).unwrap()).is_err() {
                    break;
                }
            }
        });
        let next = |want: &dyn Fn(&serde_json::Value) -> bool| {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            loop {
                assert!(std::time::Instant::now() < deadline, "no such line came");
                kova_ipc_poll();
                if let Ok(v) = rx.recv_timeout(std::time::Duration::from_millis(2)) {
                    if want(&v) {
                        return v;
                    }
                }
            }
        };
        let snapshot = next(&|v| v.get("ok").is_some());
        assert_eq!(snapshot["data"]["events"], serde_json::json!(["pane-open", "pane-close"]));
        let ids: Vec<u64> = snapshot["data"]["panes"].as_array().unwrap().iter().map(|p| p["id"].as_u64().unwrap()).collect();
        assert_eq!(ids, [a as u64], "this thread's panes");
        assert!(snapshot["data"].get("app_active").is_some());
        // Subscribers are process-wide: other tests' panes may show up too,
        // so the lines are picked by pane id.
        let (b, _reader_b) = embed::create_capture(20, 4);
        let opened = next(&|v| v["event"] == "pane-open" && v["pane"]["id"].as_u64() == Some(b as u64));
        assert_eq!(opened["pane"]["window"], 0);
        assert_eq!(opened["pane"]["tab"], 1);
        assert_eq!(kova_pane_free(a), 0);
        let closed = next(&|v| v["event"] == "pane-close" && v["pane_id"].as_u64() == Some(a as u64));
        assert_eq!(closed["tab"], 0);

        assert_eq!(kova_pane_free(b), 0);
        assert_eq!(kova_ipc_stop(), 0);
    }
}
