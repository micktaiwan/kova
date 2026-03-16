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
use crate::pane::{alloc_tab_id, NavDirection, Pane, PaneId, SplitDirection, Tab};
use crate::renderer::{FilterRenderData, PaneViewport, Renderer};
use crate::terminal::{FilterMatch, GridPos, Selection, SelectionMode};

#[derive(Clone, Copy)]
struct SeparatorDrag {
    is_column_sep: bool,
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
    /// Deferred tabs to restore progressively (tab_index, saved_tab_data).
    deferred_tabs: RefCell<Vec<(usize, crate::session::SavedTab)>>,
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

struct FilterState {
    query: String,
    matches: Vec<FilterMatch>,
}

struct RenameTabState {
    input: String,
    cursor: usize, // char index
}

struct RenamePaneState {
    input: String,
    cursor: usize, // char index
}

struct RecentProjectItem {
    entry: crate::recent_projects::RecentProject,
    /// Pre-computed render data for the renderer.
    render: crate::renderer::RecentProjectEntry,
}

struct RecentProjectsState {
    items: Vec<RecentProjectItem>,
    selected: usize,
    /// Scroll offset (index of first visible entry).
    scroll: usize,
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

struct MergeTabState {
    entries: Vec<MergeTabEntry>,
    selected: usize,
}

struct SendToWindowState {
    entries: Vec<SendToWindowEntry>,
    selected: usize,
}

fn build_items(entries: Vec<crate::recent_projects::RecentProject>) -> Vec<RecentProjectItem> {
    entries.into_iter().map(|e| {
        let render = crate::renderer::RecentProjectEntry {
            path: crate::recent_projects::tildify(&e.path),
            time_ago: crate::recent_projects::time_ago(e.last_opened),
            pane_count: crate::recent_projects::pane_count_tab(&e.tab),
            invalid: !std::path::Path::new(&e.path).is_dir(),
        };
        RecentProjectItem { entry: e, render }
    }).collect()
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
                    Action::ClearScrollback => {
                        if let Some(pane) = self.focused_pane() {
                            pane.terminal.write().clear_scrollback_and_screen();
                            pane.pty.write(b"\x0c");
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
                    Action::Equalize => {
                        let mut tabs = self.ivars().tabs.borrow_mut();
                        let idx = self.ivars().active_tab.get();
                        if let Some(tab) = tabs.get_mut(idx) {
                            tab.equalize();
                            drop(tabs);
                            self.resize_all_panes();
                        }
                    }
                    Action::PrevTab => self.do_switch_tab_relative(-1),
                    Action::NextTab => self.do_switch_tab_relative(1),
                    Action::RenameTab => self.start_rename_tab(),
                    Action::RenamePane => self.start_rename_pane(),
                    Action::DetachTab => self.do_detach_tab(),
                    Action::BreakPane => self.do_break_pane(),
                    Action::MergeTab => self.do_merge_tab(),

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
                                // Close filter after copying
                                *self.ivars().filter.borrow_mut() = None;
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
                                    return objc2::runtime::Bool::NO;
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
                                let text = text.to_string();
                                let bracketed = pane.terminal.read().bracketed_paste;
                                if bracketed { pane.pty.write(b"\x1b[200~"); }
                                pane.pty.write(text.as_bytes());
                                if bracketed { pane.pty.write(b"\x1b[201~"); }
                            }
                        }
                    }
                }
                return objc2::runtime::Bool::YES;
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
                if let Some((pane, _vp)) = self.pane_at_event(event) {
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
                        let mut term = pane.terminal.write();
                        let active_tab_idx = ivars.active_tab.get();
                        log::debug!("SCROLL-EVENT tab={} pane={} term_id={} lines={} offset_before={}",
                            active_tab_idx, pane.id, term.terminal_id, lines, term.scroll_offset());
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

            // Check tab bar click
            if self.hit_test_tab_bar(px, py, event) {
                return;
            }

            // Check separator hit
            if let Some(drag) = self.hit_test_separator(px, py) {
                self.ivars().drag_separator.set(Some(drag));
                return;
            }

            // Cmd+Click opens URL
            let modifiers = event.modifierFlags();
            if modifiers.contains(NSEventModifierFlags::Command) {
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
                        tab.restore_pane(pane_id);
                        tab.focused_pane = pane_id;
                        tab.mark_all_dirty();
                        let full = self.drawable_viewport();
                        let min_w = self.min_split_width_px();
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
                    // Clear completion and bell flags on newly focused pane
                    let t = pane.terminal.read();
                    t.command_completed.store(false, std::sync::atomic::Ordering::Relaxed);
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
        fn mouse_up(&self, _event: &NSEvent) {
            self.ivars().auto_scroll_speed.set(0);
            if self.ivars().drag_tab.get().is_some() {
                self.ivars().drag_tab.set(None);
                return;
            }
            if self.ivars().drag_separator.get().is_some() {
                self.ivars().drag_separator.set(None);
                return;
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
            let tag = sender.tag();
            let tab_idx = self.ivars().color_menu_tab.get();
            let mut tabs = self.ivars().tabs.borrow_mut();
            if let Some(tab) = tabs.get_mut(tab_idx) {
                tab.color = if tag < 0 { None } else { Some(tag as usize) };
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
            deferred_tabs: RefCell::new(Vec::new()),
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

    fn focused_pane(&self) -> Option<&Pane> {
        let tabs = self.ivars().tabs.borrow();
        let idx = self.ivars().active_tab.get();
        let tab = tabs.get(idx)?;
        let pane = tab.pane(tab.focused_pane)?;
        // SAFETY: The Tab lives in RefCell inside ivars, pinned in ObjC heap.
        // We only mutate in the render timer (pane removal), never while an event handler holds this ref.
        Some(unsafe { &*(pane as *const Pane) })
    }

    /// Convert pixel coords to (visible_row, col) within a pane viewport.
    fn pixel_to_visible_row_col(&self, px: f32, py: f32, pane: &Pane, vp: &PaneViewport) -> Option<(usize, u16)> {
        let renderer = self.ivars().renderer.get()?;
        let renderer_r = renderer.read();
        let (cell_w, cell_h) = renderer_r.cell_size();
        drop(renderer_r);

        let rel_x = px - vp.x - crate::renderer::PANE_H_PADDING;
        let rel_y = py - vp.y;

        let term = pane.terminal.read();
        let y_offset = term.y_offset_rows();
        let col = (rel_x / cell_w).floor() as i32;
        let visible_row = (rel_y / cell_h).floor() as i32 - y_offset as i32;

        if visible_row < 0 || col < 0 || visible_row >= term.rows as i32 {
            return None;
        }
        Some((visible_row as usize, (col as u16).min(term.cols.saturating_sub(1))))
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

    /// Viewport for panes (below tab bar), reading scroll state from the active tab.
    /// WARNING: borrows tabs — do NOT call while tabs is already borrowed.
    fn panes_viewport(&self) -> PaneViewport {
        let tabs = self.ivars().tabs.borrow();
        let idx = self.ivars().active_tab.get();
        if let Some(tab) = tabs.get(idx) {
            let screen_w = self.drawable_viewport().width;
            let vw = tab.virtual_width(screen_w, self.min_split_width_px());
            self.panes_viewport_inner(tab.scroll_offset_x, vw)
        } else {
            self.panes_viewport_inner(0.0, self.drawable_viewport().width)
        }
    }

    /// Viewport for panes using a tab reference (no extra borrow on tabs).
    fn panes_viewport_for_tab(&self, tab: &crate::pane::Tab) -> PaneViewport {
        let screen_w = self.drawable_viewport().width;
        let vw = tab.virtual_width(screen_w, self.min_split_width_px());
        self.panes_viewport_inner(tab.scroll_offset_x, vw)
    }

    /// Scroll the tab so that the given pane is visible on screen.
    fn scroll_to_reveal_pane(&self, tab: &mut Tab, pane_id: PaneId, screen_w: f32) {
        let panes_vp = self.panes_viewport_for_tab(tab);
        if let Some(vp) = tab.viewport_for_pane(pane_id, panes_vp) {
            tab.scroll_to_reveal(&vp, screen_w);
        }
    }

    fn panes_viewport_inner(&self, scroll_offset_x: f32, virtual_width: f32) -> PaneViewport {
        let full = self.drawable_viewport();
        let tab_bar_h = self.tab_bar_height();
        let global_bar_h = self.global_bar_height();
        PaneViewport {
            x: -scroll_offset_x,
            y: full.y + tab_bar_h,
            width: virtual_width,
            height: full.height - tab_bar_h - global_bar_h,
        }
    }

    /// Global status bar height in pixels (1x cell height).
    fn global_bar_height(&self) -> f32 {
        let renderer = match self.ivars().renderer.get() {
            Some(r) => r,
            None => return 0.0,
        };
        let r = renderer.read();
        r.cell_size().1
    }

    fn backing_scale(&self) -> f32 {
        self.window().map_or(2.0, |w| w.backingScaleFactor()) as f32
    }

    /// Compute scaled min_split_width in pixels.
    fn min_split_width_px(&self) -> f32 {
        let min_w = self.ivars().config.get()
            .map(|c| c.splits.min_width)
            .unwrap_or(300.0);
        min_w * self.backing_scale()
    }

    /// Mode 2: adjust the virtual width override of the active tab (all panes scale proportionally).
    fn adjust_virtual_width(&self, dir: f32) {
        let step = 200.0 * self.backing_scale();
        let screen_w = self.drawable_viewport().width;
        let min_w = self.min_split_width_px();
        let mut tabs = self.ivars().tabs.borrow_mut();
        let idx = self.ivars().active_tab.get();
        if let Some(tab) = tabs.get_mut(idx) {
            let current_vw = tab.virtual_width(screen_w, min_w);
            let new_vw = (current_vw + dir * step).max(screen_w);
            tab.virtual_width_override = if new_vw > screen_w { new_vw } else { 0.0 };
            self.enforce_max_pane_width(tab, screen_w, min_w);
            tab.clamp_scroll(screen_w, min_w);
            self.scroll_to_reveal_pane(tab, tab.focused_pane, screen_w);
            self.set_resize_feedback("Virtual", tab, screen_w, min_w);
        }
        drop(tabs);
        self.resize_all_panes();
    }

    /// Mode 1 post-validation: reduce virtual_width so no pane exceeds screen_width.
    /// Does NOT touch ratios (the user just set them).
    fn cap_virtual_width(&self, tab: &mut Tab, screen_w: f32, min_w: f32) {
        let vw = tab.virtual_width(screen_w, min_w);
        if vw <= screen_w { return; }
        let max_frac = tab.max_leaf_width_fraction();
        if max_frac <= 0.0 { return; }
        let max_vw = screen_w / max_frac;
        if vw > max_vw {
            tab.virtual_width_override = if max_vw > screen_w { max_vw } else { 0.0 };
            tab.clamp_scroll(screen_w, min_w);
        }
    }

    /// Modes 2 & 3 post-validation: adjust ratios of oversized panes first,
    /// then reduce virtual_width as last resort.
    fn enforce_max_pane_width(&self, tab: &mut Tab, screen_w: f32, min_w: f32) {
        let vw = tab.virtual_width(screen_w, min_w);
        if vw <= screen_w { return; }
        // Step 1: adjust ratios to cap oversized panes
        tab.clamp_pane_widths(vw, screen_w);
        // Step 2: if still oversized (total too large), reduce virtual_width
        let max_frac = tab.max_leaf_width_fraction();
        if max_frac > 0.0 {
            let max_vw = screen_w / max_frac;
            let current_vw = tab.virtual_width(screen_w, min_w);
            if current_vw > max_vw {
                tab.virtual_width_override = if max_vw > screen_w { max_vw } else { 0.0 };
            }
        }
        tab.clamp_scroll(screen_w, min_w);
    }

    /// Store resize feedback info to display in the global status bar for ~2 seconds.
    fn set_resize_feedback(&self, mode: &str, tab: &Tab, screen_w: f32, min_w: f32) {
        let fps = self.ivars().config.get().map(|c| c.terminal.fps).unwrap_or(60) as u32;
        let resize_mode = match mode {
            "Virtual" => ResizeMode::Virtual,
            "Right Edge" => ResizeMode::Edge,
            _ => ResizeMode::Ratio,
        };
        self.ivars().resize_feedback.set(Some(ResizeFeedback {
            mode: resize_mode,
            screen_w: screen_w as u32,
            virtual_w: tab.virtual_width(screen_w, min_w) as u32,
            remaining_frames: fps * 2,
        }));
    }

    fn get_tab_bar_left_inset(&self) -> f32 {
        let v = self.ivars().tab_bar_left_inset.get();
        if v > 0.0 { v } else { 136.0 } // fallback 68pt * 2x
    }

    /// Tab bar height in pixels (2.0x cell height).
    fn tab_bar_height(&self) -> f32 {
        let renderer = match self.ivars().renderer.get() {
            Some(r) => r,
            None => return 0.0,
        };
        let r = renderer.read();
        let (_, cell_h) = r.cell_size();
        (cell_h * 2.0).round()
    }

    /// Hit-test separators in the active tab's tree.
    fn hit_test_separator(&self, px: f32, py: f32) -> Option<SeparatorDrag> {
        let tabs = self.ivars().tabs.borrow();
        let idx = self.ivars().active_tab.get();
        let tab = tabs.get(idx)?;
        let vp = self.panes_viewport_for_tab(tab);
        let mut seps = Vec::new();
        tab.collect_separator_info(vp, &mut seps);

        let scale = self.backing_scale();
        let tolerance = 4.0 * scale;

        // Separators are in screen space (viewport uses x: -scroll_offset_x)
        for sep in &seps {
            if sep.is_column_sep {
                if (px - sep.pos).abs() < tolerance && py >= sep.cross_start && py <= sep.cross_end {
                    return Some(SeparatorDrag {
                        is_column_sep: true,
                        origin_pixel: px,
                        parent_dim: sep.parent_dim,
                        column_sep_index: sep.column_sep_index,
                        col_index: sep.col_index,
                        row_sep_index: sep.row_sep_index,
                    });
                }
            } else {
                if (py - sep.pos).abs() < tolerance && px >= sep.cross_start && px <= sep.cross_end {
                    return Some(SeparatorDrag {
                        is_column_sep: false,
                        origin_pixel: py,
                        parent_dim: sep.parent_dim,
                        column_sep_index: sep.column_sep_index,
                        col_index: sep.col_index,
                        row_sep_index: sep.row_sep_index,
                    });
                }
            }
        }
        None
    }

    /// Hit-test the tab bar. Returns true if click was in the tab bar (and handled).
    fn hit_test_tab_bar(&self, px: f32, py: f32, event: &NSEvent) -> bool {
        let tab_bar_h = self.tab_bar_height();
        if py > tab_bar_h {
            return false;
        }
        if let Some(idx) = self.tab_index_at_x(px) {
            self.do_switch_tab(idx);
            self.ivars().drag_tab.set(Some(DragTabState {
                tab_index: idx,
                start_x: px,
                current_x: px,
                dragging: false,
            }));
        } else if let Some(win) = self.window() {
            // Click in titlebar but not on a tab — initiate window drag
            win.performWindowDragWithEvent(event);
        }
        true
    }

    /// Returns the tab index at the given x pixel position, or None if outside tabs.
    fn tab_index_at_x(&self, px: f32) -> Option<usize> {
        let tabs = self.ivars().tabs.borrow();
        let tab_count = tabs.len();
        if tab_count == 0 {
            return None;
        }
        let full = self.drawable_viewport();
        let left_inset = self.get_tab_bar_left_inset();
        let renderer = self.ivars().renderer.get()?;
        let cell_w = renderer.read().cell_size().0;
        let max_tab_w = cell_w * 20.0;
        let available_w = full.width - left_inset;
        let tab_width = (available_w / tab_count as f32).min(max_tab_w);
        for i in 0..tab_count {
            let x = left_inset + i as f32 * tab_width;
            if px >= x && px <= x + tab_width {
                return Some(i);
            }
        }
        None
    }

    /// Total drawable viewport in pixels.
    fn drawable_viewport(&self) -> PaneViewport {
        let frame = self.frame();
        let scale = self.window().map_or(2.0, |w| w.backingScaleFactor());
        PaneViewport {
            x: 0.0,
            y: 0.0,
            width: (frame.size.width * scale) as f32,
            height: (frame.size.height * scale) as f32,
        }
    }

    /// Convert an NSEvent location to Metal pixel coordinates (origin top-left).
    fn event_to_pixel(&self, event: &NSEvent) -> (f32, f32) {
        let location = event.locationInWindow();
        let local: CGPoint = unsafe { msg_send![self, convertPoint: location, fromView: std::ptr::null::<objc2::runtime::AnyObject>()] };
        let frame = self.frame();
        let scale = self.backing_scale();
        let pixel_x = local.x as f32 * scale;
        let pixel_y = (frame.size.height as f32 - local.y as f32) * scale;
        (pixel_x, pixel_y)
    }

    /// Hit-test: find which pane is under the mouse event (in active tab).
    fn pane_at_event(&self, event: &NSEvent) -> Option<(&Pane, PaneViewport)> {
        let tabs = self.ivars().tabs.borrow();
        let idx = self.ivars().active_tab.get();
        let tab = tabs.get(idx)?;
        let (px, py) = self.event_to_pixel(event);
        // Viewport is already in screen space (x: -scroll_offset_x), so use px directly
        let (pane, vp) = tab.hit_test(px, py, self.panes_viewport_for_tab(tab))?;
        Some((unsafe { &*(pane as *const Pane) }, vp))
    }

    /// Convert an NSEvent to a grid position within the given pane/viewport.
    fn pixel_to_grid_in(&self, event: &NSEvent, pane: &Pane, vp: &PaneViewport) -> Option<GridPos> {
        let renderer = self.ivars().renderer.get()?;
        let (pixel_x, pixel_y) = self.event_to_pixel(event);

        let renderer_r = renderer.read();
        let (cell_w, cell_h) = renderer_r.cell_size();
        drop(renderer_r);

        let rel_x = pixel_x - vp.x - crate::renderer::PANE_H_PADDING;
        let rel_y = pixel_y - vp.y;

        let term = pane.terminal.read();
        let y_offset = term.y_offset_rows();
        let col = (rel_x / cell_w).floor() as i32;
        let visible_row = (rel_y / cell_h).floor() as i32 - y_offset as i32;

        if visible_row < 0 || col < 0 {
            return None;
        }
        let col = (col as u16).min(term.cols.saturating_sub(1));
        let visible_row = visible_row as usize;
        if visible_row >= term.rows as usize {
            return None;
        }

        let abs_line = (term.scrollback_len() as i64 - term.scroll_offset() as i64 + visible_row as i64) as usize;
        Some(GridPos { line: abs_line, col })
    }

    /// Compute cols/rows for a pane viewport.
    fn viewport_to_grid(&self, vp: &PaneViewport) -> (u16, u16) {
        let renderer = self.ivars().renderer.get().unwrap();
        let renderer_r = renderer.read();
        let (cell_w, cell_h) = renderer_r.cell_size();
        let status_bar = renderer_r.status_bar_enabled();
        drop(renderer_r);

        let cols = ((vp.width - 2.0 * crate::renderer::PANE_H_PADDING) / cell_w).floor().max(1.0) as u16;
        let usable_h = if status_bar {
            vp.height - cell_h
        } else {
            vp.height
        };
        let rows = (usable_h / cell_h).floor().max(1.0) as u16;
        (cols, rows)
    }

    /// Create a new tab (Cmd+T).
    fn do_new_tab(&self) {
        let config = match self.ivars().config.get() {
            Some(c) => c,
            None => return,
        };

        // Get CWD from currently focused pane
        let cwd = self.focused_pane().and_then(|p| p.cwd());

        let tab = match Tab::new_with_cwd(config, cwd.as_deref()) {
            Ok(t) => t,
            Err(e) => {
                log::error!("failed to create tab: {}", e);
                return;
            }
        };

        let mut tabs = self.ivars().tabs.borrow_mut();
        tabs.push(tab);
        let new_idx = tabs.len() - 1;
        log::debug!("New tab created: index={}, total={}", new_idx, tabs.len());
        drop(tabs);
        self.ivars().active_tab.set(new_idx);
        self.resize_all_panes();
    }

    /// Switch to tab at index.
    fn do_switch_tab(&self, idx: usize) {
        let tabs = self.ivars().tabs.borrow();
        if idx >= tabs.len() || idx == self.ivars().active_tab.get() {
            return;
        }
        log::debug!("Switch to tab {}", idx);
        // Mark all panes of new tab dirty so the next render tick draws them
        tabs[idx].for_each_pane(&mut |pane| {
            pane.terminal.read().dirty.store(true, std::sync::atomic::Ordering::Relaxed);
        });
        drop(tabs);
        self.ivars().active_tab.set(idx);
        // Clear bell/attention indicator on the newly focused tab
        {
            let mut tabs = self.ivars().tabs.borrow_mut();
            tabs[idx].clear_bell();
            tabs[idx].clear_completion();
        }
        // Lazy resize: resize panes when switching to them
        self.resize_all_panes();
    }

    /// Show a context menu to pick a color for a tab.
    fn show_tab_color_menu(&self, event: &NSEvent, tab_idx: usize) {
        use objc2_app_kit::{NSMenu, NSMenuItem};

        self.ivars().color_menu_tab.set(tab_idx);
        let mtm = unsafe { MainThreadMarker::new_unchecked() };
        let pastilles = ["🔴", "🟠", "🟡", "🟢", "🔵", "🟣"];
        let menu = NSMenu::new(mtm);
        let action = objc2::sel!(tabColorSelected:);
        let empty_ke = NSString::from_str("");

        for (i, emoji) in pastilles.iter().enumerate() {
            let title = NSString::from_str(emoji);
            let item = unsafe {
                NSMenuItem::initWithTitle_action_keyEquivalent(
                    NSMenuItem::alloc(mtm),
                    &title,
                    Some(action),
                    &empty_ke,
                )
            };
            item.setTag(i as isize);
            unsafe { item.setTarget(Some(&*self)) };
            menu.addItem(&item);
        }

        // Separator + "Aucune" item
        menu.addItem(&NSMenuItem::separatorItem(mtm));
        let none_title = NSString::from_str("Aucune");
        let none_item = unsafe {
            NSMenuItem::initWithTitle_action_keyEquivalent(
                NSMenuItem::alloc(mtm),
                &none_title,
                Some(action),
                &empty_ke,
            )
        };
        none_item.setTag(-1);
        unsafe { none_item.setTarget(Some(&*self)) };
        menu.addItem(&none_item);

        // Show menu at click location (synchronous, blocks until user picks or dismisses)
        let location = event.locationInWindow();
        let _ok: bool = unsafe {
            objc2::msg_send![&menu, popUpMenuPositioningItem: std::ptr::null::<NSMenuItem>(), atLocation: location, inView: self]
        };
    }

    /// Switch to relative tab (delta = -1 for prev, +1 for next).
    fn do_switch_tab_relative(&self, delta: i32) {
        let tabs = self.ivars().tabs.borrow();
        let count = tabs.len();
        if count <= 1 {
            return;
        }
        drop(tabs);
        let current = self.ivars().active_tab.get() as i32;
        let new_idx = ((current + delta) % count as i32 + count as i32) as usize % count;
        self.do_switch_tab(new_idx);
    }

    /// Split the focused pane in the given direction.
    fn do_split(&self, direction: SplitDirection) {
        let config = match self.ivars().config.get() {
            Some(c) => c,
            None => return,
        };

        let (focused_id, current_vp, focused_cwd) = {
            let tabs = self.ivars().tabs.borrow();
            let idx = self.ivars().active_tab.get();
            let tab = match tabs.get(idx) {
                Some(t) => t,
                None => return,
            };
            let fid = tab.focused_pane;
            let vp = match tab.viewport_for_pane(fid, self.panes_viewport_for_tab(tab)) {
                Some(vp) => vp,
                None => return,
            };
            let cwd = tab.pane(fid).and_then(|p| p.cwd());
            (fid, vp, cwd)
        };

        let half_vp = match direction {
            SplitDirection::Horizontal => PaneViewport {
                x: current_vp.x,
                y: current_vp.y,
                width: current_vp.width / 2.0,
                height: current_vp.height,
            },
            SplitDirection::Vertical => PaneViewport {
                x: current_vp.x,
                y: current_vp.y,
                width: current_vp.width,
                height: current_vp.height / 2.0,
            },
        };
        let (cols, rows) = self.viewport_to_grid(&half_vp);

        let dir_name = match direction {
            SplitDirection::Horizontal => "horizontal",
            SplitDirection::Vertical => "vertical",
        };
        log::debug!("Split pane {}: direction={}, new size={}x{}", focused_id, dir_name, cols, rows);

        let new_pane = match Pane::spawn(cols, rows, config, focused_cwd.as_deref()) {
            Ok(p) => p,
            Err(e) => {
                log::error!("failed to spawn pane for split: {}", e);
                return;
            }
        };
        let new_id = new_pane.id;

        let mut tabs = self.ivars().tabs.borrow_mut();
        let idx = self.ivars().active_tab.get();
        if let Some(tab) = tabs.get_mut(idx) {
            match direction {
                SplitDirection::Horizontal => {
                    // Insert new column after the focused pane's column
                    tab.insert_column_after_focused(new_pane);
                }
                SplitDirection::Vertical => {
                    // Split focused pane vertically within its column
                    tab.vsplit_at_pane(focused_id, new_pane);
                }
            }
            tab.focused_pane = new_id;
            // Auto-scroll to reveal the new pane
            self.scroll_to_reveal_pane(tab, new_id, self.drawable_viewport().width);
        }
        drop(tabs);

        self.resize_all_panes();
    }

    /// Split at the root level: the new pane spans the full width/height.
    fn do_split_root(&self, direction: SplitDirection) {
        let config = match self.ivars().config.get() {
            Some(c) => c,
            None => return,
        };

        let focused_cwd = {
            let tabs = self.ivars().tabs.borrow();
            let idx = self.ivars().active_tab.get();
            tabs.get(idx).and_then(|tab| {
                tab.pane(tab.focused_pane).and_then(|p| p.cwd())
            })
        };

        let panes_vp = self.panes_viewport();
        let half_vp = match direction {
            SplitDirection::Horizontal => PaneViewport {
                x: panes_vp.x,
                y: panes_vp.y,
                width: panes_vp.width / 2.0,
                height: panes_vp.height,
            },
            SplitDirection::Vertical => PaneViewport {
                x: panes_vp.x,
                y: panes_vp.y,
                width: panes_vp.width,
                height: panes_vp.height / 2.0,
            },
        };
        let (cols, rows) = self.viewport_to_grid(&half_vp);

        let dir_name = match direction {
            SplitDirection::Horizontal => "horizontal",
            SplitDirection::Vertical => "vertical",
        };
        log::debug!("Split root: direction={}, new size={}x{}", dir_name, cols, rows);

        let new_pane = match Pane::spawn(cols, rows, config, focused_cwd.as_deref()) {
            Ok(p) => p,
            Err(e) => {
                log::error!("failed to spawn pane for root split: {}", e);
                return;
            }
        };
        let new_id = new_pane.id;

        let mut tabs = self.ivars().tabs.borrow_mut();
        let idx = self.ivars().active_tab.get();
        if let Some(tab) = tabs.get_mut(idx) {
            match direction {
                SplitDirection::Horizontal => {
                    // Append new column at the end
                    tab.append_column(new_pane);
                }
                SplitDirection::Vertical => {
                    // Wrap column at bottom
                    tab.vsplit_root_at_column(new_pane);
                }
            }
            tab.focused_pane = new_id;
            if direction == SplitDirection::Horizontal {
                // Auto-scroll to reveal the new pane (rightmost)
                let full = self.drawable_viewport();
                let min_w = self.min_split_width_px();
                let vw = tab.virtual_width(full.width, min_w);
                if vw > full.width {
                    tab.scroll_offset_x = (vw - full.width).max(0.0);
                }
            }
        }
        drop(tabs);

        self.resize_all_panes();
    }

    /// Close focused pane. If it's the last pane in the tab, close the tab.
    fn do_close_pane_or_tab(&self) {
        // Collect info for confirmation dialog BEFORE holding the borrow,
        // because NSAlert runs a modal run loop that can dispatch events
        // which access tabs → would panic on double borrow.
        let proc = {
            let tabs = self.ivars().tabs.borrow();
            let idx = self.ivars().active_tab.get();
            if idx >= tabs.len() {
                return;
            }
            tabs[idx].pane(tabs[idx].focused_pane)
                .and_then(|p| p.foreground_process_name().map(|name| (tabs[idx].title(), name)))
        };
        if let Some(proc) = proc {
            let mtm = unsafe { MainThreadMarker::new_unchecked() };
            if !confirm_running_processes(mtm, &[proc], "Close this pane?", "Close") {
                return;
            }
        }

        let mut tabs = self.ivars().tabs.borrow_mut();
        let idx = self.ivars().active_tab.get();
        if idx >= tabs.len() {
            return;
        }

        if tabs[idx].is_single_pane() {
            log::debug!("Closing tab {}", idx);
            drop(tabs);
            self.remove_tab(idx);
            return;
        }

        // Multiple panes → close focused pane
        let focused_id = tabs[idx].focused_pane;
        log::debug!("Closing pane {} in tab {}", focused_id, idx);

        // Find a neighbor to focus before removing (prefer right, then left, then any)
        let panes_vp = self.panes_viewport_for_tab(&tabs[idx]);
        let next_focus = tabs[idx].neighbor(focused_id, NavDirection::Right, panes_vp)
            .or_else(|| tabs[idx].neighbor(focused_id, NavDirection::Left, panes_vp))
            .or_else(|| tabs[idx].neighbor(focused_id, NavDirection::Down, panes_vp))
            .or_else(|| tabs[idx].neighbor(focused_id, NavDirection::Up, panes_vp));

        let old_columns = tabs[idx].num_columns();
        if !tabs[idx].remove_pane(focused_id) {
            // Tab became empty
            drop(tabs);
            self.remove_tab(idx);
            return;
        }
        let new_focus = next_focus
            .filter(|id| tabs[idx].contains(*id))
            .unwrap_or_else(|| tabs[idx].first_pane().id);
        tabs[idx].focused_pane = new_focus;
        let new_columns = tabs[idx].num_columns();
        tabs[idx].scale_virtual_width(old_columns, new_columns);
        // Clean up minimized_stack (closed pane may have been minimized)
        tabs[idx].minimized_stack.retain(|&pid| pid != focused_id);
        // Clamp scroll and auto-scroll to reveal focused pane
        let full = self.drawable_viewport();
        let min_w = self.min_split_width_px();
        tabs[idx].clamp_scroll(full.width, min_w);
        let tab = &mut tabs[idx];
        self.scroll_to_reveal_pane(tab, new_focus, full.width);
        drop(tabs);
        self.resize_all_panes();
    }

    /// Remove a tab by index: save to recent projects, remove from list,
    /// update active_tab, terminate if empty, then resize.
    /// Caller must NOT hold `tabs` borrow when calling this.
    fn remove_tab(&self, idx: usize) {
        let mut tabs = self.ivars().tabs.borrow_mut();
        if idx >= tabs.len() { return; }
        crate::recent_projects::add(&tabs[idx]);
        tabs.remove(idx);
        if tabs.is_empty() {
            drop(tabs);
            unsafe {
                let mtm = MainThreadMarker::new_unchecked();
                let app = NSApplication::sharedApplication(mtm);
                app.terminate(None);
            }
            return;
        }
        let new_idx = if idx >= tabs.len() { tabs.len() - 1 } else { idx };
        drop(tabs);
        self.ivars().active_tab.set(new_idx);
        self.resize_all_panes();
    }

    /// Close the entire active tab (all its panes), with confirmation.
    /// Saves to recent projects before closing.
    fn do_close_tab(&self) {
        // Collect running processes for confirmation BEFORE borrowing tabs
        let procs = {
            let tabs = self.ivars().tabs.borrow();
            let idx = self.ivars().active_tab.get();
            if idx >= tabs.len() {
                return;
            }
            let title = tabs[idx].title();
            let mut result = Vec::new();
            tabs[idx].for_each_pane(&mut |pane| {
                if let Some(name) = pane.foreground_process_name() {
                    result.push((title.clone(), name));
                }
            });
            result
        };

        let mtm = unsafe { MainThreadMarker::new_unchecked() };
        if !confirm_running_processes(mtm, &procs, "Close this tab?", "Close") {
            return;
        }

        let idx = self.ivars().active_tab.get();
        log::debug!("Closing entire tab {}", idx);
        self.remove_tab(idx);
    }

    /// Open the recent projects overlay.
    fn do_open_recent_projects(&self) {
        use std::collections::HashSet;
        // Collect CWDs of ALL panes across ALL windows to filter them out.
        // Use NSApplication::windows() to avoid borrowing the app delegate's
        // window list (which may be borrowed by the timer tick).
        let open_cwds: HashSet<String> = {
            let mtm = unsafe { MainThreadMarker::new_unchecked() };
            let app = NSApplication::sharedApplication(mtm);
            let ns_windows = app.windows();
            let mut cwds = HashSet::new();
            for i in 0..ns_windows.count() {
                let win = &ns_windows.objectAtIndex(i);
                if let Some(view) = crate::app::kova_view(win) {
                    let tabs = view.ivars().tabs.borrow();
                    for tab in tabs.iter() {
                        tab.for_each_pane(&mut |pane| {
                            if let Some(cwd) = pane.cwd() {
                                cwds.insert(cwd);
                            }
                        });
                    }
                }
            }
            cwds
        };
        let all = crate::recent_projects::load();
        let entries: Vec<_> = all.projects.into_iter()
            .filter(|p| !open_cwds.contains(&p.path))
            .collect();

        *self.ivars().recent_projects.borrow_mut() = Some(RecentProjectsState {
            items: build_items(entries),
            selected: 0,
            scroll: 0,
        });
        self.mark_dirty();
    }

    /// Handle key events in the recent projects overlay.
    fn handle_recent_projects_key(&self, event: &NSEvent) {
        let keycode = event.keyCode();

        // Escape → close
        if keycode == 0x35 {
            *self.ivars().recent_projects.borrow_mut() = None;
            self.mark_dirty();
            return;
        }

        // Enter — extract entry and close overlay, then restore outside borrow
        if keycode == 0x24 {
            let entry = {
                let state = self.ivars().recent_projects.borrow();
                state.as_ref().and_then(|s| {
                    let item = s.items.get(s.selected)?;
                    if !item.render.invalid { Some(item.entry.clone()) } else { None }
                })
            };
            if let Some(entry) = entry {
                *self.ivars().recent_projects.borrow_mut() = None;
                self.restore_recent_project(&entry);
            }
            return;
        }

        // Cmd+Backspace — remove entry
        if keycode == 0x33 {
            let has_cmd = event.modifierFlags().contains(NSEventModifierFlags::Command);
            if has_cmd {
                let path = {
                    let mut guard = self.ivars().recent_projects.borrow_mut();
                    let state = match guard.as_mut() {
                        Some(s) => s,
                        None => return,
                    };
                    if state.selected >= state.items.len() {
                        return;
                    }
                    let path = state.items[state.selected].entry.path.clone();
                    state.items.remove(state.selected);
                    if state.items.is_empty() {
                        *guard = None;
                    } else if state.selected >= state.items.len() {
                        state.selected = state.items.len() - 1;
                    }
                    path
                };
                crate::recent_projects::remove(&path);
                self.mark_dirty();
                return;
            }
        }

        // Arrow keys
        {
            let mut guard = self.ivars().recent_projects.borrow_mut();
            let state = match guard.as_mut() {
                Some(s) => s,
                None => return,
            };
            match keycode {
                0x7E => { // Up
                    if state.selected > 0 {
                        state.selected -= 1;
                        if state.selected < state.scroll {
                            state.scroll = state.selected;
                        }
                    }
                }
                0x7D => { // Down
                    if state.selected + 1 < state.items.len() {
                        state.selected += 1;
                    }
                }
                _ => {}
            }
        }
        self.mark_dirty();
    }

    /// Restore a recent project as a new tab in this window.
    fn restore_recent_project(&self, entry: &crate::recent_projects::RecentProject) {
        let config = self.ivars().config.get().unwrap();
        let cols = config.terminal.columns;
        let rows = config.terminal.rows;

        match crate::session::restore_saved_tab(&entry.tab, cols, rows, config) {
            Some(tab) => {
                let mut tabs = self.ivars().tabs.borrow_mut();
                let new_idx = tabs.len();
                tabs.push(tab);
                drop(tabs);
                self.ivars().active_tab.set(new_idx);
                self.resize_all_panes();
                log::info!("Restored recent project: {}", entry.path);
            }
            None => {
                log::warn!("Failed to restore recent project: {}", entry.path);
            }
        }
    }

    /// Handle key events in the "Send Tab to Window" overlay.
    fn handle_send_to_window_key(&self, event: &NSEvent) {
        let keycode = event.keyCode();

        // Escape → close
        if keycode == 0x35 {
            *self.ivars().send_to_window.borrow_mut() = None;
            self.mark_dirty();
            return;
        }

        // Enter → confirm selection
        if keycode == 0x24 {
            let target = {
                let state = self.ivars().send_to_window.borrow();
                state.as_ref().map(|s| s.entries[s.selected].window_index)
            };
            if let Some(window_index) = target {
                *self.ivars().send_to_window.borrow_mut() = None;
                self.send_active_tab_to(window_index);
            }
            return;
        }

        // Arrow keys
        {
            let mut guard = self.ivars().send_to_window.borrow_mut();
            let state = match guard.as_mut() {
                Some(s) => s,
                None => return,
            };
            match keycode {
                0x7E => { // Up
                    if state.selected > 0 {
                        state.selected -= 1;
                    }
                }
                0x7D => { // Down
                    if state.selected + 1 < state.entries.len() {
                        state.selected += 1;
                    }
                }
                _ => {}
            }
        }
        self.mark_dirty();
    }

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

    /// Send the active tab to another window.
    /// - 1 tab + no other window → no-op (would leave nothing)
    /// - 1 tab + other windows → overlay (no "New Window" option)
    /// - 2+ tabs + no other window → detach to new window directly
    /// - 2+ tabs + other windows → overlay with "New Window" option
    fn do_detach_tab(&self) {
        let tab_count = self.ivars().tabs.borrow().len();
        let mtm = unsafe { MainThreadMarker::new_unchecked() };
        let source = self.window().unwrap();
        let others = crate::app::list_other_windows(mtm, &source);
        let is_last_tab = tab_count <= 1;

        if is_last_tab && others.is_empty() {
            log::debug!("do_detach_tab: single tab, single window, ignoring");
            return;
        }

        if others.is_empty() {
            // Multiple tabs, no other window — detach directly
            self.detach_active_tab_to_new_window();
        } else if others.len() == 1 && is_last_tab {
            // Last tab, single other window — send directly
            self.send_active_tab_to(Some(others[0].index));
        } else {
            // Show overlay
            let mut entries: Vec<SendToWindowEntry> = others.into_iter()
                .map(|info| SendToWindowEntry {
                    label: info.label,
                    window_index: Some(info.index),
                })
                .collect();
            // Only offer "New Window" if this isn't the last tab
            if !is_last_tab {
                entries.push(SendToWindowEntry {
                    label: "New Window".to_string(),
                    window_index: None,
                });
            }
            *self.ivars().send_to_window.borrow_mut() = Some(SendToWindowState {
                entries,
                selected: 0,
            });
            self.mark_dirty();
        }
    }

    /// Detach the active tab to a new window (no overlay).
    fn detach_active_tab_to_new_window(&self) {
        let mut tabs = self.ivars().tabs.borrow_mut();
        let idx = self.ivars().active_tab.get();
        if idx >= tabs.len() || tabs.len() <= 1 {
            return;
        }
        let tab = tabs.remove(idx);
        let new_idx = if idx >= tabs.len() { tabs.len() - 1 } else { idx };
        self.ivars().active_tab.set(new_idx);
        drop(tabs);
        self.resize_all_panes();

        let source_frame = self.window().map(|w| w.frame());
        let mtm = unsafe { MainThreadMarker::new_unchecked() };
        crate::app::detach_tab_to_new_window(mtm, tab, source_frame);
    }

    /// Send the active tab to a specific window (by index) or a new window.
    fn send_active_tab_to(&self, window_index: Option<usize>) {
        let mut tabs = self.ivars().tabs.borrow_mut();
        let idx = self.ivars().active_tab.get();
        if idx >= tabs.len() {
            return;
        }
        let is_last = tabs.len() == 1;
        let tab = tabs.remove(idx);
        if !is_last {
            let new_idx = if idx >= tabs.len() { tabs.len() - 1 } else { idx };
            self.ivars().active_tab.set(new_idx);
        }
        drop(tabs);

        let mtm = unsafe { MainThreadMarker::new_unchecked() };
        match window_index {
            Some(wi) => crate::app::send_tab_to_window(mtm, tab, wi),
            None => {
                let source_frame = self.window().map(|w| w.frame());
                crate::app::detach_tab_to_new_window(mtm, tab, source_frame);
            }
        }

        if is_last {
            // Close this window — it's now empty
            self.ivars().skip_session_save.set(true);
            self.ivars().closing.set(true);
        } else {
            self.resize_all_panes();
        }
    }

    /// Break the focused pane out of its split into a new tab.
    /// No-op if the pane is already alone (single leaf tab).
    fn do_break_pane(&self) {
        let mut tabs = self.ivars().tabs.borrow_mut();
        let idx = self.ivars().active_tab.get();
        if idx >= tabs.len() {
            return;
        }

        // No-op if already a single pane
        if tabs[idx].is_single_pane() {
            log::debug!("do_break_pane: pane is already alone, ignoring");
            return;
        }

        let focused_id = tabs[idx].focused_pane;
        log::debug!("do_break_pane: extracting pane {} from tab {}", focused_id, idx);

        // Find a neighbor to focus in the remaining tree
        let panes_vp = self.panes_viewport_for_tab(&tabs[idx]);
        let next_focus = tabs[idx].neighbor(focused_id, NavDirection::Right, panes_vp)
            .or_else(|| tabs[idx].neighbor(focused_id, NavDirection::Left, panes_vp))
            .or_else(|| tabs[idx].neighbor(focused_id, NavDirection::Down, panes_vp))
            .or_else(|| tabs[idx].neighbor(focused_id, NavDirection::Up, panes_vp));

        let old_columns = tabs[idx].num_columns();

        // Extract the pane from the tab
        match tabs[idx].extract_pane(focused_id) {
            Some(extracted) => {
                // Update the source tab
                let new_focus = next_focus
                    .filter(|id| tabs[idx].contains(*id))
                    .unwrap_or_else(|| tabs[idx].first_pane().id);
                tabs[idx].focused_pane = new_focus;
                let new_columns = tabs[idx].num_columns();
                tabs[idx].scale_virtual_width(old_columns, new_columns);
                tabs[idx].minimized_stack.retain(|&pid| pid != focused_id);

                let full = self.drawable_viewport();
                let min_w = self.min_split_width_px();
                tabs[idx].clamp_scroll(full.width, min_w);
                let tab = &mut tabs[idx];
                self.scroll_to_reveal_pane(tab, new_focus, full.width);

                // Create a new tab from the extracted pane
                let new_tab = Tab {
                    id: alloc_tab_id(),
                    columns: vec![crate::pane::Column::new(extracted)],
                    column_weights: vec![1.0],
                    custom_weights: vec![false],
                    focused_pane: focused_id,
                    custom_title: None,
                    color: None,
                    has_bell: false,
                    has_completion: false,
                    minimized_stack: Vec::new(),
                    scroll_offset_x: 0.0,
                    virtual_width_override: 0.0,
                    cell_h: std::cell::Cell::new(0.0),
                };

                // Resize the source tab's remaining panes while it's still active
                drop(tabs);
                self.resize_all_panes();

                // Insert the new tab right after the current one and switch to it
                let mut tabs = self.ivars().tabs.borrow_mut();
                let new_idx = idx + 1;
                tabs.insert(new_idx, new_tab);
                self.ivars().active_tab.set(new_idx);
                drop(tabs);
                self.resize_all_panes();
            }
            None => {
                log::error!("do_break_pane: extract_pane returned None unexpectedly");
            }
        }
    }

    /// Merge the current tab into another tab (show overlay to pick target).
    /// No-op if there's only one tab.
    fn do_merge_tab(&self) {
        let tabs = self.ivars().tabs.borrow();
        if tabs.len() <= 1 {
            log::debug!("do_merge_tab: only one tab, ignoring");
            return;
        }
        let active = self.ivars().active_tab.get();
        let entries: Vec<MergeTabEntry> = tabs.iter().enumerate()
            .filter(|(i, _)| *i != active)
            .map(|(i, t)| MergeTabEntry {
                label: t.title(),
                tab_index: i,
            })
            .collect();
        drop(tabs);

        if entries.len() == 1 {
            // Only one possible target — merge directly
            let target = entries[0].tab_index;
            self.merge_active_tab_into(target);
        } else {
            *self.ivars().merge_tab.borrow_mut() = Some(MergeTabState {
                entries,
                selected: 0,
            });
            self.mark_dirty();
        }
    }

    /// Handle key events in the "Merge Tab" overlay.
    fn handle_merge_tab_key(&self, event: &NSEvent) {
        let keycode = event.keyCode();

        // Escape → close
        if keycode == 0x35 {
            *self.ivars().merge_tab.borrow_mut() = None;
            self.mark_dirty();
            return;
        }

        // Enter → confirm selection
        if keycode == 0x24 {
            let target = {
                let state = self.ivars().merge_tab.borrow();
                state.as_ref().map(|s| s.entries[s.selected].tab_index)
            };
            if let Some(target_idx) = target {
                *self.ivars().merge_tab.borrow_mut() = None;
                self.merge_active_tab_into(target_idx);
            }
            return;
        }

        // Arrow keys
        {
            let mut guard = self.ivars().merge_tab.borrow_mut();
            let state = match guard.as_mut() {
                Some(s) => s,
                None => return,
            };
            match keycode {
                0x7E => { // Up
                    if state.selected > 0 {
                        state.selected -= 1;
                    }
                }
                0x7D => { // Down
                    if state.selected + 1 < state.entries.len() {
                        state.selected += 1;
                    }
                }
                _ => {}
            }
        }
        self.mark_dirty();
    }

    /// Merge the active tab's columns into the target tab (appended to the right).
    /// The active tab is removed. Focus moves to the leftmost pane of the merged columns.
    fn merge_active_tab_into(&self, target_idx: usize) {
        let mut tabs = self.ivars().tabs.borrow_mut();
        let active = self.ivars().active_tab.get();
        if active >= tabs.len() || target_idx >= tabs.len() || active == target_idx {
            return;
        }

        // Remove the source tab first
        let source = tabs.remove(active);

        // Adjust target index after removal
        let target = if target_idx > active { target_idx - 1 } else { target_idx };

        // The leftmost pane in the source becomes the new focus
        let new_focus = source.columns.first()
            .and_then(|col| col.panes.first())
            .map(|p| p.id)
            .unwrap_or(source.focused_pane);

        // Append source columns to target tab
        let avg_weight: f32 = tabs[target].column_weights.iter().sum::<f32>()
            / tabs[target].columns.len() as f32;
        for (col, weight) in source.columns.into_iter().zip(source.column_weights.into_iter()) {
            tabs[target].columns.push(col);
            // Use source weights scaled to target's average
            tabs[target].column_weights.push(avg_weight * weight);
            tabs[target].custom_weights.push(false);
        }

        // Merge minimized stacks
        tabs[target].minimized_stack.extend(source.minimized_stack);

        // Focus the leftmost pane from the merged columns
        tabs[target].focused_pane = new_focus;

        // Switch to the target tab
        self.ivars().active_tab.set(target);

        drop(tabs);
        self.resize_all_panes();
    }

    /// Get tab titles for this window (used by "Send Tab to Window" overlay).
    pub fn tab_titles(&self) -> Vec<String> {
        let tabs = self.ivars().tabs.borrow();
        tabs.iter().map(|t| t.title()).collect()
    }

    /// Append external tabs (used by send-tab-to-window).
    pub fn append_tabs(&self, new_tabs: Vec<crate::pane::Tab>) {
        let mut tabs = self.ivars().tabs.borrow_mut();
        let first_new = tabs.len();
        tabs.extend(new_tabs);
        drop(tabs);
        self.ivars().active_tab.set(first_new);
        self.resize_all_panes();
    }

    // ---------------------------------------------------------------
    // IPC methods (called from app.rs IPC command handlers)
    // ---------------------------------------------------------------

    /// IPC: create a split in the active tab's focused pane.
    /// Returns the new pane's ID on success.
    pub fn ipc_split(
        &self,
        config: &crate::config::Config,
        direction: SplitDirection,
        cwd: Option<&str>,
        command: Option<String>,
    ) -> Option<PaneId> {
        let (focused_id, current_vp) = {
            let tabs = self.ivars().tabs.borrow();
            let idx = self.ivars().active_tab.get();
            let tab = tabs.get(idx)?;
            let fid = tab.focused_pane;
            let vp = tab.viewport_for_pane(fid, self.panes_viewport_for_tab(tab))?;
            (fid, vp)
        };

        let half_vp = match direction {
            SplitDirection::Horizontal => PaneViewport {
                x: current_vp.x,
                y: current_vp.y,
                width: current_vp.width / 2.0,
                height: current_vp.height,
            },
            SplitDirection::Vertical => PaneViewport {
                x: current_vp.x,
                y: current_vp.y,
                width: current_vp.width,
                height: current_vp.height / 2.0,
            },
        };
        let (cols, rows) = self.viewport_to_grid(&half_vp);

        let new_pane = match Pane::spawn(cols, rows, config, cwd) {
            Ok(p) => p,
            Err(e) => {
                log::error!("IPC split: failed to spawn pane: {}", e);
                return None;
            }
        };

        // If a command was provided, set it as pending (will be injected once shell is ready)
        if let Some(cmd) = command {
            new_pane.pending_command.set(Some(cmd));
        }

        let new_id = new_pane.id;

        let mut tabs = self.ivars().tabs.borrow_mut();
        let idx = self.ivars().active_tab.get();
        if let Some(tab) = tabs.get_mut(idx) {
            match direction {
                SplitDirection::Horizontal => {
                    tab.insert_column_after_focused(new_pane);
                }
                SplitDirection::Vertical => {
                    tab.vsplit_at_pane(focused_id, new_pane);
                }
            }
            tab.focused_pane = new_id;
            self.scroll_to_reveal_pane(tab, new_id, self.drawable_viewport().width);
        }
        drop(tabs);

        self.resize_all_panes();
        log::info!("IPC: split created pane {}", new_id);
        Some(new_id)
    }

    /// IPC: close a specific pane by ID. Returns true if found and closed.
    /// Try to close a pane by ID. Returns:
    /// - Some(true) = closed successfully
    /// - Some(false) = found but refused (last pane in last tab)
    /// - None = pane not in this window
    pub fn ipc_close_pane(&self, pane_id: PaneId) -> Option<bool> {
        let mut tabs = self.ivars().tabs.borrow_mut();

        // Find which tab contains this pane
        let tab_idx = match tabs.iter().position(|tab| tab.contains(pane_id)) {
            Some(i) => i,
            None => return None,
        };

        // If it's the sole pane in the sole tab, refuse (would close the window)
        if tabs.len() == 1 && tabs[0].is_single_pane() {
            return Some(false);
        }

        if tabs[tab_idx].is_single_pane() {
            // Close the entire tab
            crate::recent_projects::add(&tabs[tab_idx]);
            tabs.remove(tab_idx);
            if tabs.is_empty() {
                drop(tabs);
                self.ivars().closing.set(true);
                return Some(true);
            }
            let new_idx = if tab_idx >= tabs.len() { tabs.len() - 1 } else { tab_idx };
            drop(tabs);
            self.ivars().active_tab.set(new_idx);
            self.resize_all_panes();
            log::info!("IPC: closed tab containing pane {}", pane_id);
            return Some(true);
        }

        // Multiple panes — close just this pane
        let panes_vp = self.panes_viewport_for_tab(&tabs[tab_idx]);
        let next_focus = tabs[tab_idx].neighbor(pane_id, crate::pane::NavDirection::Right, panes_vp)
            .or_else(|| tabs[tab_idx].neighbor(pane_id, crate::pane::NavDirection::Left, panes_vp))
            .or_else(|| tabs[tab_idx].neighbor(pane_id, crate::pane::NavDirection::Down, panes_vp))
            .or_else(|| tabs[tab_idx].neighbor(pane_id, crate::pane::NavDirection::Up, panes_vp));

        let old_columns = tabs[tab_idx].num_columns();
        if !tabs[tab_idx].remove_pane(pane_id) {
            drop(tabs);
            return Some(false);
        }
        let new_focus = next_focus
            .filter(|id| tabs[tab_idx].contains(*id))
            .unwrap_or_else(|| tabs[tab_idx].first_pane().id);
        tabs[tab_idx].focused_pane = new_focus;
        let new_columns = tabs[tab_idx].num_columns();
        tabs[tab_idx].scale_virtual_width(old_columns, new_columns);
        tabs[tab_idx].minimized_stack.retain(|&pid| pid != pane_id);
        let full = self.drawable_viewport();
        let min_w = self.min_split_width_px();
        tabs[tab_idx].clamp_scroll(full.width, min_w);
        let tab = &mut tabs[tab_idx];
        self.scroll_to_reveal_pane(tab, new_focus, full.width);
        drop(tabs);
        self.resize_all_panes();
        log::info!("IPC: closed pane {}", pane_id);
        Some(true)
    }

    /// IPC: get the CWD of the focused pane (for split fallback).
    pub fn ipc_focused_cwd(&self) -> Option<String> {
        let tabs = self.ivars().tabs.borrow();
        let idx = self.ivars().active_tab.get();
        tabs.get(idx).and_then(|tab| {
            tab.pane(tab.focused_pane).and_then(|p| p.cwd())
        })
    }

    /// IPC: collect pane info as JSON values for the list-panes command.
    pub fn ipc_collect_panes(&self, win_idx: usize, is_key_window: bool, out: &mut Vec<serde_json::Value>) {
        let tabs = self.ivars().tabs.borrow();
        let active_tab = self.ivars().active_tab.get();
        for (tab_idx, tab) in tabs.iter().enumerate() {
            let focused_id = tab.focused_pane;
            let is_active_tab = tab_idx == active_tab;
            tab.for_each_pane(&mut |pane| {
                let is_focused = pane.id == focused_id && is_active_tab && is_key_window;
                out.push(serde_json::json!({
                    "id": pane.id,
                    "window": win_idx,
                    "tab": tab_idx,
                    "cwd": pane.cwd().unwrap_or_default(),
                    "title": pane.display_title("shell"),
                    "focused": is_focused,
                }));
            });
        }
    }

    /// IPC: write text to a pane's PTY. Returns true if the pane was found.
    pub fn ipc_send_keys(&self, pane_id: PaneId, text: &str) -> bool {
        let tabs = self.ivars().tabs.borrow();
        for tab in tabs.iter() {
            if let Some(pane) = tab.pane(pane_id) {
                pane.pty.write(text.as_bytes());
                return true;
            }
        }
        false
    }

    /// IPC: focus a pane by ID (switch tab if needed). Returns true if found.
    pub fn ipc_focus_pane(&self, pane_id: PaneId) -> bool {
        let mut tabs = self.ivars().tabs.borrow_mut();
        let tab_idx = match tabs.iter().position(|tab| tab.contains(pane_id)) {
            Some(i) => i,
            None => return false,
        };

        tabs[tab_idx].focused_pane = pane_id;
        let full = self.drawable_viewport();
        let min_w = self.min_split_width_px();
        tabs[tab_idx].clamp_scroll(full.width, min_w);
        let tab = &mut tabs[tab_idx];
        self.scroll_to_reveal_pane(tab, pane_id, full.width);
        drop(tabs);

        self.ivars().active_tab.set(tab_idx);
        self.resize_all_panes();
        log::info!("IPC: focused pane {}", pane_id);
        true
    }

    /// Minimize the focused pane.
    fn do_minimize_pane(&self) {
        let mut tabs = self.ivars().tabs.borrow_mut();
        let idx = self.ivars().active_tab.get();
        if let Some(tab) = tabs.get_mut(idx) {
            let focused_id = tab.focused_pane;
            if tab.minimize_pane(focused_id) {
                tab.mark_all_dirty();
                let full = self.drawable_viewport();
                let min_w = self.min_split_width_px();
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
            if tab.restore_last_minimized() {
                tab.mark_all_dirty();
                let full = self.drawable_viewport();
                let min_w = self.min_split_width_px();
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
                // Clear completion and bell flags on the newly focused pane
                let t = new.terminal.read();
                t.command_completed.store(false, std::sync::atomic::Ordering::Relaxed);
                t.bell.store(false, std::sync::atomic::Ordering::Relaxed);
                t.dirty.store(true, std::sync::atomic::Ordering::Relaxed);
            }
            // Auto-scroll to reveal the newly focused pane
            self.scroll_to_reveal_pane(tab, neighbor_id, self.drawable_viewport().width);
        } else {
            // No neighbor in this direction → overflow to adjacent tab
            let count = tabs.len();
            if count <= 1 {
                return;
            }
            drop(tabs);
            let delta: i32 = match dir {
                NavDirection::Left | NavDirection::Up => -1,
                NavDirection::Right | NavDirection::Down => 1,
            };
            self.do_switch_tab_relative(delta);
            // Focus the appropriate pane in the new tab:
            // going right/down → first pane, going left/up → last pane
            let mut tabs = self.ivars().tabs.borrow_mut();
            let new_idx = self.ivars().active_tab.get();
            if let Some(new_tab) = tabs.get_mut(new_idx) {
                let target_id = match dir {
                    NavDirection::Right | NavDirection::Down => new_tab.first_pane().id,
                    NavDirection::Left | NavDirection::Up => new_tab.last_pane().id,
                };
                new_tab.focused_pane = target_id;
                // Auto-scroll to reveal the focused pane in the new tab
                self.scroll_to_reveal_pane(new_tab, target_id, self.drawable_viewport().width);
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

    /// Resize all panes in the active tab to match their current viewports.
    fn resize_all_panes(&self) {
        let renderer = match self.ivars().renderer.get() {
            Some(r) => r,
            None => return,
        };
        let renderer_r = renderer.read();
        let (cell_w, cell_h) = renderer_r.cell_size();
        let status_bar = renderer_r.status_bar_enabled();
        drop(renderer_r);

        let panes_vp = self.panes_viewport();
        let tabs = self.ivars().tabs.borrow();
        let idx = self.ivars().active_tab.get();
        if let Some(tab) = tabs.get(idx) {
            tab.cell_h.set(cell_h);
            tab.for_each_pane_with_viewport(panes_vp, &mut |pane, vp| {
                // Skip PTY resize for minimized panes (keep old dimensions)
                if pane.minimized {
                    return;
                }
                let cols = ((vp.width - 2.0 * crate::renderer::PANE_H_PADDING) / cell_w).floor().max(1.0) as u16;
                let usable_h = if status_bar { vp.height - cell_h } else { vp.height };
                let rows = (usable_h / cell_h).floor().max(1.0) as u16;
                let mut term = pane.terminal.write();
                if cols != term.cols || rows != term.rows {
                    term.resize(cols, rows);
                    drop(term);
                    pane.pty.resize(cols, rows);
                }
            });
        }
    }

    fn mark_dirty(&self) {
        if let Some(pane) = self.focused_pane() {
            pane.terminal.read().dirty.store(true, std::sync::atomic::Ordering::Relaxed);
        }
    }

    fn toggle_filter(&self) {
        let mut filter = self.ivars().filter.borrow_mut();
        if filter.is_some() {
            *filter = None;
        } else {
            *filter = Some(FilterState {
                query: String::new(),
                matches: Vec::new(),
            });
        }
        drop(filter);
        // Mark dirty to trigger redraw
        if let Some(pane) = self.focused_pane() {
            pane.terminal.read().dirty.store(true, std::sync::atomic::Ordering::Relaxed);
        }
    }

    fn handle_filter_key(&self, event: &NSEvent) {
        let chars = event.charactersIgnoringModifiers();
        let ch_str = chars.map(|s| s.to_string()).unwrap_or_default();
        let ch = ch_str.chars().next().unwrap_or('\0');

        let mut filter = self.ivars().filter.borrow_mut();
        let state = match filter.as_mut() {
            Some(s) => s,
            None => return,
        };

        match ch {
            '\u{1B}' => {
                // Escape → close filter without scrolling
                *filter = None;
                drop(filter);
                if let Some(pane) = self.focused_pane() {
                    pane.terminal.read().dirty.store(true, std::sync::atomic::Ordering::Relaxed);
                }
                return;
            }
            '\r' => {
                // Enter → close filter and scroll to first match
                let first_match = state.matches.first().map(|m| m.abs_line);
                *filter = None;
                drop(filter);
                if let Some(abs_line) = first_match {
                    if let Some(pane) = self.focused_pane() {
                        let mut term = pane.terminal.write();
                        term.scroll_to_abs_line(abs_line);
                    }
                }
                return;
            }
            '\u{7F}' | '\u{08}' => {
                // Backspace
                state.query.pop();
            }
            c if c >= ' ' && !c.is_control() => {
                state.query.push(c);
            }
            _ => return,
        }

        // Re-run search
        if let Some(pane) = self.focused_pane() {
            let term = pane.terminal.read();
            state.matches = term.search_lines(&state.query);
            term.dirty.store(true, std::sync::atomic::Ordering::Relaxed);
        }
    }

    fn start_rename_tab(&self) {
        // Pre-fill with current tab title
        let current_title = {
            let tabs = self.ivars().tabs.borrow();
            let idx = self.ivars().active_tab.get();
            tabs.get(idx).map(|t| t.title()).unwrap_or_default()
        };
        let cursor = current_title.chars().count();
        *self.ivars().rename_tab.borrow_mut() = Some(RenameTabState {
            input: current_title,
            cursor,
        });
        self.mark_dirty();
    }

    fn handle_rename_tab_key(&self, event: &NSEvent) {
        let key_code = event.keyCode();
        let chars = event.charactersIgnoringModifiers();
        let ch_str = chars.map(|s| s.to_string()).unwrap_or_default();
        let ch = ch_str.chars().next().unwrap_or('\0');

        let mut rename = self.ivars().rename_tab.borrow_mut();
        let state = match rename.as_mut() {
            Some(s) => s,
            None => return,
        };

        match key_code {
            123 => {
                // Left arrow
                if state.cursor > 0 { state.cursor -= 1; }
            }
            124 => {
                // Right arrow
                let len = state.input.chars().count();
                if state.cursor < len { state.cursor += 1; }
            }
            _ => match ch {
                '\u{1B}' => {
                    // Escape → cancel rename
                    *rename = None;
                    drop(rename);
                    self.mark_dirty();
                    return;
                }
                '\r' => {
                    // Enter → apply rename (empty = reset to auto)
                    let new_title = if state.input.trim().is_empty() {
                        None
                    } else {
                        Some(state.input.clone())
                    };
                    *rename = None;
                    drop(rename);
                    let mut tabs = self.ivars().tabs.borrow_mut();
                    let idx = self.ivars().active_tab.get();
                    if let Some(tab) = tabs.get_mut(idx) {
                        tab.custom_title = new_title;
                    }
                    drop(tabs);
                    self.mark_dirty();
                    return;
                }
                '\u{7F}' | '\u{08}' => {
                    // Backspace — remove char before cursor
                    if state.cursor > 0 {
                        if let Some((byte_idx, _)) = state.input.char_indices().nth(state.cursor - 1) {
                            state.input.remove(byte_idx);
                            state.cursor -= 1;
                        }
                    }
                }
                c if c >= ' ' && !c.is_control() => {
                    let byte_idx = state.input.char_indices()
                        .nth(state.cursor).map(|(i, _)| i)
                        .unwrap_or(state.input.len());
                    state.input.insert(byte_idx, c);
                    state.cursor += 1;
                }
                _ => return,
            }
        }
        drop(rename);
        self.mark_dirty();
    }

    fn start_rename_pane(&self) {
        let current_title = {
            let tabs = self.ivars().tabs.borrow();
            let idx = self.ivars().active_tab.get();
            tabs.get(idx).and_then(|tab| {
                let pane = tab.pane(tab.focused_pane)?;
                if let Some(ref custom) = pane.custom_title {
                    Some(custom.clone())
                } else {
                    pane.terminal.read().title.clone()
                }
            }).unwrap_or_default()
        };
        let cursor = current_title.chars().count();
        *self.ivars().rename_pane.borrow_mut() = Some(RenamePaneState {
            input: current_title,
            cursor,
        });
        self.mark_dirty();
    }

    fn handle_rename_pane_key(&self, event: &NSEvent) {
        let key_code = event.keyCode();
        let chars = event.charactersIgnoringModifiers();
        let ch_str = chars.map(|s| s.to_string()).unwrap_or_default();
        let ch = ch_str.chars().next().unwrap_or('\0');

        let mut rename = self.ivars().rename_pane.borrow_mut();
        let state = match rename.as_mut() {
            Some(s) => s,
            None => return,
        };

        match key_code {
            123 => {
                // Left arrow
                if state.cursor > 0 { state.cursor -= 1; }
            }
            124 => {
                // Right arrow
                let len = state.input.chars().count();
                if state.cursor < len { state.cursor += 1; }
            }
            _ => match ch {
                '\u{1B}' => {
                    // Escape → cancel rename
                    *rename = None;
                    drop(rename);
                    self.mark_dirty();
                    return;
                }
                '\r' => {
                    // Enter → apply rename (empty = reset to auto)
                    let new_title = if state.input.trim().is_empty() {
                        None
                    } else {
                        Some(state.input.clone())
                    };
                    *rename = None;
                    drop(rename);
                    let mut tabs = self.ivars().tabs.borrow_mut();
                    let idx = self.ivars().active_tab.get();
                    if let Some(tab) = tabs.get_mut(idx) {
                        if let Some(pane) = tab.pane_mut(tab.focused_pane) {
                            pane.custom_title = new_title;
                        }
                    }
                    drop(tabs);
                    self.mark_dirty();
                    return;
                }
                '\u{7F}' | '\u{08}' => {
                    // Backspace — remove char before cursor
                    if state.cursor > 0 {
                        if let Some((byte_idx, _)) = state.input.char_indices().nth(state.cursor - 1) {
                            state.input.remove(byte_idx);
                            state.cursor -= 1;
                        }
                    }
                }
                c if c >= ' ' && !c.is_control() => {
                    let byte_idx = state.input.char_indices()
                        .nth(state.cursor).map(|(i, _)| i)
                        .unwrap_or(state.input.len());
                    state.input.insert(byte_idx, c);
                    state.cursor += 1;
                }
                _ => return,
            }
        }
        drop(rename);
        self.mark_dirty();
    }

    fn handle_filter_click(&self, _px: f32, py: f32) {
        let renderer = match self.ivars().renderer.get() {
            Some(r) => r,
            None => return,
        };
        let (_, cell_h) = renderer.read().cell_size();

        // The overlay starts with: 1 row search bar + matches below
        let match_start_y = {
            let panes_vp = self.panes_viewport();
            panes_vp.y + cell_h // search bar takes 1 row
        };

        let click_row = ((py - match_start_y) / cell_h).floor() as i32;
        if click_row < 0 {
            return;
        }

        let mut filter = self.ivars().filter.borrow_mut();
        let abs_line = match filter.as_ref() {
            Some(state) => {
                let idx = click_row as usize;
                state.matches.get(idx).map(|m| m.abs_line)
            }
            None => return,
        };

        *filter = None;
        drop(filter);

        if let Some(abs_line) = abs_line {
            if let Some(pane) = self.focused_pane() {
                let mut term = pane.terminal.write();
                term.scroll_to_abs_line(abs_line);
            }
        }
    }

    fn handle_resize(&self) {
        let Some(layer) = self.ivars().metal_layer.get() else { return };
        let Some(renderer) = self.ivars().renderer.get() else { return };

        let scale = self.window().map_or(2.0, |w| w.backingScaleFactor());
        let frame = self.frame();
        layer.setContentsScale(scale);
        layer.setDrawableSize(CGSize {
            width: frame.size.width * scale,
            height: frame.size.height * scale,
        });

        // Rebuild glyph atlas if scale changed (e.g. moved to different display)
        if (scale - self.ivars().last_scale.get()).abs() > 0.01 {
            log::debug!("Scale changed: {} -> {}", self.ivars().last_scale.get(), scale);
            self.ivars().last_scale.set(scale);
            renderer.write().rebuild_atlas(scale);
        }

        self.resize_all_panes();
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
    pub fn append_session_data(&self, out: &mut Vec<crate::session::WindowSession>) {
        let tabs = self.ivars().tabs.borrow();
        let active_tab = self.ivars().active_tab.get();
        let frame = self.window().map(|win| {
            let f = win.frame();
            (f.origin.x, f.origin.y, f.size.width, f.size.height)
        });
        out.push(crate::session::WindowSession::from_tabs(&tabs, active_tab, frame));
    }

    /// Initialize Metal rendering with the given tabs.
    pub fn setup_metal(&self, _mtm: MainThreadMarker, config: &Config, tabs: Vec<Tab>, active_tab: usize) {
        log::info!("Setting up Metal");
        let device = MTLCreateSystemDefaultDevice()
            .expect("no Metal device");

        let layer = CAMetalLayer::new();
        layer.setDevice(Some(&device));
        layer.setPixelFormat(objc2_metal::MTLPixelFormat::BGRA8Unorm);
        layer.setFramebufferOnly(true);

        let frame = self.frame();
        let scale = if let Some(window) = self.window() {
            window.backingScaleFactor()
        } else {
            2.0
        };
        layer.setContentsScale(scale);
        layer.setDrawableSize(CGSize {
            width: frame.size.width * scale,
            height: frame.size.height * scale,
        });

        self.setWantsLayer(true);
        self.setLayer(Some(&layer));
        self.ivars().metal_layer.set(layer.clone()).ok();

        self.ivars().last_scale.set(scale);

        let terminal_for_renderer = tabs[active_tab].first_pane().terminal.clone();

        let renderer = Arc::new(parking_lot::RwLock::new(
            Renderer::new(&device, &layer, terminal_for_renderer, scale, config),
        ));

        self.ivars().renderer.set(renderer).ok();
        self.ivars().config.set(config.clone()).ok();
        self.ivars().keybindings.set(Keybindings::from_config(&config.keys)).ok();
        self.ivars().git_poll_interval.set(config.terminal.fps * 2);
        self.ivars().help_hint_frames.set(config.terminal.fps * 3);
        *self.ivars().tabs.borrow_mut() = tabs;
        self.ivars().active_tab.set(active_tab);
    }

    /// Called by the global render timer in AppDelegate for each window.
    /// Handles all per-frame work: command injection, auto-scroll, git polling,
    /// pane reaping, rendering, focus reporting, and window title updates.
    /// Returns `false` if the window has no tabs left and should be closed.
    pub fn tick(&self) -> bool {
        let ivars = self.ivars();
        if ivars.closing.get() {
            return false;
        }
        let renderer = match ivars.renderer.get() {
            Some(r) => r.clone(),
            None => return true, // not yet initialized
        };
        let layer = match ivars.metal_layer.get() {
            Some(l) => l.clone(),
            None => return true,
        };

        // --- Inject pending commands for restored panes ---
        {
            let tabs = ivars.tabs.borrow();
            for tab in tabs.iter() {
                tab.for_each_pane(&mut |pane| {
                    pane.inject_pending_command();
                });
            }
        }

        // --- Progressive restore of deferred tabs (one per tick) ---
        {
            let mut deferred = ivars.deferred_tabs.borrow_mut();
            if let Some((tab_idx, saved_tab)) = deferred.pop() {
                let config = ivars.config.get().unwrap();
                let cols = config.terminal.columns;
                let rows = config.terminal.rows;
                let t = std::time::Instant::now();
                let pane_count = crate::session::count_panes_in_saved_tab(&saved_tab);
                match crate::session::restore_saved_tab(&saved_tab, cols, rows, config) {
                    Some(tab) => {
                        log::info!("[STARTUP] deferred tab {} restored ({} panes) in {:?}", tab_idx, pane_count, t.elapsed());
                        let mut tabs = ivars.tabs.borrow_mut();
                        if tab_idx < tabs.len() {
                            // Kill the placeholder pane before replacing
                            tabs[tab_idx] = tab;
                        }
                        drop(tabs);
                        self.resize_all_panes();
                    }
                    None => log::warn!("Failed to restore deferred tab {}", tab_idx),
                }
            }
        }

        // --- Auto-scroll during drag selection ---
        {
            let speed = ivars.auto_scroll_speed.get();
            if speed != 0 {
                let tabs = ivars.tabs.borrow();
                let idx = ivars.active_tab.get();
                if let Some(tab) = tabs.get(idx) {
                    if let Some(pane) = tab.pane(tab.focused_pane) {
                        let mut term = pane.terminal.write();
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
                }
            }
        }

        // --- Poll git branch for all panes with a CWD ---
        let git_poll_interval = ivars.git_poll_interval.get();
        let count = ivars.git_poll_counter.get() + 1;
        ivars.git_poll_counter.set(count);
        if count >= git_poll_interval {
            ivars.git_poll_counter.set(0);
            let tabs = ivars.tabs.borrow();
            for tab in tabs.iter() {
                tab.for_each_pane(&mut |pane| {
                    let term = pane.terminal.read();
                    let cwd = term.cwd.clone();
                    let old_branch = term.git_branch.clone();
                    drop(term);
                    if let Some(ref cwd) = cwd {
                        let new_branch = crate::terminal::parser::resolve_git_branch(cwd);
                        if new_branch != old_branch {
                            let mut term = pane.terminal.write();
                            term.git_branch = new_branch;
                            term.dirty.store(true, std::sync::atomic::Ordering::Relaxed);
                        }
                    }
                });
            }
        }

        // --- Reap exited panes across ALL tabs ---
        let mut any_removed = false;
        let mut tabs_to_remove: Vec<usize> = Vec::new();
        {
            let mut tabs = ivars.tabs.borrow_mut();
            for (tab_idx, tab) in tabs.iter_mut().enumerate() {
                let exited = tab.exited_pane_ids();
                if exited.is_empty() {
                    continue;
                }
                any_removed = true;
                log::debug!("Reaping exited panes in tab {}: {:?}", tab_idx, exited);
                for id in &exited {
                    let old_cols = tab.num_columns();
                    if !tab.remove_pane(*id) {
                        tabs_to_remove.push(tab_idx);
                        break;
                    }
                    let new_cols = tab.num_columns();
                    tab.scale_virtual_width(old_cols, new_cols);
                    tab.minimized_stack.retain(|&pid| pid != *id);
                }
                if exited.contains(&tab.focused_pane) {
                    if !tabs_to_remove.contains(&tab_idx) {
                        tab.focused_pane = tab.first_pane().id;
                    }
                }
            }
            for &idx in tabs_to_remove.iter().rev() {
                tabs.remove(idx);
            }
        }

        // Adjust active_tab if needed; signal close if no tabs left
        if any_removed {
            let tabs = ivars.tabs.borrow();
            if tabs.is_empty() {
                drop(tabs);
                return false;
            }
            let active = ivars.active_tab.get();
            if active >= tabs.len() {
                ivars.active_tab.set(tabs.len() - 1);
            }
        }

        // Build pane render list from active tab only
        let active_idx = ivars.active_tab.get();
        let split_min_w = ivars.config.get()
            .map(|c| c.splits.min_width)
            .unwrap_or(300.0)
            * ivars.last_scale.get().max(1.0) as f32;
        let (pane_data, pty_ptr, focus_reporting, tab_titles, active_panes_vp, screen_width, total_columns, focused_column, active_tab, total_tabs, active_tab_name) = {
            let mut tabs = ivars.tabs.borrow_mut();
            if tabs.is_empty() {
                return false;
            }
            let tab = &mut tabs[active_idx];
            let focused_id = tab.focused_pane;

            let mut pane_data: Vec<crate::renderer::PaneRenderData> = Vec::new();
            let cell_h = renderer.read().cell_size().1;
            tab.cell_h.set(cell_h);
            let tab_bar_h = (cell_h * 2.0).round();
            let drawable_size = layer.drawableSize();
            let screen_width = drawable_size.width as f32;
            let virtual_width = tab.virtual_width(screen_width, split_min_w);
            let global_bar_h = cell_h;
            let panes_vp = PaneViewport {
                x: -tab.scroll_offset_x,
                y: tab_bar_h,
                width: virtual_width,
                height: drawable_size.height as f32 - tab_bar_h - global_bar_h,
            };
            tab.for_each_pane_with_viewport(panes_vp, &mut |pane, vp| {
                let is_focused = pane.id == focused_id;
                let term = pane.terminal.read();
                let completed = !is_focused
                    && term.command_completed.load(std::sync::atomic::Ordering::Relaxed);
                // Read bell without consuming — check_bell will drain it for tab-level
                let has_bell = !is_focused
                    && term.bell.load(std::sync::atomic::Ordering::Relaxed);
                // Log pane→terminal mapping when scrolled (cross-terminal bug investigation)
                if term.scroll_offset() > 0 {
                    log::info!("RENDER-SCROLLED tab={} pane={} term_id={} scroll_offset={} sb_len={} cwd={:?}",
                        active_idx, pane.id, term.terminal_id, term.scroll_offset(), term.scrollback_len(),
                        term.cwd);
                }
                drop(term);
                pane_data.push(crate::renderer::PaneRenderData {
                    terminal: pane.terminal.clone(),
                    viewport: vp,
                    shell_ready: pane.is_ready(),
                    is_focused,
                    pane_id: pane.id,
                    display_title: pane.display_title("shell"),
                    custom_title: pane.custom_title.clone(),
                    has_completion: completed,
                    has_bell,
                    minimized: pane.minimized,
                    input_chars: pane.pty.input_chars.clone(),
                });
            });

            // Propagate OSC 1 sticky titles to pane custom_title
            for entry in &mut pane_data {
                let has_osc1 = entry.terminal.read().osc1_title.is_some();
                if has_osc1 {
                    let sticky = entry.terminal.write().osc1_title.take().unwrap();
                    let title = if sticky.is_empty() { None } else { Some(sticky) };
                    if let Some(pane) = tab.pane_mut(entry.pane_id) {
                        pane.custom_title = title.clone();
                    }
                    entry.custom_title = title;
                }
            }

            // Override custom_title for focused pane when rename_pane is active
            {
                let rename_pane = ivars.rename_pane.borrow();
                if let Some(ref rs) = *rename_pane {
                    for entry in &mut pane_data {
                        if entry.is_focused {
                            let before: String = rs.input.chars().take(rs.cursor).collect();
                            let after: String = rs.input.chars().skip(rs.cursor).collect();
                            entry.custom_title = Some(format!("{}▏{}", before, after));
                        }
                    }
                }
            }

            let focused = tab.pane(focused_id);
            let pty_ptr = focused.map(|p| &p.pty as *const crate::terminal::pty::Pty);
            let focus_reporting = focused.map_or(false, |p| p.terminal.read().focus_reporting);

            for (i, t) in tabs.iter_mut().enumerate() {
                t.check_bell();
                // Skip active tab: completion already read into pane_data
                if i != active_idx {
                    t.check_completion();
                }
            }
            tabs[active_idx].clear_bell();
            // Derive active tab's completion from pane_data (avoids double atomic read)
            tabs[active_idx].has_completion = pane_data.iter().any(|p| p.has_completion);

            let rename = ivars.rename_tab.borrow();
            let tab_titles: Vec<(String, bool, Option<usize>, bool, bool, bool)> = tabs.iter().enumerate()
                .map(|(i, t)| {
                    let is_renaming = i == active_idx && rename.is_some();
                    let title = if is_renaming {
                        let rs = rename.as_ref().unwrap();
                        let before: String = rs.input.chars().take(rs.cursor).collect();
                        let after: String = rs.input.chars().skip(rs.cursor).collect();
                        format!("{}▏{}", before, after)
                    } else {
                        t.title()
                    };
                    (title, i == active_idx, t.color, is_renaming, t.has_bell, t.has_completion)
                })
                .collect();
            drop(rename);
            let total_columns = tabs[active_idx].num_columns();
            let focused_column = tabs[active_idx].column_index(tabs[active_idx].focused_pane).unwrap_or(1);
            let active_tab_1based = active_idx + 1;
            let total_tabs = tabs.len();
            let active_tab_name = tabs[active_idx].title();
            (pane_data, pty_ptr, focus_reporting, tab_titles, panes_vp, screen_width, total_columns, focused_column, active_tab_1based, total_tabs, active_tab_name)
        };

        // Focus reporting (DEC mode 1004) — send to focused pane only
        unsafe {
            let mtm = MainThreadMarker::new_unchecked();
            let app = NSApplication::sharedApplication(mtm);
            let focused = app.isActive();
            let prev = ivars.last_focused.get();
            if focused != prev {
                ivars.last_focused.set(focused);
                if focus_reporting {
                    if let Some(pty_ptr) = pty_ptr {
                        let seq = if focused { b"\x1b[I" as &[u8] } else { b"\x1b[O" };
                        (*pty_ptr).write(seq);
                    }
                }
            }
        }

        // Update NSWindow title from focused pane's OSC 0/2
        if let Some(focused_pane) = pane_data.iter().find(|p| p.is_focused) {
            let term = focused_pane.terminal.read();
            let current = term.title.clone();
            drop(term);
            let mut prev = ivars.last_title.borrow_mut();
            if current != *prev {
                if let Some(win) = self.window() {
                    let title_str = match current {
                        Some(ref t) => format!("Kova — {}", t),
                        None => "Kova".to_string(),
                    };
                    win.setTitle(&NSString::from_str(&title_str));
                }
                *prev = current;
            }
        }

        // Collect split separators from active tab
        let separators = {
            let tabs = ivars.tabs.borrow();
            if let Some(tab) = tabs.get(active_idx) {
                let mut seps = Vec::new();
                tab.collect_separators(active_panes_vp, &mut seps);
                seps
            } else {
                Vec::new()
            }
        };

        // Decrement help hint countdown
        let help_hint_remaining = ivars.help_hint_frames.get();
        if help_hint_remaining > 0 {
            ivars.help_hint_frames.set(help_hint_remaining - 1);
        }
        let show_help = ivars.show_help.get();
        let show_mem_report = ivars.show_mem_report.get();

        // Build filter render data if active
        let filter_data = {
            let filter = ivars.filter.borrow();
            filter.as_ref().map(|f| FilterRenderData {
                query: f.query.clone(),
                matches: f.matches.clone(),
            })
        };

        // Compute left_inset from traffic light buttons
        let left_inset = {
            let inset = self.window()
                .and_then(|win| {
                    let scale = win.backingScaleFactor() as f32;
                    win.standardWindowButton(NSWindowButton::ZoomButton)
                        .map(|btn| {
                            let frame = btn.frame();
                            let right_edge = (frame.origin.x + frame.size.width) as f32;
                            (right_edge + 8.0) * scale
                        })
                })
                .unwrap_or(140.0);
            ivars.tab_bar_left_inset.set(inset);
            inset
        };
        let (hover_segments, hover_text, hover_pane_id) = {
            let h = ivars.hovered_url.borrow();
            (
                h.as_ref().map(|(_, segs, _)| segs.clone()),
                h.as_ref().map(|(_, _, url)| url.clone()),
                h.as_ref().map(|(pid, _, _)| *pid),
            )
        };
        let mut r = renderer.write();
        r.hovered_url = hover_segments;
        r.hovered_url_text = hover_text;
        r.hovered_url_pane_id = hover_pane_id;
        // Count hidden panes (fully off-screen)
        let mut hidden_left = 0usize;
        let mut hidden_right = 0usize;
        for p in &pane_data {
            if p.viewport.x + p.viewport.width <= 0.0 {
                hidden_left += 1;
            } else if p.viewport.x >= screen_width {
                hidden_right += 1;
            }
        }
        let keys_config = ivars.config.get().map(|c| &c.keys);

        // Build recent projects render data if overlay is active (uses cached data)
        let rp_guard = ivars.recent_projects.borrow();
        let rp_entries: Vec<&crate::renderer::RecentProjectEntry> = rp_guard.as_ref()
            .map(|state| state.items.iter().map(|item| &item.render).collect())
            .unwrap_or_default();
        let rp_data = rp_guard.as_ref().map(|state| {
            crate::renderer::RecentProjectsRenderData {
                entries: &rp_entries,
                selected: state.selected,
                scroll: state.scroll,
            }
        });

        // Build list-overlay render data (send-to-window or merge-tab)
        let stw_guard = ivars.send_to_window.borrow();
        let mt_guard = ivars.merge_tab.borrow();
        let overlay_labels: Vec<String> = if stw_guard.is_some() {
            stw_guard.as_ref().unwrap().entries.iter().map(|e| e.label.clone()).collect()
        } else if mt_guard.is_some() {
            mt_guard.as_ref().unwrap().entries.iter().map(|e| e.label.clone()).collect()
        } else {
            Vec::new()
        };
        let stw_data = if let Some(state) = stw_guard.as_ref() {
            Some(crate::renderer::SendToWindowRenderData {
                title: "Send Tab to Window",
                entries: &overlay_labels,
                selected: state.selected,
                has_new_entry: state.entries.last().map_or(false, |e| e.window_index.is_none()),
            })
        } else if let Some(state) = mt_guard.as_ref() {
            Some(crate::renderer::SendToWindowRenderData {
                title: "Merge Tab Into",
                entries: &overlay_labels,
                selected: state.selected,
                has_new_entry: false,
            })
        } else {
            None
        };

        // Update resize feedback (decrement frames, build text)
        if let Some(mut fb) = ivars.resize_feedback.get() {
            if fb.remaining_frames > 0 {
                fb.remaining_frames -= 1;
                ivars.resize_feedback.set(Some(fb));
                let mode_str = match fb.mode {
                    ResizeMode::Ratio => "Ratio",
                    ResizeMode::Virtual => "Virtual",
                    ResizeMode::Edge => "Right Edge",
                };
                r.resize_feedback_text = Some(format!("{} — screen {}px — virtual {}px", mode_str, fb.screen_w, fb.virtual_w));
            } else {
                ivars.resize_feedback.set(None);
                r.resize_feedback_text = None;
            }
        } else {
            r.resize_feedback_text = None;
        }

        // Update loading progress: count shell_ready across ALL tabs
        {
            let tabs = ivars.tabs.borrow();
            let deferred_remaining = ivars.deferred_tabs.borrow().len() as u32;
            let mut ready: u32 = 0;
            let mut total: u32 = 0;
            for tab in tabs.iter() {
                tab.for_each_pane(&mut |pane| {
                    total += 1;
                    if pane.is_ready() {
                        ready += 1;
                    }
                });
            }
            // Add deferred tabs' pane count to total
            let deferred_panes: u32 = ivars.deferred_tabs.borrow().iter()
                .map(|(_, saved)| crate::session::count_panes_in_saved_tab(saved) as u32)
                .sum();
            total += deferred_panes;
            if ready < total || deferred_remaining > 0 {
                r.loading_progress = Some((ready, total));
            } else {
                r.loading_progress = None;
            }
        }

        r.render_panes(&layer, &pane_data, &separators, &tab_titles, filter_data.as_ref(), left_inset, hidden_left, hidden_right, focused_column, total_columns, active_tab, total_tabs, &active_tab_name, show_help, show_mem_report, rp_data.as_ref(), stw_data.as_ref(), help_hint_remaining, keys_config);
        true
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
        *view.ivars().deferred_tabs.borrow_mut() = deferred_tabs;
    }
    window.setContentView(Some(&view));
    window.setDelegate(Some(objc2::runtime::ProtocolObject::from_ref(&*view)));
    window.makeFirstResponder(Some(&view));
    window.setAcceptsMouseMovedEvents(true);

    window
}
