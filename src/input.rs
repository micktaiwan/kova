//! The typed input API of a terminal surface: keys, text, mouse and scroll
//! as plain Rust values, and every rule that turns them into PTY bytes or
//! terminal state — which bytes a key sends, Ctrl+L, kitty direct routing,
//! the compose-or-send-Option choice, selection on click and drag, SGR mouse
//! reports, scroll stepping. Written once here; a host translates its own
//! platform events into these values (Kova.app: `crate::appkit_input`) and
//! calls the `Surface` methods at the bottom of this file, in the spirit of
//! Ghostty's `ghostty_surface_key/text/mouse/scroll`.

use crate::keybindings::{Keybindings, KeyCombo, TerminalAction};
use crate::surface::Surface;
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

/// Write the bytes of a key press to the PTY; nothing when the key sends nothing.
pub fn write_key(key: &KeyEvent, pty: &Pty, cursor_keys_app: bool, keybindings: &Keybindings, kitty_flags: u8) {
    let unmod_str = key.unmod.as_deref().unwrap_or_default();
    let bytes = key_bytes(&key.combo, key.chars.as_deref(), unmod_str, cursor_keys_app, keybindings, kitty_flags);
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
        /// True: straight to the PTY (kitty mode, modified key that does
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

/// What to tell a program about a focus change (DEC mode 1004): `CSI I` on
/// gaining focus, `CSI O` on losing it — only when the focus really changed
/// (`was` != `now`) and the program turned the mode on. Kova.app reports the
/// app's activation with it, a library pane its view's first-responder state.
pub fn focus_report(was: bool, now: bool, reporting: bool) -> Option<&'static [u8]> {
    if was == now || !reporting {
        return None;
    }
    Some(if now { b"\x1b[I" } else { b"\x1b[O" })
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

/// A key press, as a host's platform hands it over.
#[derive(Debug, Clone, PartialEq)]
pub struct KeyEvent {
    /// Modifiers and the layout-resolved key, what terminal keybindings match on.
    pub combo: KeyCombo,
    /// The text the key produces with its modifiers (macOS `characters`);
    /// `None` when the platform gives none, empty for a dead key.
    pub chars: Option<String>,
    /// The text the key produces without modifiers (macOS
    /// `charactersIgnoringModifiers`).
    pub unmod: Option<String>,
}

/// Keys that belong to the host, not the terminal: nothing reaches the PTY.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum HostKey {
    /// Ctrl+F: toggle the filter bar.
    ToggleFilter,
    /// Ctrl+Option+Left/Right: grow or shrink the virtual width.
    AdjustVirtualWidth(f32),
}

/// What [`Surface::key`] did with a key.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum KeyResult {
    /// A host shortcut; the surface did nothing with it.
    Host(HostKey),
    /// Encoded and written to the PTY (kitty mode, modified key that does not
    /// compose text).
    Sent,
    /// Must go through the platform's text input (dead keys, IME), which ends
    /// in [`Surface::text`] or [`Surface::send_key`].
    Compose,
}

/// The host shortcut a key is, if any — answerable without a surface, so a
/// host whose window has no pane under focus still handles it.
pub fn host_key(key: &KeyEvent) -> Option<HostKey> {
    match pane_key_action(key.combo.ctrl, key.combo.option, key.combo.cmd, key.unmod.as_deref(), 0) {
        PaneKeyAction::ToggleFilter => Some(HostKey::ToggleFilter),
        PaneKeyAction::AdjustVirtualWidth(dir) => Some(HostKey::AdjustVirtualWidth(dir)),
        PaneKeyAction::Send { .. } => None,
    }
}

/// The visible cell under a point `(rel_x, rel_y)` pixels from the pane's text
/// origin (left padding already taken off), for cells of `cell` pixels: the
/// row on screen (`url_at`'s `visible_row`, bottom gravity taken off) and the
/// column, clamped to the last one. `None` above, left of or below the grid.
pub fn visible_row_col(rel_x: f32, rel_y: f32, cell: (f32, f32), y_offset_rows: usize, rows: u16, cols: u16) -> Option<(usize, u16)> {
    let (cell_w, cell_h) = cell;
    let col = (rel_x / cell_w).floor() as i32;
    let visible_row = (rel_y / cell_h).floor() as i32 - y_offset_rows as i32;
    if visible_row < 0 || col < 0 || visible_row >= rows as i32 {
        return None;
    }
    Some((visible_row as usize, (col as u16).min(cols.saturating_sub(1))))
}

/// The URL under a point of the pane (see `visible_row_col`): its segments
/// (`(visible_row, col_start, col_end)`, one per wrapped row) and its text.
/// What Cmd+hover underlines and Cmd+click opens, in Kova.app and in a
/// library pane.
pub fn url_at_point(term: &crate::terminal::TerminalState, rel_x: f32, rel_y: f32, cell: (f32, f32)) -> Option<(Vec<(usize, u16, u16)>, String)> {
    let (row, col) = visible_row_col(rel_x, rel_y, cell, term.y_offset_rows(), term.rows, term.cols)?;
    term.url_at(row, col)
}

/// What a mouse move or a modifier change does to a pane's hovered URL.
#[derive(Debug, Clone, PartialEq)]
pub enum HoverUpdate {
    /// Nothing changed: no redraw, no cursor change.
    Keep,
    /// A new URL is under the pointer with Cmd held: underline it, show the
    /// pointing hand.
    Set(Vec<(usize, u16, u16)>, String),
    /// The hover is gone (Cmd released, or no URL under the pointer).
    Clear,
}

/// Kova.app's Cmd+hover rule: a URL is underlined only while Cmd is held and
/// the pointer is on it; the same URL (same segments, same text) stays as
/// is. `hit` is only asked for with Cmd held. `current` is the segments and
/// text hovered now. Another URL over the same cells (output slid it under a
/// still pointer) is a new hover: its text is what the status bar shows and
/// what Cmd+click opens.
pub fn url_hover_update(
    cmd: bool,
    hit: impl FnOnce() -> Option<(Vec<(usize, u16, u16)>, String)>,
    current: Option<(&[(usize, u16, u16)], &str)>,
) -> HoverUpdate {
    let found = if cmd { hit() } else { None };
    match found {
        Some((segments, url)) if current == Some((segments.as_slice(), url.as_str())) => HoverUpdate::Keep,
        Some((segments, url)) => HoverUpdate::Set(segments, url),
        None if current.is_some() => HoverUpdate::Clear,
        None => HoverUpdate::Keep,
    }
}

/// What the cells under a still pointer depend on. When any of it changes —
/// new output (`content_generation`), the user scrolling the scrollback, the
/// pane laid out elsewhere, another pane under the pointer — the text under
/// the pointer may have moved, and a hovered URL must be checked again.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct HoverStamp {
    pub terminal_id: u32,
    pub generation: u64,
    pub scroll_offset: i32,
    /// Where the pane's text sits, in pixels (its viewport in Kova.app's
    /// window, the cell size and padding in a library pane).
    pub layout: [f32; 4],
}

impl HoverStamp {
    pub fn of(term: &crate::terminal::TerminalState, layout: [f32; 4]) -> Self {
        HoverStamp {
            terminal_id: term.terminal_id,
            generation: term.content_generation(),
            scroll_offset: term.scroll_offset(),
            layout,
        }
    }
}

/// Keeps a Cmd+hover honest while the pointer does not move: the underline
/// is computed on a mouse move, but output or a scroll can slide other text
/// under the pointer afterwards. Remembers the last pointer position and the
/// content the hover was computed against; once per tick, `recheck` says
/// whether the hover must be computed again at that position.
#[derive(Debug, Default)]
pub struct HoverTracker {
    point: Option<(f32, f32)>,
    stamp: Option<HoverStamp>,
}

impl HoverTracker {
    /// The pointer is at `point` and the hover was just computed there,
    /// against the content `stamp` describes (`None` when nothing is hovered).
    pub fn seen(&mut self, point: (f32, f32), stamp: Option<HoverStamp>) {
        self.point = Some(point);
        self.stamp = stamp;
    }

    /// Once per tick. `hovering`: whether a URL is underlined now;
    /// `stamp_at` describes the content under a point (`None`: no pane
    /// there), and is only called while hovering. Returns the point to
    /// compute the hover at again when the content under it changed since the
    /// hover was computed, `None` when nothing changed (the cheap case).
    pub fn recheck(
        &mut self,
        hovering: bool,
        stamp_at: impl FnOnce((f32, f32)) -> Option<HoverStamp>,
    ) -> Option<(f32, f32)> {
        if !hovering {
            self.stamp = None;
            return None;
        }
        let point = self.point?;
        let now = stamp_at(point);
        if now.is_some() && now == self.stamp {
            return None;
        }
        self.stamp = now;
        Some(point)
    }
}

/// Modifier keys held during a mouse or scroll event (the ones SGR reports carry).
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Mods {
    pub shift: bool,
    pub ctrl: bool,
    pub option: bool,
}

/// What the pointer did.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum MouseAction {
    Press,
    Drag,
    Release,
    /// Moved with no button held.
    Move,
}

/// A mouse event over a surface. `x`, `y` are pixels from the pane's text
/// origin: the pane's left edge plus its horizontal padding, and its top edge
/// (the same frame as [`TerminalState::grid_pos_at`](crate::terminal::TerminalState::grid_pos_at)).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MouseEvent {
    pub action: MouseAction,
    /// SGR base button code: 0, 1 or 2. Ignored by `Move`, which reports
    /// "no button".
    pub button: u8,
    pub mods: Mods,
    pub x: f32,
    pub y: f32,
    /// 1 for a click, 2 to select a word, 3 or more for the whole row.
    pub click_count: isize,
}

/// What [`Surface::mouse`] did with an event.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum MouseResult {
    /// The app in the pane asked for mouse reports: the event is its own (sent
    /// as an SGR report when it lands on the grid), the host does nothing else.
    Reported,
    /// Not reported. A press started a selection if it landed on the grid; a
    /// release or a move did nothing, the host may use it.
    Local,
    /// A drag without reporting: lines per host tick to auto-scroll the
    /// selection (0 = the pointer is on the grid, stop). See
    /// [`Surface::autoscroll_selection`].
    Autoscroll(i32),
}

/// A vertical scroll over a surface. `x`, `y` as in [`MouseEvent`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ScrollEvent {
    /// Delta, positive toward older output.
    pub dy: f64,
    /// Precise deltas (trackpad): accumulated into whole lines. A mouse wheel
    /// scrolls the truncated delta.
    pub precise: bool,
    pub mods: Mods,
    pub x: f32,
    pub y: f32,
}

impl Surface {
    /// A key press. Host shortcuts come back untouched; otherwise Ctrl+L wipes
    /// the scrollback (not in alt-screen), then the key is either written to
    /// the PTY or handed back for the platform's text input.
    pub fn key(&self, key: &KeyEvent, keybindings: &Keybindings) -> KeyResult {
        let route = |kitty_flags: u8| pane_key_action(
            key.combo.ctrl,
            key.combo.option,
            key.combo.cmd,
            key.unmod.as_deref(),
            kitty_flags,
        );
        let wipe_scrollback = match route(0) {
            PaneKeyAction::ToggleFilter => return KeyResult::Host(HostKey::ToggleFilter),
            PaneKeyAction::AdjustVirtualWidth(dir) => return KeyResult::Host(HostKey::AdjustVirtualWidth(dir)),
            PaneKeyAction::Send { wipe_scrollback, .. } => wipe_scrollback,
        };

        if wipe_scrollback {
            let mut term = self.terminal.write();
            if !term.in_alt_screen {
                term.clear_scrollback_and_screen();
            }
        }

        let (kitty_flags, cursor_keys_app) = {
            let term = self.terminal.read();
            (term.kitty_flags(), term.cursor_keys_application)
        };

        let direct = matches!(route(kitty_flags), PaneKeyAction::Send { direct: true, .. });
        if direct {
            // Kitty mode: bypass macOS text input for modified keys
            self.write_key_answering(key, keybindings, cursor_keys_app, kitty_flags);
            KeyResult::Sent
        } else {
            KeyResult::Compose
        }
    }

    /// A key the platform's text input did not turn into text (macOS
    /// `doCommandBySelector:`): encode it for the PTY.
    pub fn send_key(&self, key: &KeyEvent, keybindings: &Keybindings) {
        let (cursor_keys_app, kitty_flags) = {
            let term = self.terminal.read();
            (term.cursor_keys_application, term.kitty_flags())
        };
        self.write_key_answering(key, keybindings, cursor_keys_app, kitty_flags);
    }

    /// Back to the live screen, the pane is answered, then the key's bytes.
    fn write_key_answering(&self, key: &KeyEvent, keybindings: &Keybindings, cursor_keys_app: bool, kitty_flags: u8) {
        self.terminal.write().reset_scroll();
        // Typing into a pane answers it: it is no longer waiting on us.
        self.clear_awaiting();
        write_key(key, &self.pty, cursor_keys_app, keybindings, kitty_flags);
    }

    /// The pane's focus went from `was` to `now`: tell the program when it
    /// asked to hear about it (`focus_report`).
    pub fn report_focus(&self, was: bool, now: bool) {
        let reporting = self.terminal.read().focus_reporting;
        if let Some(seq) = focus_report(was, now, reporting) {
            self.pty.write(seq);
        }
    }

    /// Text committed by the platform's text input (macOS `insertText:`):
    /// typed characters, a composed accent, an IME result.
    pub fn text(&self, text: &str) {
        self.terminal.write().reset_scroll();
        // Typing into a pane answers it: it is no longer waiting on us.
        self.clear_awaiting();
        write_text(text, &self.pty);
    }

    /// A paste: wrapped in bracketed-paste markers when the app asked for them.
    /// Does not scroll back to the live screen nor answer the pane.
    pub fn paste(&self, text: &str) {
        let mut text = text.to_string();
        let bracketed = self.terminal.read().bracketed_paste;
        if bracketed {
            // A paste containing the bracketed-paste
            // terminator would break out of the paste
            // and inject keystrokes into the app.
            // Loop to a fixpoint: a single replace can
            // re-form the terminator from its halves.
            while text.contains("\x1b[201~") {
                text = text.replace("\x1b[201~", "");
            }
            self.pty.write(b"\x1b[200~");
        }
        self.pty.write(text.as_bytes());
        if bracketed { self.pty.write(b"\x1b[201~"); }
    }

    /// A mouse event at a point of this surface; `cell` is the cell size in
    /// pixels. Reported to the app when it asked (press and release from mode
    /// 1000, drag from 1002, bare moves from 1003, SGR encoding only);
    /// otherwise a press starts a selection and a drag extends it.
    pub fn mouse(&self, ev: &MouseEvent, cell: (f32, f32)) -> MouseResult {
        let (cell_w, cell_h) = cell;
        let min_mode = match ev.action {
            MouseAction::Press | MouseAction::Release => 1000,
            MouseAction::Drag => 1002,
            MouseAction::Move => 1003,
        };
        {
            let term = self.terminal.read();
            if term.mouse_mode >= min_mode && term.sgr_mouse {
                let at = term.sgr_cell_at(ev.x, ev.y, cell_w, cell_h);
                drop(term);
                if let Some((col, row)) = at {
                    let mods = mouse_modifiers(ev.mods.shift, ev.mods.option, ev.mods.ctrl);
                    let bytes = match ev.action {
                        MouseAction::Press => sgr_mouse_bytes(ev.button, mods, col, row, true, false),
                        MouseAction::Drag => sgr_mouse_bytes(ev.button, mods, col, row, true, true),
                        MouseAction::Release => sgr_mouse_bytes(ev.button, mods, col, row, false, false),
                        // No button pressed during move → button code 3 (no button) + motion flag
                        MouseAction::Move => sgr_mouse_bytes(3, mods, col, row, true, true),
                    };
                    self.pty.write(&bytes);
                }
                return MouseResult::Reported;
            }
        }
        match ev.action {
            MouseAction::Press => {
                let pos = self.terminal.read().grid_pos_at(ev.x, ev.y, cell_w, cell_h);
                if let Some(pos) = pos {
                    self.terminal.write().start_selection(pos, ev.click_count);
                }
                MouseResult::Local
            }
            MouseAction::Drag => {
                let pos = self.terminal.read().grid_pos_at(ev.x, ev.y, cell_w, cell_h);
                if let Some(pos) = pos {
                    // Pointer inside the grid — normal drag
                    self.terminal.write().extend_selection(pos);
                    MouseResult::Autoscroll(0)
                } else {
                    // Pointer outside the grid — auto-scroll speed
                    MouseResult::Autoscroll(self.terminal.read().drag_autoscroll_speed(ev.y, cell_h))
                }
            }
            MouseAction::Release | MouseAction::Move => MouseResult::Local,
        }
    }

    /// End of a selection gesture: a click that did not move clears its empty
    /// selection; otherwise the selected text, for the host to copy.
    pub fn finish_selection(&self) -> Option<String> {
        self.terminal.write().finish_selection()
    }

    /// One host tick of a selection dragged past the pane's edge: scroll by
    /// `speed` lines (negative = up, toward older output) and stretch the
    /// selection to the first or last visible row.
    pub fn autoscroll_selection(&self, speed: i32) {
        let mut term = self.terminal.write();
        if term.selection.is_some() {
            term.scroll(-speed);
            let sb_len = term.scrollback_len();
            let scroll_off = term.scroll_offset();
            if speed < 0 {
                let first_visible = (sb_len as i64 - scroll_off as i64) as usize;
                if let Some(ref mut sel) = term.selection {
                    sel.end = crate::terminal::GridPos { line: first_visible, col: 0 };
                }
            } else {
                let last_visible = (sb_len as i64 - scroll_off as i64 + term.rows as i64 - 1) as usize;
                let last_col = term.cols.saturating_sub(1);
                if let Some(ref mut sel) = term.selection {
                    sel.end = crate::terminal::GridPos { line: last_visible, col: last_col };
                }
            }
            term.dirty.store(true, std::sync::atomic::Ordering::Relaxed);
        }
    }

    /// A vertical scroll at a point of this surface. Whole lines go to the app
    /// as SGR wheel reports when it asked for mouse reports, otherwise they
    /// move the scrollback. `log_tab` is the host's index for this surface,
    /// only printed in the SCROLL-EVENT / SCROLL-BEGIN log lines.
    pub fn scroll(&self, ev: &ScrollEvent, cell: (f32, f32), sensitivity: f64, log_tab: usize) {
        let (lines, acc) = scroll_lines(self.scroll_accumulator.get(), ev.dy, ev.precise, sensitivity);
        self.scroll_accumulator.set(acc);
        if lines == 0 {
            return;
        }
        // Forward scroll to PTY if mouse reporting is active
        let mouse_mode = self.terminal.read().mouse_mode;
        let sgr = self.terminal.read().sgr_mouse;
        if mouse_mode >= 1000 && sgr {
            let at = self.terminal.read().sgr_cell_at(ev.x, ev.y, cell.0, cell.1);
            if let Some((col, row)) = at {
                // Each discrete line = one scroll event
                let (button, count) = wheel_reports(lines);
                let mods = mouse_modifiers(ev.mods.shift, ev.mods.option, ev.mods.ctrl);
                for _ in 0..count {
                    self.pty.write(&sgr_mouse_bytes(button, mods, col, row, true, false));
                }
            }
        } else {
            let mut term = self.terminal.write();
            log::debug!("SCROLL-EVENT tab={} pane={} term_id={} lines={} offset_before={}",
                log_tab, self.id, term.terminal_id, lines, term.scroll_offset());
            // One info line per scroll session (offset 0 → >0), to pair
            // tab/pane with the SCROLL-START line for the cross-tab
            // scrollback bug — without needing RUST_LOG=debug.
            if term.scroll_offset() == 0 && lines > 0 {
                log::info!("SCROLL-BEGIN tab={} pane={} term_id={}",
                    log_tab, self.id, term.terminal_id);
            }
            term.scroll(lines);
            // Reset accumulator when hitting bounds to avoid residual drift
            let at_bound = term.scroll_offset() == 0
                || term.scroll_offset() == term.scrollback_len() as i32;
            if at_bound {
                self.scroll_accumulator.set(0.0);
            }
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    //! Characterization of the key → bytes table as of 13/10/2026, before the
    //! embedding refactor (docs/embedding.md). Every case is played from a
    //! synthetic NSEvent through the real translation (`appkit_input::key_event`)
    //! and the real writers (`write_key`, `Surface::send_key`, `Surface::key`)
    //! into a capture PTY.
    use super::*;
    use crate::config::KeysConfig;
    use objc2_app_kit::{NSEvent, NSEventModifierFlags, NSEventType};
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
            write_key(&crate::appkit_input::key_event(&ev), &pty, k.app_cursor, &kb, k.kitty);
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

    // The typed Surface API: the same table and the same gestures, played
    // through `Surface::key/send_key/text/mouse/scroll` on a capture PTY.

    use crate::appkit_input;
    use crate::terminal::{GridPos, SelectionMode};

    /// A surface whose screen shows `lines` (cursor on the last one).
    fn surface(cols: u16, rows: u16, lines: &[&str]) -> (Surface, OwnedFd) {
        let (s, fd) = Surface::capture(cols, rows);
        {
            let mut t = s.terminal.write();
            for (i, l) in lines.iter().enumerate() {
                if i > 0 {
                    t.newline();
                    t.carriage_return();
                }
                for c in l.chars() {
                    t.put_char(c);
                }
            }
        }
        (s, fd)
    }

    fn set_key_modes(s: &Surface, app_cursor: bool, kitty: u8) {
        let mut t = s.terminal.write();
        t.cursor_keys_application = app_cursor;
        t.kitty_keyboard_flags = if kitty > 0 { vec![kitty] } else { Vec::new() };
    }

    pub(crate) fn typed(chars: &str, unmod: &str, keycode: u16, mods: &str) -> KeyEvent {
        appkit_input::key_event(&key_event(chars, unmod, keycode, mods))
    }

    #[test]
    fn surface_send_key_matches_the_frozen_table() {
        let kb = keybindings();
        let (s, reader) = Surface::capture(20, 4);
        let mut failures = Vec::new();
        for k in TABLE {
            set_key_modes(&s, k.app_cursor, k.kitty);
            s.send_key(&typed(k.chars, k.unmod, k.keycode, k.mods), &kb);
            let got = drain(&reader);
            if got != k.out {
                failures.push(format!("{}: expected {:?}, got {:?}",
                    k.name, String::from_utf8_lossy(k.out), String::from_utf8_lossy(&got)));
            }
        }
        assert!(failures.is_empty(), "{} case(s) moved:\n{}", failures.len(), failures.join("\n"));
    }

    #[test]
    fn surface_key_sends_direct_keys_as_the_frozen_table_and_composes_the_rest() {
        let kb = keybindings();
        let (s, reader) = Surface::capture(20, 4);
        let mut failures = Vec::new();
        for k in TABLE {
            set_key_modes(&s, k.app_cursor, k.kitty);
            let ev = key_event(k.chars, k.unmod, k.keycode, k.mods);
            let m = ev.modifierFlags();
            let route = pane_key_action(
                m.contains(NSEventModifierFlags::Control),
                m.contains(NSEventModifierFlags::Option),
                m.contains(NSEventModifierFlags::Command),
                Some(k.unmod),
                k.kitty,
            );
            let (want, want_bytes): (KeyResult, &[u8]) = match route {
                PaneKeyAction::ToggleFilter => (KeyResult::Host(HostKey::ToggleFilter), b""),
                PaneKeyAction::AdjustVirtualWidth(d) => (KeyResult::Host(HostKey::AdjustVirtualWidth(d)), b""),
                PaneKeyAction::Send { direct: true, .. } => (KeyResult::Sent, k.out),
                PaneKeyAction::Send { direct: false, .. } => (KeyResult::Compose, b""),
            };
            let got = s.key(&appkit_input::key_event(&ev), &kb);
            let bytes = drain(&reader);
            if got != want || bytes != want_bytes {
                failures.push(format!("{}: expected {:?} {:?}, got {:?} {:?}", k.name,
                    want, String::from_utf8_lossy(want_bytes), got, String::from_utf8_lossy(&bytes)));
            }
        }
        assert!(failures.is_empty(), "{} case(s) moved:\n{}", failures.len(), failures.join("\n"));
        // The table has both kinds, so the loop above tested both paths.
        assert!(TABLE.iter().any(|k| k.kitty > 0 && k.mods.contains("ctrl")));
    }

    const K_F: u16 = 0x03;

    #[test]
    fn host_keys_never_reach_the_pane() {
        let kb = keybindings();
        let (s, reader) = surface(10, 2, &["one", "two", "three"]);
        set_key_modes(&s, false, 1);
        let ctrl_f = typed("\u{6}", "f", K_F, "ctrl");
        assert_eq!(host_key(&ctrl_f), Some(HostKey::ToggleFilter));
        assert_eq!(s.key(&ctrl_f, &kb), KeyResult::Host(HostKey::ToggleFilter));
        let widen = typed("\u{F703}", "\u{F703}", K_RIGHT, "ctrl alt");
        assert_eq!(host_key(&widen), Some(HostKey::AdjustVirtualWidth(1.0)));
        assert_eq!(s.key(&widen, &kb), KeyResult::Host(HostKey::AdjustVirtualWidth(1.0)));
        assert_eq!(drain(&reader), b"");
        assert_eq!(s.terminal.read().scrollback_len(), 1, "nothing wiped");
        assert_eq!(host_key(&typed("a", "a", K_A, "ctrl")), None);
    }

    #[test]
    fn ctrl_l_wipes_the_scrollback_then_still_sends_the_key() {
        let kb = keybindings();
        let ctrl_l = typed("\u{c}", "l", K_L, "ctrl");

        let (s, reader) = surface(10, 2, &["one", "two", "three"]);
        assert_eq!(s.key(&ctrl_l, &kb), KeyResult::Compose, "without kitty: through text input");
        assert_eq!(s.terminal.read().scrollback_len(), 0);
        assert_eq!(drain(&reader), b"", "the key itself comes back through send_key");

        let (s, reader) = surface(10, 2, &["one", "two", "three"]);
        set_key_modes(&s, false, 1);
        assert_eq!(s.key(&ctrl_l, &kb), KeyResult::Sent);
        assert_eq!(s.terminal.read().scrollback_len(), 0);
        assert_eq!(drain(&reader), b"\x1b[108;5u");

        let (s, _reader) = surface(10, 2, &["one", "two", "three"]);
        s.terminal.write().in_alt_screen = true;
        assert_eq!(s.key(&ctrl_l, &kb), KeyResult::Compose);
        assert_eq!(s.terminal.read().scrollback_len(), 1, "alt-screen keeps the scrollback");
    }

    #[test]
    fn a_sent_key_and_committed_text_answer_the_pane_and_return_to_the_live_screen() {
        let kb = keybindings();
        let (s, reader) = surface(10, 2, &["one", "two", "three"]);
        set_key_modes(&s, false, 1);

        s.terminal.write().scroll(1);
        s.set_awaiting();
        assert_eq!(s.key(&typed("\u{1}", "a", K_A, "ctrl"), &kb), KeyResult::Sent);
        assert_eq!(s.terminal.read().scroll_offset(), 0);
        assert!(!s.is_awaiting());
        assert_eq!(drain(&reader), b"\x1b[97;5u");

        s.terminal.write().scroll(1);
        s.set_awaiting();
        s.send_key(&typed("\u{F700}", "\u{F700}", K_UP, ""), &kb);
        assert_eq!(s.terminal.read().scroll_offset(), 0);
        assert!(!s.is_awaiting());
        assert_eq!(drain(&reader), b"\x1b[A");

        s.terminal.write().scroll(1);
        s.set_awaiting();
        s.text("ê");
        assert_eq!(s.terminal.read().scroll_offset(), 0);
        assert!(!s.is_awaiting());
        assert_eq!(drain(&reader), "ê".as_bytes());
        s.text("");
        assert_eq!(drain(&reader), b"");
    }

    #[test]
    fn a_key_handed_to_text_input_leaves_the_pane_as_it_was() {
        let kb = keybindings();
        let (s, reader) = surface(10, 2, &["one", "two", "three"]);
        s.terminal.write().scroll(1);
        s.set_awaiting();
        assert_eq!(s.key(&typed("a", "a", K_A, ""), &kb), KeyResult::Compose);
        assert_eq!(s.terminal.read().scroll_offset(), 1);
        assert!(s.is_awaiting());
        assert_eq!(drain(&reader), b"");
    }

    #[test]
    fn paste_brackets_and_strips_the_terminator_without_answering_the_pane() {
        let (s, reader) = Surface::capture(10, 2);
        s.set_awaiting();
        s.paste("ls\n");
        assert_eq!(drain(&reader), b"ls\n");
        s.terminal.write().bracketed_paste = true;
        s.paste("a\x1b[20\x1b[201~1~b");
        assert_eq!(drain(&reader), b"\x1b[200~ab\x1b[201~", "the terminator re-formed by one pass is stripped too");
        assert!(s.is_awaiting(), "a paste is not an answer");
    }

    // Mouse: cell 10×20 px, a full 2-row screen (no bottom gravity).

    const CELL: (f32, f32) = (10.0, 20.0);

    fn mouse(action: MouseAction, col: f32, row: f32, click_count: isize) -> MouseEvent {
        MouseEvent { action, button: 0, mods: Mods::default(), x: col * 10.0 + 5.0, y: row * 20.0 + 5.0, click_count }
    }

    fn selection(s: &Surface) -> Option<(GridPos, GridPos, SelectionMode)> {
        s.terminal.read().selection.as_ref().map(|sel| (sel.anchor, sel.end, sel.mode))
    }

    fn gp(line: usize, col: u16) -> GridPos { GridPos { line, col } }

    #[test]
    fn press_drag_release_selects_and_returns_the_text() {
        let (s, reader) = surface(20, 2, &["hello world", "second"]);
        assert_eq!(s.mouse(&mouse(MouseAction::Press, 6.0, 0.0, 1), CELL), MouseResult::Local);
        assert_eq!(selection(&s), Some((gp(0, 6), gp(0, 6), SelectionMode::Normal)));
        assert_eq!(s.mouse(&mouse(MouseAction::Drag, 2.0, 1.0, 0), CELL), MouseResult::Autoscroll(0));
        assert_eq!(selection(&s), Some((gp(0, 6), gp(1, 2), SelectionMode::Normal)));
        assert_eq!(s.mouse(&mouse(MouseAction::Release, 2.0, 1.0, 0), CELL), MouseResult::Local);
        assert_eq!(s.finish_selection().as_deref(), Some("world\nsec"));
        assert_eq!(drain(&reader), b"", "no report without mouse mode");
    }

    #[test]
    fn a_plain_click_selects_nothing_and_a_double_click_the_word() {
        let (s, _reader) = surface(20, 2, &["hello world", "second"]);
        s.mouse(&mouse(MouseAction::Press, 3.0, 0.0, 1), CELL);
        assert_eq!(s.finish_selection(), None);
        assert_eq!(selection(&s), None);
        s.mouse(&mouse(MouseAction::Press, 8.0, 0.0, 2), CELL);
        assert_eq!(s.finish_selection().as_deref(), Some("world"));
        s.mouse(&mouse(MouseAction::Press, 1.0, 1.0, 3), CELL);
        assert_eq!(selection(&s).map(|x| x.2), Some(SelectionMode::Line));
    }

    #[test]
    fn a_press_off_the_grid_starts_nothing() {
        let (s, _reader) = surface(20, 2, &["hello world", "second"]);
        let off = MouseEvent { x: -1.0, ..mouse(MouseAction::Press, 0.0, 0.0, 1) };
        assert_eq!(s.mouse(&off, CELL), MouseResult::Local);
        assert_eq!(selection(&s), None);
    }

    #[test]
    fn dragging_past_the_pane_asks_for_autoscroll_by_distance() {
        let (s, _reader) = surface(20, 2, &["hello world", "second"]);
        s.mouse(&mouse(MouseAction::Press, 0.0, 0.0, 1), CELL);
        let at_y = |y: f32| MouseEvent { y, ..mouse(MouseAction::Drag, 1.0, 0.0, 0) };
        assert_eq!(s.mouse(&at_y(81.0), CELL), MouseResult::Autoscroll(3), "41 px below the 40 px grid");
        assert_eq!(s.mouse(&at_y(-1.0), CELL), MouseResult::Autoscroll(-1));
        assert_eq!(s.mouse(&at_y(5000.0), CELL), MouseResult::Autoscroll(10));
        assert_eq!(s.mouse(&at_y(10.0), CELL), MouseResult::Autoscroll(0));
    }

    #[test]
    fn autoscroll_stretches_the_selection_to_the_visible_edge() {
        let (s, _reader) = surface(10, 2, &["one", "two", "three", "four"]);
        assert_eq!(s.terminal.read().scrollback_len(), 2);
        s.autoscroll_selection(-1);
        assert_eq!(s.terminal.read().scroll_offset(), 0, "no selection, no scroll");

        s.mouse(&mouse(MouseAction::Press, 1.0, 0.0, 1), CELL);
        s.autoscroll_selection(-1);
        assert_eq!(s.terminal.read().scroll_offset(), 1);
        assert_eq!(selection(&s).map(|x| x.1), Some(gp(1, 0)), "first visible line");
        s.autoscroll_selection(1);
        assert_eq!(s.terminal.read().scroll_offset(), 0);
        assert_eq!(selection(&s).map(|x| x.1), Some(gp(3, 9)), "last visible line, last column");
    }

    #[test]
    fn mouse_reporting_sends_sgr_and_skips_the_selection() {
        let (s, reader) = surface(20, 2, &["hello world", "second"]);
        {
            let mut t = s.terminal.write();
            t.mouse_mode = 1000;
            t.sgr_mouse = true;
        }
        assert_eq!(s.mouse(&mouse(MouseAction::Press, 2.0, 1.0, 1), CELL), MouseResult::Reported);
        assert_eq!(drain(&reader), b"\x1b[<0;3;2M");
        assert_eq!(selection(&s), None);
        assert_eq!(s.mouse(&mouse(MouseAction::Release, 2.0, 1.0, 0), CELL), MouseResult::Reported);
        assert_eq!(drain(&reader), b"\x1b[<0;3;2m");
        // Mode 1000 does not report drags or bare moves.
        assert_eq!(s.mouse(&mouse(MouseAction::Drag, 4.0, 1.0, 0), CELL), MouseResult::Autoscroll(0));
        assert_eq!(s.mouse(&mouse(MouseAction::Move, 4.0, 1.0, 0), CELL), MouseResult::Local);
        assert_eq!(drain(&reader), b"");

        s.terminal.write().mouse_mode = 1002;
        let right = MouseEvent { button: 2, ..mouse(MouseAction::Drag, 4.0, 1.0, 0) };
        assert_eq!(s.mouse(&right, CELL), MouseResult::Reported);
        assert_eq!(drain(&reader), b"\x1b[<34;5;2M");
        assert_eq!(s.mouse(&mouse(MouseAction::Move, 4.0, 1.0, 0), CELL), MouseResult::Local);

        s.terminal.write().mouse_mode = 1003;
        let moved = MouseEvent { button: 2, ..mouse(MouseAction::Move, 4.0, 0.0, 0) };
        assert_eq!(s.mouse(&moved, CELL), MouseResult::Reported);
        assert_eq!(drain(&reader), b"\x1b[<35;5;1M", "a move reports no button");

        let mods = Mods { shift: true, ctrl: true, option: false };
        let pressed = MouseEvent { mods, ..mouse(MouseAction::Press, 0.0, 0.0, 1) };
        s.mouse(&pressed, CELL);
        assert_eq!(drain(&reader), b"\x1b[<20;1;1M");

        let off = MouseEvent { x: -1.0, ..mouse(MouseAction::Press, 0.0, 0.0, 1) };
        assert_eq!(s.mouse(&off, CELL), MouseResult::Reported, "still the app's, even off the grid");
        assert_eq!(drain(&reader), b"");
    }

    #[test]
    fn mouse_mode_without_sgr_is_not_reported() {
        let (s, reader) = surface(20, 2, &["hello world", "second"]);
        s.terminal.write().mouse_mode = 1003;
        assert_eq!(s.mouse(&mouse(MouseAction::Press, 2.0, 0.0, 1), CELL), MouseResult::Local);
        assert!(selection(&s).is_some());
        assert_eq!(drain(&reader), b"");
    }

    // Scroll

    fn wheel(dy: f64, precise: bool) -> ScrollEvent {
        ScrollEvent { dy, precise, mods: Mods::default(), x: 5.0, y: 25.0 }
    }

    #[test]
    fn scroll_moves_the_scrollback_by_whole_lines() {
        let (s, reader) = surface(10, 2, &["1", "2", "3", "4", "5", "6"]);
        assert_eq!(s.terminal.read().scrollback_len(), 4);
        s.scroll(&wheel(4.0, true), CELL, 6.0, 0);
        assert_eq!(s.terminal.read().scroll_offset(), 0, "not a whole line yet");
        s.scroll(&wheel(4.0, true), CELL, 6.0, 0);
        assert_eq!(s.terminal.read().scroll_offset(), 1);
        assert!((s.scroll_accumulator.get() - 2.0 / 6.0).abs() < 1e-12, "remainder kept");
        s.scroll(&wheel(3.9, false), CELL, 6.0, 0);
        assert_eq!(s.terminal.read().scroll_offset(), 4);
        assert_eq!(s.scroll_accumulator.get(), 0.0, "reset at the top bound");
        s.scroll(&wheel(-1.5, false), CELL, 6.0, 0);
        assert_eq!(s.terminal.read().scroll_offset(), 3);
        assert_eq!(drain(&reader), b"");
    }

    #[test]
    fn scroll_with_mouse_reporting_sends_one_wheel_report_per_line() {
        let (s, reader) = surface(10, 2, &["1", "2", "3", "4", "5", "6"]);
        {
            let mut t = s.terminal.write();
            t.mouse_mode = 1000;
            t.sgr_mouse = true;
        }
        s.scroll(&wheel(-2.0, false), CELL, 6.0, 0);
        assert_eq!(drain(&reader), b"\x1b[<65;1;2M\x1b[<65;1;2M");
        let ctrl_up = ScrollEvent { mods: Mods { ctrl: true, ..Mods::default() }, ..wheel(1.0, false) };
        s.scroll(&ctrl_up, CELL, 6.0, 0);
        assert_eq!(drain(&reader), b"\x1b[<80;1;2M");
        assert_eq!(s.terminal.read().scroll_offset(), 0, "the scrollback does not move");
        s.scroll(&wheel(4.0, true), CELL, 6.0, 0);
        assert_eq!(drain(&reader), b"", "less than a line: nothing yet");
    }

    #[test]
    fn focus_is_reported_only_on_a_real_change_with_mode_1004() {
        assert_eq!(focus_report(false, true, true), Some(&b"\x1b[I"[..]));
        assert_eq!(focus_report(true, false, true), Some(&b"\x1b[O"[..]));
        assert_eq!(focus_report(true, true, true), None, "no change, nothing sent");
        assert_eq!(focus_report(false, false, true), None);
        assert_eq!(focus_report(false, true, false), None, "mode 1004 off");
        assert_eq!(focus_report(true, false, false), None);
    }

    #[test]
    fn a_surface_reports_focus_once_the_program_turned_mode_1004_on() {
        let (s, reader) = Surface::capture(80, 24);
        s.report_focus(false, true);
        assert_eq!(drain(&reader), b"", "mode off: nothing");
        s.terminal.write().focus_reporting = true;
        s.report_focus(false, true);
        assert_eq!(drain(&reader), b"\x1b[I");
        s.report_focus(true, true);
        assert_eq!(drain(&reader), b"", "no change: nothing");
        s.report_focus(true, false);
        assert_eq!(drain(&reader), b"\x1b[O");
    }

    // --- Cmd+hover / Cmd+click on a URL: the hit at a point, shared by
    // Kova.app's window and the library pane ---

    #[test]
    fn visible_row_col_maps_points_to_cells() {
        let cell = (10.0, 20.0);
        assert_eq!(visible_row_col(0.0, 0.0, cell, 0, 3, 8), Some((0, 0)));
        assert_eq!(visible_row_col(25.0, 45.0, cell, 0, 3, 8), Some((2, 2)));
        assert_eq!(visible_row_col(9.9, 19.9, cell, 0, 3, 8), Some((0, 0)), "a cell's own pixels");
        assert_eq!(visible_row_col(500.0, 0.0, cell, 0, 3, 8), Some((0, 7)), "past the right edge: last column");
        assert_eq!(visible_row_col(-1.0, 0.0, cell, 0, 3, 8), None, "in the left padding");
        assert_eq!(visible_row_col(0.0, -1.0, cell, 0, 3, 8), None, "above the grid");
        assert_eq!(visible_row_col(0.0, 60.0, cell, 0, 3, 8), None, "below the grid");
        // Bottom gravity: two blank rows drawn on top shift every row down.
        assert_eq!(visible_row_col(0.0, 45.0, cell, 2, 3, 8), Some((0, 0)));
        assert_eq!(visible_row_col(0.0, 25.0, cell, 2, 3, 8), None, "in the gravity band");
    }

    #[test]
    fn url_at_point_finds_the_url_under_the_pointer() {
        let (s, _fd) = surface(30, 3, &["see https://a.example/x now", "plain"]);
        let t = s.terminal.read();
        let y_off = t.y_offset_rows() as f32;
        let cell = (10.0, 20.0);
        let row0 = y_off * 20.0 + 5.0;
        let (segments, url) = url_at_point(&t, 4.0 * 10.0 + 1.0, row0, cell).expect("on the URL");
        assert_eq!(url, "https://a.example/x");
        assert_eq!(segments, vec![(0, 4, 23)]);
        assert_eq!(url_at_point(&t, 25.0 * 10.0, row0, cell), None, "after it");
        assert_eq!(url_at_point(&t, 1.0, row0 + 20.0, cell), None, "the next line");
    }

    #[test]
    fn url_hover_follows_cmd_and_the_pointer() {
        let hit = || Some((vec![(0, 4, 23)], "https://a.example/x".to_string()));
        let segments = [(0usize, 4u16, 23u16)];
        let current = Some((&segments[..], "https://a.example/x"));
        assert_eq!(url_hover_update(true, hit, None), HoverUpdate::Set(vec![(0, 4, 23)], "https://a.example/x".into()));
        assert_eq!(url_hover_update(true, hit, current), HoverUpdate::Keep, "same URL: no redraw");
        assert_eq!(url_hover_update(true, || None, current), HoverUpdate::Clear, "moved off it");
        assert_eq!(url_hover_update(true, || None, None), HoverUpdate::Keep);
        let never = || -> Option<(Vec<(usize, u16, u16)>, String)> { panic!("no hit test without Cmd") };
        assert_eq!(url_hover_update(false, never, current), HoverUpdate::Clear, "Cmd released");
        assert_eq!(url_hover_update(false, never, None), HoverUpdate::Keep);
        let other = [(1usize, 0u16, 5u16)];
        assert!(matches!(url_hover_update(true, hit, Some((&other[..], "https://a.example/x"))), HoverUpdate::Set(..)), "another URL");
        // Same cells, other text: output slid another URL of the same length
        // under the pointer. Keeping it would open the old URL on Cmd+click.
        assert_eq!(
            url_hover_update(true, hit, Some((&segments[..], "https://b.example/y"))),
            HoverUpdate::Set(vec![(0, 4, 23)], "https://a.example/x".into()),
        );
    }

    // --- The hover under a still pointer, re-checked when the text moves ---

    const LAYOUT: [f32; 4] = [0.0, 0.0, 300.0, 60.0];

    /// One tick of the hover refresh, as Kova.app's and the library pane's
    /// ticks run it: recheck, then (if asked) the same update a mouse move
    /// does, with Cmd held. Returns the update, `None` when not re-run.
    fn tick_hover(
        tracker: &mut HoverTracker,
        hover: &mut Option<(Vec<(usize, u16, u16)>, String)>,
        t: &crate::terminal::TerminalState,
        cell: (f32, f32),
    ) -> Option<HoverUpdate> {
        let (px, py) = tracker.recheck(hover.is_some(), |_| Some(HoverStamp::of(t, LAYOUT)))?;
        let current = hover.clone();
        let update = url_hover_update(true, || url_at_point(t, px, py, cell), current.as_ref().map(|(s, u)| (s.as_slice(), u.as_str())));
        match &update {
            HoverUpdate::Set(segs, url) => *hover = Some((segs.clone(), url.clone())),
            HoverUpdate::Clear => *hover = None,
            HoverUpdate::Keep => {}
        }
        Some(update)
    }

    /// A hover computed at a mouse move, with its stamp, as the views do.
    fn hover_at(
        tracker: &mut HoverTracker,
        t: &crate::terminal::TerminalState,
        point: (f32, f32),
        cell: (f32, f32),
    ) -> Option<(Vec<(usize, u16, u16)>, String)> {
        let hover = url_at_point(t, point.0, point.1, cell);
        tracker.seen(point, hover.as_ref().map(|_| HoverStamp::of(t, LAYOUT)));
        hover
    }

    fn push_line(t: &mut crate::terminal::TerminalState, line: &str) {
        t.newline();
        t.carriage_return();
        for c in line.chars() {
            t.put_char(c);
        }
        // What every batch of PTY output does (`VteHandler::apply_ops`).
        t.bump_content_generation();
    }

    #[test]
    fn hover_is_not_recomputed_while_nothing_moves() {
        let (s, _fd) = surface(30, 3, &["see https://a.example/x now", "plain", "end"]);
        let t = s.terminal.read();
        let cell = (10.0, 20.0);
        let point = (41.0, 5.0);
        let mut tracker = HoverTracker::default();
        let mut hover = hover_at(&mut tracker, &t, point, cell);
        assert!(hover.is_some());
        for _ in 0..3 {
            assert_eq!(tick_hover(&mut tracker, &mut hover, &t, cell), None, "same generation, same scroll: no hit test");
        }
        let mut called = false;
        assert_eq!(tracker.recheck(false, |_| { called = true; None }), None, "nothing hovered: nothing to check");
        assert!(!called, "no stamp built while nothing is hovered");
    }

    #[test]
    fn hover_follows_output_that_slides_text_under_the_pointer() {
        let (s, _fd) = surface(40, 3, &["see https://a.example/x now", "plain", "end"]);
        let cell = (10.0, 20.0);
        let point = (41.0, 5.0); // row 0, column 4: on the URL
        let mut tracker = HoverTracker::default();
        let mut hover = hover_at(&mut tracker, &s.terminal.read(), point, cell);
        assert_eq!(hover.as_ref().map(|h| h.1.as_str()), Some("https://a.example/x"));

        // One line of output: "plain" is now on row 0, under the pointer.
        push_line(&mut s.terminal.write(), "more");
        let t = s.terminal.read();
        assert_eq!(tick_hover(&mut tracker, &mut hover, &t, cell), Some(HoverUpdate::Clear));
        assert_eq!(hover, None, "the underline does not stay on the text that took its place");
        assert_eq!(tick_hover(&mut tracker, &mut hover, &t, cell), None, "cleared: nothing left to check");
    }

    #[test]
    fn hover_moves_to_the_url_that_slid_under_the_pointer() {
        let (s, _fd) = surface(40, 3, &["see https://a.example/x now", "see https://b.example/y", "end"]);
        let cell = (10.0, 20.0);
        let point = (41.0, 5.0);
        let mut tracker = HoverTracker::default();
        let mut hover = hover_at(&mut tracker, &s.terminal.read(), point, cell);
        assert_eq!(hover.as_ref().map(|h| h.1.as_str()), Some("https://a.example/x"));

        push_line(&mut s.terminal.write(), "more");
        let t = s.terminal.read();
        match tick_hover(&mut tracker, &mut hover, &t, cell) {
            Some(HoverUpdate::Set(_, url)) => assert_eq!(url, "https://b.example/y"),
            other => panic!("expected the new URL, got {other:?}"),
        }
        assert_eq!(tick_hover(&mut tracker, &mut hover, &t, cell), None, "stamp taken: no second recompute");
    }

    #[test]
    fn hover_follows_a_scroll_of_the_scrollback() {
        let (s, _fd) = surface(40, 3, &["old line", "see https://a.example/x now", "b", "c"]);
        let cell = (10.0, 20.0);
        let point = (41.0, 5.0); // row 0 = "see https://…" once "old line" is in the scrollback
        let mut tracker = HoverTracker::default();
        let mut hover = hover_at(&mut tracker, &s.terminal.read(), point, cell);
        assert!(hover.is_some());

        // The user scrolls one line up: "old line" is under the pointer, and
        // no output arrived (the generation did not move).
        let g = s.terminal.read().content_generation();
        s.terminal.write().scroll(1);
        let t = s.terminal.read();
        assert_eq!(t.content_generation(), g);
        assert_eq!(tick_hover(&mut tracker, &mut hover, &t, cell), Some(HoverUpdate::Clear));
    }

    #[test]
    fn hover_rechecks_when_the_pane_is_gone_or_laid_out_elsewhere() {
        let (s, _fd) = surface(40, 3, &["see https://a.example/x now"]);
        let t = s.terminal.read();
        let stamp = HoverStamp::of(&t, LAYOUT);
        let mut tracker = HoverTracker::default();
        tracker.seen((1.0, 2.0), Some(stamp));
        assert_eq!(tracker.recheck(true, |_| Some(stamp)), None);
        let moved = HoverStamp { layout: [10.0, 0.0, 300.0, 60.0], ..stamp };
        assert_eq!(tracker.recheck(true, |_| Some(moved)), Some((1.0, 2.0)), "resized or moved pane");
        assert_eq!(tracker.recheck(true, |_| None), Some((1.0, 2.0)), "no pane under the pointer any more");
        assert_eq!(tracker.recheck(true, |_| None), Some((1.0, 2.0)), "still none: the hover check clears it");
        let fresh = HoverTracker::default().recheck(true, |_| Some(stamp));
        assert_eq!(fresh, None, "no pointer position known: nothing to recompute from");
    }
}
