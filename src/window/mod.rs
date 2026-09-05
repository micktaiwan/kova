mod attention;
use attention::{do_history_step, pane_history_state};
mod geometry;
mod ipc_handlers;
mod overlays;
use overlays::{FilterState, RenamePaneState, RenameTabState};
pub use ipc_handlers::{IpcCloseTabResult, IpcMergeTabResult, IpcSwapPaneResult};
mod recent_projects_overlay;
use recent_projects_overlay::RecentProjectsState;
mod resize;
mod search_palette;
use search_palette::{is_typed_char, SearchPaletteState, SearchRow};
mod switcher;
mod tabs;
mod tick;
use tabs::{MergeTabState, SendToWindowState};
use switcher::{PaneSwitcherState, SwitcherRow};

use objc2::rc::Retained;
use objc2::{define_class, msg_send, DefinedClass, MainThreadMarker, MainThreadOnly, Message};
use objc2_app_kit::{NSAlert, NSAlertStyle, NSApplication, NSBackingStoreType, NSCursor, NSEvent, NSEventModifierFlags, NSEventPhase, NSPasteboard, NSTextInputClient, NSTrackingArea, NSTrackingAreaOptions, NSWindow, NSWindowButton, NSWindowDelegate, NSWindowStyleMask, NSWindowTitleVisibility};
use objc2_core_foundation::{CGPoint, CGRect, CGSize};
use objc2_foundation::{NSArray, NSObjectProtocol, NSString};
use objc2_metal::MTLCreateSystemDefaultDevice;
use objc2_quartz_core::CAMetalLayer;
use std::cell::{Cell, OnceCell, RefCell};
use std::sync::Arc;

use crate::config::{Config, TerminalConfig};
use crate::input;
use crate::keybindings::{Action, Keybindings, KeyCombo};
use crate::pane::{alloc_tab_id, NavDirection, Pane, PaneId, SplitDirection, Tab, TabId};
use crate::renderer::{FilterRenderData, PaneViewport, Renderer};
use crate::terminal::pty::ProcessInfo;
use crate::terminal::{FilterMatch, GridPos, Selection, SelectionMode};

#[derive(Clone, Copy)]
struct SeparatorDrag {
    origin_pixel: f32,
    parent_dim: f32,
    column_sep_index: Option<usize>,
    col_index: usize,
    row_sep_index: Option<usize>,
}

#[derive(Clone, Copy)]
struct DragTabState {
    tab_index: usize,
    start_x: f32,
    current_x: f32,
    dragging: bool,
}

#[derive(Clone, Copy, PartialEq)]
enum ScrollAxisLock {
    None,
    Vertical,
    Horizontal,
}

pub struct KovaViewIvars {
    renderer: OnceCell<Arc<parking_lot::RwLock<Renderer>>>,
    tabs: RefCell<Vec<Tab>>,
    active_tab: Cell<usize>,
    metal_layer: OnceCell<Retained<CAMetalLayer>>,
    last_scale: Cell<f64>,
    /// Width, in logical points, of the screen this window was on at the last
    /// `handle_resize`. Kept so a display switch can tell how wide the *previous*
    /// screen was and pin the pane widths that were derived from it — the scale
    /// alone cannot say that, and it may change on a later event than the move
    /// (see `handle_resize`).
    last_screen_w: Cell<f32>,
    last_focused: Cell<bool>,
    config: OnceCell<Config>,
    keybindings: OnceCell<Keybindings>,
    drag_separator: Cell<Option<SeparatorDrag>>,
    filter: RefCell<Option<FilterState>>,
    rename_tab: RefCell<Option<RenameTabState>>,
    rename_pane: RefCell<Option<RenamePaneState>>,
    /// Left inset (pixels) for tab bar, cached from traffic light button positions.
    tab_bar_left_inset: Cell<f32>,
    /// Tab index targeted by right-click color menu.
    color_menu_tab: Cell<usize>,
    drag_tab: Cell<Option<DragTabState>>,
    /// URL currently hovered (pane_id, per-row segments [(row, col_start, col_end)], url) — set by mouseMoved when Cmd held
    hovered_url: RefCell<Option<(PaneId, Vec<(usize, u16, u16)>, String)>>,
    /// Whether Cmd key is currently held (for URL hover detection)
    cmd_held: Cell<bool>,
    /// Auto-scroll speed during drag selection (lines/tick, positive = down, negative = up, 0 = inactive)
    auto_scroll_speed: Cell<i32>,
    /// Marked text from IME composition (dead keys, etc.)
    marked_text: RefCell<Option<String>>,
    /// Current NSEvent being processed by interpretKeyEvents, so doCommandBySelector can access it.
    /// SAFETY: pointer is only live during the synchronous keyDown → interpretKeyEvents → doCommandBySelector
    /// call chain, and cleared immediately after. Never accessed outside that stack frame.
    current_event: Cell<Option<*const NSEvent>>,
    /// Window is closing — tick() should return false immediately.
    closing: Cell<bool>,
    /// Skip session save for this window (Cmd+Shift+Q kill).
    skip_session_save: Cell<bool>,
    /// Cached last window title (for OSC 0/2 dedup).
    last_title: RefCell<Option<String>>,
    /// Git branch poll counter (ticks since last poll).
    git_poll_counter: Cell<u32>,
    /// Foreground-process poll counter for the tab running indicator
    /// (tcgetpgrp per pane — probed every ~0.5s, not every tick).
    fg_poll_counter: Cell<u32>,
    /// Git branch poll interval in ticks (fps * 2 ≈ every 2 seconds).
    git_poll_interval: Cell<u32>,
    /// Whether the help overlay is visible.
    show_help: Cell<bool>,
    /// Whether the memory report overlay is visible.
    show_mem_report: Cell<bool>,
    /// Recent projects overlay state.
    recent_projects: RefCell<Option<RecentProjectsState>>,
    /// Countdown frames for "⌘? for help" hint in global status bar (fps * 3).
    help_hint_frames: Cell<u32>,
    /// Axis lock for trackpad scroll gestures (prevents cross-axis drift).
    scroll_axis_lock: Cell<ScrollAxisLock>,
    /// "Send Tab to Window" overlay state.
    send_to_window: RefCell<Option<SendToWindowState>>,
    /// "Merge Tab" overlay state.
    merge_tab: RefCell<Option<MergeTabState>>,
    /// Resize feedback: (mode_name, screen_w, virtual_w, remaining_frames).
    resize_feedback: Cell<Option<ResizeFeedback>>,
    /// Transient status-bar message (text, remaining frames) — used for one-off
    /// hints like "no-op" feedback when an action can't apply in the current layout.
    transient_status: RefCell<Option<(String, u32)>>,
    /// Banner painted across the focused pane's status bar (text, colour,
    /// remaining frames): says which attention tier the last Cmd+J landed in.
    attention_banner: RefCell<Option<(String, [f32; 3], u32)>>,
    /// Deferred tabs to restore progressively (tab_index, saved_tab_data).
    /// Deferred tabs keyed by their placeholder's TabId (not by index: the
    /// window is interactive during progressive restore, so indices shift
    /// when the user creates/closes/reorders tabs before an entry fires).
    deferred_tabs: RefCell<Vec<(TabId, crate::session::SavedTab)>>,
    /// Fixed total pane count for the loading counter (computed once at startup).
    loading_total_panes: Cell<u32>,
    /// Tab boundary guard: last time navigation hit a tab edge, and which direction.
    boundary_hit: Cell<Option<BoundaryHit>>,
    /// Boundary flash: remaining frames and which edge of the focused pane to flash.
    boundary_flash: Cell<Option<BoundaryFlash>>,
    /// Search palette overlay state (Cmd+Shift+F — global search across all panes).
    search_palette: RefCell<Option<SearchPaletteState>>,
    /// Tab/pane switcher overlay state (Cmd+P — list tabs & panes, click to focus).
    pane_switcher: RefCell<Option<PaneSwitcherState>>,
    /// Highlight a pane after a search-palette jump (decremented per tick).
    pane_flash: RefCell<Option<PaneFlash>>,
    /// Deferred PTY winsize restore after a Cmd+R nudge (decremented per tick).
    pty_restore: RefCell<Vec<PtyRestore>>,
    /// Recent pane sizes (bounded history per pane) for round-trip
    /// detection: a rapid return to ANY recently-seen size can coalesce the
    /// SIGWINCHs — the child reads an unchanged winsize and skips its
    /// repaint while our grid went through a lossy reflow round-trip.
    recent_resizes: RefCell<std::collections::HashMap<PaneId, Vec<((u16, u16), std::time::Instant)>>>,
    /// Debounce countdown (frames) per pane until a post-resize robust repaint
    /// fires. Reset on every resize; when it hits 0 (the resize burst has
    /// settled) Kova runs the Cmd+R repaint (soft_reset + winsize nudge) once,
    /// so a differential TUI (Claude Code) fully repaints against a clean
    /// state instead of leaving stale blank bands after a resize.
    resize_settle: RefCell<std::collections::HashMap<PaneId, u32>>,
    /// Countdown (frames) per pane until a post-nudge-restore hole check runs:
    /// the app may answer the restore SIGWINCH with a clear-screen followed by
    /// a PARTIAL differential repaint (skipping rows it wrongly believes
    /// unchanged), leaving a permanent blank band. ~0.5s after each restore we
    /// scan the grid and force another repaint if a band is found.
    post_restore_checks: RefCell<Vec<PtyRestore>>,
    /// Band-repair repaints already fired per pane since its last real resize,
    /// bounding the nudge → broken frame → re-nudge loop (see MAX_BAND_REPAIRS).
    band_repair_attempts: RefCell<std::collections::HashMap<PaneId, u32>>,
    /// Original `SavedTab` for tabs that are still placeholders or whose
    /// deferred restoration failed. Looked up by `TabId` at save time so we
    /// snapshot the placeholder's *original* data rather than its empty live
    /// state — otherwise periodic autosave silently overwrites the user's tab.
    tab_backup: RefCell<std::collections::HashMap<TabId, crate::session::SavedTab>>,
}

#[derive(Clone, Copy)]
struct ResizeFeedback {
    mode: ResizeMode,
    screen_w: u32,
    virtual_w: u32,
    remaining_frames: u32,
}

#[derive(Clone, Copy)]
enum ResizeMode { Ratio, Virtual, Edge }

#[derive(Clone, Copy)]
struct BoundaryHit {
    time: std::time::Instant,
    direction: NavDirection,
}

#[derive(Clone, Copy)]
struct BoundaryFlash {
    /// Which edge to flash (Left or Right).
    edge: NavDirection,
    remaining_frames: u32,
}






/// Debounce window before a keystroke kicks off a live search.
const SEARCH_DEBOUNCE: std::time::Duration = std::time::Duration::from_millis(140);


#[derive(Clone)]
struct PaneFlash {
    pane_id: PaneId,
    remaining_frames: u32,
    /// Big label drawn over the pane while it flashes. Set on jumps that can
    /// land far from where the eye was (Cmd+J), so the pane says where it is
    /// instead of leaving the reader to hunt for the status bar. `None` keeps
    /// the plain border pulse.
    label: Option<PaneFlashLabel>,
}

/// The two lines of a flash label: the directory the pane sits in, and the
/// path above it.
#[derive(Clone)]
struct PaneFlashLabel {
    name: String,
    parent: String,
}

/// Split a working directory into the big line (the directory's own name) and
/// the dim line above it, with `$HOME` folded to `~`. The root and the home
/// directory are their own name and have no parent line.
fn flash_label_parts(cwd: &str, home: &str) -> (String, String) {
    let display = if !home.is_empty() && (cwd == home || cwd.starts_with(&format!("{home}/"))) {
        format!("~{}", &cwd[home.len()..])
    } else {
        cwd.to_string()
    };
    let trimmed = display.trim_end_matches('/');
    if trimmed.is_empty() {
        return ("/".to_string(), String::new());
    }
    match trimmed.rsplit_once('/') {
        Some((parent, name)) => {
            let parent = if parent.is_empty() { "/".to_string() } else { parent.to_string() };
            (name.to_string(), parent)
        }
        None => (trimmed.to_string(), String::new()),
    }
}

/// Pending restore of a PTY winsize after the Cmd+R repaint nudge.
/// The restore must NOT happen back-to-back with the nudge: two immediate
/// TIOCSWINSZ calls coalesce their SIGWINCHs, and the foreground program
/// then reads a winsize identical to its cached one — programs that compare
/// old == new size skip the redraw entirely.
#[derive(Clone, Copy)]
struct PtyRestore {
    pane_id: PaneId,
    remaining_frames: u32,
}





struct SendToWindowEntry {
    label: String,
    /// Index in app delegate's window list, or None for "New Window".
    window_index: Option<usize>,
}

struct MergeTabEntry {
    label: String,
    /// Tab index in the current window's tab list.
    tab_index: usize,
}






















define_class!(
    #[unsafe(super(objc2_app_kit::NSView))]
    #[thread_kind = MainThreadOnly]
    #[name = "KovaView"]
    #[ivars = KovaViewIvars]
    pub struct KovaView;

    unsafe impl NSObjectProtocol for KovaView {}
    unsafe impl NSWindowDelegate for KovaView {
        /// Intercept the close button (traffic light) to use our closing flow
        /// instead of letting AppKit destroy the window directly.
        #[unsafe(method(windowShouldClose:))]
        fn window_should_close(&self, _sender: &objc2::runtime::AnyObject) -> bool {
            self.do_close_window();
            false // we handle closing via the closing flag + timer
        }
    }
    unsafe impl NSTextInputClient for KovaView {
        #[unsafe(method(insertText:replacementRange:))]
        unsafe fn insert_text_replacement_range(&self, string: &objc2::runtime::AnyObject, _replacement_range: objc2_foundation::NSRange) {
            let text = unsafe { nsstring_from_input(string) };
            // Clear marked text
            *self.ivars().marked_text.borrow_mut() = None;
            // Write to PTY
            if let Some(pane) = self.focused_pane() {
                pane.terminal.write().reset_scroll();
                // Typing into a pane answers it: it is no longer waiting on us.
                pane.clear_awaiting();
                input::write_text(&text, &pane.pty);
            }
        }

        #[unsafe(method(doCommandBySelector:))]
        unsafe fn do_command_by_selector(&self, _selector: objc2::runtime::Sel) {
            if let Some(event_ptr) = self.ivars().current_event.get() {
                let event = unsafe { &*event_ptr };
                if let Some(pane) = self.focused_pane() {
                    let (cursor_keys_app, kitty_flags) = {
                        let term = pane.terminal.read();
                        (term.cursor_keys_application, term.kitty_flags())
                    };
                    pane.terminal.write().reset_scroll();
                    pane.clear_awaiting();
                    if let Some(kb) = self.ivars().keybindings.get() {
                        input::handle_key_event(event, &pane.pty, cursor_keys_app, kb, kitty_flags);
                    }
                }
            }
        }

        #[unsafe(method(setMarkedText:selectedRange:replacementRange:))]
        unsafe fn set_marked_text_selected_range_replacement_range(
            &self,
            string: &objc2::runtime::AnyObject,
            _selected_range: objc2_foundation::NSRange,
            _replacement_range: objc2_foundation::NSRange,
        ) {
            let text = unsafe { nsstring_from_input(string) };
            *self.ivars().marked_text.borrow_mut() = if text.is_empty() { None } else { Some(text) };
        }

        #[unsafe(method(unmarkText))]
        fn unmark_text(&self) {
            *self.ivars().marked_text.borrow_mut() = None;
        }

        #[unsafe(method(hasMarkedText))]
        fn has_marked_text(&self) -> bool {
            self.ivars().marked_text.borrow().is_some()
        }

        #[unsafe(method(markedRange))]
        fn marked_range(&self) -> objc2_foundation::NSRange {
            if self.ivars().marked_text.borrow().is_some() {
                objc2_foundation::NSRange { location: 0, length: 1 }
            } else {
                objc2_foundation::NSRange { location: objc2_foundation::NSNotFound as usize, length: 0 }
            }
        }

        #[unsafe(method(selectedRange))]
        fn selected_range(&self) -> objc2_foundation::NSRange {
            objc2_foundation::NSRange { location: objc2_foundation::NSNotFound as usize, length: 0 }
        }

        #[unsafe(method_id(attributedSubstringForProposedRange:actualRange:))]
        #[unsafe(method_family = none)]
        unsafe fn attributed_substring_for_proposed_range(
            &self,
            _range: objc2_foundation::NSRange,
            _actual_range: objc2_foundation::NSRangePointer,
        ) -> Option<objc2::rc::Retained<objc2_foundation::NSAttributedString>> {
            None
        }

        #[unsafe(method_id(validAttributesForMarkedText))]
        #[unsafe(method_family = none)]
        fn valid_attributes_for_marked_text(&self) -> objc2::rc::Retained<objc2_foundation::NSArray<objc2_foundation::NSAttributedStringKey>> {
            objc2_foundation::NSArray::new()
        }

        #[unsafe(method(firstRectForCharacterRange:actualRange:))]
        unsafe fn first_rect_for_character_range(
            &self,
            _range: objc2_foundation::NSRange,
            _actual_range: objc2_foundation::NSRangePointer,
        ) -> objc2_core_foundation::CGRect {
            let frame = self.frame();
            let window_frame = if let Some(window) = self.window() {
                window.frame()
            } else {
                return objc2_core_foundation::CGRect::ZERO;
            };
            objc2_core_foundation::CGRect {
                origin: objc2_core_foundation::CGPoint {
                    x: window_frame.origin.x + frame.origin.x,
                    y: window_frame.origin.y + frame.origin.y,
                },
                size: objc2_core_foundation::CGSize { width: 0.0, height: 0.0 },
            }
        }

        #[unsafe(method(characterIndexForPoint:))]
        fn character_index_for_point(&self, _point: objc2_core_foundation::CGPoint) -> usize {
            objc2_foundation::NSNotFound as usize
        }
    }

    impl KovaView {
        #[unsafe(method(acceptsFirstResponder))]
        fn accepts_first_responder(&self) -> bool {
            true
        }

        #[unsafe(method(mouseDownCanMoveWindow))]
        fn mouse_down_can_move_window(&self) -> bool {
            // Must be false so we get mouseDown events in the titlebar area.
            // We handle window dragging ourselves in hit_test_tab_bar when clicking
            // outside of tabs.
            false
        }

        #[unsafe(method(keyDown:))]
        fn key_down(&self, event: &NSEvent) {
            // Send-to-window overlay handles its own keys
            if self.ivars().send_to_window.borrow().is_some() {
                self.handle_send_to_window_key(event);
                return;
            }

            // Merge-tab overlay handles its own keys
            if self.ivars().merge_tab.borrow().is_some() {
                self.handle_merge_tab_key(event);
                return;
            }

            // Recent projects overlay handles its own keys
            if self.ivars().recent_projects.borrow().is_some() {
                self.handle_recent_projects_key(event);
                return;
            }

            // Search palette overlay handles its own keys
            if self.ivars().search_palette.borrow().is_some() {
                self.handle_search_palette_key(event);
                return;
            }

            // Pane switcher overlay handles its own keys
            if self.ivars().pane_switcher.borrow().is_some() {
                self.handle_pane_switcher_key(event);
                return;
            }

            // Escape closes help/mem report overlays
            if event.keyCode() == 0x35 {
                if self.ivars().show_help.get() {
                    self.ivars().show_help.set(false);
                    self.mark_dirty();
                    return;
                }
                if self.ivars().show_mem_report.get() {
                    self.ivars().show_mem_report.set(false);
                    self.mark_dirty();
                    return;
                }
            }
            // Block all keys when help/mem report overlay is shown (handled in performKeyEquivalent)
            if self.ivars().show_help.get() || self.ivars().show_mem_report.get() {
                return;
            }

            // If rename tab is active, route keys to rename
            if self.ivars().rename_tab.borrow().is_some() {
                self.handle_rename_tab_key(event);
                return;
            }

            // If rename pane is active, route keys to rename
            if self.ivars().rename_pane.borrow().is_some() {
                self.handle_rename_pane_key(event);
                return;
            }

            // If filter is active, route keys to filter
            if self.ivars().filter.borrow().is_some() {
                self.handle_filter_key(event);
                return;
            }

            // Ctrl+F → toggle filter (in addition to Cmd+F via performKeyEquivalent)
            {
                let modifiers = event.modifierFlags();
                let has_ctrl = modifiers.contains(NSEventModifierFlags::Control);
                let has_cmd = modifiers.contains(NSEventModifierFlags::Command);
                if has_ctrl && !has_cmd {
                    if let Some(chars) = event.charactersIgnoringModifiers() {
                        if chars.to_string() == "f" {
                            self.toggle_filter();
                            return;
                        }
                    }
                }
            }

            // Ctrl+L → wipe Kova's scrollback, then let the key through so the app
            // redraws. The app's own answer to Ctrl+L is `ESC[H ESC[2J` (terminfo
            // `clear`), which only erases the visible screen — the scrollback needs
            // `ESC[3J`, which nothing sends here. Skipped in alt-screen, where Ctrl+L
            // means "repaint" and the scrollback holds whatever ran before the
            // full-screen app.
            {
                let modifiers = event.modifierFlags();
                let has_ctrl = modifiers.contains(NSEventModifierFlags::Control);
                let has_option = modifiers.contains(NSEventModifierFlags::Option);
                let has_cmd = modifiers.contains(NSEventModifierFlags::Command);
                if has_ctrl && !has_option && !has_cmd {
                    if let Some(chars) = event.charactersIgnoringModifiers() {
                        if chars.to_string() == "l" {
                            if let Some(pane) = self.focused_pane() {
                                let mut term = pane.terminal.write();
                                if !term.in_alt_screen {
                                    term.clear_scrollback_and_screen();
                                }
                            }
                        }
                    }
                }
            }

            // Ctrl+Option+arrows → adjust virtual width
            {
                let modifiers = event.modifierFlags();
                let has_ctrl = modifiers.contains(NSEventModifierFlags::Control);
                let has_option = modifiers.contains(NSEventModifierFlags::Option);
                let has_cmd = modifiers.contains(NSEventModifierFlags::Command);
                if has_ctrl && has_option && !has_cmd {
                    if let Some(chars) = event.charactersIgnoringModifiers() {
                        let dir = match chars.to_string().as_str() {
                            "\u{f703}" => Some(1.0_f32),
                            "\u{f702}" => Some(-1.0_f32),
                            _ => None,
                        };
                        if let Some(dir) = dir {
                            self.adjust_virtual_width(dir);
                            return;
                        }
                    }
                }
            }

            if let Some(pane) = self.focused_pane() {
                let (kitty_flags, cursor_keys_app) = {
                    let term = pane.terminal.read();
                    (term.kitty_flags(), term.cursor_keys_application)
                };

                let modifiers = event.modifierFlags();
                let has_ctrl = modifiers.contains(NSEventModifierFlags::Control);
                let has_alt = modifiers.contains(NSEventModifierFlags::Option);
                let has_cmd = modifiers.contains(NSEventModifierFlags::Command);

                if kitty_flags > 0 && (has_ctrl || has_alt) && !has_cmd {
                    // Kitty mode: bypass macOS text input for modified keys
                    pane.terminal.write().reset_scroll();
                    pane.clear_awaiting();
                    if let Some(kb) = self.ivars().keybindings.get() {
                        input::handle_key_event(event, &pane.pty, cursor_keys_app, kb, kitty_flags);
                    }
                } else {
                    // Normal path: macOS text input (dead keys, IME)
                    self.ivars().current_event.set(Some(event as *const NSEvent));
                    let event_retained: Retained<NSEvent> = event.retain();
                    let events = NSArray::from_retained_slice(&[event_retained]);
                    self.interpretKeyEvents(&events);
                    self.ivars().current_event.set(None);
                }
            }
        }

        #[unsafe(method(performKeyEquivalent:))]
        fn perform_key_equivalent(&self, event: &NSEvent) -> objc2::runtime::Bool {
            let combo = KeyCombo::from_event(event);

            let keybindings = match self.ivars().keybindings.get() {
                Some(kb) => kb,
                None => return objc2::runtime::Bool::NO,
            };

            // When recent projects overlay is shown, route keys through the overlay handler
            if self.ivars().recent_projects.borrow().is_some() {
                self.handle_recent_projects_key(event);
                return objc2::runtime::Bool::YES;
            }

            // When the search palette is open, route keys through its handler so
            // shortcuts like Cmd+V/Cmd+P don't fall through to the global map.
            if self.ivars().search_palette.borrow().is_some() {
                self.handle_search_palette_key(event);
                return objc2::runtime::Bool::YES;
            }

            // When the pane switcher is open, route keys through its handler so
            // shortcuts don't fall through to the global map.
            if self.ivars().pane_switcher.borrow().is_some() {
                self.handle_pane_switcher_key(event);
                return objc2::runtime::Bool::YES;
            }

            // When help overlay is shown, close it first then let the action through
            if self.ivars().show_help.get() {
                self.ivars().show_help.set(false);
                self.mark_dirty();
                if matches!(keybindings.window_map.get(&combo), Some(Action::ToggleHelp)) || event.keyCode() == 0x35 {
                    return objc2::runtime::Bool::YES;
                }
                // Fall through: close overlay AND execute the action (e.g. Cmd+Q)
            }

            // When mem report overlay is shown, close it first then let the action through
            if self.ivars().show_mem_report.get() {
                self.ivars().show_mem_report.set(false);
                self.mark_dirty();
                if matches!(keybindings.window_map.get(&combo), Some(Action::MemReport)) || event.keyCode() == 0x35 {
                    return objc2::runtime::Bool::YES;
                }
                // Fall through: close overlay AND execute the action (e.g. Cmd+Q)
            }

            // When rename tab/pane is active, intercept Paste to insert into the edit field
            if self.ivars().rename_tab.borrow().is_some() || self.ivars().rename_pane.borrow().is_some() {
                if matches!(keybindings.window_map.get(&combo), Some(Action::Paste)) {
                    let pasteboard = NSPasteboard::generalPasteboard();
                    if let Some(text) = unsafe { pasteboard.stringForType(objc2_app_kit::NSPasteboardTypeString) } {
                        let text = text.to_string();
                        if !text.is_empty() {
                            if let Some(state) = self.ivars().rename_tab.borrow_mut().as_mut() {
                                let byte_idx = state.input.char_indices()
                                    .nth(state.cursor).map(|(i, _)| i)
                                    .unwrap_or(state.input.len());
                                state.input.insert_str(byte_idx, &text);
                                state.cursor += text.chars().count();
                            } else if let Some(state) = self.ivars().rename_pane.borrow_mut().as_mut() {
                                let byte_idx = state.input.char_indices()
                                    .nth(state.cursor).map(|(i, _)| i)
                                    .unwrap_or(state.input.len());
                                state.input.insert_str(byte_idx, &text);
                                state.cursor += text.chars().count();
                            }
                            self.mark_dirty();
                        }
                    }
                    return objc2::runtime::Bool::YES;
                }
                // Block other key equivalents during rename
                return objc2::runtime::Bool::NO;
            }

            if let Some(action) = keybindings.window_map.get(&combo) {
                log::debug!("performKeyEquivalent: combo={:?} action={:?}", combo, action);
                let action = action.clone();
                return if self.dispatch_action(&action) {
                    objc2::runtime::Bool::YES
                } else {
                    objc2::runtime::Bool::NO
                };
            }

            if combo.cmd {
                log::debug!("performKeyEquivalent: UNMATCHED combo={:?}", combo);
            }
            objc2::runtime::Bool::NO
        }

        #[unsafe(method(flagsChanged:))]
        fn flags_changed(&self, event: &NSEvent) {
            let modifiers = event.modifierFlags();
            let cmd = modifiers.contains(NSEventModifierFlags::Command);
            self.ivars().cmd_held.set(cmd);
            if !cmd {
                let had_hover = self.ivars().hovered_url.borrow().is_some();
                if had_hover {
                    *self.ivars().hovered_url.borrow_mut() = None;
                    NSCursor::arrowCursor().set();
                    self.mark_dirty();
                }
            }
        }

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
            let ivars = self.ivars();
            let is_trackpad = event.hasPreciseScrollingDeltas();

            // Pane switcher overlay handles its own scroll (scroll the column under the cursor).
            if ivars.pane_switcher.borrow().is_some() {
                self.handle_pane_switcher_scroll(event, is_trackpad);
                return;
            }

            // Phase-based axis lock (trackpad only)
            if is_trackpad {
                let phase = event.phase();
                let momentum = event.momentumPhase();

                if phase == NSEventPhase::Began {
                    let dy = event.scrollingDeltaY().abs();
                    let dx = event.scrollingDeltaX().abs();
                    ivars.scroll_axis_lock.set(if dy >= dx {
                        ScrollAxisLock::Vertical
                    } else {
                        ScrollAxisLock::Horizontal
                    });
                } else if phase.intersects(NSEventPhase::Ended | NSEventPhase::Cancelled)
                    && momentum == NSEventPhase::None
                {
                    ivars.scroll_axis_lock.set(ScrollAxisLock::None);
                } else if momentum.intersects(NSEventPhase::Ended | NSEventPhase::Cancelled) {
                    ivars.scroll_axis_lock.set(ScrollAxisLock::None);
                }
            }

            let lock = ivars.scroll_axis_lock.get();

            // Vertical scroll (pane under cursor)
            if lock != ScrollAxisLock::Horizontal {
                if let Some((pane, vp)) = self.pane_at_event(event) {
                    let dy = event.scrollingDeltaY();
                    let lines = if is_trackpad {
                        let sensitivity = ivars.config.get()
                            .map(|c| c.terminal.scroll_sensitivity)
                            .unwrap_or(TerminalConfig::default().scroll_sensitivity);
                        let acc = pane.scroll_accumulator.get() + dy / sensitivity;
                        let discrete = acc as i32;
                        pane.scroll_accumulator.set(acc - discrete as f64);
                        discrete
                    } else {
                        dy as i32
                    };
                    if lines != 0 {
                        // Forward scroll to PTY if mouse reporting is active
                        let mouse_mode = pane.terminal.read().mouse_mode;
                        let sgr = pane.terminal.read().sgr_mouse;
                        if mouse_mode >= 1000 && sgr {
                            if let Some((col, row)) = self.pixel_to_cell_in(event, pane, &vp) {
                                // Each discrete line = one scroll event
                                let count = lines.unsigned_abs() as usize;
                                let button = if lines > 0 { 64u8 } else { 65u8 }; // 64=up, 65=down
                                for _ in 0..count {
                                    self.send_sgr_mouse(pane, button, col, row, true, false, event);
                                }
                            }
                        } else {
                            let mut term = pane.terminal.write();
                            let active_tab_idx = ivars.active_tab.get();
                            log::debug!("SCROLL-EVENT tab={} pane={} term_id={} lines={} offset_before={}",
                                active_tab_idx, pane.id, term.terminal_id, lines, term.scroll_offset());
                            // One info line per scroll session (offset 0 → >0), to pair
                            // tab/pane with the SCROLL-START line for the cross-tab
                            // scrollback bug — without needing RUST_LOG=debug.
                            if term.scroll_offset() == 0 && lines > 0 {
                                log::info!("SCROLL-BEGIN tab={} pane={} term_id={}",
                                    active_tab_idx, pane.id, term.terminal_id);
                            }
                            term.scroll(lines);
                            // Reset accumulator when hitting bounds to avoid residual drift
                            let at_bound = term.scroll_offset() == 0
                                || term.scroll_offset() == term.scrollback_len() as i32;
                            if at_bound {
                                pane.scroll_accumulator.set(0.0);
                            }
                        }
                    }
                }
            }

            // Horizontal scroll for virtual viewport (trackpad only)
            if lock != ScrollAxisLock::Vertical && is_trackpad {
                let dx = event.scrollingDeltaX();
                if dx != 0.0 {
                    let screen_w = self.drawable_viewport().width;
                    let min_w = self.min_split_width_px();
                    let mut tabs = ivars.tabs.borrow_mut();
                    let idx = ivars.active_tab.get();
                    if let Some(tab) = tabs.get_mut(idx) {
                        let vw = tab.virtual_width(screen_w, min_w);
                        if vw > screen_w {
                            tab.scroll_offset_x = (tab.scroll_offset_x - dx as f32)
                                .clamp(0.0, vw - screen_w);
                            drop(tabs);
                            self.mark_dirty();
                        }
                    }
                }
            }
        }

        #[unsafe(method(mouseDown:))]
        fn mouse_down(&self, event: &NSEvent) {
            let (px, py) = self.event_to_pixel(event);

            // Check filter click
            if self.ivars().filter.borrow().is_some() {
                self.handle_filter_click(px, py);
                return;
            }

            // Pane switcher overlay click → focus the clicked pane
            if self.ivars().pane_switcher.borrow().is_some() {
                self.handle_pane_switcher_click(px, py);
                return;
            }

            // Check tab bar click
            if self.hit_test_tab_bar(px, py, event) {
                return;
            }

            // Click on the minimized-panes counter (global status bar) → open switcher
            if let Some(renderer) = self.ivars().renderer.get() {
                if let Some((zx, zy, zw, zh)) = renderer.read().minimized_counter_zone {
                    if px >= zx && px < zx + zw && py >= zy && py < zy + zh {
                        self.open_pane_switcher(false);
                        return;
                    }
                }
            }

            // Check separator hit
            if let Some(drag) = self.hit_test_separator(px, py) {
                self.ivars().drag_separator.set(Some(drag));
                return;
            }

            // Cmd+Click opens URL
            let modifiers = event.modifierFlags();
            if modifiers.contains(NSEventModifierFlags::Command) {
                // Re-validate: the cached hover may be stale if output arrived
                // since the last mouse move (content shifted under the cursor).
                self.update_hovered_url(event);
                if let Some(url) = self.ivars().hovered_url.borrow().as_ref().map(|h| h.2.clone()) {
                    let _ = std::process::Command::new("open").arg(&url).spawn();
                    return;
                }
            }

            // Click on minimized pane → restore it
            if let Some((pane, _vp)) = self.pane_at_event(event) {
                if pane.minimized {
                    let pane_id = pane.id;
                    let mut tabs = self.ivars().tabs.borrow_mut();
                    let idx = self.ivars().active_tab.get();
                    if let Some(tab) = tabs.get_mut(idx) {
                        let full = self.drawable_viewport();
                        let min_w = self.min_split_width_px();
                        tab.restore_pane_adjust_virtual(pane_id, full.width, min_w);
                        tab.focused_pane = pane_id;
                        tab.mark_all_dirty();
                        tab.clamp_scroll(full.width, min_w);
                        self.scroll_to_reveal_pane(tab, pane_id, full.width);
                    }
                    drop(tabs);
                    self.resize_all_panes();
                    return;
                }
            }

            // Click sets focus to the pane under the cursor
            if let Some((pane, vp)) = self.pane_at_event(event) {
                let old_focused = {
                    let tabs = self.ivars().tabs.borrow();
                    let idx = self.ivars().active_tab.get();
                    tabs.get(idx).map(|t| t.focused_pane).unwrap_or(0)
                };
                {
                    let mut tabs = self.ivars().tabs.borrow_mut();
                    let idx = self.ivars().active_tab.get();
                    if let Some(tab) = tabs.get_mut(idx) {
                        tab.focused_pane = pane.id;
                    }
                }
                // Mark old focused pane dirty so its dim overlay updates
                if old_focused != pane.id {
                    // Acknowledge completion and bell on the newly focused pane
                    let t = pane.terminal.read();
                    t.ack_completion();
                    t.bell.store(false, std::sync::atomic::Ordering::Relaxed);
                    drop(t);
                    let tabs = self.ivars().tabs.borrow();
                    let idx = self.ivars().active_tab.get();
                    if let Some(tab) = tabs.get(idx) {
                        if let Some(old) = tab.pane(old_focused) {
                            old.terminal.read().dirty.store(true, std::sync::atomic::Ordering::Relaxed);
                        }
                    }
                }
                // Forward to PTY if mouse reporting is active
                {
                    let term = pane.terminal.read();
                    if term.mouse_mode >= 1000 && term.sgr_mouse {
                        drop(term);
                        if let Some((col, row)) = self.pixel_to_cell_in(event, pane, &vp) {
                            // button 0=left, 1=middle, 2=right
                            let button_number = event.buttonNumber() as u8;
                            let button_code = button_number.min(2);
                            self.send_sgr_mouse(pane, button_code, col, row, true, false, event);
                        }
                        return;
                    }
                }
                if let Some(pos) = self.pixel_to_grid_in(event, pane, &vp) {
                    let click_count = event.clickCount();
                    let mut term = pane.terminal.write();
                    if click_count == 2 {
                        // Double-click: select word
                        let (wstart, wend) = term.word_bounds_at(pos);
                        term.selection = Some(Selection {
                            anchor: GridPos { line: pos.line, col: wstart },
                            end: GridPos { line: pos.line, col: wend },
                            mode: SelectionMode::Word,
                        });
                    } else if click_count >= 3 {
                        // Triple-click: select entire line
                        let row_len = term.row_at(pos.line)
                            .map(|r| r.cells.len().saturating_sub(1) as u16)
                            .unwrap_or(0);
                        term.selection = Some(Selection {
                            anchor: GridPos { line: pos.line, col: 0 },
                            end: GridPos { line: pos.line, col: row_len },
                            mode: SelectionMode::Line,
                        });
                    } else {
                        term.selection = Some(Selection { anchor: pos, end: pos, mode: SelectionMode::Normal });
                    }
                    term.dirty.store(true, std::sync::atomic::Ordering::Relaxed);
                }
            }
        }

        #[unsafe(method(mouseDragged:))]
        fn mouse_dragged(&self, event: &NSEvent) {
            // Handle tab drag
            if let Some(mut drag) = self.ivars().drag_tab.get() {
                let (px, _py) = self.event_to_pixel(event);
                drag.current_x = px;
                if !drag.dragging {
                    if (px - drag.start_x).abs() >= 3.0 {
                        drag.dragging = true;
                    } else {
                        self.ivars().drag_tab.set(Some(drag));
                        return;
                    }
                }
                if let Some(target) = self.tab_index_at_x(px) {
                    if target != drag.tab_index {
                        let mut tabs = self.ivars().tabs.borrow_mut();
                        tabs.swap(drag.tab_index, target);
                        drop(tabs);
                        self.ivars().active_tab.set(target);
                        drag.tab_index = target;
                        self.mark_dirty();
                    }
                }
                self.ivars().drag_tab.set(Some(drag));
                return;
            }

            // Handle separator drag
            if let Some(drag) = self.ivars().drag_separator.get() {
                let (px, py) = self.event_to_pixel(event);
                let mut tabs = self.ivars().tabs.borrow_mut();
                let idx = self.ivars().active_tab.get();
                if let Some(tab) = tabs.get_mut(idx) {
                    if let Some(col_idx) = drag.column_sep_index {
                        // Column separator: adjust weights
                        let delta_px = px - drag.origin_pixel;
                        tab.set_column_weights_by_drag(col_idx, delta_px, drag.parent_dim);
                        self.ivars().drag_separator.set(Some(SeparatorDrag {
                            origin_pixel: px,
                            ..drag
                        }));
                        drop(tabs);
                        self.resize_all_panes();
                    } else if let Some(row_idx) = drag.row_sep_index {
                        // Row separator: adjust row weights within column
                        let delta_px = py - drag.origin_pixel;
                        if drag.col_index < tab.columns.len() {
                            tab.columns[drag.col_index].set_row_weights_by_drag(row_idx, delta_px, drag.parent_dim);
                        }
                        self.ivars().drag_separator.set(Some(SeparatorDrag {
                            origin_pixel: py,
                            ..drag
                        }));
                        drop(tabs);
                        self.resize_all_panes();
                    }
                }
                return;
            }

            // Forward drag to PTY if mouse reporting mode 1002+ is active
            if let Some(pane) = self.focused_pane() {
                let term = pane.terminal.read();
                if term.mouse_mode >= 1002 && term.sgr_mouse {
                    drop(term);
                    let vp = {
                        let tabs = self.ivars().tabs.borrow();
                        let idx = self.ivars().active_tab.get();
                        tabs.get(idx).and_then(|t| t.viewport_for_pane(pane.id, self.panes_viewport_for_tab(t)))
                    };
                    if let Some(vp) = vp {
                        if let Some((col, row)) = self.pixel_to_cell_in(event, pane, &vp) {
                            let button_number = event.buttonNumber() as u8;
                            let button_code = button_number.min(2);
                            self.send_sgr_mouse(pane, button_code, col, row, true, true, event);
                        }
                    }
                    return;
                }
                drop(term);
            }

            // Drag continues on the focused pane (set by mouseDown)
            if let Some(pane) = self.focused_pane() {
                let vp = {
                    let tabs = self.ivars().tabs.borrow();
                    let idx = self.ivars().active_tab.get();
                    tabs.get(idx).and_then(|t| t.viewport_for_pane(pane.id, self.panes_viewport_for_tab(t)))
                };
                if let Some(vp) = vp {
                    if let Some(pos) = self.pixel_to_grid_in(event, pane, &vp) {
                        // Mouse is inside viewport — normal drag
                        self.ivars().auto_scroll_speed.set(0);
                        let mut term = pane.terminal.write();
                        // Read mode and anchor before mutating selection
                        let sel_info = term.selection.as_ref().map(|s| (s.mode, s.anchor));
                        if let Some((mode, anchor)) = sel_info {
                            match mode {
                                SelectionMode::Word => {
                                    let (wstart, wend) = term.word_bounds_at(pos);
                                    let anchor_before = (anchor.line, anchor.col) <= (pos.line, wstart);
                                    if let Some(sel) = term.selection.as_mut() {
                                        if anchor_before {
                                            sel.end = GridPos { line: pos.line, col: wend };
                                        } else {
                                            sel.end = GridPos { line: pos.line, col: wstart };
                                        }
                                    }
                                }
                                SelectionMode::Line => {
                                    let row_len = term.row_at(pos.line)
                                        .map(|r| r.cells.len().saturating_sub(1) as u16)
                                        .unwrap_or(0);
                                    let anchor_row_len = term.row_at(anchor.line)
                                        .map(|r| r.cells.len().saturating_sub(1) as u16)
                                        .unwrap_or(0);
                                    if let Some(sel) = term.selection.as_mut() {
                                        if pos.line >= anchor.line {
                                            sel.anchor.col = 0;
                                            sel.end = GridPos { line: pos.line, col: row_len };
                                        } else {
                                            sel.anchor.col = anchor_row_len;
                                            sel.end = GridPos { line: pos.line, col: 0 };
                                        }
                                    }
                                }
                                SelectionMode::Normal => {
                                    if let Some(sel) = term.selection.as_mut() {
                                        sel.end = pos;
                                    }
                                }
                            }
                            term.dirty.store(true, std::sync::atomic::Ordering::Relaxed);
                        }
                    } else {
                        // Mouse is outside viewport — compute auto-scroll speed
                        let renderer = self.ivars().renderer.get();
                        if let Some(renderer) = renderer {
                            let (_, pixel_y) = self.event_to_pixel(event);
                            let renderer_r = renderer.read();
                            let cell_h = renderer_r.cell_size().1;
                            drop(renderer_r);

                            let rel_y = pixel_y - vp.y;
                            let term = pane.terminal.read();
                            let y_offset = term.y_offset_rows() as f32 * cell_h;
                            let bottom = y_offset + (term.rows as f32 * cell_h);

                            if rel_y < y_offset {
                                // Above viewport — scroll up
                                let dist = y_offset - rel_y;
                                let speed = -((dist / cell_h).ceil() as i32).clamp(1, 10);
                                self.ivars().auto_scroll_speed.set(speed);
                            } else if rel_y > bottom {
                                // Below viewport — scroll down
                                let dist = rel_y - bottom;
                                let speed = ((dist / cell_h).ceil() as i32).clamp(1, 10);
                                self.ivars().auto_scroll_speed.set(speed);
                            } else {
                                // Mouse is vertically inside viewport but pixel_to_grid_in
                                // returned None (e.g. mouse to the left of the grid) — no scroll
                                self.ivars().auto_scroll_speed.set(0);
                            }
                        }
                    }
                }
            }
        }

        #[unsafe(method(mouseUp:))]
        fn mouse_up(&self, event: &NSEvent) {
            self.ivars().auto_scroll_speed.set(0);
            if self.ivars().drag_tab.get().is_some() {
                self.ivars().drag_tab.set(None);
                return;
            }
            if self.ivars().drag_separator.get().is_some() {
                self.ivars().drag_separator.set(None);
                return;
            }
            // Forward to PTY if mouse reporting is active
            if let Some((pane, vp)) = self.pane_at_event(event) {
                let term = pane.terminal.read();
                if term.mouse_mode >= 1000 && term.sgr_mouse {
                    drop(term);
                    if let Some((col, row)) = self.pixel_to_cell_in(event, pane, &vp) {
                        let button_number = event.buttonNumber() as u8;
                        let button_code = button_number.min(2);
                        self.send_sgr_mouse(pane, button_code, col, row, false, false, event);
                    }
                    return;
                }
            }
            if let Some(pane) = self.focused_pane() {
                let mut term = pane.terminal.write();
                // Single click (no drag) — clear selection
                if let Some(ref sel) = term.selection {
                    if sel.anchor == sel.end && sel.mode == SelectionMode::Normal {
                        term.selection = None;
                        term.dirty.store(true, std::sync::atomic::Ordering::Relaxed);
                        return;
                    }
                }
                let text = term.selected_text();
                if !text.is_empty() {
                    copy_to_pasteboard(&text);
                }
            }
        }

        #[unsafe(method(tabColorSelected:))]
        fn tab_color_selected(&self, sender: &objc2_app_kit::NSMenuItem) {
            const PALETTE_SIZE: isize = 6;
            let tag = sender.tag();
            let tab_idx = self.ivars().color_menu_tab.get();
            let mut tabs = self.ivars().tabs.borrow_mut();
            if let Some(tab) = tabs.get_mut(tab_idx) {
                tab.color = if (0..PALETTE_SIZE).contains(&tag) {
                    Some(tag as usize)
                } else {
                    None
                };
            }
            drop(tabs);
            self.mark_dirty();
        }

        #[unsafe(method(rightMouseDown:))]
        fn right_mouse_down(&self, event: &NSEvent) {
            let (px, py) = self.event_to_pixel(event);
            let tab_bar_h = self.tab_bar_height();
            if py <= tab_bar_h {
                if let Some(tab_idx) = self.tab_index_at_x(px) {
                    self.show_tab_color_menu(event, tab_idx);
                    return;
                }
            }
            // Default behavior for right-click outside tab bar
            unsafe { msg_send![super(self), rightMouseDown: event] }
        }

        #[unsafe(method(mouseMoved:))]
        fn mouse_moved(&self, event: &NSEvent) {
            // Forward move to PTY if all-motion tracking (mode 1003) is active
            if let Some((pane, vp)) = self.pane_at_event(event) {
                let term = pane.terminal.read();
                if term.mouse_mode >= 1003 && term.sgr_mouse {
                    drop(term);
                    if let Some((col, row)) = self.pixel_to_cell_in(event, pane, &vp) {
                        // No button pressed during move → button code 3 (no button) + motion flag
                        self.send_sgr_mouse(pane, 3, col, row, true, true, event);
                    }
                    return;
                }
            }
            self.update_separator_cursor(event);
            self.update_hovered_url(event);
            self.update_tooltip(event);
        }

        #[unsafe(method(updateTrackingAreas))]
        fn update_tracking_areas(&self) {
            // Remove old tracking areas
            let old_areas: Vec<_> = self.trackingAreas().to_vec();
            for area in &old_areas {
                self.removeTrackingArea(area);
            }
            // Add new one covering entire view
            let options = NSTrackingAreaOptions::MouseMoved
                | NSTrackingAreaOptions::ActiveInKeyWindow
                | NSTrackingAreaOptions::InVisibleRect;
            let area = unsafe {
                let alloc: objc2::rc::Allocated<NSTrackingArea> = msg_send![objc2::class!(NSTrackingArea), alloc];
                NSTrackingArea::initWithRect_options_owner_userInfo(
                    alloc,
                    self.bounds(),
                    options,
                    Some(self.as_ref()),
                    None,
                )
            };
            self.addTrackingArea(&area);
        }

    }
);

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
fn copy_to_pasteboard(text: &str) {
    let pasteboard = NSPasteboard::generalPasteboard();
    pasteboard.clearContents();
    let ns_str = NSString::from_str(text);
    unsafe {
        pasteboard.setString_forType(&ns_str, objc2_app_kit::NSPasteboardTypeString);
    }
}

impl KovaView {
    fn new(mtm: MainThreadMarker, frame: CGRect) -> Retained<Self> {
        let this = mtm.alloc::<Self>().set_ivars(KovaViewIvars {
            renderer: OnceCell::new(),
            tabs: RefCell::new(Vec::new()),
            active_tab: Cell::new(0),
            metal_layer: OnceCell::new(),
            last_scale: Cell::new(0.0),
            last_screen_w: Cell::new(0.0),
            last_focused: Cell::new(true),
            config: OnceCell::new(),
            drag_separator: Cell::new(None),
            filter: RefCell::new(None),
            rename_tab: RefCell::new(None),
            rename_pane: RefCell::new(None),
            tab_bar_left_inset: Cell::new(0.0),
            color_menu_tab: Cell::new(0),
            drag_tab: Cell::new(None),
            hovered_url: RefCell::new(None),
            cmd_held: Cell::new(false),
            auto_scroll_speed: Cell::new(0),
            marked_text: RefCell::new(None),
            current_event: Cell::new(None),
            closing: Cell::new(false),
            skip_session_save: Cell::new(false),
            last_title: RefCell::new(None),
            git_poll_counter: Cell::new(0),
            fg_poll_counter: Cell::new(0),
            git_poll_interval: Cell::new(120), // updated in setup_metal
            keybindings: OnceCell::new(),
            show_help: Cell::new(false),
            show_mem_report: Cell::new(false),
            recent_projects: RefCell::new(None),
            send_to_window: RefCell::new(None),
            merge_tab: RefCell::new(None),
            help_hint_frames: Cell::new(180), // updated in setup_metal
            scroll_axis_lock: Cell::new(ScrollAxisLock::None),
            resize_feedback: Cell::new(None),
            transient_status: RefCell::new(None),
            attention_banner: RefCell::new(None),
            deferred_tabs: RefCell::new(Vec::new()),
            loading_total_panes: Cell::new(0),
            boundary_hit: Cell::new(None),
            boundary_flash: Cell::new(None),
            search_palette: RefCell::new(None),
            pane_switcher: RefCell::new(None),
            pane_flash: RefCell::new(None),
            pty_restore: RefCell::new(Vec::new()),
            recent_resizes: RefCell::new(std::collections::HashMap::new()),
            resize_settle: RefCell::new(std::collections::HashMap::new()),
            post_restore_checks: RefCell::new(Vec::new()),
            band_repair_attempts: RefCell::new(std::collections::HashMap::new()),
            tab_backup: RefCell::new(std::collections::HashMap::new()),
        });
        unsafe { msg_send![super(this), initWithFrame: frame] }
    }

    /// Build memory report, store in renderer for overlay, and log to file.
    fn show_mem_report_overlay(&self) {
        let rss_mb = crate::get_rss_mb();

        // Per-pane stats across ALL windows
        let mut total_panes = 0usize;
        let mut total_grid_bytes = 0usize;
        let mut total_sb_lines = 0usize;
        let mut total_sb_bytes = 0usize;
        let mut total_alt_bytes = 0usize;
        let mut total_renderer_bytes = 0usize;
        let mut pane_details = Vec::new();
        let mut renderer_details = Vec::new();

        let mtm = unsafe { MainThreadMarker::new_unchecked() };
        let ad = crate::app::app_delegate(mtm);
        let all_windows = ad.ivars().windows.borrow();

        for (wi, win) in all_windows.iter().enumerate() {
            if let Some(view) = crate::app::kova_view(win) {
                let tabs = view.ivars().tabs.borrow();
                for (ti, tab) in tabs.iter().enumerate() {
                    tab.for_each_pane(&mut |pane| {
                        let term = pane.terminal.read();
                        let mem = term.mem_bytes();
                        let sb_len = term.scrollback_len();
                        let cols = term.cols;
                        let rows = term.rows;

                        let cell_size = std::mem::size_of::<crate::terminal::Cell>();
                        let row_oh = std::mem::size_of::<crate::terminal::Row>();
                        let grid_b = rows as usize * (row_oh + cols as usize * cell_size);
                        let alt_b = if term.in_alt_screen { grid_b } else { 0 };
                        let sb_b = mem - grid_b - alt_b;

                        total_panes += 1;
                        total_grid_bytes += grid_b;
                        total_sb_lines += sb_len;
                        total_sb_bytes += sb_b;
                        total_alt_bytes += alt_b;

                        pane_details.push(format!(
                            "  w{}t{} pane{}: {}x{}, sb={} lines ({:.1} KB), grid={:.1} KB",
                            wi, ti, pane.id, cols, rows, sb_len,
                            sb_b as f64 / 1024.0, grid_b as f64 / 1024.0,
                        ));
                    });
                }

                // Renderer stats for this window
                if let Some(renderer) = view.ivars().renderer.get() {
                    let r = renderer.read();
                    let (atlas_buf, atlas_dims, glyph_count, vbuf) = r.mem_report();
                    total_renderer_bytes += atlas_buf + vbuf;
                    renderer_details.push(format!(
                        "  w{}: atlas={}x{} ({:.1} KB, {} glyphs), vbufs={:.1} MB",
                        wi, atlas_dims.0, atlas_dims.1,
                        atlas_buf as f64 / 1024.0, glyph_count,
                        vbuf as f64 / (1024.0 * 1024.0),
                    ));
                }
            }
        }
        drop(all_windows);

        let total_terminal = total_grid_bytes + total_sb_bytes + total_alt_bytes;

        // Build report lines (plain text, no ANSI — rendered by overlay)
        let mut report = Vec::new();
        report.push(format!("RSS: {:.1} MB  |  Panes: {}", rss_mb, total_panes));
        report.push(format!(
            "~Terminal: {:.1} MB (grid {:.1} KB, scrollback {:.1} MB [{} lines], alt {:.1} KB)",
            total_terminal as f64 / (1024.0 * 1024.0),
            total_grid_bytes as f64 / 1024.0,
            total_sb_bytes as f64 / (1024.0 * 1024.0),
            total_sb_lines,
            total_alt_bytes as f64 / 1024.0,
        ));
        report.push(format!(
            "~Renderer: {:.1} MB total",
            total_renderer_bytes as f64 / (1024.0 * 1024.0),
        ));
        for rd in &renderer_details {
            report.push(format!("~{}", rd));
        }
        let accounted = total_terminal as f64 / (1024.0 * 1024.0) + total_renderer_bytes as f64 / (1024.0 * 1024.0);
        report.push(format!("~Unaccounted: {:.1} MB (system/Metal drawables/AppKit)", rss_mb - accounted));
        report.push(String::from("~(~ = estimated, may differ from RSS)"));
        report.push(String::new());
        for detail in &pane_details {
            report.push(detail.clone());
        }

        // Log to file
        for line in &report {
            log::info!("{}", line);
        }

        // Store in renderer and show overlay
        if let Some(renderer) = self.ivars().renderer.get() {
            renderer.write().set_mem_report(report);
        }
        self.ivars().show_mem_report.set(true);
        self.mark_dirty();
    }

    /// Force a repaint of the focused pane — workaround for occasional display
    /// corruption that otherwise only clears after a detach/reattach.
    ///
    /// Three levers:
    /// 1. Soft-reset the Kova terminal state (scroll region, cursor visibility,
    ///    SGR attributes) to fix corruption that persists in state, not just GPU.
    /// 2. Nudge the PTY winsize (rows ±1, restored a few ticks later) to provoke
    ///    a SIGWINCH so the foreground program redraws. A same-size set wouldn't
    ///    work: the kernel only emits SIGWINCH when the dimensions actually
    ///    change. The restore is deferred (see `PtyRestore`): restoring
    ///    back-to-back coalesces both SIGWINCHs and the program may then see an
    ///    unchanged winsize and skip the redraw.
    /// 3. Flash the pane border as visible confirmation that the repaint fired,
    ///    even when content doesn't change (e.g. idle shell ignores SIGWINCH).
    fn do_repaint_pane(&self) {
        let pane = match self.focused_pane() {
            Some(p) => p,
            None => return,
        };
        if pane.minimized {
            return;
        }
        let pane_id = pane.id;
        let (cols, rows) = {
            let term = pane.terminal.read();
            (term.cols, term.rows)
        };
        {
            let mut term = pane.terminal.write();
            term.soft_reset();
            term.reset_rows_touched();
        }
        let nudged = if rows > 1 { rows - 1 } else { rows + 1 };
        pane.pty.resize(cols, nudged);
        // ~50ms @60fps before restoring the real winsize, so the foreground
        // program sees two distinct SIGWINCHs with two distinct sizes.
        {
            // One slot per pane: a second Cmd+R re-arms its own pane's restore
            // without dropping another pane's pending restore (a dropped
            // restore would leave that PTY one row short permanently).
            let mut restores = self.ivars().pty_restore.borrow_mut();
            restores.retain(|r| r.pane_id != pane_id);
            restores.push(PtyRestore { pane_id, remaining_frames: 3 });
        }
        self.set_pane_flash(pane_id, 20, None);
    }

    fn focused_pane(&self) -> Option<&Pane> {
        let tabs = self.ivars().tabs.borrow();
        let idx = self.ivars().active_tab.get();
        let tab = tabs.get(idx)?;
        let pane = tab.pane(tab.focused_pane)?;
        // SAFETY: The Tab lives in RefCell inside ivars, pinned in ObjC heap.
        // Mutations (pane add/remove, IPC commands) happen only in the render timer tick,
        // never while an event handler holds this ref.
        Some(unsafe { &*(pane as *const Pane) })
    }


    /// Update mouse cursor when hovering over a separator (±3px tolerance).
    fn update_separator_cursor(&self, event: &NSEvent) {
        let (px, py) = self.event_to_pixel(event);
        let tabs = self.ivars().tabs.borrow();
        let idx = self.ivars().active_tab.get();
        let tab = match tabs.get(idx) {
            Some(t) => t,
            None => return,
        };
        // Only check if we have splits
        if tab.columns.len() < 2 && tab.columns.first().map_or(true, |c| c.panes.len() == 1) {
            return;
        }
        let vp = self.panes_viewport_for_tab(tab);
        let mut seps = Vec::new();
        tab.collect_separator_info(vp, &mut seps);
        drop(tabs);

        let scale = self.backing_scale();
        let tolerance = 3.0 * scale;

        for sep in &seps {
            if sep.is_column_sep {
                if (px - sep.pos).abs() < tolerance && py >= sep.cross_start && py <= sep.cross_end {
                    #[allow(deprecated)]
                    NSCursor::resizeLeftRightCursor().set();
                    return;
                }
            } else {
                if (py - sep.pos).abs() < tolerance && px >= sep.cross_start && px <= sep.cross_end {
                    #[allow(deprecated)]
                    NSCursor::resizeUpDownCursor().set();
                    return;
                }
            }
        }
        // Not hovering any separator — reset to arrow
        NSCursor::arrowCursor().set();
    }

    /// Update hovered URL state based on mouse position.
    fn update_hovered_url(&self, event: &NSEvent) {
        let modifiers = event.modifierFlags();
        let cmd = modifiers.contains(NSEventModifierFlags::Command);
        self.ivars().cmd_held.set(cmd);

        if !cmd {
            let had_hover = self.ivars().hovered_url.borrow().is_some();
            if had_hover {
                *self.ivars().hovered_url.borrow_mut() = None;
                NSCursor::arrowCursor().set();
                self.mark_dirty();
            }
            return;
        }

        let (px, py) = self.event_to_pixel(event);
        let tabs = self.ivars().tabs.borrow();
        let idx = self.ivars().active_tab.get();
        let tab = match tabs.get(idx) {
            Some(t) => t,
            None => return,
        };
        let panes_vp = self.panes_viewport_for_tab(tab);
        // Viewport is already in screen space (x: -scroll_offset_x), so use px directly
        let hit = tab.hit_test(px, py, panes_vp);
        let (pane, vp) = match hit {
            Some((p, v)) => (unsafe { &*(p as *const Pane) }, v),
            None => {
                let had_hover = self.ivars().hovered_url.borrow().is_some();
                if had_hover {
                    *self.ivars().hovered_url.borrow_mut() = None;
                    NSCursor::arrowCursor().set();
                    self.mark_dirty();
                }
                return;
            }
        };
        drop(tabs);

        if let Some((visible_row, col)) = self.pixel_to_visible_row_col(px, py, pane, &vp) {
            let term = pane.terminal.read();
            if let Some((segments, url)) = term.url_at(visible_row, col) {
                let old = self.ivars().hovered_url.borrow().clone();
                let changed = old.as_ref().map_or(true, |o| o.1 != segments);
                if changed {
                    *self.ivars().hovered_url.borrow_mut() = Some((pane.id, segments, url));
                    NSCursor::pointingHandCursor().set();
                    self.mark_dirty();
                }
                return;
            }
        }

        let had_hover = self.ivars().hovered_url.borrow().is_some();
        if had_hover {
            *self.ivars().hovered_url.borrow_mut() = None;
            NSCursor::arrowCursor().set();
            self.mark_dirty();
        }
    }

    fn update_tooltip(&self, event: &NSEvent) {
        let renderer = match self.ivars().renderer.get() {
            Some(r) => r,
            None => return,
        };
        let (px, py) = self.event_to_pixel(event);
        let new_tooltip = renderer.read().hit_test_tooltip(px, py);
        let mut r = renderer.write();
        if r.active_tooltip != new_tooltip {
            r.active_tooltip = new_tooltip;
            drop(r);
            self.mark_dirty();
        }
    }
























    /// Encode modifier flags for SGR mouse reporting.
    fn mouse_modifiers(event: &NSEvent) -> u8 {
        let flags = event.modifierFlags();
        let mut m: u8 = 0;
        if flags.contains(NSEventModifierFlags::Shift) { m |= 4; }
        if flags.contains(NSEventModifierFlags::Option) { m |= 8; }
        if flags.contains(NSEventModifierFlags::Control) { m |= 16; }
        m
    }

    /// Send an SGR mouse event to the PTY. `button_code` is the base button (0=left, 1=middle, 2=right, 64/65=scroll).
    /// `press` = true for press/motion ('M'), false for release ('m').
    /// `motion` = true adds +32 to the button code for motion events.
    fn send_sgr_mouse(&self, pane: &Pane, button_code: u8, col: u16, row: u16, press: bool, motion: bool, event: &NSEvent) {
        let mods = Self::mouse_modifiers(event);
        let cb = button_code | mods | if motion { 32 } else { 0 };
        let suffix = if press { 'M' } else { 'm' };
        let seq = format!("\x1b[<{};{};{}{}", cb, col, row, suffix);
        pane.pty.write(seq.as_bytes());
    }





















    /// Set the highlight pulse on a pane (used by jump_to_search_hit).
    fn set_pane_flash(&self, pane_id: PaneId, frames: u32, label: Option<PaneFlashLabel>) {
        *self.ivars().pane_flash.borrow_mut() =
            Some(PaneFlash { pane_id, remaining_frames: frames, label });
        self.mark_dirty();
    }

    /// The working directory of a pane held by this window, whichever tab it
    /// lives in.
    fn pane_cwd(&self, pane_id: PaneId) -> Option<String> {
        let tabs = self.ivars().tabs.borrow();
        let tab = tabs.iter().find(|t| t.contains(pane_id))?;
        let cwd = tab.pane(pane_id)?.terminal.read().cwd.clone();
        cwd
    }

    /// Show a transient status-bar message for ~2 seconds. Used to explain why an
    /// action did nothing (e.g. Break Pane on a tab that has a single pane).
    fn set_transient_status(&self, msg: &str) {
        let fps = self.ivars().config.get().map(|c| c.terminal.fps).unwrap_or(60) as u32;
        *self.ivars().transient_status.borrow_mut() = Some((msg.to_string(), fps * 2));
        self.mark_dirty();
    }


    /// Activate the tab containing `tab_id`. Returns true if found.
    fn activate_tab(&self, tab_id: TabId) -> bool {
        let tabs = self.ivars().tabs.borrow();
        for (idx, tab) in tabs.iter().enumerate() {
            if tab.id == tab_id {
                drop(tabs);
                self.ivars().active_tab.set(idx);
                // Lazy resize: panes of an inactive tab may carry stale grid
                // dimensions from a smaller window. Without this, the gravity
                // offset centers content in the (larger) viewport. Match the
                // other tab-switch paths which all resize after activation.
                self.resize_all_panes();
                self.mark_dirty();
                return true;
            }
        }
        false
    }

    /// Focus a specific pane within the active tab. Returns true if found.
    fn focus_pane_in_active_tab(&self, pane_id: PaneId) -> bool {
        let mut tabs = self.ivars().tabs.borrow_mut();
        let idx = self.ivars().active_tab.get();
        if let Some(tab) = tabs.get_mut(idx) {
            // Walk panes to confirm the id exists in this tab before assigning.
            let mut found = false;
            tab.for_each_pane(&mut |p| {
                if p.id == pane_id { found = true; }
            });
            if found {
                tab.focused_pane = pane_id;
                // Scroll the virtual viewport so the pane is on-screen if it
                // sits outside the visible horizontal span (e.g. jumped to from
                // global search).
                let screen_w = self.drawable_viewport().width;
                let min_w = self.min_split_width_px();
                tab.clamp_scroll(screen_w, min_w);
                self.scroll_to_reveal_pane(tab, pane_id, screen_w);
                drop(tabs);
                self.mark_dirty();
                return true;
            }
        }
        false
    }
}














impl KovaView {








    /// Close the active window (all its tabs). The timer will detect
    /// the empty tab list and remove the window. App terminates when
    /// the last window is closed (via `applicationShouldTerminateAfterLastWindowClosed`).
    fn do_close_window(&self) {
        // Check for running processes and confirm
        let procs = self.running_processes();
        let mtm = unsafe { MainThreadMarker::new_unchecked() };
        if !confirm_running_processes(mtm, &procs, "Close this window?", "Close") {
            return;
        }
        // Save all tabs to recent projects before closing (single I/O cycle)
        {
            let tabs = self.ivars().tabs.borrow();
            crate::recent_projects::add_batch(&tabs);
        }
        // Signal closing — tick() will return false and the timer will close the window
        self.ivars().closing.set(true);
    }

    /// Kill the active window immediately without saving its session.
    fn do_kill_window(&self) {
        self.ivars().skip_session_save.set(true);
        self.ivars().closing.set(true);
    }

    /// Whether this window should be excluded from session save.
    pub fn skip_session_save(&self) -> bool {
        self.ivars().skip_session_save.get()
    }

    /// Execute a window-level [`Action`] against this view. Shared by the
    /// keyboard path (`performKeyEquivalent`) and the IPC `dispatch-action`
    /// command so both go through a single implementation. Returns `true` when
    /// the action was consumed, `false` for a no-op the keyboard caller may want
    /// to propagate up the responder chain (currently only Copy with an empty
    /// selection).
    pub fn dispatch_action(&self, action: &Action) -> bool {
        match action {
            Action::ToggleHelp => {
                self.ivars().show_help.set(true);
                self.mark_dirty();
            }
            Action::ToggleFilter => self.toggle_filter(),
            Action::MemReport => {
                let showing = self.ivars().show_mem_report.get();
                if showing {
                    self.ivars().show_mem_report.set(false);
                    self.mark_dirty();
                } else {
                    self.show_mem_report_overlay();
                }
            }
            Action::NewWindow => {
                let mtm = unsafe { MainThreadMarker::new_unchecked() };
                crate::app::create_new_window(mtm);
            }
            Action::NewTab => self.do_new_tab(),
            Action::VSplit => self.do_split(SplitDirection::Horizontal),
            Action::HSplit => self.do_split(SplitDirection::Vertical),
            Action::VSplitRoot => self.do_split_root(SplitDirection::Horizontal),
            Action::HSplitRoot => self.do_split_root(SplitDirection::Vertical),
            Action::CloseWindow => self.do_close_window(),
            Action::KillWindow => self.do_kill_window(),
            Action::ClosePaneOrTab => self.do_close_pane_or_tab(),
            Action::CloseTab => self.do_close_tab(),
            Action::OpenRecentProject => self.do_open_recent_projects(),
            Action::OpenSearchPalette => self.do_open_search_palette(),
            Action::OpenPaneSwitcher => self.open_pane_switcher(false),
            Action::OpenUnreadSwitcher => self.open_pane_switcher(true),
            Action::Equalize => {
                let mut tabs = self.ivars().tabs.borrow_mut();
                let idx = self.ivars().active_tab.get();
                if let Some(tab) = tabs.get_mut(idx) {
                    tab.equalize();
                    drop(tabs);
                    self.resize_all_panes();
                }
            }
            Action::RepaintPane => self.do_repaint_pane(),
            Action::NextAttention => self.do_focus_next_attention(),
            Action::HistoryBack => do_history_step(false),
            Action::HistoryForward => do_history_step(true),
            Action::PrevTab => self.do_switch_tab_relative(-1),
            Action::NextTab => self.do_switch_tab_relative(1),
            Action::RenameTab => self.start_rename_tab(),
            Action::RenamePane => self.start_rename_pane(),
            Action::DetachTab => self.do_detach_tab(),
            Action::BreakPane => self.do_break_pane(),
            Action::MergeTab => self.do_merge_tab(),
            Action::MergeWindow => self.do_merge_window(),

            Action::SwitchTab(idx) => self.do_switch_tab(*idx),
            Action::MinimizePane => self.do_minimize_pane(),
            Action::RestoreLastMinimized => self.do_restore_last_minimized(),
            Action::Navigate(dir) => self.do_navigate(*dir),
            Action::SwapPane(dir) => self.do_swap_pane(*dir),
            Action::ReparentPane(dir) => self.do_reparent_pane(*dir),
            Action::Resize(axis, delta) => {
                // Mode 1: ratio resize — move nearest separator, virtual width unchanged
                let mut tabs = self.ivars().tabs.borrow_mut();
                let idx = self.ivars().active_tab.get();
                if let Some(tab) = tabs.get_mut(idx) {
                    let focused_id = tab.focused_pane;
                    if tab.adjust_ratio_directional(focused_id, *delta, *axis)
                        || tab.adjust_ratio_nearest(focused_id, *delta, *axis) {
                        let full = self.drawable_viewport();
                        let min_w = self.min_split_width_px();
                        self.cap_virtual_width(tab, full.width, min_w);
                        tab.clamp_scroll(full.width, min_w);
                        self.scroll_to_reveal_pane(tab, focused_id, full.width);
                        self.set_resize_feedback("Ratio", tab, full.width, min_w);
                        drop(tabs);
                        self.resize_all_panes();
                    }
                }
            }
            Action::EdgeGrow(delta) => {
                // Mode 3: edge grow — only focused pane changes size, virtual width adjusts
                let mut tabs = self.ivars().tabs.borrow_mut();
                let idx = self.ivars().active_tab.get();
                if let Some(tab) = tabs.get_mut(idx) {
                    let focused_id = tab.focused_pane;
                    let full = self.drawable_viewport();
                    let min_w = self.min_split_width_px();
                    let screen_w = full.width;
                    // Don't grow if focused pane is already at screen width
                    let pane_vp = tab.viewport_for_pane(focused_id, self.panes_viewport_for_tab(tab));
                    let pane_w = pane_vp.map(|vp| vp.width).unwrap_or(0.0);
                    let blocked = *delta > 0.0 && pane_w >= screen_w - 1.0;
                    let old_vw = tab.virtual_width(screen_w, min_w);
                    let step = (0.05 * screen_w).max(20.0);
                    let new_vw = if *delta > 0.0 {
                        old_vw + step
                    } else {
                        (old_vw - step).max(screen_w)
                    };
                    if !blocked && (new_vw - old_vw).abs() > 0.5 {
                        tab.scale_ratios_for_edge_grow(focused_id, old_vw, new_vw);
                        tab.virtual_width_override = if new_vw > screen_w { new_vw } else { 0.0 };
                        self.enforce_max_pane_width(tab, screen_w, min_w);
                        tab.clamp_scroll(screen_w, min_w);
                        self.scroll_to_reveal_pane(tab, focused_id, screen_w);
                        self.set_resize_feedback("Right Edge", tab, screen_w, min_w);
                        drop(tabs);
                        self.resize_all_panes();
                    }
                }
            }
            Action::Copy | Action::CopyRaw => {
                let raw = matches!(action, Action::CopyRaw);
                // If filter is active, copy all filtered lines
                let filter = self.ivars().filter.borrow();
                if let Some(state) = filter.as_ref() {
                    if !state.matches.is_empty() {
                        let mut text = String::new();
                        for (i, m) in state.matches.iter().enumerate() {
                            if i > 0 { text.push('\n'); }
                            text.push_str(&m.text);
                        }
                        drop(filter);
                        copy_to_pasteboard(&text);
                        // Close filter after copying — through close_filter, so
                        // the query joins the recall list like any other exit.
                        self.close_filter();
                        self.mark_dirty();
                    } else {
                        drop(filter);
                    }
                } else {
                    drop(filter);
                    if let Some(pane) = self.focused_pane() {
                        let text = if raw {
                            pane.terminal.read().selected_text()
                        } else {
                            pane.terminal.read().selected_text_joined()
                        };
                        if !text.is_empty() {
                            copy_to_pasteboard(&text);
                            pane.terminal.write().clear_selection();
                        } else {
                            return false;
                        }
                    }
                }
            }
            Action::Paste => {
                if let Some(pane) = self.focused_pane() {
                    let pasteboard = NSPasteboard::generalPasteboard();
                    let pasted_image = unsafe { pasteboard.dataForType(objc2_app_kit::NSPasteboardTypePNG) }
                        .and_then(|data| {
                            if data.is_empty() { return None; }
                            let bytes = data.to_vec();
                            let timestamp = std::time::SystemTime::now()
                                .duration_since(std::time::UNIX_EPOCH)
                                .unwrap_or_default()
                                .as_millis();
                            let path = format!("/tmp/kova-paste-{timestamp}.png");
                            std::fs::write(&path, bytes).ok().map(|_| path)
                        });

                    if let Some(path) = pasted_image {
                        let bracketed = pane.terminal.read().bracketed_paste;
                        if bracketed { pane.pty.write(b"\x1b[200~"); }
                        pane.pty.write(path.as_bytes());
                        if bracketed { pane.pty.write(b"\x1b[201~"); }
                    } else if let Some(text) = unsafe { pasteboard.stringForType(objc2_app_kit::NSPasteboardTypeString) } {
                        let mut text = text.to_string();
                        let bracketed = pane.terminal.read().bracketed_paste;
                        if bracketed {
                            // A paste containing the bracketed-paste
                            // terminator would break out of the paste
                            // and inject keystrokes into the app.
                            // Loop to a fixpoint: a single replace can
                            // re-form the terminator from its halves.
                            while text.contains("\x1b[201~") {
                                text = text.replace("\x1b[201~", "");
                            }
                            pane.pty.write(b"\x1b[200~");
                        }
                        pane.pty.write(text.as_bytes());
                        if bracketed { pane.pty.write(b"\x1b[201~"); }
                    }
                }
            }
        }
        true
    }












    // ---------------------------------------------------------------
    // IPC methods (called from app.rs IPC command handlers)
    // ---------------------------------------------------------------
























    /// Minimize the focused pane.
    fn do_minimize_pane(&self) {
        let mut tabs = self.ivars().tabs.borrow_mut();
        let idx = self.ivars().active_tab.get();
        if let Some(tab) = tabs.get_mut(idx) {
            let focused_id = tab.focused_pane;
            let full = self.drawable_viewport();
            let min_w = self.min_split_width_px();
            if tab.minimize_pane_adjust_virtual(focused_id, full.width, min_w) {
                tab.mark_all_dirty();
                tab.clamp_scroll(full.width, min_w);
                let new_focus = tab.focused_pane;
                self.scroll_to_reveal_pane(tab, new_focus, full.width);
                drop(tabs);
                self.resize_all_panes();
            }
        }
    }

    /// Restore the last minimized pane (FILO).
    fn do_restore_last_minimized(&self) {
        let mut tabs = self.ivars().tabs.borrow_mut();
        let idx = self.ivars().active_tab.get();
        if let Some(tab) = tabs.get_mut(idx) {
            let full = self.drawable_viewport();
            let min_w = self.min_split_width_px();
            if tab.restore_last_minimized(full.width, min_w) {
                tab.mark_all_dirty();
                tab.clamp_scroll(full.width, min_w);
                let focused = tab.focused_pane;
                self.scroll_to_reveal_pane(tab, focused, full.width);
                drop(tabs);
                self.resize_all_panes();
            }
        }
    }


    /// Navigate focus to an adjacent pane.
    fn do_navigate(&self, dir: NavDirection) {
        let mut tabs = self.ivars().tabs.borrow_mut();
        let idx = self.ivars().active_tab.get();
        let tab = match tabs.get_mut(idx) {
            Some(t) => t,
            None => return,
        };
        let focused_id = tab.focused_pane;
        let panes_vp = self.panes_viewport_for_tab(tab);
        if let Some(neighbor_id) = tab.neighbor(focused_id, dir, panes_vp) {
            tab.focused_pane = neighbor_id;
            if let Some(old) = tab.pane(focused_id) {
                old.terminal.read().dirty.store(true, std::sync::atomic::Ordering::Relaxed);
            }
            if let Some(new) = tab.pane(neighbor_id) {
                // Acknowledge completion and bell on the newly focused pane
                let t = new.terminal.read();
                t.ack_completion();
                t.bell.store(false, std::sync::atomic::Ordering::Relaxed);
                t.dirty.store(true, std::sync::atomic::Ordering::Relaxed);
            }
            // Auto-scroll to reveal the newly focused pane
            self.scroll_to_reveal_pane(tab, neighbor_id, self.drawable_viewport().width);
        } else {
            // No neighbor in this direction → tab boundary guard
            let count = tabs.len();
            if count <= 1 {
                return;
            }
            drop(tabs);

            // Check if we recently hit the same boundary (double-press to cross)
            let now = std::time::Instant::now();
            if let Some(prev) = self.ivars().boundary_hit.get() {
                if prev.direction == dir && now.duration_since(prev.time).as_millis() < 500 {
                    // Second press within timeout → cross the boundary
                    self.ivars().boundary_hit.set(None);
                    self.ivars().boundary_flash.set(None);
                    let delta: i32 = match dir {
                        NavDirection::Left | NavDirection::Up => -1,
                        NavDirection::Right | NavDirection::Down => 1,
                    };
                    self.do_switch_tab_relative(delta);
                    let mut tabs = self.ivars().tabs.borrow_mut();
                    let new_idx = self.ivars().active_tab.get();
                    if let Some(new_tab) = tabs.get_mut(new_idx) {
                        let target_id = match dir {
                            NavDirection::Right | NavDirection::Down => new_tab.first_pane().id,
                            NavDirection::Left | NavDirection::Up => new_tab.last_pane().id,
                        };
                        new_tab.focused_pane = target_id;
                        self.scroll_to_reveal_pane(new_tab, target_id, self.drawable_viewport().width);
                    }
                    return;
                }
            }

            // First press → record hit and flash the boundary edge
            self.ivars().boundary_hit.set(Some(BoundaryHit { time: now, direction: dir }));
            let flash_edge = match dir {
                NavDirection::Right | NavDirection::Down => NavDirection::Right,
                NavDirection::Left | NavDirection::Up => NavDirection::Left,
            };
            let fps = self.ivars().config.get().map(|c| c.terminal.fps).unwrap_or(60) as u32;
            let flash_frames = fps / 4; // ~250ms
            self.ivars().boundary_flash.set(Some(BoundaryFlash {
                edge: flash_edge,
                remaining_frames: flash_frames,
            }));
            // Mark focused pane dirty so the next tick renders the flash immediately
            let tabs = self.ivars().tabs.borrow();
            let idx = self.ivars().active_tab.get();
            if let Some(tab) = tabs.get(idx) {
                if let Some(pane) = tab.pane(tab.focused_pane) {
                    pane.terminal.read().dirty.store(true, std::sync::atomic::Ordering::Relaxed);
                }
            }
        }
    }

    /// Swap the focused pane with its neighbor in the given direction.
    fn do_swap_pane(&self, dir: NavDirection) {
        let mut tabs = self.ivars().tabs.borrow_mut();
        let idx = self.ivars().active_tab.get();
        let tab = match tabs.get_mut(idx) {
            Some(t) => t,
            None => return,
        };
        let focused_id = tab.focused_pane;
        let vp = self.panes_viewport_for_tab(tab);
        if let Some(neighbor_id) = tab.neighbor(focused_id, dir, vp) {
            if tab.swap_panes(focused_id, neighbor_id, dir) {
                // Mark both panes dirty so they redraw in their new positions
                if let Some(p) = tab.pane(focused_id) {
                    p.terminal.read().dirty.store(true, std::sync::atomic::Ordering::Relaxed);
                }
                if let Some(p) = tab.pane(neighbor_id) {
                    p.terminal.read().dirty.store(true, std::sync::atomic::Ordering::Relaxed);
                }
                // Auto-scroll to reveal the focused pane in its new position
                self.scroll_to_reveal_pane(tab, focused_id, self.drawable_viewport().width);
                drop(tabs);
                self.resize_all_panes();
            }
        }
    }

    /// Reparent the focused pane: rotate split orientation or swap (2-leaf case only).
    fn do_reparent_pane(&self, dir: NavDirection) {
        let mut tabs = self.ivars().tabs.borrow_mut();
        let idx = self.ivars().active_tab.get();
        let tab = match tabs.get_mut(idx) {
            Some(t) => t,
            None => return,
        };
        let focused_id = tab.focused_pane;
        if tab.reparent_pane(focused_id, dir) {
            drop(tabs);
            self.resize_all_panes();
        }
    }





    fn mark_dirty(&self) {
        if let Some(pane) = self.focused_pane() {
            pane.terminal.read().dirty.store(true, std::sync::atomic::Ordering::Relaxed);
        }
    }











    /// Returns (tab_title, process_name) for each pane with a running foreground process.
    pub fn running_processes(&self) -> Vec<(String, String)> {
        let tabs = self.ivars().tabs.borrow();
        let mut result = Vec::new();
        for tab in tabs.iter() {
            let title = tab.title();
            tab.for_each_pane(&mut |pane| {
                if let Some(name) = pane.foreground_process_name() {
                    result.push((title.clone(), name));
                }
            });
        }
        result
    }

    /// Append this window's session data to the given Vec.
    /// Called by AppDelegate to collect all windows before saving.
    /// Tabs that are still placeholders (or whose deferred restore failed) are
    /// serialized from `tab_backup` so the user's original data is preserved
    /// across autosave cycles.
    pub fn append_session_data(&self, out: &mut Vec<crate::session::WindowSession>) {
        let tabs = self.ivars().tabs.borrow();
        let active_tab = self.ivars().active_tab.get();
        let frame = self.window().map(|win| {
            let f = win.frame();
            (f.origin.x, f.origin.y, f.size.width, f.size.height)
        });
        let backup = self.ivars().tab_backup.borrow();
        let saved_tabs: Vec<crate::session::SavedTab> = tabs.iter().map(|t| {
            backup.get(&t.id).cloned().unwrap_or_else(|| crate::session::snapshot_tab(t))
        }).collect();
        out.push(crate::session::WindowSession {
            tabs: saved_tabs,
            active_tab,
            frame,
        });
    }



}

/// Global counter for unique window autosave names.
static WINDOW_COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// Show a confirmation alert listing running processes.
/// Returns `true` if the user confirmed (or no processes are running).
pub fn confirm_running_processes(mtm: MainThreadMarker, procs: &[(String, String)], message: &str, confirm_button: &str) -> bool {
    if procs.is_empty() {
        return true;
    }
    let alert = NSAlert::new(mtm);
    alert.setAlertStyle(NSAlertStyle::Warning);
    alert.setMessageText(&NSString::from_str(message));
    let mut lines = String::from("The following processes are running:");
    for (tab, name) in procs {
        lines.push_str(&format!("\n\u{2022} Tab \u{ab}{}\u{bb}: {}", tab, name));
    }
    alert.setInformativeText(&NSString::from_str(&lines));
    alert.addButtonWithTitle(&NSString::from_str(confirm_button));
    alert.addButtonWithTitle(&NSString::from_str("Cancel"));
    alert.runModal() == 1000 // NSAlertFirstButtonReturn
}

/// Create a new Kova window with the given tabs.
pub fn create_window(mtm: MainThreadMarker, config: &Config, tabs: Vec<Tab>, active_tab: usize, deferred_tabs: Vec<(usize, crate::session::SavedTab)>) -> Retained<NSWindow> {
    let content_rect = CGRect {
        origin: CGPoint { x: config.window.x, y: config.window.y },
        size: CGSize {
            width: config.window.width,
            height: config.window.height,
        },
    };

    let style = NSWindowStyleMask::Titled
        | NSWindowStyleMask::Closable
        | NSWindowStyleMask::Miniaturizable
        | NSWindowStyleMask::Resizable
        | NSWindowStyleMask::FullSizeContentView;

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
    window.setTitlebarAppearsTransparent(true);
    window.setTitleVisibility(NSWindowTitleVisibility::Hidden);
    window.setMinSize(CGSize {
        width: 200.0,
        height: 150.0,
    });

    // Unique autosave name per window so NSUserDefaults doesn't collide
    let win_id = WINDOW_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let autosave = format!("KovaWindow-{}", win_id);
    window.setFrameAutosaveName(&NSString::from_str(&autosave));

    let view = KovaView::new(mtm, content_rect);
    view.setup_metal(mtm, config, tabs, active_tab);
    if !deferred_tabs.is_empty() {
        // Re-key deferred entries from saved index to the placeholder's TabId:
        // indices go stale as soon as the user touches the tab strip, ids don't.
        let deferred_by_id: Vec<(TabId, crate::session::SavedTab)> = {
            let tabs_ref = view.ivars().tabs.borrow();
            deferred_tabs
                .into_iter()
                .filter_map(|(tab_idx, saved)| tabs_ref.get(tab_idx).map(|tab| (tab.id, saved)))
                .collect()
        };
        // Compute fixed total pane count: active tab's live panes + all deferred panes
        let mut total: u32 = 0;
        {
            let tabs_ref = view.ivars().tabs.borrow();
            for tab in tabs_ref.iter() {
                tab.for_each_pane(&mut |pane| {
                    if pane.pty.is_live() { total += 1; }
                });
            }
        }
        for (_, saved) in &deferred_by_id {
            total += crate::session::count_panes_in_saved_tab(saved) as u32;
        }
        view.ivars().loading_total_panes.set(total);
        // Populate tab_backup so periodic autosave preserves the original SavedTab
        // for any placeholder still waiting (or that fails to restore).
        {
            let mut backup = view.ivars().tab_backup.borrow_mut();
            for (tab_id, saved) in &deferred_by_id {
                backup.insert(*tab_id, saved.clone());
            }
        }
        *view.ivars().deferred_tabs.borrow_mut() = deferred_by_id;
    }
    window.setContentView(Some(&view));
    window.setDelegate(Some(objc2::runtime::ProtocolObject::from_ref(&*view)));
    window.makeFirstResponder(Some(&view));
    // NOT setAcceptsMouseMovedEvents(true): the view already installs an
    // NSTrackingArea with MouseMoved (see updateTrackingAreas). Both paths
    // together deliver mouseMoved: twice per physical move, so mode-1003 apps
    // receive every motion report twice. Measured against Terminal.app on the
    // same gesture: 37% duplicate reports vs 0%. The tracking area is the more
    // precise source (view bounds, key window only), so it is the one we keep.

    window
}


/// Minimum height (rows) of an interior blank band before it is treated as a
/// hole worth repairing. Real holes span dozens of rows; small gaps are
/// normal UI spacing.
const BAND_MIN_ROWS: usize = 8;
/// Row-coverage fraction above which the settle nudge is skipped: the app
/// demonstrably repainted (almost) the whole screen since the last winsize
/// change, so nudging again is pure risk — the winsize flap it causes can
/// make Claude Code emit a clear-screen followed by a partial repaint (the
/// hole, see notes/display-glitches.md round 5).
const SETTLE_SKIP_COVERAGE: f32 = 0.8;
/// Frames (~0.5s at 60fps) after a winsize restore before scanning the grid
/// for a leftover hole.
const POST_RESTORE_CHECK_FRAMES: u32 = 30;
/// Max automatic band-repair repaints per pane between real resizes, so a
/// program that keeps answering nudges with broken frames can't loop us.
const MAX_BAND_REPAIRS: u32 = 2;



#[cfg(test)]
mod tests {
    use super::*;









    #[test]
    fn flash_label_splits_a_cwd_into_directory_and_path_above() {
        let home = "/Users/mick";
        assert_eq!(
            flash_label_parts("/Users/mick/projects/perso/kova", home),
            ("kova".to_string(), "~/projects/perso".to_string())
        );
        // Home itself, and the root, are their own name with nothing above.
        assert_eq!(flash_label_parts(home, home), ("~".to_string(), String::new()));
        assert_eq!(flash_label_parts("/", home), ("/".to_string(), String::new()));
        // A top-level directory keeps the root as its path line.
        assert_eq!(flash_label_parts("/tmp", home), ("tmp".to_string(), "/".to_string()));
        // A trailing slash must not produce an empty name.
        assert_eq!(
            flash_label_parts("/Users/mick/dev/", home),
            ("dev".to_string(), "~".to_string())
        );
        // A path that merely starts with the same bytes as $HOME is not under it.
        assert_eq!(
            flash_label_parts("/Users/mickey/dev", home),
            ("dev".to_string(), "/Users/mickey".to_string())
        );
    }































}
