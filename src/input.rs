use objc2_app_kit::{NSEvent, NSEventModifierFlags};

use crate::terminal::pty::Pty;

pub fn handle_key_event(event: &NSEvent, pty: &Pty, cursor_keys_app: bool) {
    let modifiers = event.modifierFlags();

    let has_ctrl = modifiers.contains(NSEventModifierFlags::Control);
    let has_alt = modifiers.contains(NSEventModifierFlags::Option);
    let has_cmd = modifiers.contains(NSEventModifierFlags::Command);
    let has_shift = modifiers.contains(NSEventModifierFlags::Shift);

    let chars_unmod = event.charactersIgnoringModifiers();
    let unmod_str = chars_unmod.map(|s| s.to_string()).unwrap_or_default();
    let unmod_char = unmod_str.chars().next().unwrap_or('\0');

    // Shift+Enter → newline without executing (CSI u: \e[13;2u)
    if has_shift && unmod_char == '\r' {
        pty.write(b"\x1b[13;2u");
        return;
    }

    if has_cmd {
        match unmod_char {
            '\u{7F}' => pty.write(b"\x15"),         // Cmd+Backspace → kill line (Ctrl+U)
            '\u{F702}' => pty.write(b"\x1b[H"),     // Cmd+Left → Home (beginning of line)
            '\u{F703}' => pty.write(b"\x1b[F"),     // Cmd+Right → End (end of line)
            _ => {}
        }
        return;
    }

    let chars = event.characters();
    let chars = match chars {
        Some(c) => c,
        None => return,
    };
    let chars_str = chars.to_string();

    if has_ctrl && !chars_str.is_empty() {
        let c = chars_str.chars().next().unwrap();
        if c.is_ascii_alphabetic() {
            let ctrl_byte = (c.to_ascii_lowercase() as u8) - b'a' + 1;
            pty.write(&[ctrl_byte]);
            return;
        }
        match c {
            '[' | '\\' | ']' | '^' | '_' => {
                let ctrl_byte = (c as u8) - b'@';
                pty.write(&[ctrl_byte]);
                return;
            }
            _ => {}
        }
    }

    // Option+Arrow → word movement
    if has_alt {
        match unmod_char {
            '\u{F702}' => { pty.write(b"\x1bb"); return; } // Option+Left → word back
            '\u{F703}' => { pty.write(b"\x1bf"); return; } // Option+Right → word forward
            _ => {}
        }
    }

    match unmod_char {
        '\u{F700}' => { pty.write(if cursor_keys_app { b"\x1bOA" } else { b"\x1b[A" }); return; }
        '\u{F701}' => { pty.write(if cursor_keys_app { b"\x1bOB" } else { b"\x1b[B" }); return; }
        '\u{F702}' => { pty.write(if cursor_keys_app { b"\x1bOD" } else { b"\x1b[D" }); return; }
        '\u{F703}' => { pty.write(if cursor_keys_app { b"\x1bOC" } else { b"\x1b[C" }); return; }
        '\u{F727}' => { pty.write(b"\x1b[2~"); return; }
        '\u{F728}' => { pty.write(b"\x1b[3~"); return; }
        '\u{F729}' => { pty.write(b"\x1b[H"); return; }
        '\u{F72B}' => { pty.write(b"\x1b[F"); return; }
        '\u{F72C}' => { pty.write(b"\x1b[5~"); return; }
        '\u{F72D}' => { pty.write(b"\x1b[6~"); return; }
        '\u{0019}' => { pty.write(b"\x1b[Z"); return; } // Shift+Tab (backtab)
        '\u{F704}' => { pty.write(b"\x1bOP"); return; }
        '\u{F705}' => { pty.write(b"\x1bOQ"); return; }
        '\u{F706}' => { pty.write(b"\x1bOR"); return; }
        '\u{F707}' => { pty.write(b"\x1bOS"); return; }
        _ => {}
    }

    if has_alt && !chars_str.is_empty() {
        let mut bytes = vec![0x1b];
        bytes.extend_from_slice(unmod_str.as_bytes());
        pty.write(&bytes);
        return;
    }

    if !chars_str.is_empty() {
        pty.write(chars_str.as_bytes());
    }
}
