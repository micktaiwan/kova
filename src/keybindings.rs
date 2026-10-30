use objc2_app_kit::{NSEvent, NSEventModifierFlags};
use std::collections::HashMap;

use crate::pane::{NavDirection, SplitAxis};

/// A hashable key combination (modifiers + key).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct KeyCombo {
    pub cmd: bool,
    pub ctrl: bool,
    pub option: bool,
    pub shift: bool,
    pub key: Key,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Key {
    Char(char),
    Up,
    Down,
    Left,
    Right,
    Backspace,
    Enter,
}

/// Window/tab/split actions dispatched from performKeyEquivalent.
#[derive(Debug, Clone)]
pub enum Action {
    NewTab,
    ClosePaneOrTab,
    VSplit,
    HSplit,
    VSplitRoot,
    HSplitRoot,
    NewWindow,
    CloseWindow,
    KillWindow,
    Copy,
    CopyRaw,
    Paste,
    ToggleFilter,
    PrevTab,
    NextTab,
    RenameTab,
    RenamePane,
    DetachTab,
    BreakPane,
    MergeTab,
    MergeWindow,

    SwitchTab(usize),
    Navigate(NavDirection),
    SwapPane(NavDirection),
    ReparentPane(NavDirection),
    Resize(SplitAxis, f32),
    EdgeGrow(f32),
    MinimizePane,
    RestoreLastMinimized,
    ToggleHelp,
    MemReport,
    CloseTab,
    /// Put the most recently closed pane back, conversation included.
    ReopenClosedPane,
    OpenRecentProject,
    OpenSearchPalette,
    OpenPaneSwitcher,
    /// The same switcher, opened with only the panes asking for something.
    OpenUnreadSwitcher,
    /// Save the focused pane's conversation to the bookmark list, or drop it if
    /// it is already there.
    ToggleBookmark,
    /// Anchor or un-anchor the focused pane's conversation.
    ToggleAnchor,
    /// Jump to the first anchor, reopening it when no pane holds it.
    FocusAnchor,
    Equalize,
    RepaintPane,
    NextAttention,
    HistoryBack,
    HistoryForward,
}

/// Terminal-level actions dispatched from handle_key_event.
#[derive(Debug, Clone)]
pub enum TerminalAction {
    KillLine,
    Home,
    End,
    WordBack,
    WordForward,
    ShiftEnter,
}

pub struct Keybindings {
    pub window_map: HashMap<KeyCombo, Action>,
    pub terminal_map: HashMap<KeyCombo, TerminalAction>,
}

impl KeyCombo {
    pub fn from_event(event: &NSEvent) -> Self {
        let modifiers = event.modifierFlags();
        let cmd = modifiers.contains(NSEventModifierFlags::Command);
        let ctrl = modifiers.contains(NSEventModifierFlags::Control);
        let option = modifiers.contains(NSEventModifierFlags::Option);
        let shift = modifiers.contains(NSEventModifierFlags::Shift);
        Self::from_parts(cmd, ctrl, option, shift, event.keyCode(), || {
            event.charactersIgnoringModifiers().map(|s| s.to_string())
        })
    }

    /// The combo of a key event given as its parts: the modifiers, the macOS
    /// virtual keycode, and `charactersIgnoringModifiers` (asked for only
    /// when the keycode does not decide). What `from_event` reads off an
    /// `NSEvent`, and what a host that hands over plain values (the C API)
    /// provides itself.
    pub fn from_parts(
        cmd: bool,
        ctrl: bool,
        option: bool,
        shift: bool,
        keycode: u16,
        chars_ignoring_modifiers: impl FnOnce() -> Option<String>,
    ) -> Self {
        // Use keycodes only for special keys (arrows, enter, backspace) where
        // charactersIgnoringModifiers returns private-use Unicode characters.
        // For all other keys, use charactersIgnoringModifiers which respects the
        // active keyboard layout (AZERTY, QWERTZ, etc.).
        let key = keycode_to_special(keycode)
            .or_else(|| keycode_to_digit(keycode))
            .unwrap_or_else(|| {
                let ch_str = chars_ignoring_modifiers().unwrap_or_default();
                let c = ch_str.chars().next().unwrap_or('\0');
                Key::Char(c.to_ascii_lowercase())
            });

        KeyCombo { cmd, ctrl, option, shift, key }
    }
}

/// Map macOS virtual keycodes to Key for special keys only (arrows, enter,
/// backspace). Character keys are resolved via charactersIgnoringModifiers
/// to respect the active keyboard layout.
fn keycode_to_special(code: u16) -> Option<Key> {
    match code {
        0x24 => Some(Key::Enter),
        0x33 => Some(Key::Backspace),
        0x7E => Some(Key::Up),
        0x7D => Some(Key::Down),
        0x7B => Some(Key::Left),
        0x7C => Some(Key::Right),
        _ => None,
    }
}

/// Map the number-row keycodes to their digit. On AZERTY the unshifted
/// characters of that row are `&é"'(§è!çà`, so charactersIgnoringModifiers
/// would turn Cmd+1 into Cmd+& and the `cmd+1` binding would never match.
/// Keycodes are physical positions, identical on every layout.
fn keycode_to_digit(code: u16) -> Option<Key> {
    let digit = match code {
        0x12 => '1',
        0x13 => '2',
        0x14 => '3',
        0x15 => '4',
        0x17 => '5',
        0x16 => '6',
        0x1A => '7',
        0x1C => '8',
        0x19 => '9',
        0x1D => '0',
        _ => return None,
    };
    Some(Key::Char(digit))
}

/// Parse a string like "cmd+shift+d" into a KeyCombo.
fn parse_key_combo(s: &str) -> KeyCombo {
    let mut combo = KeyCombo {
        cmd: false,
        ctrl: false,
        option: false,
        shift: false,
        key: Key::Char('\0'),
    };

    // Split modifiers from key. The key is everything after the last '+' that
    // is not a known modifier. Handle trailing '+' as the literal '+' key
    // (e.g. "cmd+shift++" → modifiers=[cmd,shift], key='+').
    let parts: Vec<&str> = s.split('+').collect();
    // If the string ends with '+', the last element is "" — the key is '+'
    let (modifier_parts, key_str) = if parts.last() == Some(&"") && parts.len() >= 2 {
        (&parts[..parts.len() - 1], "+")
    } else {
        (&parts[..parts.len() - 1], parts[parts.len() - 1])
    };

    for part in modifier_parts {
        let trimmed = part.trim();
        if trimmed.is_empty() {
            // Artifact of splitting a literal '+' key on '+' (e.g.
            // "cmd+shift++" → ["cmd", "shift", "", ""]) — not a modifier.
            continue;
        }
        if trimmed.eq_ignore_ascii_case("cmd") || trimmed.eq_ignore_ascii_case("command") {
            combo.cmd = true;
        } else if trimmed.eq_ignore_ascii_case("ctrl") || trimmed.eq_ignore_ascii_case("control") {
            combo.ctrl = true;
        } else if trimmed.eq_ignore_ascii_case("option") || trimmed.eq_ignore_ascii_case("alt") || trimmed.eq_ignore_ascii_case("opt") {
            combo.option = true;
        } else if trimmed.eq_ignore_ascii_case("shift") {
            combo.shift = true;
        } else {
            log::warn!("Unknown modifier in keybinding: {}", trimmed);
        }
    }

    let key_trimmed = key_str.trim();
    let lower = key_trimmed.to_ascii_lowercase();
    combo.key = match lower.as_str() {
        "up" => Key::Up,
        "down" => Key::Down,
        "left" => Key::Left,
        "right" => Key::Right,
        "backspace" | "delete" => Key::Backspace,
        "enter" | "return" => Key::Enter,
        "[" => Key::Char('['),
        "]" => Key::Char(']'),
        "+" => Key::Char('+'),
        s if s.len() == 1 => Key::Char(s.chars().next().unwrap()),
        _ => {
            log::warn!("Unknown key in keybinding: {}", key_trimmed);
            Key::Char('\0')
        }
    };

    combo
}

use crate::config::KeysConfig;

impl Keybindings {
    pub fn from_config(keys: &KeysConfig) -> Self {
        let mut window_map = HashMap::new();
        let mut terminal_map = HashMap::new();

        let mut bind = |s: &str, action: Action| {
            let combo = parse_key_combo(s);
            window_map.insert(combo, action);
        };

        bind(&keys.new_tab, Action::NewTab);
        bind(&keys.close_pane_or_tab, Action::ClosePaneOrTab);
        bind(&keys.vsplit, Action::VSplit);
        bind(&keys.hsplit, Action::HSplit);
        bind(&keys.vsplit_root, Action::VSplitRoot);
        bind(&keys.hsplit_root, Action::HSplitRoot);
        bind(&keys.new_window, Action::NewWindow);
        bind(&keys.close_window, Action::CloseWindow);
        bind(&keys.kill_window, Action::KillWindow);
        bind(&keys.copy, Action::Copy);
        bind(&keys.copy_raw, Action::CopyRaw);
        bind(&keys.paste, Action::Paste);
        bind(&keys.toggle_filter, Action::ToggleFilter);
        bind(&keys.prev_tab, Action::PrevTab);
        bind(&keys.next_tab, Action::NextTab);
        bind(&keys.rename_tab, Action::RenameTab);
        bind(&keys.rename_pane, Action::RenamePane);
        bind(&keys.detach_tab, Action::DetachTab);
        bind(&keys.break_pane, Action::BreakPane);
        bind(&keys.merge_tab, Action::MergeTab);
        bind(&keys.merge_window, Action::MergeWindow);


        for (i, s) in [
            &keys.switch_tab_1, &keys.switch_tab_2, &keys.switch_tab_3,
            &keys.switch_tab_4, &keys.switch_tab_5, &keys.switch_tab_6,
            &keys.switch_tab_7, &keys.switch_tab_8, &keys.switch_tab_9,
        ].iter().enumerate() {
            bind(s, Action::SwitchTab(i));
        }

        bind(&keys.navigate_up, Action::Navigate(NavDirection::Up));
        bind(&keys.navigate_down, Action::Navigate(NavDirection::Down));
        bind(&keys.navigate_left, Action::Navigate(NavDirection::Left));
        bind(&keys.navigate_right, Action::Navigate(NavDirection::Right));

        bind(&keys.swap_up, Action::SwapPane(NavDirection::Up));
        bind(&keys.swap_down, Action::SwapPane(NavDirection::Down));
        bind(&keys.swap_left, Action::SwapPane(NavDirection::Left));
        bind(&keys.swap_right, Action::SwapPane(NavDirection::Right));

        bind(&keys.reparent_up, Action::ReparentPane(NavDirection::Up));
        bind(&keys.reparent_down, Action::ReparentPane(NavDirection::Down));
        bind(&keys.reparent_left, Action::ReparentPane(NavDirection::Left));
        bind(&keys.reparent_right, Action::ReparentPane(NavDirection::Right));

        bind(&keys.resize_left, Action::Resize(SplitAxis::Horizontal, -0.05));
        bind(&keys.resize_right, Action::Resize(SplitAxis::Horizontal, 0.05));
        bind(&keys.resize_up, Action::Resize(SplitAxis::Vertical, -0.05));
        bind(&keys.resize_down, Action::Resize(SplitAxis::Vertical, 0.05));
        bind(&keys.edge_grow_right, Action::EdgeGrow(1.0));
        bind(&keys.edge_grow_left, Action::EdgeGrow(-1.0));
        bind(&keys.minimize_pane, Action::MinimizePane);
        bind(&keys.restore_minimized, Action::RestoreLastMinimized);
        bind(&keys.toggle_help, Action::ToggleHelp);
        bind(&keys.close_tab, Action::CloseTab);
        bind(&keys.reopen_closed_pane, Action::ReopenClosedPane);
        bind(&keys.open_recent_project, Action::OpenRecentProject);
        bind(&keys.open_search, Action::OpenSearchPalette);
        bind(&keys.open_pane_switcher, Action::OpenPaneSwitcher);
        bind(&keys.open_unread_switcher, Action::OpenUnreadSwitcher);
        bind(&keys.toggle_bookmark, Action::ToggleBookmark);
        bind(&keys.toggle_anchor, Action::ToggleAnchor);
        bind(&keys.focus_anchor, Action::FocusAnchor);
        bind(&keys.equalize, Action::Equalize);
        bind(&keys.repaint_pane, Action::RepaintPane);
        bind(&keys.next_attention, Action::NextAttention);
        bind(&keys.history_back, Action::HistoryBack);
        bind(&keys.history_forward, Action::HistoryForward);

        // Hard-coded debug binding (not user-configurable)
        window_map.insert(parse_key_combo("cmd+shift+i"), Action::MemReport);

        let term = &keys.terminal;
        let mut tbind = |s: &str, action: TerminalAction| {
            let combo = parse_key_combo(s);
            terminal_map.insert(combo, action);
        };

        tbind(&term.kill_line, TerminalAction::KillLine);
        tbind(&term.home, TerminalAction::Home);
        tbind(&term.end, TerminalAction::End);
        tbind(&term.word_back, TerminalAction::WordBack);
        tbind(&term.word_forward, TerminalAction::WordForward);
        tbind(&term.shift_enter, TerminalAction::ShiftEnter);

        Keybindings { window_map, terminal_map }
    }
}

/// Map a stable kebab-case action name (used by the IPC `dispatch-action`
/// command) to an [`Action`]. This is the single canonical name → action table
/// so external scripts can trigger any keyboard action. Returns `None` for an
/// unknown name.
///
/// Resize/edge-grow deltas mirror the keyboard bindings in
/// [`Keybindings::from_config`] so a dispatched action behaves identically to
/// its keystroke.
pub fn action_from_ipc_name(name: &str) -> Option<Action> {
    let action = match name {
        "new-tab" => Action::NewTab,
        "close-pane-or-tab" => Action::ClosePaneOrTab,
        "vsplit" => Action::VSplit,
        "hsplit" => Action::HSplit,
        "vsplit-root" => Action::VSplitRoot,
        "hsplit-root" => Action::HSplitRoot,
        "new-window" => Action::NewWindow,
        "close-window" => Action::CloseWindow,
        "kill-window" => Action::KillWindow,
        "copy" => Action::Copy,
        "copy-raw" => Action::CopyRaw,
        "paste" => Action::Paste,
        "toggle-filter" => Action::ToggleFilter,
        "prev-tab" => Action::PrevTab,
        "next-tab" => Action::NextTab,
        "rename-tab" => Action::RenameTab,
        "rename-pane" => Action::RenamePane,
        "detach-tab" => Action::DetachTab,
        "break-pane" => Action::BreakPane,
        "merge-tab" => Action::MergeTab,
        "merge-window" => Action::MergeWindow,

        "switch-tab-1" => Action::SwitchTab(0),
        "switch-tab-2" => Action::SwitchTab(1),
        "switch-tab-3" => Action::SwitchTab(2),
        "switch-tab-4" => Action::SwitchTab(3),
        "switch-tab-5" => Action::SwitchTab(4),
        "switch-tab-6" => Action::SwitchTab(5),
        "switch-tab-7" => Action::SwitchTab(6),
        "switch-tab-8" => Action::SwitchTab(7),
        "switch-tab-9" => Action::SwitchTab(8),

        "navigate-up" => Action::Navigate(NavDirection::Up),
        "navigate-down" => Action::Navigate(NavDirection::Down),
        "navigate-left" => Action::Navigate(NavDirection::Left),
        "navigate-right" => Action::Navigate(NavDirection::Right),

        "swap-up" => Action::SwapPane(NavDirection::Up),
        "swap-down" => Action::SwapPane(NavDirection::Down),
        "swap-left" => Action::SwapPane(NavDirection::Left),
        "swap-right" => Action::SwapPane(NavDirection::Right),

        "reparent-up" => Action::ReparentPane(NavDirection::Up),
        "reparent-down" => Action::ReparentPane(NavDirection::Down),
        "reparent-left" => Action::ReparentPane(NavDirection::Left),
        "reparent-right" => Action::ReparentPane(NavDirection::Right),

        "resize-left" => Action::Resize(SplitAxis::Horizontal, -0.05),
        "resize-right" => Action::Resize(SplitAxis::Horizontal, 0.05),
        "resize-up" => Action::Resize(SplitAxis::Vertical, -0.05),
        "resize-down" => Action::Resize(SplitAxis::Vertical, 0.05),
        "edge-grow-right" => Action::EdgeGrow(1.0),
        "edge-grow-left" => Action::EdgeGrow(-1.0),

        "minimize-pane" => Action::MinimizePane,
        "restore-minimized" => Action::RestoreLastMinimized,
        "toggle-help" => Action::ToggleHelp,
        "mem-report" => Action::MemReport,
        "close-tab" => Action::CloseTab,
        "reopen-closed-pane" => Action::ReopenClosedPane,
        "open-recent-project" => Action::OpenRecentProject,
        "open-search" => Action::OpenSearchPalette,
        "open-pane-switcher" => Action::OpenPaneSwitcher,
        "open-unread-switcher" => Action::OpenUnreadSwitcher,
        "toggle-bookmark" => Action::ToggleBookmark,
        "toggle-anchor" => Action::ToggleAnchor,
        "focus-anchor" => Action::FocusAnchor,
        "equalize" => Action::Equalize,
        "repaint-pane" => Action::RepaintPane,
        "next-attention" => Action::NextAttention,
        "history-back" => Action::HistoryBack,
        "history-forward" => Action::HistoryForward,

        _ => return None,
    };
    Some(action)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn number_row_keycodes_map_to_digits() {
        let expected = [
            (0x12, '1'), (0x13, '2'), (0x14, '3'), (0x15, '4'), (0x17, '5'),
            (0x16, '6'), (0x1A, '7'), (0x1C, '8'), (0x19, '9'), (0x1D, '0'),
        ];
        for (code, digit) in expected {
            assert_eq!(keycode_to_digit(code), Some(Key::Char(digit)));
        }
        // 'A' on QWERTY / 'Q' on AZERTY: not a digit key.
        assert_eq!(keycode_to_digit(0x00), None);
    }

    #[test]
    fn cmd_digit_binding_matches_number_row_combo() {
        let combo = KeyCombo {
            cmd: true, ctrl: false, option: false, shift: false,
            key: keycode_to_digit(0x12).unwrap(),
        };
        assert_eq!(parse_key_combo("cmd+1"), combo);
    }

    /// Keycodes of the letter and punctuation keys: their position moves with
    /// the layout (AZERTY's `m` is QWERTY's `;`, its `a` is QWERTY's `q`), so
    /// comparing one of them reads the wrong key on half the keyboards. Digits
    /// (0x12..0x1D) are left out on purpose: see `keycode_to_digit`.
    const LAYOUT_DEPENDENT_KEYCODES: &[u16] = &[
        0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0B, 0x0C, 0x0D, 0x0E,
        0x0F, 0x10, 0x11, 0x18, 0x1B, 0x1E, 0x1F, 0x20, 0x21, 0x22, 0x23, 0x25, 0x26, 0x27,
        0x28, 0x29, 0x2A, 0x2B, 0x2C, 0x2D, 0x2E, 0x2F, 0x32,
    ];

    /// The hex keycodes a line of key-handling code tests: `keycode == 0x2E`
    /// or a `0x2E =>` match arm.
    fn compared_keycodes(line: &str) -> Vec<u16> {
        let code = line.split("//").next().unwrap_or("");
        let mut found = Vec::new();
        let mut rest = code;
        while let Some(i) = rest.find("0x") {
            let after = &rest[i + 2..];
            let hex: String = after.chars().take_while(|c| c.is_ascii_hexdigit()).collect();
            let before = rest[..i].trim_end();
            let tail = after[hex.len()..].trim_start();
            let compared = before.ends_with("==") || before.ends_with('|') || tail.starts_with("=>") || tail.starts_with('|');
            if compared {
                if let Ok(v) = u16::from_str_radix(&hex, 16) {
                    found.push(v);
                }
            }
            rest = &after[hex.len()..];
        }
        found
    }

    #[test]
    fn compared_keycodes_reads_comparisons_and_match_arms_only() {
        assert_eq!(compared_keycodes("if keycode == 0x2E {"), vec![0x2E]);
        assert_eq!(compared_keycodes("            0x0B => Some(View::Bookmarks),"), vec![0x0B]);
        assert_eq!(compared_keycodes("0x00 | 0x0C => x,"), vec![0x00, 0x0C]);
        assert_eq!(compared_keycodes("let ev = Ev { keycode: 0x03 };"), Vec::<u16>::new());
        assert_eq!(compared_keycodes("x // keycode == 0x2E"), Vec::<u16>::new());
    }

    /// Every letter or punctuation shortcut is read from the typed character
    /// (`charactersIgnoringModifiers`), never from `keyCode()`. Scans the
    /// non-test code of each file that reads `keyCode()`.
    #[test]
    fn no_letter_key_is_matched_by_its_keycode() {
        fn walk(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
            for entry in std::fs::read_dir(dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    walk(&path, out);
                } else if path.extension().is_some_and(|e| e == "rs") {
                    out.push(path);
                }
            }
        }
        let mut files = Vec::new();
        walk(&std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src"), &mut files);
        assert!(files.len() > 10, "the scan found the sources");
        let mut offenders = Vec::new();
        for file in files {
            let text = std::fs::read_to_string(&file).unwrap();
            if !text.contains("keyCode()") {
                continue;
            }
            let body = text.split("#[cfg(test)]").next().unwrap_or("");
            for (n, line) in body.lines().enumerate() {
                if compared_keycodes(line).iter().any(|k| LAYOUT_DEPENDENT_KEYCODES.contains(k)) {
                    offenders.push(format!("{}:{}: {}", file.display(), n + 1, line.trim()));
                }
            }
        }
        assert!(offenders.is_empty(), "letter keys matched by keycode (read charactersIgnoringModifiers instead):\n{}", offenders.join("\n"));
    }
}
