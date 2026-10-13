use objc2_app_kit::NSEvent;

use crate::keybindings::{Keybindings, KeyCombo, TerminalAction};
use crate::terminal::pty::Pty;

/// Write raw UTF-8 text to PTY (used by insertText: from NSTextInputClient).
pub fn write_text(text: &str, pty: &Pty) {
    if !text.is_empty() {
        pty.write(text.as_bytes());
    }
}

/// Kitty modifier encoding: 1 + bitmask (shift=1, alt=2, ctrl=4)
fn kitty_mods(ctrl: bool, alt: bool, shift: bool) -> u8 {
    let mut m: u8 = 0;
    if shift { m |= 1; }
    if alt   { m |= 2; }
    if ctrl  { m |= 4; }
    m + 1
}

/// A key in CSI u format: ESC [ codepoint ; mods u
fn csi_u(codepoint: u32, ctrl: bool, alt: bool, shift: bool) -> Vec<u8> {
    let mods = kitty_mods(ctrl, alt, shift);
    if mods > 1 {
        format!("\x1b[{};{}u", codepoint, mods).into_bytes()
    } else {
        format!("\x1b[{}u", codepoint).into_bytes()
    }
}

/// A special key with xterm-style modifier: ESC [ 1 ; mods suffix
fn modified_special(suffix: u8, ctrl: bool, alt: bool, shift: bool) -> Vec<u8> {
    let mods = kitty_mods(ctrl, alt, shift);
    format!("\x1b[1;{}{}", mods, suffix as char).into_bytes()
}

/// A tilde-style key with modifier: ESC [ code ; mods ~
fn modified_tilde(code: u32, ctrl: bool, alt: bool, shift: bool) -> Vec<u8> {
    let mods = kitty_mods(ctrl, alt, shift);
    if mods > 1 {
        format!("\x1b[{};{}~", code, mods).into_bytes()
    } else {
        format!("\x1b[{}~", code).into_bytes()
    }
}

pub fn handle_key_event(event: &NSEvent, pty: &Pty, cursor_keys_app: bool, keybindings: &Keybindings, kitty_flags: u8) {
    let combo = KeyCombo::from_event(event);
    let unmod_str = event.charactersIgnoringModifiers().map(|s| s.to_string()).unwrap_or_default();
    let chars_str = event.characters().map(|s| s.to_string());
    let bytes = key_bytes(&combo, chars_str.as_deref(), &unmod_str, cursor_keys_app, keybindings, kitty_flags);
    if !bytes.is_empty() {
        pty.write(&bytes);
    }
}

/// The bytes a key press sends to the PTY; empty when it sends nothing.
///
/// `chars` and `unmod` are NSEvent's `characters` and `charactersIgnoringModifiers`
/// (`chars` is None when AppKit gives none).
pub fn key_bytes(
    combo: &KeyCombo,
    chars: Option<&str>,
    unmod_str: &str,
    cursor_keys_app: bool,
    keybindings: &Keybindings,
    kitty_flags: u8,
) -> Vec<u8> {
    // Check configurable terminal keybindings first
    if let Some(action) = keybindings.terminal_map.get(combo) {
        return match action {
            TerminalAction::KillLine => b"\x15".to_vec(),
            TerminalAction::Home => b"\x1b[H".to_vec(),
            TerminalAction::End => b"\x1b[F".to_vec(),
            TerminalAction::WordBack => b"\x1bb".to_vec(),
            TerminalAction::WordForward => b"\x1bf".to_vec(),
            TerminalAction::ShiftEnter => b"\x1b[13;2u".to_vec(),
        };
    }

    let has_ctrl = combo.ctrl;
    let has_alt = combo.option;
    let has_shift = combo.shift;
    let has_cmd = combo.cmd;

    let unmod_char = unmod_str.chars().next().unwrap_or('\0');

    // Cmd key with no matching terminal binding — ignore (handled by performKeyEquivalent)
    if has_cmd {
        return Vec::new();
    }

    // Kitty keyboard protocol — disambiguate mode (flags & 1)
    if kitty_flags & 1 != 0 && (has_ctrl || has_alt) {
        // Special keys → xterm-style modifier sequences
        match unmod_char {
            '\u{F700}' => return modified_special(b'A', has_ctrl, has_alt, has_shift),
            '\u{F701}' => return modified_special(b'B', has_ctrl, has_alt, has_shift),
            '\u{F702}' => return modified_special(b'D', has_ctrl, has_alt, has_shift),
            '\u{F703}' => return modified_special(b'C', has_ctrl, has_alt, has_shift),
            '\u{F729}' => return modified_special(b'H', has_ctrl, has_alt, has_shift),
            '\u{F72B}' => return modified_special(b'F', has_ctrl, has_alt, has_shift),
            '\u{F727}' => return modified_tilde(2, has_ctrl, has_alt, has_shift),
            '\u{F728}' => return modified_tilde(3, has_ctrl, has_alt, has_shift),
            '\u{F72C}' => return modified_tilde(5, has_ctrl, has_alt, has_shift),
            '\u{F72D}' => return modified_tilde(6, has_ctrl, has_alt, has_shift),
            '\u{F704}' => return modified_special(b'P', has_ctrl, has_alt, has_shift),
            '\u{F705}' => return modified_special(b'Q', has_ctrl, has_alt, has_shift),
            '\u{F706}' => return modified_special(b'R', has_ctrl, has_alt, has_shift),
            '\u{F707}' => return modified_special(b'S', has_ctrl, has_alt, has_shift),
            _ => {}
        }

        // Regular keys → CSI u
        let codepoint = match unmod_char {
            '\r' => 13,
            '\t' => 9,
            '\u{7f}' | '\u{0008}' => 127,
            '\u{001b}' => 27,
            c if !c.is_control() => c as u32,
            _ => return Vec::new(),
        };
        return csi_u(codepoint, has_ctrl, has_alt, has_shift);
    }

    let Some(chars_str) = chars else { return Vec::new() };

    if has_ctrl && !chars_str.is_empty() {
        let c = chars_str.chars().next().unwrap();
        if c.is_ascii_alphabetic() {
            let ctrl_byte = (c.to_ascii_lowercase() as u8) - b'a' + 1;
            return vec![ctrl_byte];
        }
        match c {
            '[' | '\\' | ']' | '^' | '_' => {
                let ctrl_byte = (c as u8) - b'@';
                return vec![ctrl_byte];
            }
            _ => {}
        }
    }

    match unmod_char {
        '\u{F700}' => return if cursor_keys_app { b"\x1bOA".to_vec() } else { b"\x1b[A".to_vec() },
        '\u{F701}' => return if cursor_keys_app { b"\x1bOB".to_vec() } else { b"\x1b[B".to_vec() },
        '\u{F702}' => return if cursor_keys_app { b"\x1bOD".to_vec() } else { b"\x1b[D".to_vec() },
        '\u{F703}' => return if cursor_keys_app { b"\x1bOC".to_vec() } else { b"\x1b[C".to_vec() },
        '\u{F727}' => return b"\x1b[2~".to_vec(),
        '\u{F728}' => return b"\x1b[3~".to_vec(),
        '\u{F729}' => return b"\x1b[H".to_vec(),
        '\u{F72B}' => return b"\x1b[F".to_vec(),
        '\u{F72C}' => return b"\x1b[5~".to_vec(),
        '\u{F72D}' => return b"\x1b[6~".to_vec(),
        '\u{0019}' => return b"\x1b[Z".to_vec(),
        '\u{F704}' => return b"\x1bOP".to_vec(),
        '\u{F705}' => return b"\x1bOQ".to_vec(),
        '\u{F706}' => return b"\x1bOR".to_vec(),
        '\u{F707}' => return b"\x1bOS".to_vec(),
        _ => {}
    }

    if has_alt && !chars_str.is_empty() {
        if chars_str != unmod_str {
            return chars_str.as_bytes().to_vec();
        }
        let mut bytes = vec![0x1b];
        bytes.extend_from_slice(unmod_str.as_bytes());
        return bytes;
    }

    chars_str.as_bytes().to_vec()
}

/// What `keyDown` does with a key once no overlay or edit field has claimed it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum PaneKeyAction {
    /// Ctrl+F: toggle the filter bar; nothing reaches the PTY.
    ToggleFilter,
    /// Ctrl+Option+Left/Right: grow or shrink the virtual width; nothing reaches the PTY.
    AdjustVirtualWidth(f32),
    /// The key goes to the focused pane.
    Send {
        /// Ctrl+L: wipe Kova's scrollback first (the caller skips it in alt-screen),
        /// then let the key through so the app redraws.
        wipe_scrollback: bool,
        /// True: straight to `handle_key_event` (kitty mode, modified key that does
        /// not compose text). False: through macOS text input (dead keys, IME).
        direct: bool,
    },
}

/// The routing decision of `keyDown` for the focused pane, from the event's modifiers,
/// its `charactersIgnoringModifiers` and the pane's kitty flags.
pub fn pane_key_action(ctrl: bool, option: bool, cmd: bool, unmod: Option<&str>, kitty_flags: u8) -> PaneKeyAction {
    // Ctrl+F → toggle filter (in addition to Cmd+F via performKeyEquivalent)
    if ctrl && !cmd && unmod == Some("f") {
        return PaneKeyAction::ToggleFilter;
    }

    // Ctrl+L → wipe Kova's scrollback, then let the key through so the app
    // redraws. The app's own answer to Ctrl+L is `ESC[H ESC[2J` (terminfo
    // `clear`), which only erases the visible screen — the scrollback needs
    // `ESC[3J`, which nothing sends here. Skipped in alt-screen (by the caller),
    // where Ctrl+L means "repaint" and the scrollback holds whatever ran before
    // the full-screen app.
    let wipe_scrollback = ctrl && !option && !cmd && unmod == Some("l");

    // Ctrl+Option+arrows → adjust virtual width
    if ctrl && option && !cmd {
        match unmod {
            Some("\u{f703}") => return PaneKeyAction::AdjustVirtualWidth(1.0),
            Some("\u{f702}") => return PaneKeyAction::AdjustVirtualWidth(-1.0),
            _ => {}
        }
    }

    // Option alone on a printable key composes a character on macOS
    // layouts (French: Option+$ = €, Option+( = {). It must go through
    // text input, otherwise kitty mode sends Alt+$ and the char is lost.
    let option_composes_text = option && !ctrl && unmod
        .and_then(|s| s.chars().next())
        .is_some_and(|c| !c.is_control() && !('\u{F700}'..='\u{F8FF}').contains(&c));

    let direct = kitty_flags > 0 && (ctrl || option) && !cmd && !option_composes_text;
    PaneKeyAction::Send { wipe_scrollback, direct }
}

/// SGR mouse modifier bits: shift=4, option=8, ctrl=16.
pub fn mouse_modifiers(shift: bool, option: bool, ctrl: bool) -> u8 {
    let mut m: u8 = 0;
    if shift { m |= 4; }
    if option { m |= 8; }
    if ctrl { m |= 16; }
    m
}

/// An SGR mouse report. `button_code` is the base button (0=left, 1=middle, 2=right,
/// 3=no button, 64/65=scroll). `press` = true for press/motion ('M'), false for
/// release ('m'). `motion` = true adds +32 to the button code for motion events.
pub fn sgr_mouse_bytes(button_code: u8, mods: u8, col: u16, row: u16, press: bool, motion: bool) -> Vec<u8> {
    let cb = button_code | mods | if motion { 32 } else { 0 };
    let suffix = if press { 'M' } else { 'm' };
    format!("\x1b[<{};{};{}{}", cb, col, row, suffix).into_bytes()
}

/// Lines to scroll for one scroll-wheel event, and the pane's new accumulator.
/// A trackpad accumulates `dy / sensitivity` and scrolls whole lines, keeping the
/// remainder; a mouse wheel scrolls `dy` truncated, leaving the accumulator alone.
/// Positive = toward older output.
pub fn scroll_lines(accumulator: f64, dy: f64, is_trackpad: bool, sensitivity: f64) -> (i32, f64) {
    if is_trackpad {
        let acc = accumulator + dy / sensitivity;
        let discrete = acc as i32;
        (discrete, acc - discrete as f64)
    } else {
        (dy as i32, accumulator)
    }
}

/// Scroll lines forwarded to an app with mouse reporting: the SGR button (64=up,
/// 65=down) and how many reports to send, one per line.
pub fn wheel_reports(lines: i32) -> (u8, usize) {
    (if lines > 0 { 64 } else { 65 }, lines.unsigned_abs() as usize)
}

#[cfg(test)]
pub(crate) mod tests {
    //! Characterization of the key → bytes table as of 13/10/2026, before the
    //! embedding refactor (docs/embedding.md). Every case is played through the
    //! real `handle_key_event` with a synthetic NSEvent and a capture PTY.
    use super::*;
    use crate::config::KeysConfig;
    use objc2_app_kit::{NSEventModifierFlags, NSEventType};
    use objc2_foundation::{NSPoint, NSString};
    use std::os::fd::{AsRawFd, OwnedFd};

    pub(crate) struct Case {
        pub name: &'static str,
        pub chars: &'static str,
        pub unmod: &'static str,
        pub keycode: u16,
        pub mods: &'static str,
        pub app_cursor: bool,
        pub kitty: u8,
        pub out: &'static [u8],
    }

    const fn c(name: &'static str, chars: &'static str, unmod: &'static str, keycode: u16,
               mods: &'static str, out: &'static [u8]) -> Case {
        Case { name, chars, unmod, keycode, mods, app_cursor: false, kitty: 0, out }
    }
    const fn app(mut k: Case) -> Case { k.app_cursor = true; k }
    const fn kitty(mut k: Case) -> Case { k.kitty = 1; k }

    // macOS virtual keycodes
    const K_A: u16 = 0x00; const K_C: u16 = 0x08; const K_L: u16 = 0x25; const K_X: u16 = 0x07;
    const K_E: u16 = 0x0E; const K_1: u16 = 0x12; const K_LBRACKET: u16 = 0x21;
    const K_RBRACKET: u16 = 0x1E; const K_BACKSLASH: u16 = 0x2A; const K_6: u16 = 0x16;
    const K_MINUS: u16 = 0x1B;
    const K_RETURN: u16 = 0x24; const K_TAB: u16 = 0x30; const K_BACKSPACE: u16 = 0x33;
    const K_ESC: u16 = 0x35; const K_UP: u16 = 0x7E; const K_DOWN: u16 = 0x7D;
    const K_LEFT: u16 = 0x7B; const K_RIGHT: u16 = 0x7C; const K_HOME: u16 = 0x73;
    const K_END: u16 = 0x77; const K_PGUP: u16 = 0x74; const K_PGDN: u16 = 0x79;
    const K_FWD_DEL: u16 = 0x75; const K_HELP: u16 = 0x72; const K_F1: u16 = 0x7A;
    const K_F2: u16 = 0x78; const K_F3: u16 = 0x63; const K_F4: u16 = 0x76;

    pub(crate) const TABLE: &[Case] = &[
        // Plain text
        c("letter", "a", "a", K_A, "", b"a"),
        c("shifted letter", "A", "A", K_A, "shift", b"A"),
        c("utf-8 text", "é", "é", K_E, "", "é".as_bytes()),
        c("dead key yields nothing", "", "e", K_E, "", b""),
        c("return", "\r", "\r", K_RETURN, "", b"\r"),
        c("tab", "\t", "\t", K_TAB, "", b"\t"),
        c("backspace", "\u{7f}", "\u{7f}", K_BACKSPACE, "", b"\x7f"),
        c("escape", "\u{1b}", "\u{1b}", K_ESC, "", b"\x1b"),
        // Ctrl + letter / punctuation
        c("ctrl+a as macOS sends it", "\u{1}", "a", K_A, "ctrl", b"\x01"),
        c("ctrl+a with a letter in characters", "a", "a", K_A, "ctrl", b"\x01"),
        c("ctrl+shift+a with uppercase", "A", "A", K_A, "ctrl shift", b"\x01"),
        c("ctrl+c", "\u{3}", "c", K_C, "ctrl", b"\x03"),
        c("ctrl+l", "\u{c}", "l", K_L, "ctrl", b"\x0c"),
        c("ctrl+[ literal", "[", "[", K_LBRACKET, "ctrl", b"\x1b"),
        c("ctrl+\\ literal", "\\", "\\", K_BACKSLASH, "ctrl", b"\x1c"),
        c("ctrl+] literal", "]", "]", K_RBRACKET, "ctrl", b"\x1d"),
        c("ctrl+^ literal", "^", "^", K_6, "ctrl shift", b"\x1e"),
        c("ctrl+_ literal", "_", "_", K_MINUS, "ctrl shift", b"\x1f"),
        c("ctrl+digit passes the digit", "1", "1", K_1, "ctrl", b"1"),
        c("ctrl+option+a", "\u{1}", "a", K_A, "ctrl alt", b"\x01"),
        // Arrows, normal and application cursor mode
        c("up", "\u{F700}", "\u{F700}", K_UP, "", b"\x1b[A"),
        c("down", "\u{F701}", "\u{F701}", K_DOWN, "", b"\x1b[B"),
        c("left", "\u{F702}", "\u{F702}", K_LEFT, "", b"\x1b[D"),
        c("right", "\u{F703}", "\u{F703}", K_RIGHT, "", b"\x1b[C"),
        app(c("up app", "\u{F700}", "\u{F700}", K_UP, "", b"\x1bOA")),
        app(c("down app", "\u{F701}", "\u{F701}", K_DOWN, "", b"\x1bOB")),
        app(c("left app", "\u{F702}", "\u{F702}", K_LEFT, "", b"\x1bOD")),
        app(c("right app", "\u{F703}", "\u{F703}", K_RIGHT, "", b"\x1bOC")),
        c("ctrl+up drops the modifier without kitty", "\u{F700}", "\u{F700}", K_UP, "ctrl", b"\x1b[A"),
        c("shift+up drops the modifier", "\u{F700}", "\u{F700}", K_UP, "shift", b"\x1b[A"),
        c("option+up drops the modifier", "\u{F700}", "\u{F700}", K_UP, "alt", b"\x1b[A"),
        // Navigation and function keys
        c("help/insert", "\u{F727}", "\u{F727}", K_HELP, "", b"\x1b[2~"),
        c("forward delete", "\u{F728}", "\u{F728}", K_FWD_DEL, "", b"\x1b[3~"),
        c("home", "\u{F729}", "\u{F729}", K_HOME, "", b"\x1b[H"),
        c("end", "\u{F72B}", "\u{F72B}", K_END, "", b"\x1b[F"),
        c("page up", "\u{F72C}", "\u{F72C}", K_PGUP, "", b"\x1b[5~"),
        c("page down", "\u{F72D}", "\u{F72D}", K_PGDN, "", b"\x1b[6~"),
        c("F1", "\u{F704}", "\u{F704}", K_F1, "", b"\x1bOP"),
        c("F2", "\u{F705}", "\u{F705}", K_F2, "", b"\x1bOQ"),
        c("F3", "\u{F706}", "\u{F706}", K_F3, "", b"\x1bOR"),
        c("F4", "\u{F707}", "\u{F707}", K_F4, "", b"\x1bOS"),
        c("shift+tab", "\u{19}", "\u{19}", K_TAB, "shift", b"\x1b[Z"),
        // Option
        c("option composes a different char", "å", "a", K_A, "alt", "å".as_bytes()),
        c("option with an unchanged char is meta", "x", "x", K_X, "alt", b"\x1bx"),
        c("option dead key yields nothing", "", "e", K_E, "alt", b""),
        // Configurable terminal bindings (defaults), checked before everything
        c("cmd+backspace kills the line", "\u{7f}", "\u{7f}", K_BACKSPACE, "cmd", b"\x15"),
        c("cmd+left is home", "\u{F702}", "\u{F702}", K_LEFT, "cmd", b"\x1b[H"),
        c("cmd+right is end", "\u{F703}", "\u{F703}", K_RIGHT, "cmd", b"\x1b[F"),
        c("option+left is word back", "\u{F702}", "\u{F702}", K_LEFT, "alt", b"\x1bb"),
        c("option+right is word forward", "\u{F703}", "\u{F703}", K_RIGHT, "alt", b"\x1bf"),
        c("shift+enter", "\r", "\r", K_RETURN, "shift", b"\x1b[13;2u"),
        kitty(c("shift+enter wins over kitty", "\r", "\r", K_RETURN, "shift", b"\x1b[13;2u")),
        kitty(c("option+left wins over kitty", "\u{F702}", "\u{F702}", K_LEFT, "alt", b"\x1bb")),
        // Cmd without a terminal binding is left to performKeyEquivalent
        c("cmd+a sends nothing", "a", "a", K_A, "cmd", b""),
        c("cmd+up sends nothing", "\u{F700}", "\u{F700}", K_UP, "cmd", b""),
        // Kitty disambiguate (flags & 1), only with ctrl or option
        kitty(c("kitty plain letter stays legacy", "a", "a", K_A, "", b"a")),
        kitty(c("kitty shift letter stays legacy", "A", "A", K_A, "shift", b"A")),
        kitty(c("kitty ctrl+a", "\u{1}", "a", K_A, "ctrl", b"\x1b[97;5u")),
        kitty(c("kitty ctrl+shift+A", "\u{1}", "A", K_A, "ctrl shift", b"\x1b[65;6u")),
        kitty(c("kitty option+a", "å", "a", K_A, "alt", b"\x1b[97;3u")),
        kitty(c("kitty ctrl+option+a", "\u{1}", "a", K_A, "ctrl alt", b"\x1b[97;7u")),
        kitty(c("kitty ctrl+return", "\r", "\r", K_RETURN, "ctrl", b"\x1b[13;5u")),
        kitty(c("kitty option+return", "\r", "\r", K_RETURN, "alt", b"\x1b[13;3u")),
        kitty(c("kitty ctrl+tab", "\t", "\t", K_TAB, "ctrl", b"\x1b[9;5u")),
        kitty(c("kitty ctrl+backspace", "\u{7f}", "\u{7f}", K_BACKSPACE, "ctrl", b"\x1b[127;5u")),
        kitty(c("kitty ctrl+escape", "\u{1b}", "\u{1b}", K_ESC, "ctrl", b"\x1b[27;5u")),
        kitty(c("kitty ctrl+up", "\u{F700}", "\u{F700}", K_UP, "ctrl", b"\x1b[1;5A")),
        kitty(c("kitty ctrl+down", "\u{F701}", "\u{F701}", K_DOWN, "ctrl", b"\x1b[1;5B")),
        kitty(c("kitty ctrl+left", "\u{F702}", "\u{F702}", K_LEFT, "ctrl", b"\x1b[1;5D")),
        kitty(c("kitty ctrl+right", "\u{F703}", "\u{F703}", K_RIGHT, "ctrl", b"\x1b[1;5C")),
        kitty(c("kitty ctrl+shift+up", "\u{F700}", "\u{F700}", K_UP, "ctrl shift", b"\x1b[1;6A")),
        kitty(c("kitty option+up", "\u{F700}", "\u{F700}", K_UP, "alt", b"\x1b[1;3A")),
        kitty(c("kitty ctrl+home", "\u{F729}", "\u{F729}", K_HOME, "ctrl", b"\x1b[1;5H")),
        kitty(c("kitty ctrl+end", "\u{F72B}", "\u{F72B}", K_END, "ctrl", b"\x1b[1;5F")),
        kitty(c("kitty ctrl+insert", "\u{F727}", "\u{F727}", K_HELP, "ctrl", b"\x1b[2;5~")),
        kitty(c("kitty ctrl+delete", "\u{F728}", "\u{F728}", K_FWD_DEL, "ctrl", b"\x1b[3;5~")),
        kitty(c("kitty ctrl+page up", "\u{F72C}", "\u{F72C}", K_PGUP, "ctrl", b"\x1b[5;5~")),
        kitty(c("kitty ctrl+page down", "\u{F72D}", "\u{F72D}", K_PGDN, "ctrl", b"\x1b[6;5~")),
        kitty(c("kitty ctrl+F1", "\u{F704}", "\u{F704}", K_F1, "ctrl", b"\x1b[1;5P")),
        kitty(c("kitty ctrl+F2", "\u{F705}", "\u{F705}", K_F2, "ctrl", b"\x1b[1;5Q")),
        kitty(c("kitty ctrl+F3", "\u{F706}", "\u{F706}", K_F3, "ctrl", b"\x1b[1;5R")),
        kitty(c("kitty ctrl+F4", "\u{F707}", "\u{F707}", K_F4, "ctrl", b"\x1b[1;5S")),
        kitty(c("kitty ctrl with a control unmod sends nothing", "\u{1}", "\u{1}", K_A, "ctrl", b"")),
        kitty(c("kitty plain up stays legacy", "\u{F700}", "\u{F700}", K_UP, "", b"\x1b[A")),
    ];

    fn flags(mods: &str) -> NSEventModifierFlags {
        let mut f = NSEventModifierFlags::empty();
        for m in mods.split_whitespace() {
            f |= match m {
                "cmd" => NSEventModifierFlags::Command,
                "ctrl" => NSEventModifierFlags::Control,
                "alt" => NSEventModifierFlags::Option,
                "shift" => NSEventModifierFlags::Shift,
                other => panic!("unknown modifier {other}"),
            };
        }
        f
    }

    pub(crate) fn key_event(chars: &str, unmod: &str, keycode: u16, mods: &str)
        -> objc2::rc::Retained<NSEvent>
    {
        NSEvent::keyEventWithType_location_modifierFlags_timestamp_windowNumber_context_characters_charactersIgnoringModifiers_isARepeat_keyCode(
            NSEventType::KeyDown,
            NSPoint::new(0.0, 0.0),
            flags(mods),
            0.0,
            0,
            None,
            &NSString::from_str(chars),
            &NSString::from_str(unmod),
            false,
            keycode,
        )
        .expect("synthetic key event")
    }

    /// Everything readable on the raw end, waiting briefly for late bytes.
    pub(crate) fn drain(fd: &OwnedFd) -> Vec<u8> {
        let mut out = Vec::new();
        loop {
            let mut pfd = libc::pollfd { fd: fd.as_raw_fd(), events: libc::POLLIN, revents: 0 };
            let ready = unsafe { libc::poll(&mut pfd, 1, 30) };
            if ready <= 0 {
                return out;
            }
            let mut buf = [0u8; 256];
            let n = unsafe { libc::read(fd.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len()) };
            if n <= 0 {
                return out;
            }
            out.extend_from_slice(&buf[..n as usize]);
        }
    }

    fn keybindings() -> Keybindings {
        Keybindings::from_config(&KeysConfig::default())
    }

    #[test]
    fn handle_key_event_matches_the_frozen_table() {
        let kb = keybindings();
        let (pty, reader) = Pty::capture();
        let mut failures = Vec::new();
        for k in TABLE {
            let ev = key_event(k.chars, k.unmod, k.keycode, k.mods);
            handle_key_event(&ev, &pty, k.app_cursor, &kb, k.kitty);
            let got = drain(&reader);
            if got != k.out {
                failures.push(format!("{}: expected {:?}, got {:?}",
                    k.name, String::from_utf8_lossy(k.out), String::from_utf8_lossy(&got)));
            }
        }
        assert!(failures.is_empty(), "{} case(s) moved:\n{}", failures.len(), failures.join("\n"));
    }

    #[test]
    fn key_bytes_matches_the_frozen_table() {
        let kb = keybindings();
        let mut failures = Vec::new();
        for k in TABLE {
            let combo = KeyCombo::from_event(&key_event(k.chars, k.unmod, k.keycode, k.mods));
            let got = key_bytes(&combo, Some(k.chars), k.unmod, k.app_cursor, &kb, k.kitty);
            if got != k.out {
                failures.push(format!("{}: expected {:?}, got {:?}",
                    k.name, String::from_utf8_lossy(k.out), String::from_utf8_lossy(&got)));
            }
        }
        assert!(failures.is_empty(), "{} case(s) moved:\n{}", failures.len(), failures.join("\n"));
    }

    #[test]
    fn key_bytes_without_characters_sends_nothing_outside_kitty() {
        let kb = keybindings();
        let combo = KeyCombo { cmd: false, ctrl: false, option: false, shift: false,
            key: crate::keybindings::Key::Char('a') };
        assert_eq!(key_bytes(&combo, None, "a", false, &kb, 0), b"");
        let ctrl = KeyCombo { ctrl: true, ..combo };
        assert_eq!(key_bytes(&ctrl, None, "a", false, &kb, 1), b"\x1b[97;5u",
            "kitty reads only the unmodified character");
    }

    // pane_key_action (keyDown routing)

    fn route(mods: &str, unmod: Option<&str>, kitty: u8) -> PaneKeyAction {
        let has = |m: &str| mods.split_whitespace().any(|x| x == m);
        pane_key_action(has("ctrl"), has("alt"), has("cmd"), unmod, kitty)
    }
    const TEXT: PaneKeyAction = PaneKeyAction::Send { wipe_scrollback: false, direct: false };
    const DIRECT: PaneKeyAction = PaneKeyAction::Send { wipe_scrollback: false, direct: true };

    #[test]
    fn ctrl_f_toggles_the_filter() {
        assert_eq!(route("ctrl", Some("f"), 0), PaneKeyAction::ToggleFilter);
        assert_eq!(route("ctrl", Some("f"), 1), PaneKeyAction::ToggleFilter, "even in kitty mode");
        assert_eq!(route("ctrl alt", Some("f"), 0), PaneKeyAction::ToggleFilter, "option does not block it");
        assert_eq!(route("ctrl shift", Some("f"), 0), PaneKeyAction::ToggleFilter, "shift does not block it");
        assert_eq!(route("ctrl shift", Some("F"), 0), TEXT, "an uppercase F is not Ctrl+F");
        assert_eq!(route("ctrl cmd", Some("f"), 0), TEXT, "cmd blocks it");
        assert_eq!(route("", Some("f"), 0), TEXT);
    }

    #[test]
    fn ctrl_l_wipes_the_scrollback_and_still_sends_the_key() {
        assert_eq!(route("ctrl", Some("l"), 0), PaneKeyAction::Send { wipe_scrollback: true, direct: false });
        assert_eq!(route("ctrl", Some("l"), 1), PaneKeyAction::Send { wipe_scrollback: true, direct: true });
        assert_eq!(route("ctrl shift", Some("l"), 0), PaneKeyAction::Send { wipe_scrollback: true, direct: false });
        assert_eq!(route("ctrl alt", Some("l"), 0), TEXT, "option blocks the wipe");
        assert_eq!(route("ctrl alt", Some("l"), 1), DIRECT);
        assert_eq!(route("ctrl cmd", Some("l"), 0), TEXT, "cmd blocks the wipe");
        assert_eq!(route("ctrl", Some("L"), 0), TEXT, "uppercase L does not wipe");
    }

    #[test]
    fn ctrl_option_left_right_adjust_the_virtual_width() {
        assert_eq!(route("ctrl alt", Some("\u{f703}"), 0), PaneKeyAction::AdjustVirtualWidth(1.0));
        assert_eq!(route("ctrl alt", Some("\u{f702}"), 1), PaneKeyAction::AdjustVirtualWidth(-1.0));
        assert_eq!(route("ctrl alt shift", Some("\u{f703}"), 0), PaneKeyAction::AdjustVirtualWidth(1.0));
        assert_eq!(route("ctrl alt cmd", Some("\u{f703}"), 0), TEXT);
        assert_eq!(route("ctrl alt", Some("\u{f700}"), 0), TEXT, "up is not a width key");
        assert_eq!(route("ctrl alt", Some("\u{f700}"), 1), DIRECT);
        assert_eq!(route("ctrl", Some("\u{f703}"), 1), DIRECT);
    }

    #[test]
    fn without_kitty_every_key_goes_through_text_input() {
        for mods in ["", "shift", "ctrl", "alt", "ctrl alt", "cmd"] {
            assert_eq!(route(mods, Some("a"), 0), TEXT, "{mods}");
            assert_eq!(route(mods, Some("\u{f700}"), 0), TEXT, "{mods} up");
        }
    }

    #[test]
    fn kitty_sends_ctrl_and_option_keys_directly() {
        assert_eq!(route("ctrl", Some("a"), 1), DIRECT);
        assert_eq!(route("ctrl alt", Some("a"), 1), DIRECT);
        assert_eq!(route("alt", Some("\u{f700}"), 1), DIRECT, "option+arrow is not text");
        assert_eq!(route("alt", Some("\u{1b}"), 1), DIRECT, "option+escape is a control char");
        assert_eq!(route("alt", Some("\r"), 1), DIRECT);
        assert_eq!(route("ctrl", Some("a"), 31), DIRECT, "any non-zero flags");
        assert_eq!(route("alt", None, 1), DIRECT, "no characters at all");
    }

    #[test]
    fn kitty_leaves_plain_shift_and_cmd_keys_to_text_input() {
        assert_eq!(route("", Some("a"), 1), TEXT);
        assert_eq!(route("shift", Some("A"), 1), TEXT);
        assert_eq!(route("cmd", Some("a"), 1), TEXT);
        assert_eq!(route("ctrl cmd", Some("a"), 1), TEXT);
    }

    #[test]
    fn option_on_a_printable_key_composes_even_in_kitty_mode() {
        // French layout: Option+$ = €, Option+( = {, Option+e = dead acute.
        assert_eq!(route("alt", Some("$"), 1), TEXT);
        assert_eq!(route("alt", Some("("), 1), TEXT);
        assert_eq!(route("alt", Some("e"), 1), TEXT);
        assert_eq!(route("alt shift", Some("E"), 1), TEXT);
        assert_eq!(route("alt", Some("é"), 1), TEXT, "non-ASCII printable");
        assert_eq!(route("alt", Some("\u{f704}"), 1), DIRECT, "F1 is in the private-use range");
        assert_eq!(route("alt", Some("\u{f8ff}"), 1), DIRECT, "upper bound of the excluded range");
        assert_eq!(route("ctrl alt", Some("$"), 1), DIRECT, "ctrl+option never composes");
    }

    // Mouse reports and scroll step

    #[test]
    fn mouse_modifier_bits() {
        assert_eq!(mouse_modifiers(false, false, false), 0);
        assert_eq!(mouse_modifiers(true, false, false), 4);
        assert_eq!(mouse_modifiers(false, true, false), 8);
        assert_eq!(mouse_modifiers(false, false, true), 16);
        assert_eq!(mouse_modifiers(true, true, true), 28);
    }

    #[test]
    fn sgr_reports() {
        let s = |b, m, c, r, p, mo| String::from_utf8(sgr_mouse_bytes(b, m, c, r, p, mo)).unwrap();
        assert_eq!(s(0, 0, 1, 1, true, false), "\x1b[<0;1;1M", "left press");
        assert_eq!(s(0, 0, 12, 7, false, false), "\x1b[<0;12;7m", "left release");
        assert_eq!(s(2, 0, 3, 4, true, false), "\x1b[<2;3;4M", "right press");
        assert_eq!(s(0, 0, 5, 6, true, true), "\x1b[<32;5;6M", "left drag");
        assert_eq!(s(3, 0, 5, 6, true, true), "\x1b[<35;5;6M", "motion without button");
        assert_eq!(s(64, 0, 9, 2, true, false), "\x1b[<64;9;2M", "wheel up");
        assert_eq!(s(65, 16, 9, 2, true, false), "\x1b[<81;9;2M", "ctrl+wheel down");
        assert_eq!(s(0, 4 | 8, 300, 120, true, false), "\x1b[<12;300;120M", "shift+option, large coords");
    }

    #[test]
    fn trackpad_scroll_accumulates_and_keeps_the_remainder() {
        // Default sensitivity 6.0: 6 points of delta = one line.
        let (l, acc) = scroll_lines(0.0, 4.0, true, 6.0);
        assert_eq!(l, 0);
        assert!((acc - 4.0 / 6.0).abs() < 1e-12);
        let (l, acc) = scroll_lines(acc, 4.0, true, 6.0);
        assert_eq!(l, 1);
        assert!((acc - 2.0 / 6.0).abs() < 1e-12);
        let (l, acc) = scroll_lines(0.0, 20.0, true, 6.0);
        assert_eq!(l, 3);
        assert!((acc - 2.0 / 6.0).abs() < 1e-12);
        // Downward deltas truncate toward zero too.
        let (l, acc) = scroll_lines(0.0, -13.0, true, 6.0);
        assert_eq!(l, -2);
        assert!((acc + 1.0 / 6.0).abs() < 1e-12);
        // A direction change eats the remainder first.
        let (l, _) = scroll_lines(0.5, -4.0, true, 6.0);
        assert_eq!(l, 0);
        // Sensitivity divides the delta.
        assert_eq!(scroll_lines(0.0, 6.0, true, 2.0).0, 3);
    }

    #[test]
    fn mouse_wheel_scrolls_the_truncated_delta_and_leaves_the_accumulator() {
        assert_eq!(scroll_lines(0.25, 3.9, false, 6.0), (3, 0.25));
        assert_eq!(scroll_lines(0.0, -1.5, false, 6.0), (-1, 0.0));
        assert_eq!(scroll_lines(0.0, 0.4, false, 6.0), (0, 0.0));
    }

    #[test]
    fn wheel_reports_one_per_line() {
        assert_eq!(wheel_reports(3), (64, 3));
        assert_eq!(wheel_reports(-2), (65, 2));
        assert_eq!(wheel_reports(1), (64, 1));
    }

    #[test]
    fn write_text_sends_utf8_and_skips_empty() {
        let (pty, reader) = Pty::capture();
        write_text("", &pty);
        assert_eq!(drain(&reader), b"");
        write_text("héllo ✓", &pty);
        assert_eq!(drain(&reader), "héllo ✓".as_bytes());
    }
}
