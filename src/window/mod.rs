mod attention;
use attention::{do_history_step, pane_history_state};
mod duplicates;
mod geometry;
mod ipc_handlers;
pub(crate) mod overlays;
use overlays::{FilterState, RenamePaneState, RenameTabState};
pub use ipc_handlers::{IpcMergeTabResult, IpcSwapPaneResult, SubjectError};
mod recent_projects_overlay;
use recent_projects_overlay::RecentProjectsState;
mod resize;
mod search_palette;
use search_palette::{is_shortcut, is_typed_char, typed_text, SearchPaletteState, SearchRow};
mod switcher;
mod tabs;
mod tick;
use tabs::{MergeTabState, SendToWindowState};
use switcher::{PaneSwitcherState, SwitcherRow};

use objc2::rc::Retained;
use objc2::{define_class, msg_send, DefinedClass, MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{NSAlert, NSAlertStyle, NSApplication, NSBackingStoreType, NSCursor, NSEvent, NSEventModifierFlags, NSPasteboard, NSTextInputClient, NSTrackingArea, NSTrackingAreaOptions, NSWindow, NSWindowButton, NSWindowDelegate, NSWindowStyleMask, NSWindowTitleVisibility};
use objc2_core_foundation::{CGPoint, CGRect, CGSize};
use objc2_foundation::{NSObjectProtocol, NSString};
use objc2_metal::MTLCreateSystemDefaultDevice;
use objc2_quartz_core::CAMetalLayer;
use std::cell::{Cell, OnceCell, RefCell};
use std::sync::Arc;

use crate::appkit_input::{self, copy_to_pasteboard, AppKitInput, PaneOrigin, ScrollAxisLock};
use crate::config::{Config, TerminalConfig};
use crate::input::{self, MouseAction, MouseResult};
use crate::keybindings::{Action, Keybindings, KeyCombo};
use crate::pane::{alloc_tab_id, NavDirection, Pane, PaneId, SplitDirection, Tab, TabId};
use crate::renderer::{PaneViewport, Renderer};
use crate::resize_repair::ResizeRepair;
use crate::terminal::pty::ProcessInfo;
use crate::terminal::FilterMatch;

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
    /// A `resize_all_panes` is due at the start of the next frame
    /// (`request_resize_all_panes`).
    resize_pending: Cell<bool>,
    /// The active tab drew a subject strip last frame. When that flips, the
    /// panes gain or lose a row and must be resized.
    subject_strip_shown: Cell<bool>,
    /// (active tab, its scroll offset) when the last `resize_all_panes` left
    /// off-screen panes at their old size; a different value means they may
    /// be on screen now (`flush_pending_resize`).
    offscreen_resize_stale: Cell<Option<(usize, f32)>>,
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
    /// Subject (its tab, its name) targeted by the strip's right-click color menu.
    color_menu_subject: RefCell<Option<(TabId, String)>>,
    drag_tab: Cell<Option<DragTabState>>,
    /// URL currently hovered (pane_id, per-row segments [(row, col_start, col_end)], url) — set by mouseMoved when Cmd held
    hovered_url: RefCell<Option<(PaneId, Vec<(usize, u16, u16)>, String)>>,
    /// Whether Cmd key is currently held (for URL hover detection)
    cmd_held: Cell<bool>,
    /// The pointer position and content the URL hover was computed against:
    /// `tick` re-runs the hover when output or a scroll moved the text under a
    /// still pointer (`refresh_stale_url_hover`).
    url_hover_tracker: RefCell<crate::input::HoverTracker>,
    /// Terminal input from AppKit events, for the focused pane (or the pane
    /// under the pointer): composition, drag auto-scroll, scroll axis lock.
    input: AppKitInput,
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
    /// "Send Tab to Window" overlay state.
    send_to_window: RefCell<Option<SendToWindowState>>,
    /// "Merge Tab" overlay state.
    merge_tab: RefCell<Option<MergeTabState>>,
    /// Resize feedback: (mode_name, screen_w, virtual_w, remaining_frames).
    resize_feedback: Cell<Option<ResizeFeedback>>,
    /// Transient status-bar message (text, remaining frames) — used for one-off
    /// hints like "no-op" feedback when an action can't apply in the current layout.
    transient_status: RefCell<Option<(String, u32)>>,
    /// Keys of the saved bookmarks, so the status bar can tell a bookmarked
    /// pane apart without reading `bookmarks.json` on every frame. Refreshed
    /// when the list changes (Cmd+B) — the file is only written from here.
    bookmark_keys: RefCell<std::collections::HashSet<String>>,
    /// Keys of the saved anchors, read by the same per-frame test as
    /// `bookmark_keys`. Kept apart because the two lists mean different things
    /// and are written by different gestures — an anchor also arrives from
    /// outside, through the `set-anchor` IPC command.
    anchor_keys: RefCell<std::collections::HashSet<String>>,
    /// Two panes found running the same agent conversation, newest first:
    /// (duplicate, original). Raised by the session poll, answered by an alert
    /// at the top of the next tick — the poll runs with the tabs borrowed, and
    /// a modal there would re-enter them.
    duplicate_session: Cell<Option<(PaneId, PaneId)>>,
    /// Duplicates the user chose to keep, by the pane that carries them. Without
    /// it the poll would ask again every half-second, forever.
    accepted_duplicates: RefCell<std::collections::HashSet<PaneId>>,
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
    /// The repairs that follow a resize or a Cmd+R nudge: deferred winsize
    /// restore, round-trip nudge, debounced settle repaint, post-restore hole
    /// check (`crate::resize_repair`, shared with the library's panes).
    resize_repair: RefCell<ResizeRepair>,
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

/// The three lines of a flash label: the directory the pane sits in, the path
/// above it, and the name of the agent conversation it holds (empty when the
/// pane has none).
#[derive(Clone)]
struct PaneFlashLabel {
    name: String,
    parent: String,
    session: String,
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
            unsafe { self.ivars().input.insert_text(string, self.focused_pane().map(|p| &**p)) }
        }

        #[unsafe(method(doCommandBySelector:))]
        unsafe fn do_command_by_selector(&self, _selector: objc2::runtime::Sel) {
            self.ivars().input.do_command_by_selector(|| self.focused_pane().map(|p| &**p), self.ivars().keybindings.get());
        }

        #[unsafe(method(setMarkedText:selectedRange:replacementRange:))]
        unsafe fn set_marked_text_selected_range_replacement_range(
            &self,
            string: &objc2::runtime::AnyObject,
            _selected_range: objc2_foundation::NSRange,
            _replacement_range: objc2_foundation::NSRange,
        ) {
            unsafe { self.ivars().input.set_marked_text(string) }
        }

        #[unsafe(method(unmarkText))]
        fn unmark_text(&self) {
            self.ivars().input.unmark_text();
        }

        #[unsafe(method(hasMarkedText))]
        fn has_marked_text(&self) -> bool {
            self.ivars().input.has_marked_text()
        }

        #[unsafe(method(markedRange))]
        fn marked_range(&self) -> objc2_foundation::NSRange {
            self.ivars().input.marked_range()
        }

        #[unsafe(method(selectedRange))]
        fn selected_range(&self) -> objc2_foundation::NSRange {
            self.ivars().input.selected_range()
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
            self.ivars().input.first_rect_for_character_range(self)
        }

        #[unsafe(method(characterIndexForPoint:))]
        fn character_index_for_point(&self, _point: objc2_core_foundation::CGPoint) -> usize {
            self.ivars().input.character_index_for_point()
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

            // Window keys first, even with no pane under focus
            let key = appkit_input::key_event(event);
            match input::host_key(&key) {
                Some(input::HostKey::ToggleFilter) => {
                    self.toggle_filter();
                    return;
                }
                Some(input::HostKey::AdjustVirtualWidth(dir)) => {
                    self.adjust_virtual_width(dir);
                    return;
                }
                None => {}
            }

            if let (Some(pane), Some(kb)) = (self.focused_pane(), self.ivars().keybindings.get()) {
                self.ivars().input.key_down(self, event, &key, pane, kb);
            }
        }

        #[unsafe(method(performKeyEquivalent:))]
        fn perform_key_equivalent(&self, event: &NSEvent) -> objc2::runtime::Bool {
            let combo = KeyCombo::from_event(event);

            let keybindings = match self.ivars().keybindings.get() {
                Some(kb) => kb,
                None => return objc2::runtime::Bool::NO,
            };

            // Same routing as the three overlays below, and for the same reason:
            // these two pick a target tab and read `active_tab` at Enter, so a
            // shortcut falling through to the global map (Cmd+A, a tab switch)
            // would act on one tab while the overlay names another.
            if self.ivars().send_to_window.borrow().is_some() {
                self.handle_send_to_window_key(event);
                return objc2::runtime::Bool::YES;
            }
            if self.ivars().merge_tab.borrow().is_some() {
                self.handle_merge_tab_key(event);
                return objc2::runtime::Bool::YES;
            }

            // When recent projects overlay is shown, route keys through the overlay handler
            if self.ivars().recent_projects.borrow().is_some() {
                if matches!(keybindings.window_map.get(&combo), Some(Action::Paste)) {
                    let pasteboard = NSPasteboard::generalPasteboard();
                    if let Some(text) = unsafe { pasteboard.stringForType(objc2_app_kit::NSPasteboardTypeString) } {
                        self.paste_into_recent_projects(&text.to_string());
                    }
                } else {
                    self.handle_recent_projects_key(event);
                }
                return objc2::runtime::Bool::YES;
            }

            // When the search palette is open, route keys through its handler so
            // shortcuts like Cmd+V/Cmd+P don't fall through to the global map.
            if self.ivars().search_palette.borrow().is_some() {
                if matches!(keybindings.window_map.get(&combo), Some(Action::Paste)) {
                    let pasteboard = NSPasteboard::generalPasteboard();
                    if let Some(text) = unsafe { pasteboard.stringForType(objc2_app_kit::NSPasteboardTypeString) } {
                        self.paste_into_search_palette(&text.to_string());
                    }
                } else {
                    self.handle_search_palette_key(event);
                }
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
                        self.paste_into_rename(&text.to_string());
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

            if ivars.search_palette.borrow().is_some() {
                self.handle_search_palette_scroll(event, is_trackpad);
                return;
            }

            // Pane switcher overlay handles its own scroll (scroll the column under the cursor).
            if ivars.pane_switcher.borrow().is_some() {
                self.handle_pane_switcher_scroll(event, is_trackpad);
                return;
            }

            let lock = ivars.input.scroll_axis_lock(event);

            // Vertical scroll (pane under cursor)
            if lock != ScrollAxisLock::Horizontal {
                if let Some((pane, vp)) = self.pane_at_event(event) {
                    let sensitivity = ivars.config.get()
                        .map(|c| c.terminal.scroll_sensitivity)
                        .unwrap_or(TerminalConfig::default().scroll_sensitivity);
                    if let Some(cell) = self.cell_size() {
                        ivars.input.scroll(self, event, pane, self.pane_origin(&vp), cell, sensitivity, ivars.active_tab.get());
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

            // The search palette covers the window: a click there never
            // reaches the tab bar or the panes behind it.
            if self.ivars().search_palette.borrow().is_some() {
                self.handle_search_palette_click(py);
                return;
            }

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
                        self.open_pane_switcher(false, switcher::SwitcherView::Panes);
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
                // Reported to the app, or starts a selection
                if let Some(cell) = self.cell_size() {
                    self.ivars().input.mouse(self, event, MouseAction::Press, pane, self.pane_origin(&vp), cell);
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
                        self.request_resize_all_panes();
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
                        self.request_resize_all_panes();
                    }
                }
                return;
            }

            // Drag continues on the focused pane (set by mouseDown): reported
            // to the app, or extends the selection
            if let Some(pane) = self.focused_pane() {
                let vp = {
                    let tabs = self.ivars().tabs.borrow();
                    let idx = self.ivars().active_tab.get();
                    tabs.get(idx).and_then(|t| t.viewport_for_pane(pane.id, self.panes_viewport_for_tab(t)))
                };
                if let (Some(vp), Some(cell)) = (vp, self.cell_size()) {
                    self.ivars().input.mouse(self, event, MouseAction::Drag, pane, self.pane_origin(&vp), cell);
                }
            }
        }

        #[unsafe(method(mouseUp:))]
        fn mouse_up(&self, event: &NSEvent) {
            self.ivars().input.end_drag();
            if self.ivars().drag_tab.get().is_some() {
                self.ivars().drag_tab.set(None);
                return;
            }
            if self.ivars().drag_separator.get().is_some() {
                self.ivars().drag_separator.set(None);
                return;
            }
            // Reported to the app under the pointer, if it asked
            if let Some((pane, vp)) = self.pane_at_event(event) {
                if let Some(cell) = self.cell_size() {
                    let result = self.ivars().input.mouse(self, event, MouseAction::Release, pane, self.pane_origin(&vp), cell);
                    if result == MouseResult::Reported {
                        return;
                    }
                }
            }
            // Otherwise the focused pane's selection ends and is copied
            if let Some(pane) = self.focused_pane() {
                self.ivars().input.finish_selection(pane);
            }
        }

        /// A pick in the subject strip's color menu: a palette color by its
        /// tag, or -1 for "Draw another color".
        #[unsafe(method(subjectColorSelected:))]
        fn subject_color_selected(&self, sender: &objc2_app_kit::NSMenuItem) {
            let tag = sender.tag();
            let Some((tab_id, name)) = self.ivars().color_menu_subject.borrow_mut().take() else { return };
            if let Some(tab) = self.ivars().tabs.borrow_mut().iter_mut().find(|t| t.id == tab_id) {
                if tag >= 0 {
                    tab.set_subject_color(&name, tag as usize);
                } else {
                    tab.redraw_subject_color(&name);
                }
            }
            self.mark_dirty();
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
            log::debug!(
                "rightMouseDown at ({:.0}, {:.0}), tab bar {:.0}px, subject under it: {:?}",
                px,
                py,
                tab_bar_h,
                self.subject_at_strip(px, py).map(|(_, name, _)| name)
            );
            if py <= tab_bar_h {
                if let Some(tab_idx) = self.tab_index_at_x(px) {
                    self.show_tab_color_menu(event, tab_idx);
                    return;
                }
            }
            if let Some((tab_id, name, color)) = self.subject_at_strip(px, py) {
                self.show_subject_color_menu(event, tab_id, name, color);
                return;
            }
            // Default behavior for right-click outside tab bar
            unsafe { msg_send![super(self), rightMouseDown: event] }
        }

        #[unsafe(method(mouseMoved:))]
        fn mouse_moved(&self, event: &NSEvent) {
            // Reported to the app if it tracks all motion (mode 1003)
            if let Some((pane, vp)) = self.pane_at_event(event) {
                if let Some(cell) = self.cell_size() {
                    let result = self.ivars().input.mouse(self, event, MouseAction::Move, pane, self.pane_origin(&vp), cell);
                    if result == MouseResult::Reported {
                        return;
                    }
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

impl KovaView {
    fn new(mtm: MainThreadMarker, frame: CGRect) -> Retained<Self> {
        let this = mtm.alloc::<Self>().set_ivars(KovaViewIvars {
            renderer: OnceCell::new(),
            tabs: RefCell::new(Vec::new()),
            active_tab: Cell::new(0),
            metal_layer: OnceCell::new(),
            last_scale: Cell::new(0.0),
            last_screen_w: Cell::new(0.0),
            resize_pending: Cell::new(false),
            subject_strip_shown: Cell::new(false),
            offscreen_resize_stale: Cell::new(None),
            last_focused: Cell::new(true),
            config: OnceCell::new(),
            drag_separator: Cell::new(None),
            filter: RefCell::new(None),
            rename_tab: RefCell::new(None),
            rename_pane: RefCell::new(None),
            tab_bar_left_inset: Cell::new(0.0),
            color_menu_tab: Cell::new(0),
            color_menu_subject: RefCell::new(None),
            drag_tab: Cell::new(None),
            hovered_url: RefCell::new(None),
            cmd_held: Cell::new(false),
            url_hover_tracker: RefCell::new(crate::input::HoverTracker::default()),
            input: AppKitInput::new(),
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
            resize_feedback: Cell::new(None),
            transient_status: RefCell::new(None),
            bookmark_keys: RefCell::new(crate::bookmarks::keys(&crate::bookmarks::load().items)),
            anchor_keys: RefCell::new(crate::anchors::keys(&crate::anchors::load().items)),
            duplicate_session: Cell::new(None),
            accepted_duplicates: RefCell::new(std::collections::HashSet::new()),
            attention_banner: RefCell::new(None),
            deferred_tabs: RefCell::new(Vec::new()),
            loading_total_panes: Cell::new(0),
            boundary_hit: Cell::new(None),
            boundary_flash: Cell::new(None),
            search_palette: RefCell::new(None),
            pane_switcher: RefCell::new(None),
            pane_flash: RefCell::new(None),
            resize_repair: RefCell::new(ResizeRepair::default()),
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
        pane.pty.resize(cols, crate::resize_repair::nudged_rows(rows));
        // ~50ms @60fps before restoring the real winsize, so the foreground
        // program sees two distinct SIGWINCHs with two distinct sizes. One
        // slot per pane: a second Cmd+R re-arms its own pane's restore only.
        self.ivars().resize_repair.borrow_mut().schedule_restore(pane_id);
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
        let (px, py) = self.event_to_pixel(event);
        self.update_hovered_url_at(cmd, px, py);
        let stamp = if self.ivars().hovered_url.borrow().is_some() { self.url_hover_stamp_at(px, py) } else { None };
        self.ivars().url_hover_tracker.borrow_mut().seen((px, py), stamp);
    }

    /// What the cells under `(px, py)` depend on (`input::HoverStamp`): the
    /// pane there, its content generation, its scroll and its viewport.
    fn url_hover_stamp_at(&self, px: f32, py: f32) -> Option<crate::input::HoverStamp> {
        let tabs = self.ivars().tabs.borrow();
        let tab = tabs.get(self.ivars().active_tab.get())?;
        let (pane, vp) = tab.hit_test(px, py, self.panes_viewport_for_tab(tab))?;
        let term = pane.terminal.read();
        Some(crate::input::HoverStamp::of(&term, [vp.x, vp.y, vp.width, vp.height]))
    }

    /// Once per tick: when output, a scroll or a layout change moved the text
    /// under a still pointer, compute the URL hover again at the last pointer
    /// position, so the underline never stays on text that slid under it.
    /// Free when nothing is hovered or nothing changed.
    pub(super) fn refresh_stale_url_hover(&self) {
        let hovering = self.ivars().hovered_url.borrow().is_some();
        let point = self.ivars().url_hover_tracker.borrow_mut()
            .recheck(hovering, |(px, py)| self.url_hover_stamp_at(px, py));
        if let Some((px, py)) = point {
            // A hover only exists while Cmd is held (`flagsChanged:` drops it).
            self.update_hovered_url_at(true, px, py);
        }
    }

    /// The hover update at a pixel position, `cmd` telling whether Cmd is held.
    fn update_hovered_url_at(&self, cmd: bool, px: f32, py: f32) {
        if !cmd {
            let had_hover = self.ivars().hovered_url.borrow().is_some();
            if had_hover {
                *self.ivars().hovered_url.borrow_mut() = None;
                NSCursor::arrowCursor().set();
                self.mark_dirty();
            }
            return;
        }

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
                // Same cells, other text (output slid another URL under a
                // still pointer) is a new hover too: Cmd+click opens `url`.
                let changed = old.as_ref().map_or(true, |o| o.0 != pane.id || o.1 != segments || o.2 != url);
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

    /// The name of the agent conversation held by a pane of this window, if it
    /// holds one and it has been named.
    fn pane_session_name(&self, pane_id: PaneId) -> Option<String> {
        let tabs = self.ivars().tabs.borrow();
        let tab = tabs.iter().find(|t| t.contains(pane_id))?;
        tab.pane(pane_id)?.agent_session_name()
    }

    /// Keep the focused pane's conversation in the bookmark list, or drop it if
    /// it is already there. The list is what Cmd+P offers, grouped by directory.
    ///
    /// Unlike the session file, this survives closing the pane: it is the answer
    /// to "I want this conversation back tomorrow", not to "put back what was
    /// open when Kova quit".
    fn do_toggle_bookmark(&self) {
        let focused = {
            let tabs = self.ivars().tabs.borrow();
            let Some(tab) = tabs.get(self.ivars().active_tab.get()) else { return };
            tab.focused_pane
        };
        self.toggle_pane_bookmark(focused);
    }

    /// What a pane would be saved as, in either list: the conversation it runs,
    /// or its directory alone at a bare shell. `None` when the pane holds
    /// neither — there would be nothing to bring back.
    pub(super) fn pane_as_saved(&self, pane_id: PaneId) -> Option<crate::bookmarks::Bookmark> {
        let tabs = self.ivars().tabs.borrow();
        let pane = tabs.iter().find_map(|t| t.pane(pane_id))?;
        let session = pane.agent_session.borrow().clone();
        let saved = crate::bookmarks::Bookmark {
            agent: session.as_ref().map(|s| s.agent),
            session_id: session.as_ref().map(|s| s.id.clone()),
            cwd: pane.cwd().unwrap_or_default(),
            label: pane.display_title("shell"),
        };
        (!saved.cwd.is_empty() || saved.session_id.is_some()).then_some(saved)
    }

    /// Keep the focused pane's conversation among today's anchors, or drop it.
    /// Cmd+Shift+A — the deliberate gesture, next to Cmd+B, one modifier apart
    /// because the two lists are neighbours.
    fn do_toggle_anchor(&self) {
        let focused = {
            let tabs = self.ivars().tabs.borrow();
            let Some(tab) = tabs.get(self.ivars().active_tab.get()) else { return };
            tab.focused_pane
        };
        self.toggle_pane_anchor(focused);
    }

    /// Anchor or un-anchor the conversation of `pane_id`. Cmd+Shift+A on the
    /// focused pane, and on a pane row of Cmd+P.
    pub(super) fn toggle_pane_anchor(&self, pane_id: PaneId) {
        let Some(candidate) = self.pane_as_saved(pane_id) else {
            self.set_transient_status("Nothing to anchor in this pane");
            return;
        };
        let label = candidate.label.clone();
        let mut anchors = crate::anchors::load();
        let outcome = crate::anchors::toggle(&mut anchors.items, candidate);
        if outcome == crate::anchors::Toggled::Full {
            self.set_transient_status(&format!(
                "Already {} anchors — drop one first",
                crate::anchors::MAX_ANCHORS
            ));
            return;
        }
        crate::anchors::save(&anchors);
        self.refresh_anchor_keys(&anchors);
        self.set_transient_status(&if outcome == crate::anchors::Toggled::Added {
            format!("Anchored {}", label)
        } else {
            format!("Removed anchor {}", label)
        });
    }

    /// Cmd+A: back to the first anchor — the pane still running it, or the
    /// conversation reopened where it belongs when none does. One key, one
    /// target, nothing to pick: that is the whole point of an ordered list.
    fn do_focus_anchor(&self) {
        // Cmd+A is "select all" everywhere else on this Mac, so while a text
        // field of Kova's own is up it must not jump the window somewhere else.
        if self.ivars().filter.borrow().is_some() {
            return;
        }
        let anchors = crate::anchors::load();
        let Some(first) = anchors.items.first() else {
            self.set_transient_status("No anchor yet — Cmd+Shift+A anchors this pane");
            return;
        };
        self.open_saved_conversation(first);
    }

    /// Re-read the anchor cache the status bars paint from, and repaint.
    pub(super) fn refresh_anchor_keys(&self, anchors: &crate::anchors::Anchors) {
        *self.ivars().anchor_keys.borrow_mut() = crate::anchors::keys(&anchors.items);
        self.mark_dirty();
    }

    /// Bookmark or un-bookmark the conversation of `pane_id`, whichever tab of
    /// this window holds it. Cmd+B on the focused pane, and on a pane row of Cmd+P.
    pub(super) fn toggle_pane_bookmark(&self, pane_id: PaneId) {
        let Some(candidate) = self.pane_as_saved(pane_id) else {
            self.set_transient_status("Nothing to bookmark in this pane");
            return;
        };
        let mut bookmarks = crate::bookmarks::load();
        let label = candidate.label.clone();
        let kept = crate::bookmarks::toggle(&mut bookmarks.items, candidate);
        crate::bookmarks::save(&bookmarks);
        *self.ivars().bookmark_keys.borrow_mut() = crate::bookmarks::keys(&bookmarks.items);
        self.set_transient_status(&if kept {
            format!("Bookmarked {}", label)
        } else {
            format!("Removed bookmark {}", label)
        });
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
}














impl KovaView {








    /// Close the active window (all its tabs). The timer will detect
    /// the empty tab list and remove the window. App terminates when
    /// the last window is closed (via `applicationShouldTerminateAfterLastWindowClosed`).
    fn do_close_window(&self) {
        // Check for running processes and confirm
        let procs = self.running_processes();
        // The open todos of every conversation the window's panes hold or held.
        let sessions = {
            let tabs = self.ivars().tabs.borrow();
            let mut sessions: Vec<String> = Vec::new();
            for tab in tabs.iter() {
                tab.for_each_pane(&mut |pane| push_raf_session(&mut sessions, pane));
            }
            sessions
        };
        let (todos, conversations) = open_todos_of(&sessions);
        let mtm = unsafe { MainThreadMarker::new_unchecked() };
        if !confirm_close(mtm, &procs, &todos, conversations, "Close this window?", "Close") {
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
            Action::OpenPaneSwitcher => self.open_pane_switcher(false, switcher::SwitcherView::Panes),
            Action::OpenUnreadSwitcher => self.open_pane_switcher(true, switcher::SwitcherView::Panes),
            Action::ToggleBookmark => self.do_toggle_bookmark(),
            Action::ToggleAnchor => self.do_toggle_anchor(),
            Action::FocusAnchor => self.do_focus_anchor(),
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
                    if let Some(text) = overlays::filter_copy_text(state) {
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
                // Every overlay that owns the keyboard owns the paste too, so
                // IPC `dispatch-action paste` lands where Cmd+V would, never in
                // the PTY hidden behind it.
                let palette_open = self.ivars().search_palette.borrow().is_some();
                let rename_open = self.ivars().rename_tab.borrow().is_some()
                    || self.ivars().rename_pane.borrow().is_some();
                let filter_open = self.ivars().filter.borrow().is_some();
                if palette_open || rename_open || filter_open {
                    let pasteboard = NSPasteboard::generalPasteboard();
                    if let Some(text) = unsafe { pasteboard.stringForType(objc2_app_kit::NSPasteboardTypeString) } {
                        let text = text.to_string();
                        if palette_open {
                            self.paste_into_search_palette(&text);
                        } else if rename_open {
                            self.paste_into_rename(&text);
                        } else {
                            self.paste_into_filter(&text);
                        }
                    }
                } else if let Some(pane) = self.focused_pane() {
                    let pasteboard = NSPasteboard::generalPasteboard();
                    if let Some(path) = appkit_input::pasted_image_path(&pasteboard) {
                        // The path Kova just built holds no paste terminator.
                        pane.paste(&path);
                    } else if let Some(text) = unsafe { pasteboard.stringForType(objc2_app_kit::NSPasteboardTypeString) } {
                        pane.paste(&text.to_string());
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





    /// Paste into whichever rename editor is open, as one sanitized line.
    fn paste_into_rename(&self, text: &str) {
        // A title is one line: no newline, tab or escape may reach the tab bar
        // or session.json.
        let text = overlays::line_paste_text(text);
        if text.is_empty() {
            return;
        }
        let insert = |input: &mut String, cursor: &mut usize| {
            let byte_idx = input.char_indices().nth(*cursor).map(|(i, _)| i).unwrap_or(input.len());
            input.insert_str(byte_idx, &text);
            *cursor += text.chars().count();
        };
        if let Some(state) = self.ivars().rename_tab.borrow_mut().as_mut() {
            insert(&mut state.input, &mut state.cursor);
        } else if let Some(state) = self.ivars().rename_pane.borrow_mut().as_mut() {
            insert(&mut state.input, &mut state.cursor);
        }
        self.mark_dirty();
    }

    fn mark_dirty(&self) {
        if let Some(pane) = self.focused_pane() {
            // read_recursive: callers may already hold a read guard on this terminal,
            // and a plain read() queued behind a waiting writer would deadlock.
            pane.terminal.read_recursive().dirty.store(true, std::sync::atomic::Ordering::Relaxed);
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
    confirm_close(mtm, procs, &[], 0, message, confirm_button)
}

/// `confirm_running_processes`, plus what the closing conversations still have
/// to do: `todos` are their open todo texts (`crate::raf`), `conversations` how
/// many conversations those come from. The dialog appears when a process runs
/// or when there are open todos — the latter covers a pane whose `claude` has
/// exited but whose conversation still has work left. With neither, nothing is
/// asked, as before.
pub fn confirm_close(
    mtm: MainThreadMarker,
    procs: &[(String, String)],
    todos: &[String],
    conversations: usize,
    message: &str,
    confirm_button: &str,
) -> bool {
    if procs.is_empty() && todos.is_empty() {
        return true;
    }
    confirm_action(mtm, message, &close_dialog_text(procs, todos, conversations), confirm_button)
}

/// Informative text of the close / quit confirmation: the running processes,
/// then, when there are open todos, a blank line and their list. Without a
/// running process, the todo list alone.
fn close_dialog_text(procs: &[(String, String)], todos: &[String], conversations: usize) -> String {
    let mut lines = String::new();
    if !procs.is_empty() {
        lines.push_str("The following processes are running:");
        for (tab, name) in procs {
            lines.push_str(&format!("\n\u{2022} Tab \u{ab}{}\u{bb}: {}", tab, name));
        }
    }
    let warning = crate::raf::close_warning(todos, conversations);
    if !warning.is_empty() {
        if !lines.is_empty() {
            lines.push_str("\n\n");
        }
        lines.push_str(&warning);
    }
    lines
}

/// Add the Claude conversation `pane` holds or held — live, left behind by an
/// exited `claude`, or waiting in a restored resume line — to `sessions`,
/// once. That is the conversation whose open todos a close warns about.
pub(super) fn push_raf_session(sessions: &mut Vec<String>, pane: &Pane) {
    let id = pane.last_claude_session_id().or_else(|| {
        let last = pane.last_command();
        switcher::pending_session_id(last.as_deref()).map(str::to_string)
    });
    if let Some(id) = id {
        if !sessions.contains(&id) {
            sessions.push(id);
        }
    }
}

/// Open todos of every distinct Claude conversation in `sessions`, flattened,
/// and how many conversations they came from. Reads `~/.claude/raf/` — only
/// call it when a close is under way, never per frame.
pub(super) fn open_todos_of(sessions: &[String]) -> (Vec<String>, usize) {
    let lists: Vec<Vec<String>> = sessions
        .iter()
        .filter_map(|id| crate::raf::load(id))
        .map(|r| r.todos)
        .filter(|t| !t.is_empty())
        .collect();
    let n = lists.len();
    (lists.into_iter().flatten().collect(), n)
}

/// Ask a yes/no question in a warning alert. Returns `true` if the user picked
/// `confirm_button`. The caller must hold no `RefCell` borrow on the view:
/// `runModal` spins a run loop that dispatches events, which borrow again.
pub fn confirm_action(mtm: MainThreadMarker, message: &str, informative: &str, confirm_button: &str) -> bool {
    confirm_choice(mtm, message, informative, confirm_button, "Cancel")
}

/// Same alert with both buttons named: for a question whose second answer is a
/// choice of its own ("Keep both") rather than backing out.
pub fn confirm_choice(
    mtm: MainThreadMarker,
    message: &str,
    informative: &str,
    first_button: &str,
    second_button: &str,
) -> bool {
    let alert = NSAlert::new(mtm);
    alert.setAlertStyle(NSAlertStyle::Warning);
    alert.setMessageText(&NSString::from_str(message));
    alert.setInformativeText(&NSString::from_str(informative));
    alert.addButtonWithTitle(&NSString::from_str(first_button));
    alert.addButtonWithTitle(&NSString::from_str(second_button));
    alert.runModal() == 1000 // NSAlertFirstButtonReturn
}

/// The same alert with a one-line text field under its text: what was typed
/// on the first button, `None` on Cancel. Untrimmed — the caller decides what
/// an empty answer means.
pub fn prompt_text(
    mtm: MainThreadMarker,
    message: &str,
    informative: &str,
    confirm_button: &str,
    placeholder: &str,
    initial: &str,
) -> Option<String> {
    let alert = NSAlert::new(mtm);
    alert.setAlertStyle(NSAlertStyle::Informational);
    alert.setMessageText(&NSString::from_str(message));
    alert.setInformativeText(&NSString::from_str(informative));
    alert.addButtonWithTitle(&NSString::from_str(confirm_button));
    alert.addButtonWithTitle(&NSString::from_str("Cancel"));
    let field = objc2_app_kit::NSTextField::new(mtm);
    field.setFrame(CGRect { origin: CGPoint { x: 0.0, y: 0.0 }, size: CGSize { width: 280.0, height: 24.0 } });
    field.setPlaceholderString(Some(&NSString::from_str(placeholder)));
    field.setStringValue(&NSString::from_str(initial));
    let view: &objc2_app_kit::NSView = &field;
    alert.setAccessoryView(Some(view));
    // Typing goes straight into the field, no click first.
    alert.window().setInitialFirstResponder(Some(view));
    if alert.runModal() != 1000 {
        return None;
    }
    Some(field.stringValue().to_string())
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





#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn close_dialog_text_lists_the_running_processes_as_before() {
        // Frozen: the text the dialog showed before todos were added to it.
        let procs = vec![("kova".to_string(), "claude".to_string()), ("logs".to_string(), "tail".to_string())];
        assert_eq!(
            close_dialog_text(&procs, &[], 0),
            "The following processes are running:\n\u{2022} Tab \u{ab}kova\u{bb}: claude\n\u{2022} Tab \u{ab}logs\u{bb}: tail"
        );
    }

    #[test]
    fn close_dialog_text_without_a_process_is_the_todo_list_alone() {
        let todos = vec!["ship it".to_string()];
        assert_eq!(
            close_dialog_text(&[], &todos, 1),
            "1 thing left to do in this conversation:\n\u{2022} ship it"
        );
    }

    #[test]
    fn close_dialog_text_adds_the_open_todos_after_a_blank_line() {
        let procs = vec![("kova".to_string(), "claude".to_string())];
        let todos = vec!["ship it".to_string()];
        assert_eq!(
            close_dialog_text(&procs, &todos, 1),
            "The following processes are running:\n\u{2022} Tab \u{ab}kova\u{bb}: claude\n\n\
             1 thing left to do in this conversation:\n\u{2022} ship it"
        );
    }









    #[test]
    fn flash_label_splits_a_cwd_into_directory_and_path_above() {
        let home = "/Users/sam";
        assert_eq!(
            flash_label_parts("/Users/sam/projects/perso/kova", home),
            ("kova".to_string(), "~/projects/perso".to_string())
        );
        // Home itself, and the root, are their own name with nothing above.
        assert_eq!(flash_label_parts(home, home), ("~".to_string(), String::new()));
        assert_eq!(flash_label_parts("/", home), ("/".to_string(), String::new()));
        // A top-level directory keeps the root as its path line.
        assert_eq!(flash_label_parts("/tmp", home), ("tmp".to_string(), "/".to_string()));
        // A trailing slash must not produce an empty name.
        assert_eq!(
            flash_label_parts("/Users/sam/dev/", home),
            ("dev".to_string(), "~".to_string())
        );
        // A path that merely starts with the same bytes as $HOME is not under it.
        assert_eq!(
            flash_label_parts("/Users/samuel/dev", home),
            ("dev".to_string(), "/Users/samuel".to_string())
        );
    }































}
