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
    ClearScrollback,
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
    OpenRecentProject,
    OpenSearchPalette,
    OpenPaneSwitcher,
    Equalize,
    RepaintPane,
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
        bind(&keys.clear_scrollback, Action::ClearScrollback);
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
        bind(&keys.open_recent_project, Action::OpenRecentProject);
        bind(&keys.open_search, Action::OpenSearchPalette);
        bind(&keys.open_pane_switcher, Action::OpenPaneSwitcher);
        bind(&keys.equalize, Action::Equalize);
        bind(&keys.repaint_pane, Action::RepaintPane);

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
        "clear-scrollback" => Action::ClearScrollback,
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
        "open-recent-project" => Action::OpenRecentProject,
        "open-search" => Action::OpenSearchPalette,
        "open-pane-switcher" => Action::OpenPaneSwitcher,
        "equalize" => Action::Equalize,
        "repaint-pane" => Action::RepaintPane,

        _ => return None,
    };
    Some(action)
}
