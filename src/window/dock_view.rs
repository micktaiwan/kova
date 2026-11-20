//! Native AppKit views shown inside panes (`docs/pane-ui-spec.md`).
//!
//! Kova draws every pane of a window into one Metal view; a pane view is an
//! AppKit subview of that view, placed over the part of the pane the terminal
//! gave up (`crate::pane::split_dock`), or over the whole of a UI pane. Its
//! frame follows the pane on every frame: splits, window resize, tab switch,
//! minimize (checked on the § 12 prototype).
//!
//! A view is a header (title, number of cards, ✕) and its cards in a
//! scrolling list; each card draws its blocks with `crate::card_view`, its
//! text field and its buttons. Clicks and closes are queued here and drained
//! by the app's tick ([`take_ui_clicks`], [`take_ui_closes`]), which hands
//! them to `crate::ui_board`.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Mutex;

use objc2::rc::Retained;
use objc2::runtime::{AnyObject, Sel};
use objc2::{define_class, msg_send, sel, ClassType, DefinedClass, MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{
    NSApplication, NSButton, NSColor, NSControl, NSControlTextEditingDelegate, NSEvent, NSEventModifierFlags,
    NSFocusRingType, NSFont, NSFontWeightRegular, NSFontWeightSemibold, NSLayoutAttribute,
    NSLineBreakMode, NSProgressIndicator, NSProgressIndicatorStyle, NSScrollView,
    NSResponder, NSStackView, NSTextField, NSTextFieldDelegate, NSTextView, NSUserInterfaceLayoutOrientation, NSView,
};
use objc2_core_foundation::{CGFloat, CGPoint, CGRect, CGSize};
use objc2_foundation::{NSArray, NSEdgeInsets, NSObject, NSObjectProtocol, NSString};

use super::KovaView;
use crate::ask::Trigger;
use crate::card_view::{activate, attributed, block_view, FlippedView};
use crate::decision_card::Style;
use crate::pane::PaneId;
use crate::pane_ui::{CardSlot, View};

/// A click on a card of a pane view, for the app's tick.
pub struct UiClick {
    pub view_id: String,
    pub card_id: String,
    pub action: String,
    pub text: String,
    pub trigger: Trigger,
}

static CLICKS: Mutex<Vec<UiClick>> = Mutex::new(Vec::new());
/// Views the user closed (✕, or their pane closed), for the app's tick.
static CLOSES: Mutex<Vec<View>> = Mutex::new(Vec::new());
/// The last refusal of a click, shown in red on its card until it changes:
/// view id → (card id, card version, message).
static ERRORS: Mutex<Option<HashMap<String, (String, u32, String)>>> = Mutex::new(None);

pub fn take_ui_clicks() -> Vec<UiClick> {
    CLICKS.lock().map(|mut c| std::mem::take(&mut *c)).unwrap_or_default()
}

pub fn take_ui_closes() -> Vec<View> {
    CLOSES.lock().map(|mut c| std::mem::take(&mut *c)).unwrap_or_default()
}

/// Show `message` in red on card `card_id` (at `version`) of view `view_id`.
pub fn set_ui_error(view_id: &str, card_id: &str, version: u32, message: &str) {
    if let Ok(mut e) = ERRORS.lock() {
        e.get_or_insert_with(HashMap::new).insert(view_id.to_string(), (card_id.to_string(), version, message.to_string()));
    }
}

fn ui_error(view_id: &str) -> Option<(String, u32, String)> {
    ERRORS.lock().ok().and_then(|e| e.as_ref().and_then(|m| m.get(view_id).cloned()))
}

/// What the controls of one drawn view stand for, read by the target.
#[derive(Default)]
struct Controls {
    view_id: String,
    /// By button tag: (card id, action id).
    buttons: Vec<(String, String)>,
    /// Each card's text field, by card id.
    fields: Vec<(String, Retained<NSTextField>)>,
    /// Each card's primary action, by card id.
    primaries: Vec<(String, String)>,
    /// Fields and buttons in reading order, for Tab.
    key_views: Vec<Retained<NSView>>,
    /// The card whose field Escape left: ⌘Return on the view answers it.
    focused_card: Option<String>,
    /// Docked in a terminal pane (a second Escape goes back to the terminal),
    /// not a UI pane.
    docked: bool,
    snapshot: Option<View>,
}

pub struct DockTargetIvars {
    controls: Rc<RefCell<Controls>>,
}

define_class!(
    /// Target of a pane view's buttons and delegate of its text fields.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "KovaPaneViewTarget"]
    #[ivars = DockTargetIvars]
    pub struct DockTarget;

    unsafe impl NSObjectProtocol for DockTarget {}

    unsafe impl NSControlTextEditingDelegate for DockTarget {
        /// Return adds a line; ⌘Return answers with the card's primary
        /// action; Escape gives the keyboard back to the terminal.
        #[unsafe(method(control:textView:doCommandBySelector:))]
        fn control_text_view_do_command(&self, control: &NSControl, text_view: &NSTextView, selector: Sel) -> bool {
            field_command(self, control, text_view, selector)
        }
    }

    unsafe impl NSTextFieldDelegate for DockTarget {}

    impl DockTarget {
        #[unsafe(method(pressed:))]
        fn pressed(&self, sender: &NSButton) {
            let controls = self.ivars().controls.borrow();
            let Some((card, action)) = usize::try_from(sender.tag()).ok().and_then(|i| controls.buttons.get(i)).cloned() else { return };
            let text = field_text(&controls, &card);
            push_click(&controls.view_id, &card, &action, text, Trigger::Click);
        }

        #[unsafe(method(closePressed:))]
        fn close_pressed(&self, _sender: &NSButton) {
            let controls = self.ivars().controls.borrow();
            if let Some(view) = controls.snapshot.clone() {
                if let Ok(mut c) = CLOSES.lock() {
                    c.push(view);
                }
            }
        }
    }
);

pub struct DockRootIvars {
    controls: Rc<RefCell<Controls>>,
}

define_class!(
    /// The view's root: it takes the keyboard when clicked, or when Escape
    /// leaves a card's field, and keeps keys away from the terminal under it.
    #[unsafe(super(NSView, NSResponder, NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "KovaPaneViewRoot"]
    #[ivars = DockRootIvars]
    pub struct DockRoot;

    unsafe impl NSObjectProtocol for DockRoot {}

    impl DockRoot {
        #[unsafe(method(acceptsFirstResponder))]
        fn accepts_first_responder(&self) -> bool {
            true
        }

        /// A click on the view (not on a control) gives it the keyboard;
        /// it never reaches the terminal pane under it.
        #[unsafe(method(mouseDown:))]
        fn mouse_down(&self, _event: &NSEvent) {
            self.ivars().controls.borrow_mut().focused_card = None;
            if let Some(window) = self.window() {
                window.makeFirstResponder(Some(self));
            }
        }

        #[unsafe(method(keyDown:))]
        fn key_down(&self, event: &NSEvent) {
            root_key(self, event);
        }
    }
);

/// Keys on the view itself: Escape gives the keyboard back to the terminal
/// (docked views only, a UI pane has none), Tab goes back into a field.
/// Every other key stops here. ⌘Return is a key equivalent, handled by
/// `KovaView::pane_view_key_equivalent`.
fn root_key(root: &DockRoot, event: &NSEvent) {
    let controls = root.ivars().controls.clone();
    let Some(window) = root.window() else { return };
    match event.keyCode() {
        0x35 => {
            if controls.borrow().docked {
                if let Some(view) = window.contentView() {
                    window.makeFirstResponder(Some(&view));
                }
            }
        }
        0x30 => {
            let next = {
                let c = controls.borrow();
                c.focused_card
                    .as_ref()
                    .and_then(|card| c.fields.iter().find(|(id, _)| id == card))
                    .map(|(_, f)| Retained::into_super(Retained::into_super(f.clone())))
                    .or_else(|| c.key_views.first().cloned())
            };
            if let Some(next) = next {
                window.makeFirstResponder(Some(&next));
            }
        }
        _ => {}
    }
}

/// The root of the pane view holding `view`, if any.
fn root_of(view: &NSView) -> Option<Retained<NSView>> {
    // The views are ours and alive: their superview chain is safe to walk.
    let mut at = unsafe { view.superview() };
    while let Some(v) = at {
        if v.isKindOfClass(DockRoot::class()) {
            return Some(v);
        }
        at = unsafe { v.superview() };
    }
    None
}

fn field_text(controls: &Controls, card: &str) -> String {
    controls.fields.iter().find(|(c, _)| c == card).map(|(_, f)| f.stringValue().to_string()).unwrap_or_default()
}

fn push_click(view_id: &str, card: &str, action: &str, text: String, trigger: Trigger) {
    if let Ok(mut c) = CLICKS.lock() {
        c.push(UiClick { view_id: view_id.to_string(), card_id: card.to_string(), action: action.to_string(), text, trigger });
    }
}

fn field_command(target: &DockTarget, control: &NSControl, text_view: &NSTextView, selector: Sel) -> bool {
    if selector == sel!(cancelOperation:) {
        // Escape leaves the field for its card (the view's root keeps the
        // keyboard); a second Escape there goes back to the terminal.
        let card = {
            let controls = target.ivars().controls.borrow();
            controls.fields.iter().find(|(_, f)| std::ptr::eq(&**f as &NSControl, control)).map(|(c, _)| c.clone())
        };
        target.ivars().controls.borrow_mut().focused_card = card;
        if let Some(window) = control.window() {
            match root_of(control) {
                Some(root) => {
                    window.makeFirstResponder(Some(&root));
                }
                None => {
                    if let Some(view) = window.contentView() {
                        window.makeFirstResponder(Some(&view));
                    }
                }
            }
        }
        return true;
    }
    if selector != sel!(insertNewline:) {
        return false;
    }
    let mtm = MainThreadMarker::from(target);
    let cmd = NSApplication::sharedApplication(mtm)
        .currentEvent()
        .is_some_and(|e| e.modifierFlags().contains(NSEventModifierFlags::Command));
    if !cmd {
        let _: () = unsafe { msg_send![text_view, insertNewlineIgnoringFieldEditor: None::<&AnyObject>] };
        return true;
    }
    let controls = target.ivars().controls.borrow();
    let card = controls.fields.iter().find(|(_, f)| std::ptr::eq(&**f as &NSControl, control)).map(|(c, _)| c.clone());
    if let Some(card) = card {
        if let Some((_, action)) = controls.primaries.iter().find(|(c, _)| *c == card) {
            let text = field_text(&controls, &card);
            push_click(&controls.view_id, &card, action, text, Trigger::CmdReturn);
        }
    }
    true
}

/// One pane view on screen.
pub struct DockedView {
    root: Retained<NSView>,
    title: Retained<NSTextField>,
    scroll: Retained<NSScrollView>,
    list: Retained<NSStackView>,
    target: Retained<DockTarget>,
    controls: Rc<RefCell<Controls>>,
    /// What the list was last built from: skip rebuilding when unchanged.
    key: RefCell<String>,
    width: Cell<CGFloat>,
    /// Frames left during which a new view is put back at its top: AppKit
    /// lays the list out again after the first draw and scrolled it down.
    top_frames: Cell<u8>,
}

/// The view's frame in the Metal view's points, from a rectangle in drawable
/// pixels (origin top-left): the Metal view is not flipped.
pub fn pixel_rect_to_points(x: f32, y: f32, w: f32, h: f32, scale: f32, view_height_pt: f64) -> CGRect {
    let s = scale.max(0.1) as f64;
    CGRect::new(
        CGPoint::new(x as f64 / s, view_height_pt - (y as f64 + h as f64) / s),
        CGSize::new(w as f64 / s, h as f64 / s),
    )
}

fn ns(s: &str) -> Retained<NSString> {
    NSString::from_str(s)
}

fn label(mtm: MainThreadMarker, text: &str, size: CGFloat, color: &NSColor) -> Retained<NSTextField> {
    let l = NSTextField::wrappingLabelWithString(&ns(text), mtm);
    l.setFont(Some(&NSFont::systemFontOfSize(size)));
    l.setTextColor(Some(color));
    l
}

fn build(mtm: MainThreadMarker) -> DockedView {
    let controls = Rc::new(RefCell::new(Controls::default()));
    let root: Retained<NSView> = {
        let this = DockRoot::alloc(mtm).set_ivars(DockRootIvars { controls: controls.clone() });
        let root: Retained<DockRoot> = unsafe { msg_send![super(this), init] };
        Retained::into_super(root)
    };
    root.setWantsLayer(true);
    if let Some(layer) = root.layer() {
        layer.setBackgroundColor(Some(&NSColor::windowBackgroundColor().CGColor()));
        layer.setBorderWidth(1.0);
        layer.setBorderColor(Some(&NSColor::separatorColor().CGColor()));
    }
    let target: Retained<DockTarget> = {
        let this = DockTarget::alloc(mtm).set_ivars(DockTargetIvars { controls: controls.clone() });
        unsafe { msg_send![super(this), init] }
    };
    let title = NSTextField::labelWithString(&ns(""), mtm);
    title.setFont(Some(&NSFont::systemFontOfSize_weight(13.0, unsafe { NSFontWeightSemibold })));
    title.setTextColor(Some(&NSColor::labelColor()));
    title.setLineBreakMode(NSLineBreakMode::ByTruncatingTail);
    let close = unsafe { NSButton::buttonWithTitle_target_action(&ns("✕"), Some(&target), Some(sel!(closePressed:)), mtm) };
    close.setBordered(false);
    close.setToolTip(Some(&ns("Close this view")));
    let spacer = NSView::new(mtm);
    let header = NSStackView::stackViewWithViews(&NSArray::from_slice(&[title.as_ref() as &NSView, &spacer, close.as_ref()]), mtm);
    header.setOrientation(NSUserInterfaceLayoutOrientation::Horizontal);
    header.setEdgeInsets(NSEdgeInsets { top: 6.0, left: 12.0, bottom: 6.0, right: 8.0 });
    header.setTranslatesAutoresizingMaskIntoConstraints(false);
    let list = NSStackView::new(mtm);
    list.setOrientation(NSUserInterfaceLayoutOrientation::Vertical);
    list.setAlignment(NSLayoutAttribute::Leading);
    list.setSpacing(10.0);
    list.setEdgeInsets(NSEdgeInsets { top: 4.0, left: 12.0, bottom: 12.0, right: 12.0 });
    list.setTranslatesAutoresizingMaskIntoConstraints(false);
    let document = FlippedView::new_view(mtm);
    document.setTranslatesAutoresizingMaskIntoConstraints(false);
    document.addSubview(&list);
    let scroll = NSScrollView::new(mtm);
    scroll.setHasVerticalScroller(true);
    scroll.setAutohidesScrollers(true);
    scroll.setDrawsBackground(false);
    scroll.setDocumentView(Some(&document));
    scroll.setTranslatesAutoresizingMaskIntoConstraints(false);
    root.addSubview(&header);
    root.addSubview(&scroll);
    let clip = scroll.contentView();
    activate(&[
        header.topAnchor().constraintEqualToAnchor(&root.topAnchor()),
        header.leadingAnchor().constraintEqualToAnchor(&root.leadingAnchor()),
        header.trailingAnchor().constraintEqualToAnchor(&root.trailingAnchor()),
        scroll.topAnchor().constraintEqualToAnchor(&header.bottomAnchor()),
        scroll.leadingAnchor().constraintEqualToAnchor(&root.leadingAnchor()),
        scroll.trailingAnchor().constraintEqualToAnchor(&root.trailingAnchor()),
        scroll.bottomAnchor().constraintEqualToAnchor(&root.bottomAnchor()),
        list.leadingAnchor().constraintEqualToAnchor(&document.leadingAnchor()),
        list.trailingAnchor().constraintEqualToAnchor(&document.trailingAnchor()),
        list.topAnchor().constraintEqualToAnchor(&document.topAnchor()),
        list.bottomAnchor().constraintEqualToAnchor(&document.bottomAnchor()),
        document.leadingAnchor().constraintEqualToAnchor(&clip.leadingAnchor()),
        document.topAnchor().constraintEqualToAnchor(&clip.topAnchor()),
        document.widthAnchor().constraintEqualToAnchor(&clip.widthAnchor()),
    ]);
    DockedView { root, title, scroll, list, target, controls, key: RefCell::new(String::new()), width: Cell::new(0.0), top_frames: Cell::new(0) }
}

impl DockedView {
    /// Draw `view` (rebuilding its cards only when something changed), or
    /// just its header bar when `collapsed`.
    fn update(&self, mtm: MainThreadMarker, view: &View, width: CGFloat, collapsed: bool) {
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
        let n = view.cards.len();
        let count = if n == 1 { "1 card".to_string() } else { format!("{} cards", n) };
        let head = if collapsed { format!("{} · {}", view.title, count) } else { view.title.clone() };
        self.title.setStringValue(&ns(&head));
        self.scroll.setHidden(collapsed);
        let error = ui_error(&view.view_id);
        let locks: Vec<bool> = view.cards.iter().map(|c| c.locked(now)).collect();
        let key = format!(
            "{}|{:.0}|{:?}|{:?}",
            serde_json::to_string(&view.cards).unwrap_or_default(),
            width,
            locks,
            error
        );
        {
            let mut c = self.controls.borrow_mut();
            c.view_id = view.view_id.clone();
            c.snapshot = Some(view.clone());
            c.docked = view.placement.docked();
        }
        if self.top_frames.get() > 0 {
            self.top_frames.set(self.top_frames.get() - 1);
            if let Some(document) = self.scroll.documentView() {
                document.scrollPoint(CGPoint::new(0.0, 0.0));
            }
        }
        if *self.key.borrow() == key && (self.width.get() - width).abs() < 1.0 {
            return;
        }
        // A new view opens at its top; a rebuilt one stays where the user scrolled.
        let first = self.key.borrow().is_empty();
        let scrolled = self.scroll.contentView().bounds().origin;
        *self.key.borrow_mut() = key;
        self.width.set(width);
        // What the user typed stays across a rebuild, card by card.
        let drafts: Vec<(String, String)> = self
            .controls
            .borrow()
            .fields
            .iter()
            .map(|(id, f)| (id.clone(), f.stringValue().to_string()))
            .collect();
        for v in self.list.arrangedSubviews().iter() {
            v.removeFromSuperview();
        }
        let mut controls = Controls {
            view_id: view.view_id.clone(),
            snapshot: Some(view.clone()),
            docked: view.placement.docked(),
            focused_card: self.controls.borrow().focused_card.clone().filter(|c| view.cards.iter().any(|s| s.id == *c)),
            ..Default::default()
        };
        if view.cards.is_empty() {
            let empty = label(mtm, "Nothing to decide", 13.0, &NSColor::secondaryLabelColor());
            self.list.addArrangedSubview(&empty);
        }
        let inner = (width - 24.0).max(120.0);
        for (i, slot) in view.cards.iter().enumerate() {
            let err = error.as_ref().filter(|(c, v, _)| *c == slot.id && *v == slot.version).map(|(_, _, m)| m.as_str());
            let draft = drafts.iter().find(|(c, _)| *c == slot.id).map(|(_, t)| t.as_str()).unwrap_or("");
            let card = self.card_view(mtm, slot, locks[i], err, inner, draft, &mut controls);
            self.list.addArrangedSubview(&card);
        }
        // Tab walks the fields and buttons in reading order, and loops.
        let n = controls.key_views.len();
        for (i, v) in controls.key_views.iter().enumerate() {
            unsafe { v.setNextKeyView(Some(&controls.key_views[(i + 1) % n])) };
        }
        *self.controls.borrow_mut() = controls;
        self.root.layoutSubtreeIfNeeded();
        if let Some(document) = self.scroll.documentView() {
            document.scrollPoint(if first { CGPoint::new(0.0, 0.0) } else { scrolled });
        }
        if first {
            self.top_frames.set(10);
        }
    }

    fn card_view(
        &self,
        mtm: MainThreadMarker,
        slot: &CardSlot,
        locked: bool,
        error: Option<&str>,
        width: CGFloat,
        draft: &str,
        controls: &mut Controls,
    ) -> Retained<NSView> {
        let tint = NSColor::controlAccentColor();
        let doc = &slot.card;
        let pad: CGFloat = 12.0;
        let body = width - 2.0 * pad;
        let mut rows: Vec<Retained<NSView>> = Vec::new();
        let title = label(mtm, &doc.title, 14.0, &NSColor::labelColor());
        title.setFont(Some(&NSFont::systemFontOfSize_weight(14.0, unsafe { NSFontWeightSemibold })));
        title.setPreferredMaxLayoutWidth(body);
        rows.push(Retained::into_super(Retained::into_super(title)));
        if let Some(sub) = &doc.subtitle {
            let s = label(mtm, sub, 12.0, &NSColor::secondaryLabelColor());
            s.setPreferredMaxLayoutWidth(body);
            rows.push(Retained::into_super(Retained::into_super(s)));
        }
        for block in &doc.blocks {
            rows.push(block_view(mtm, block, body, &tint));
        }
        if let Some(g) = doc.grant_action().and_then(|a| a.grant.as_ref()) {
            let l = label(mtm, &format!("Authorizes: {}", g.target), 11.0, &NSColor::secondaryLabelColor());
            l.setPreferredMaxLayoutWidth(body);
            rows.push(Retained::into_super(Retained::into_super(l)));
        }
        if let Some(e) = error {
            let l = label(mtm, e, 12.0, &NSColor::systemRedColor());
            l.setPreferredMaxLayoutWidth(body);
            rows.push(Retained::into_super(Retained::into_super(l)));
        }
        if let Some(input) = &doc.input {
            let field = NSTextField::new(mtm);
            field.setStringValue(&ns(draft));
            field.setPlaceholderString(input.placeholder.as_deref().map(ns).as_deref());
            field.setFont(Some(&NSFont::systemFontOfSize(13.0)));
            field.setFocusRingType(NSFocusRingType::Default);
            field.setUsesSingleLineMode(false);
            if let Some(cell) = field.cell() {
                cell.setWraps(true);
                cell.setScrollable(false);
            }
            unsafe { field.setDelegate(Some(objc2::runtime::ProtocolObject::from_ref(&*self.target))) };
            field.setTranslatesAutoresizingMaskIntoConstraints(false);
            activate(&[
                field.widthAnchor().constraintEqualToConstant(body),
                field.heightAnchor().constraintGreaterThanOrEqualToConstant(44.0),
            ]);
            controls.fields.push((slot.id.clone(), field.clone()));
            controls.key_views.push(Retained::into_super(Retained::into_super(field.clone())));
            rows.push(Retained::into_super(Retained::into_super(field)));
        }
        if let Some(p) = doc.primary() {
            controls.primaries.push((slot.id.clone(), p.id.clone()));
        }
        // Buttons, the primary rightmost; dimmed and inert while the click waits for the agent.
        let mut made: Vec<Retained<NSView>> = Vec::new();
        let mut keys: Vec<Retained<NSView>> = Vec::new();
        if locked {
            let spinner = NSProgressIndicator::new(mtm);
            spinner.setStyle(NSProgressIndicatorStyle::Spinning);
            spinner.setControlSize(objc2_app_kit::NSControlSize::Small);
            unsafe { spinner.startAnimation(None) };
            made.push(Retained::into_super(spinner));
        }
        for action in doc.actions.iter().rev() {
            let primary = action.style == Style::Primary;
            let hint = if action.grant.is_some() && primary { "  ⌘↩" } else { "" };
            let text = format!("{}{}", action.label, hint);
            let button = unsafe { NSButton::buttonWithTitle_target_action(&ns(&text), Some(&self.target), Some(sel!(pressed:)), mtm) };
            button.setTag(controls.buttons.len() as isize);
            controls.buttons.push((slot.id.clone(), action.id.clone()));
            button.setBordered(false);
            button.setWantsLayer(true);
            if let Some(layer) = button.layer() {
                layer.setCornerRadius(7.0);
                let fill = if primary { tint.clone() } else { NSColor::labelColor().colorWithAlphaComponent(0.1) };
                layer.setBackgroundColor(Some(&fill.CGColor()));
            }
            let weight = unsafe { if primary { NSFontWeightSemibold } else { NSFontWeightRegular } };
            let color = if primary {
                NSColor::whiteColor()
            } else if action.style == Style::Destructive {
                NSColor::systemRedColor()
            } else {
                NSColor::labelColor()
            };
            button.setAttributedTitle(&attributed(&text, &NSFont::systemFontOfSize_weight(12.0, weight), &color));
            if locked {
                button.setAlphaValue(0.4);
            }
            let w = (objc2_app_kit::NSAttributedStringNSStringDrawing::size(&*button.attributedTitle()).width + 24.0).max(64.0);
            button.setTranslatesAutoresizingMaskIntoConstraints(false);
            activate(&[button.widthAnchor().constraintEqualToConstant(w), button.heightAnchor().constraintEqualToConstant(26.0)]);
            keys.push(Retained::into_super(Retained::into_super(button.clone())));
            made.push(Retained::into_super(Retained::into_super(button)));
        }
        made.reverse();
        keys.reverse();
        controls.key_views.extend(keys);
        let refs: Vec<&NSView> = made.iter().map(|v| v.as_ref()).collect();
        let buttons = NSStackView::stackViewWithViews(&NSArray::from_slice(&refs), mtm);
        buttons.setOrientation(NSUserInterfaceLayoutOrientation::Horizontal);
        buttons.setSpacing(8.0);
        rows.push(Retained::into_super(buttons));
        let refs: Vec<&NSView> = rows.iter().map(|v| v.as_ref()).collect();
        let column = NSStackView::stackViewWithViews(&NSArray::from_slice(&refs), mtm);
        column.setOrientation(NSUserInterfaceLayoutOrientation::Vertical);
        column.setAlignment(NSLayoutAttribute::Leading);
        column.setSpacing(8.0);
        column.setEdgeInsets(NSEdgeInsets { top: pad, left: pad, bottom: pad, right: pad });
        column.setWantsLayer(true);
        if let Some(layer) = column.layer() {
            layer.setCornerRadius(10.0);
            layer.setBorderWidth(1.0);
            layer.setBorderColor(Some(&NSColor::separatorColor().CGColor()));
            layer.setBackgroundColor(Some(&NSColor::labelColor().colorWithAlphaComponent(0.03).CGColor()));
        }
        column.setTranslatesAutoresizingMaskIntoConstraints(false);
        activate(&[column.widthAnchor().constraintEqualToConstant(width)]);
        Retained::into_super(column)
    }
}

impl KovaView {
    /// Key equivalents while a pane view has the keyboard. ⌘V, ⌘C, ⌘X and
    /// ⌘A in a text field go to that field: Kova has no Edit menu, and its
    /// own Paste would otherwise paste into the terminal pane. ⌘Return
    /// answers a card. True when handled.
    pub(super) fn pane_view_key_equivalent(&self, event: &NSEvent) -> bool {
        let flags = event.modifierFlags();
        if !flags.contains(NSEventModifierFlags::Command)
            || flags.intersects(NSEventModifierFlags::Control | NSEventModifierFlags::Option | NSEventModifierFlags::Shift)
        {
            return false;
        }
        let Some(responder) = self.window().and_then(|w| w.firstResponder()) else { return false };
        if matches!(event.keyCode(), 0x24 | 0x4C) {
            return self.cmd_return_in_pane_view(&responder);
        }
        if !responder.isKindOfClass(NSTextView::class()) {
            return false;
        }
        let key = event.charactersIgnoringModifiers().map(|s| s.to_string().to_lowercase()).unwrap_or_default();
        let action = match key.as_str() {
            "v" => sel!(paste:),
            "c" => sel!(copy:),
            "x" => sel!(cut:),
            "a" => sel!(selectAll:),
            _ => return false,
        };
        let _: bool = unsafe { msg_send![&*responder, tryToPerform: action, with: None::<&AnyObject>] };
        true
    }

    /// ⌘Return while a pane view has the keyboard: the primary action of
    /// the card being typed in, or of the card whose field Escape left. It
    /// never reaches the text field (AppKit offers it as a key equivalent
    /// first, and the field editor has no binding for it).
    fn cmd_return_in_pane_view(&self, responder: &NSResponder) -> bool {
        if !responder.isKindOfClass(NSView::class()) {
            return false;
        }
        // Checked just above: the first responder is a view.
        let view: &NSView = unsafe { &*(responder as *const NSResponder as *const NSView) };
        let docks = self.ivars().docks.borrow();
        let Some(dock) = docks.values().find(|d| view.isDescendantOf(&d.root)) else { return false };
        let c = dock.controls.borrow();
        let card = if std::ptr::eq(view, &*dock.root) {
            c.focused_card.clone()
        } else {
            c.fields.iter().find(|(_, f)| view.isDescendantOf(f)).map(|(id, _)| id.clone())
        };
        if let Some(card) = card {
            if let Some((_, action)) = c.primaries.iter().find(|(id, _)| *id == card) {
                push_click(&c.view_id, &card, action, field_text(&c, &card), Trigger::CmdReturn);
            }
        }
        true
    }

    /// Put every pane view of the active tab over its rectangle, hide the
    /// others (other tabs, minimized panes), and drop the views that left.
    /// A view whose pane is gone was closed by the user: it is queued for the
    /// board, which sends `closed` unless the agent closed it itself.
    pub(super) fn sync_docks(&self) {
        let ivars = self.ivars();
        let Some(mtm) = MainThreadMarker::new() else { return };
        let scale = self.backing_scale();
        let height_pt = self.frame().size.height;
        let panes_vp = self.panes_viewport();
        let mut placed: Vec<(PaneId, CGRect, bool, View)> = Vec::new();
        let mut showing: Vec<PaneId> = Vec::new();
        let mut alive: Vec<PaneId> = Vec::new();
        {
            let tabs = ivars.tabs.borrow();
            for tab in tabs.iter() {
                tab.for_each_pane(&mut |pane| {
                    alive.push(pane.id);
                    if pane.ui_view.borrow().is_some() {
                        showing.push(pane.id);
                    }
                });
            }
            if let Some(tab) = tabs.get(ivars.active_tab.get()) {
                tab.for_each_dock(panes_vp, &mut |pane, split| {
                    if let Some(view) = pane.ui_view.borrow().clone() {
                        let v = split.view;
                        placed.push((pane.id, pixel_rect_to_points(v.x, v.y, v.width, v.height, scale, height_pt), split.collapsed, view));
                    }
                });
            }
        }
        let mut docks = ivars.docks.borrow_mut();
        docks.retain(|id, view| {
            let keep = showing.contains(id);
            if !keep {
                if !alive.contains(id) {
                    if let Some(snapshot) = view.controls.borrow().snapshot.clone() {
                        if let Ok(mut c) = CLOSES.lock() {
                            c.push(snapshot);
                        }
                    }
                }
                view.root.removeFromSuperview();
            }
            keep
        });
        for id in &showing {
            if !docks.contains_key(id) {
                let view = build(mtm);
                view.root.setHidden(true);
                self.addSubview(&view.root);
                docks.insert(*id, view);
            }
        }
        // Full-window overlays (Cmd+P, the search palette, help…) are drawn
        // by Metal, under every AppKit subview: the views hide meanwhile.
        let overlay = ivars.pane_switcher.borrow().is_some()
            || ivars.search_palette.borrow().is_some()
            || ivars.recent_projects.borrow().is_some()
            || ivars.merge_tab.borrow().is_some()
            || ivars.send_to_window.borrow().is_some()
            || ivars.show_help.get()
            || ivars.show_mem_report.get();
        if overlay {
            // The overlay reads its keys from this view: take them back from
            // a field of a view about to hide.
            if let Some(window) = self.window() {
                let inside = window.firstResponder().is_some_and(|r| {
                    r.isKindOfClass(NSView::class())
                        && docks.values().any(|d| {
                            // Checked just above: the responder is a view.
                            let v: &NSView = unsafe { &*(&*r as *const NSResponder as *const NSView) };
                            v.isDescendantOf(&d.root)
                        })
                });
                if inside {
                    window.makeFirstResponder(Some(self));
                }
            }
        }
        for (id, view) in docks.iter() {
            match placed.iter().find(|(p, ..)| p == id).filter(|_| !overlay) {
                Some((_, rect, collapsed, doc)) => {
                    if view.root.frame() != *rect {
                        view.root.setFrame(*rect);
                    }
                    view.update(mtm, doc, rect.size.width, *collapsed);
                    view.root.setHidden(false);
                }
                None => view.root.setHidden(true),
            }
        }
    }
}

impl KovaView {
    /// What the pane UI board needs to know of pane `id`, if it is here.
    pub fn ui_pane_info(&self, id: PaneId) -> Option<crate::ui_board::PaneInfo> {
        let tabs = self.ivars().tabs.borrow();
        tabs.iter().find_map(|t| t.pane(id)).map(pane_info)
    }

    pub fn ui_pane_by_uid(&self, uid: &str) -> Option<crate::ui_board::PaneInfo> {
        let tabs = self.ivars().tabs.borrow();
        let mut found = None;
        for tab in tabs.iter() {
            tab.for_each_pane(&mut |p| {
                if found.is_none() && *p.uid.borrow() == uid {
                    found = Some(pane_info(p));
                }
            });
        }
        found
    }

    /// The pane showing view `view_id`, with the view.
    pub fn ui_find_view(&self, view_id: &str) -> Option<(crate::ui_board::PaneInfo, crate::pane_ui::View)> {
        let tabs = self.ivars().tabs.borrow();
        let mut found = None;
        for tab in tabs.iter() {
            tab.for_each_pane(&mut |p| {
                if found.is_none() {
                    if let Some(v) = p.ui_view.borrow().as_ref().filter(|v| v.view_id == view_id) {
                        found = Some((pane_info(p), v.clone()));
                    }
                }
            });
        }
        found
    }

    pub fn ui_view_in(&self, id: PaneId) -> Option<crate::pane_ui::View> {
        let tabs = self.ivars().tabs.borrow();
        tabs.iter().find_map(|t| t.pane(id)).and_then(|p| p.ui_view.borrow().clone())
    }

    /// Put `view` on pane `id` (docked unless it is a UI pane) or take it off;
    /// the terminal is resized to what is left. `None` when the pane is not here.
    pub fn ui_set_view(&self, id: PaneId, view: Option<crate::pane_ui::View>) -> Option<()> {
        let changed_dock = {
            let tabs = self.ivars().tabs.borrow();
            let pane = tabs.iter().find_map(|t| t.pane(id))?;
            let dock = match (&view, pane.ui_pane.get()) {
                (Some(v), false) => Some(crate::pane::Dock {
                    side: if v.placement == crate::pane_ui::Placement::DockRight {
                        crate::pane::DockSide::Right
                    } else {
                        crate::pane::DockSide::Bottom
                    },
                    size: v.size,
                }),
                _ => None,
            };
            *pane.ui_view.borrow_mut() = view;
            pane.dock.replace(dock) != dock
        };
        if changed_dock {
            self.request_resize_all_panes();
        }
        self.mark_dirty();
        Some(())
    }

    /// Open a UI pane (no terminal) next to `opener`, without moving the
    /// focus. `None` when `opener` is not here.
    /// It gets `size` of the room `opener` had.
    pub fn ui_open_pane(&self, opener: PaneId, placement: crate::pane_ui::Placement, size: f32) -> Option<PaneId> {
        let config = self.ivars().config.get()?;
        let pane = crate::pane::Pane::placeholder(20, 6, config).ok()?;
        pane.ui_pane.set(true);
        let new_id = pane.id;
        {
            let mut tabs = self.ivars().tabs.borrow_mut();
            let tab = tabs.iter_mut().find(|t| t.contains(opener))?;
            let focused = tab.focused_pane;
            match placement {
                crate::pane_ui::Placement::PaneDown => tab.vsplit_at_pane(opener, pane),
                _ => {
                    tab.focused_pane = opener;
                    tab.insert_column_after_focused(pane);
                }
            }
            tab.share_with_ui_pane(opener, new_id, size);
            // ui-show never takes the keyboard (§ 12).
            tab.focused_pane = focused;
        }
        self.request_resize_all_panes();
        self.mark_dirty();
        Some(new_id)
    }

    /// UI panes of this window whose view the pane `uid` opened.
    pub fn ui_panes_of(&self, uid: &str) -> usize {
        let tabs = self.ivars().tabs.borrow();
        let mut n = 0;
        for tab in tabs.iter() {
            tab.for_each_pane(&mut |p| {
                if p.ui_pane.get() && p.ui_view.borrow().as_ref().and_then(|v| v.opener_uid.as_deref()) == Some(uid) {
                    n += 1;
                }
            });
        }
        n
    }
}

fn pane_info(p: &crate::pane::Pane) -> crate::ui_board::PaneInfo {
    crate::ui_board::PaneInfo {
        id: p.id,
        shell_pid: p.pty.pid() as i32,
        ui_pane: p.ui_pane.get(),
        uid: p.ensure_uid(),
        agent: p.agent_kind().zip(p.agent_session_id()),
        cwd: p.cwd(),
        typed_pending: p.pty.typed_pending.load(std::sync::atomic::Ordering::Relaxed),
    }
}

/// Docked views of this window, by pane.
pub type Docks = HashMap<PaneId, DockedView>;

#[cfg(test)]
mod tests {
    use super::pixel_rect_to_points;

    #[test]
    fn pixels_top_left_become_points_bottom_left() {
        // A 2x screen, a view 900 pt high: a rect at the top of the drawable lands at the top.
        let r = pixel_rect_to_points(200.0, 0.0, 400.0, 100.0, 2.0, 900.0);
        assert_eq!((r.origin.x, r.origin.y, r.size.width, r.size.height), (100.0, 850.0, 200.0, 50.0));
        let r = pixel_rect_to_points(0.0, 1700.0, 100.0, 100.0, 2.0, 900.0);
        assert_eq!(r.origin.y, 0.0, "the bottom edge");
    }
}
