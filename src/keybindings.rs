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
    Paste,
    ToggleFilter,
    ClearScrollback,
    PrevTab,
    NextTab,
    RenameTab,
    DetachTab,
    MergeWindow,
    SwitchTab(usize),
    Navigate(NavDirection),
    SwapPane(NavDirection),
    Resize(SplitAxis, f32),
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

        // Use keycodes only for special keys (arrows, enter, backspace) where
        // charactersIgnoringModifiers returns private-use Unicode characters.
        // For all other keys, use charactersIgnoringModifiers which respects the
        // active keyboard layout (AZERTY, QWERTZ, etc.).
        let key = keycode_to_special(event.keyCode())
            .unwrap_or_else(|| {
                let chars = event.charactersIgnoringModifiers();
                let ch_str = chars.map(|s| s.to_string()).unwrap_or_default();
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

/// Parse a string like "cmd+shift+d" into a KeyCombo.
fn parse_key_combo(s: &str) -> KeyCombo {
    let mut combo = KeyCombo {
        cmd: false,
        ctrl: false,
        option: false,
        shift: false,
        key: Key::Char('\0'),
    };

    let num_parts = s.split('+').count();
    for (i, part) in s.split('+').enumerate() {
        let trimmed = part.trim();
        if i < num_parts - 1 {
            // Modifier
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
        } else {
            // Key (last token)
            let lower = trimmed.to_ascii_lowercase();
            combo.key = match lower.as_str() {
                "up" => Key::Up,
                "down" => Key::Down,
                "left" => Key::Left,
                "right" => Key::Right,
                "backspace" | "delete" => Key::Backspace,
                "enter" | "return" => Key::Enter,
                "[" => Key::Char('['),
                "]" => Key::Char(']'),
                s if s.len() == 1 => Key::Char(s.chars().next().unwrap()),
                _ => {
                    log::warn!("Unknown key in keybinding: {}", trimmed);
                    Key::Char('\0')
                }
            };
        }
    }

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
        bind(&keys.paste, Action::Paste);
        bind(&keys.toggle_filter, Action::ToggleFilter);
        bind(&keys.clear_scrollback, Action::ClearScrollback);
        bind(&keys.prev_tab, Action::PrevTab);
        bind(&keys.next_tab, Action::NextTab);
        bind(&keys.rename_tab, Action::RenameTab);
        bind(&keys.detach_tab, Action::DetachTab);
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

        bind(&keys.resize_left, Action::Resize(SplitAxis::Horizontal, -0.05));
        bind(&keys.resize_right, Action::Resize(SplitAxis::Horizontal, 0.05));
        bind(&keys.resize_up, Action::Resize(SplitAxis::Vertical, -0.05));
        bind(&keys.resize_down, Action::Resize(SplitAxis::Vertical, 0.05));

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
