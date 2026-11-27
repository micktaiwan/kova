//! The card an agent's question appears on (`crate::ask`).
//!
//! A port of an earlier Swift prototype's `QuestionCard.swift`, whose look the user validated:
//! same classes, sizes, colours, fonts and corner radii — do not redesign it here.
//!
//! The card sits top-right of the screen, over everything, without taking the
//! keyboard until the user clicks into it. One question at a time; the others
//! wait behind it, browsable with ‹ › (or ⌘[ / ⌘]) and counted `n / N`.
//!
//! Layout: a borderless rounded panel on a translucent HUD background, the
//! asker's colour washing down from the top edge and rimming the card. Header
//! (badge, who asks, the queue, the time), the message, the answer field, then
//! the buttons right-aligned with the first one as the default (tinted, Enter).
//!
//! A decision card (`crate::decision_card`) draws instead a title, a subtitle
//! and its blocks in a scrolling area, 560 points wide and at most 70 % of the
//! screen high, with the text field and the buttons pinned below. Its keys:
//! ⌘Return answers with the primary action from anywhere; Return does too
//! outside the field, except for a grant action, which only a click or
//! ⌘Return can trigger.
//!
//! Main thread only. The answers are queued here and drained by Kova's render
//! tick through [`take_answers`] and [`take_browse`]: AppKit calls the button
//! and field callbacks from inside its event loop, where the window list may
//! be borrowed.

use objc2::rc::Retained;
use objc2::runtime::{AnyObject, ProtocolObject, Sel};
use objc2::{define_class, msg_send, sel, MainThreadOnly};
use objc2_app_kit::{
    NSApplication, NSAttributedStringNSExtendedStringDrawing, NSAttributedStringNSStringDrawing,
    NSBackingStoreType, NSButton, NSColor, NSControl, NSControlTextEditingDelegate, NSEvent,
    NSEventModifierFlags, NSFloatingWindowLevel, NSFocusRingType, NSFont, NSFontWeightBold, NSImage,
    NSFontWeightRegular, NSFontWeightSemibold, NSLayoutAttribute, NSLayoutConstraint,
    NSLayoutConstraintOrientation, NSLayoutManager, NSLayoutPriorityRequired, NSLineBreakMode,
    NSControlSize, NSPanel, NSPopUpButton, NSRequestUserAttentionType, NSResponder, NSScreen, NSScrollView, NSScroller, NSSound,
    NSStackView,
    NSStringDrawingOptions, NSTextAlignment, NSTextDelegate, NSTextField, NSTextFieldDelegate, NSTextView,
    NSTextViewDelegate,
    NSUserInterfaceLayoutOrientation, NSView, NSVisualEffectBlendingMode, NSVisualEffectMaterial,
    NSVisualEffectState, NSWindow, NSWindowCollectionBehavior, NSWindowStyleMask,
};
use objc2_core_foundation::{CGFloat, CGPoint, CGRect, CGSize};
use objc2_foundation::{
    MainThreadMarker, NSArray, NSDate, NSDateFormatter, NSDateFormatterStyle, NSEdgeInsets,
    NSNotification, NSObject, NSObjectProtocol,
};
use objc2_quartz_core::{CAGradientLayer, CATransaction};
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Mutex;

use crate::ask::Trigger;
use crate::card_view::{activate, attributed, button_rows, cg_colors, ns, CodeEditor, FlippedView};
use crate::decision_card::{Card as Doc, Style};

/// One question as the card shows it.
pub struct CardQuestion<'a> {
    pub id: &'a str,
    /// The agent's name, shown on the card and hashed into its colour.
    pub from: &'a str,
    pub message: &'a str,
    /// Never empty, in order; the first is the primary (drawn last, rightmost).
    /// For a decision card: its action ids, then "Dismiss" when superseded.
    pub buttons: &'a [String],
    /// Whether to show the answer field.
    pub input: bool,
    /// Epoch seconds; shown as the local short time.
    pub asked_at: u64,
    /// A line shown above the message, greyed (a superseded question).
    pub note: Option<&'a str>,
    /// The decision card to draw instead of `message`.
    pub card: Option<&'a Doc>,
    /// The version shown: a click carries it back.
    pub card_version: u32,
    /// Whether "Dismiss" closes the question without an answer.
    pub dismissable: bool,
    /// 1-based rank in the queue.
    pub position: usize,
    /// A red line above the buttons (the authorization could not be recorded).
    pub error: Option<&'a str>,
}

/// What the user answered: the button (or the primary one, for Return) and
/// the field's text, trimmed, with the version shown and what set it off.
pub struct CardAnswer {
    pub id: String,
    pub action: String,
    pub text: String,
    pub card_version: u32,
    pub trigger: Trigger,
    /// The text of each editable `code` block (block index, text).
    pub edits: Vec<(usize, String)>,
}

/// The plain card's width, as in Swift.
const WIDTH: CGFloat = 400.0;
/// A decision card's width.
const CARD_WIDTH: CGFloat = 560.0;
/// Horizontal inset of every row; the vertical one is 4 points less.
const INSET: CGFloat = 20.0;
/// Beyond this the field stops growing and scrolls.
const MAX_LINES: CGFloat = 8.0;
/// Gap between two buttons, and between two rows of buttons.
const BUTTON_SPACING: CGFloat = 8.0;
const FIELD_FONT_SIZE: CGFloat = 14.0;
const PLACEHOLDER: &str = "Type an answer, or just click a button";
/// A decision card is at most this share of the visible screen height.
const MAX_SCREEN_SHARE: CGFloat = 0.70;
/// The block area never shrinks below this, however long the field grows.
const MIN_BLOCKS: CGFloat = 60.0;

/// Answers given since the last [`take_answers`]. A `Mutex` although only the
/// main thread touches it: `take_answers` takes no `MainThreadMarker`, so a
/// thread_local would silently hand another thread an empty queue.
static ANSWERS: Mutex<Vec<CardAnswer>> = Mutex::new(Vec::new());
/// ‹ (-1) and › (+1) presses since the last [`take_browse`].
static BROWSE: Mutex<Vec<i32>> = Mutex::new(Vec::new());
/// "Not now" pressed since the last [`take_snooze`]: the pause asked, in seconds.
static SNOOZE: Mutex<Option<u64>> = Mutex::new(None);

thread_local! {
    /// The card, built on the first `show`. Kept for the life of the process:
    /// the panel is ordered out, never closed.
    static CARD: RefCell<Option<Rc<Card>>> = const { RefCell::new(None) };
    /// The count last written to the Dock badge: `show` may run every tick, and
    /// the badge is only touched when the count changes.
    static BADGE: Cell<Option<usize>> = const { Cell::new(None) };
}

define_class!(
    /// Swift's `FloatPanel`: a non-activating panel that still accepts the
    /// keyboard when clicked. A borderless window answers NO to
    /// `canBecomeKeyWindow` by default, so without this the field could never
    /// be typed into. Also takes the card's keys: ⌘Return, ⌘[ / ⌘], and
    /// Return outside the field on a decision card.
    #[unsafe(super(NSPanel, NSWindow, NSResponder, NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "KovaAskCardPanel"]
    struct CardPanel;

    impl CardPanel {
        #[unsafe(method(canBecomeKeyWindow))]
        fn can_become_key_window(&self) -> bool {
            true
        }

        #[unsafe(method(performKeyEquivalent:))]
        fn perform_key_equivalent(&self, event: &NSEvent) -> bool {
            let handled = command_key(event);
            handled || unsafe { msg_send![super(self), performKeyEquivalent: event] }
        }

        #[unsafe(method(keyDown:))]
        fn key_down(&self, event: &NSEvent) {
            if !return_key(event) {
                let _: () = unsafe { msg_send![super(self), keyDown: event] };
            }
        }
    }
);


define_class!(
    /// Target of the buttons and delegate of the answer field. Holds nothing:
    /// the state lives in `CARD`, read when a callback fires.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "KovaAskCardTarget"]
    struct CardTarget;

    unsafe impl NSObjectProtocol for CardTarget {}

    unsafe impl NSControlTextEditingDelegate for CardTarget {
        #[unsafe(method(controlTextDidChange:))]
        fn control_text_did_change(&self, _notification: &NSNotification) {
            if let Some(card) = card() {
                card.fit_field();
                card.update_enabled();
            }
        }

        /// Enter in the field sends with "Other" when text was typed and the
        /// card has it, else with the first button; Shift+Enter (or
        /// Option+Enter) adds a line. On a decision card Return always adds a
        /// line: ⌘Return sends.
        #[unsafe(method(control:textView:doCommandBySelector:))]
        fn control_text_view_do_command(
            &self,
            _control: &NSControl,
            text_view: &NSTextView,
            selector: Sel,
        ) -> bool {
            enter_in_field(MainThreadMarker::from(self), text_view, selector)
        }
    }

    unsafe impl NSTextFieldDelegate for CardTarget {}

    /// An editable `code` block changed: keep its text, grow its box.
    unsafe impl NSTextDelegate for CardTarget {
        #[unsafe(method(textDidChange:))]
        fn text_did_change(&self, _notification: &NSNotification) {
            if let Some(card) = card() {
                card.code_edited(MainThreadMarker::from(self));
            }
        }
    }

    unsafe impl NSTextViewDelegate for CardTarget {}

    impl CardTarget {
        /// A button: its tag is its index in the question's buttons.
        #[unsafe(method(pressed:))]
        fn pressed(&self, sender: &NSButton) {
            let Some(card) = card() else { return };
            let index = sender.tag();
            let action = card.current.borrow().as_ref().and_then(|c| {
                usize::try_from(index).ok().and_then(|i| c.buttons.get(i).cloned())
            });
            if let Some(action) = action.filter(|_| card.button_enabled(index)) {
                card.answer(action, Trigger::Click);
            }
        }

        #[unsafe(method(previous:))]
        fn previous(&self, _sender: &NSButton) {
            browse(-1);
        }

        /// "Not now": every card waits for the duration picked in the header menu.
        #[unsafe(method(snooze:))]
        fn snooze(&self, sender: &NSPopUpButton) {
            // Item 0 of a pull-down is the button's face; the durations follow.
            let Some(index) = snooze_choice(sender.indexOfSelectedItem()) else { return };
            let (seconds, label) = crate::ask::DND_CHOICES[index];
            log::info!("Ask card: not now, for {}", label);
            if let Ok(mut snooze) = SNOOZE.lock() {
                *snooze = Some(seconds);
            }
        }

        #[unsafe(method(next:))]
        fn next(&self, _sender: &NSButton) {
            browse(1);
        }
    }
);

fn browse(delta: i32) {
    if let Ok(mut queue) = BROWSE.lock() {
        queue.push(delta);
    }
}

/// ⌘Return (the primary action) and ⌘[ / ⌘] (browse), read from the typed
/// character, never the keycode. True when handled.
fn command_key(event: &NSEvent) -> bool {
    let flags = event.modifierFlags();
    if !flags.contains(NSEventModifierFlags::Command) {
        return false;
    }
    let chars = event.charactersIgnoringModifiers().map(|s| s.to_string()).unwrap_or_default();
    match chars.as_str() {
        "[" => {
            browse(-1);
            true
        }
        "]" => {
            browse(1);
            true
        }
        "\r" | "\u{3}" => card().is_some_and(|card| card.trigger_primary(Trigger::CmdReturn)),
        _ => false,
    }
}

/// Return outside the field: the primary action of a decision card, never a
/// grant one (and no beep then). False for a plain card, whose default button
/// carries Return itself.
fn return_key(event: &NSEvent) -> bool {
    let flags = event.modifierFlags();
    if flags.intersects(NSEventModifierFlags::Command | NSEventModifierFlags::Shift | NSEventModifierFlags::Option | NSEventModifierFlags::Control) {
        return false;
    }
    let chars = event.charactersIgnoringModifiers().map(|s| s.to_string()).unwrap_or_default();
    if chars != "\r" && chars != "\u{3}" {
        return false;
    }
    let Some(card) = card() else { return false };
    if !card.rich.get() {
        return false;
    }
    card.trigger_primary(Trigger::Return);
    true
}

/// The body of `control:textView:doCommandBySelector:`, outside the macro:
/// an early `return` inside a `define_class!` method fights its `bool`
/// conversion.
fn enter_in_field(mtm: MainThreadMarker, text_view: &NSTextView, selector: Sel) -> bool {
    if selector != sel!(insertNewline:) {
        return false;
    }
    let Some(card) = card() else { return false };
    if card.rich.get() {
        let _: () = unsafe { msg_send![text_view, insertNewlineIgnoringFieldEditor: None::<&AnyObject>] };
        return true;
    }
    // Typed text sent with Enter is the free answer: it goes out as "Other" when the card has
    // that button, never under the first proposed choice.
    let typed = !card.field.stringValue().to_string().trim().is_empty();
    let Some(primary) = card.current.borrow().as_ref().map(|c| {
        match c
            .buttons
            .iter()
            .find(|b| typed && b.as_str() == crate::ask::OTHER)
        {
            Some(other) => other.clone(),
            None => c.buttons[0].clone(),
        }
    }) else {
        return false;
    };
    let flags = NSApplication::sharedApplication(mtm)
        .currentEvent()
        .map(|event| event.modifierFlags())
        .unwrap_or(NSEventModifierFlags::empty());
    if flags.contains(NSEventModifierFlags::Shift) || flags.contains(NSEventModifierFlags::Option) {
        let _: () = unsafe { msg_send![text_view, insertNewlineIgnoringFieldEditor: None::<&AnyObject>] };
        return true;
    }
    card.answer(primary, Trigger::Return);
    true
}

/// The question on screen, as much of it as the callbacks need.
struct Current {
    id: String,
    buttons: Vec<String>,
    card_version: u32,
    /// The primary action of a decision card, and whether it is a grant.
    primary: Option<(String, bool)>,
    /// The buttons drawn, each with whether it needs text in the field.
    made: Vec<(Retained<NSButton>, bool)>,
}

/// Everything `show` last drew, to skip a redraw when nothing changed: the tick
/// may call `show` on every frame, and redrawing would rebuild the buttons and
/// snap a card the user dragged back to the corner.
#[derive(PartialEq)]
struct Shown {
    id: String,
    from: String,
    message: String,
    buttons: Vec<String>,
    input: bool,
    asked_at: u64,
    note: Option<String>,
    card: Option<Doc>,
    card_version: u32,
    position: usize,
    total: usize,
    error: Option<String>,
}

struct Card {
    panel: Retained<CardPanel>,
    target: Retained<CardTarget>,
    background: Retained<objc2_app_kit::NSVisualEffectView>,
    wash: Retained<CAGradientLayer>,
    badge_holder: Retained<NSView>,
    who: Retained<NSTextField>,
    meta: Retained<NSTextField>,
    /// `‹ n / N ›`, hidden when the question is alone.
    nav: Retained<NSStackView>,
    prev: Retained<NSButton>,
    next: Retained<NSButton>,
    position: Retained<NSTextField>,
    note: Retained<NSTextField>,
    body: Retained<NSTextField>,
    title: Retained<NSTextField>,
    subtitle: Retained<NSTextField>,
    scroll: Retained<NSScrollView>,
    scroll_height: Retained<NSLayoutConstraint>,
    blocks: Retained<NSStackView>,
    authorizes: Retained<NSTextField>,
    error: Retained<NSTextField>,
    field: Retained<NSTextField>,
    field_box: Retained<NSView>,
    /// The field's height, set from its text: it grows a line at a time as the
    /// answer gets longer.
    field_height: Retained<NSLayoutConstraint>,
    buttons: Retained<NSStackView>,
    width: Retained<NSLayoutConstraint>,
    line_height: CGFloat,
    current: RefCell<Option<Current>>,
    shown: RefCell<Option<Shown>>,
    /// What the user typed in each question's field, kept while browsing.
    drafts: RefCell<HashMap<String, String>>,
    /// The editable `code` blocks of the card shown, by block index.
    editors: RefCell<Vec<(usize, CodeEditor)>>,
    /// The user's text of each question's code blocks, for the card version it
    /// was typed on: kept through redraws and browsing, dropped when the
    /// question leaves the board or the asker sends a new version.
    edits: RefCell<HashMap<String, (u32, HashMap<usize, String>)>>,
    /// The card shows a decision card (wide, scrolling blocks).
    rich: Cell<bool>,
    /// Questions already announced with a sound: browsing back to one is silent.
    announced: RefCell<Vec<String>>,
    tint: RefCell<Retained<NSColor>>,
}

fn card() -> Option<Rc<Card>> {
    CARD.with(|c| c.borrow().clone())
}

fn card_or_build(mtm: MainThreadMarker) -> Rc<Card> {
    if let Some(card) = card() {
        return card;
    }
    let card = Rc::new(Card::new(mtm));
    CARD.with(|c| *c.borrow_mut() = Some(card.clone()));
    card
}

fn field_font() -> Retained<NSFont> {
    NSFont::systemFontOfSize(FIELD_FONT_SIZE)
}


/// Set a layer property with Core Animation's implicit animation off, as
/// Swift's `CATransaction.setDisableActions(true)` blocks do.
fn without_animation(f: impl FnOnce()) {
    CATransaction::begin();
    CATransaction::setDisableActions(true);
    f();
    CATransaction::commit();
}

fn small_label(mtm: MainThreadMarker, size: CGFloat, color: &NSColor) -> Retained<NSTextField> {
    let label = NSTextField::wrappingLabelWithString(&ns(""), mtm);
    label.setFont(Some(&NSFont::systemFontOfSize(size)));
    label.setTextColor(Some(color));
    label.setPreferredMaxLayoutWidth(CARD_WIDTH - 2.0 * INSET);
    label.setHidden(true);
    label
}

/// The pull-down behind "Not now": a moon face, then one item per pause.
fn snooze_menu(mtm: MainThreadMarker, target: &CardTarget) -> Retained<NSPopUpButton> {
    let menu = NSPopUpButton::new(mtm);
    menu.setPullsDown(true);
    menu.setBordered(false);
    menu.addItemWithTitle(&ns(""));
    if let Some(face) = menu.itemAtIndex(0) {
        let image = NSImage::imageWithSystemSymbolName_accessibilityDescription(&ns("moon.zzz"), Some(&ns("Not now")));
        face.setImage(image.as_deref());
    }
    for (_, label) in crate::ask::DND_CHOICES {
        menu.addItemWithTitle(&ns(&format!("Not now, for {}", label)));
    }
    menu.setControlSize(NSControlSize::Small);
    menu.setFont(Some(&NSFont::systemFontOfSize(12.0)));
    menu.setToolTip(Some(&ns("Not now: hide every card for a while; they come back unanswered")));
    unsafe {
        menu.setTarget(Some(target));
        menu.setAction(Some(sel!(snooze:)));
    }
    menu.setContentHuggingPriority_forOrientation(NSLayoutPriorityRequired, NSLayoutConstraintOrientation::Horizontal);
    menu
}

/// The pause picked in the "Not now" pull-down, as an index into
/// `crate::ask::DND_CHOICES`: item 0 is the button's face, not a choice.
fn snooze_choice(selected: isize) -> Option<usize> {
    let index = usize::try_from(selected).ok()?.checked_sub(1)?;
    (index < crate::ask::DND_CHOICES.len()).then_some(index)
}

fn nav_button(mtm: MainThreadMarker, title: &str, target: &CardTarget, action: Sel) -> Retained<NSButton> {
    let button = unsafe { NSButton::buttonWithTitle_target_action(&ns(title), Some(target), Some(action), mtm) };
    button.setBordered(false);
    button.setAttributedTitle(&attributed(title, &NSFont::systemFontOfSize_weight(15.0, unsafe { NSFontWeightSemibold }), &NSColor::labelColor()));
    button.setTranslatesAutoresizingMaskIntoConstraints(false);
    activate(&[button.widthAnchor().constraintEqualToConstant(18.0)]);
    button
}

impl Card {
    fn new(mtm: MainThreadMarker) -> Self {
        let panel: Retained<CardPanel> = {
            let this = CardPanel::alloc(mtm).set_ivars(());
            let rect = CGRect::new(CGPoint::new(0.0, 0.0), CGSize::new(WIDTH, 180.0));
            unsafe {
                msg_send![
                    super(this),
                    initWithContentRect: rect,
                    styleMask: NSWindowStyleMask::Borderless | NSWindowStyleMask::NonactivatingPanel,
                    backing: NSBackingStoreType::Buffered,
                    defer: false
                ]
            }
        };
        panel.setOpaque(false);
        panel.setBackgroundColor(Some(&NSColor::clearColor()));
        panel.setHasShadow(true);
        panel.setMovableByWindowBackground(true);
        panel.setLevel(NSFloatingWindowLevel);
        panel.setFloatingPanel(true);
        panel.setHidesOnDeactivate(false);
        panel.setBecomesKeyOnlyIfNeeded(true);
        unsafe { panel.setReleasedWhenClosed(false) };
        panel.setCollectionBehavior(
            NSWindowCollectionBehavior::CanJoinAllSpaces | NSWindowCollectionBehavior::FullScreenAuxiliary,
        );

        let target: Retained<CardTarget> = {
            let this = CardTarget::alloc(mtm).set_ivars(());
            unsafe { msg_send![super(this), init] }
        };

        let background = objc2_app_kit::NSVisualEffectView::new(mtm);
        background.setMaterial(NSVisualEffectMaterial::HUDWindow);
        background.setBlendingMode(NSVisualEffectBlendingMode::BehindWindow);
        background.setState(NSVisualEffectState::Active);
        background.setWantsLayer(true);
        let wash = CAGradientLayer::new();
        if let Some(layer) = background.layer() {
            layer.setCornerRadius(16.0);
            layer.setMasksToBounds(true);
            // The asker's colour washes down from the top edge, and rims the card.
            wash.setStartPoint(CGPoint::new(0.5, 1.0));
            wash.setEndPoint(CGPoint::new(0.5, 0.25));
            layer.addSublayer(&wash);
            layer.setBorderWidth(1.0);
        }
        panel.setContentView(Some(&background));

        // Header: a round badge, who asks, and on the right the queue and the time.
        let badge = NSTextField::labelWithString(&ns("📎"), mtm);
        badge.setFont(Some(&NSFont::systemFontOfSize(15.0)));
        badge.setAlignment(NSTextAlignment::Center);
        let badge_holder = NSView::new(mtm);
        badge_holder.setWantsLayer(true);
        if let Some(layer) = badge_holder.layer() {
            layer.setCornerRadius(14.0);
        }
        badge.setTranslatesAutoresizingMaskIntoConstraints(false);
        badge_holder.addSubview(&badge);
        let who = NSTextField::labelWithString(&ns(""), mtm);
        who.setFont(Some(&NSFont::systemFontOfSize_weight(13.0, unsafe { NSFontWeightSemibold })));
        who.setTextColor(Some(&NSColor::labelColor()));
        who.setLineBreakMode(NSLineBreakMode::ByTruncatingTail);
        let meta = NSTextField::labelWithString(&ns(""), mtm);
        meta.setFont(Some(&NSFont::systemFontOfSize(11.0)));
        meta.setTextColor(Some(&NSColor::secondaryLabelColor()));
        meta.setContentCompressionResistancePriority_forOrientation(
            NSLayoutPriorityRequired,
            NSLayoutConstraintOrientation::Horizontal,
        );
        let prev = nav_button(mtm, "‹", &target, sel!(previous:));
        let next = nav_button(mtm, "›", &target, sel!(next:));
        let position = NSTextField::labelWithString(&ns(""), mtm);
        position.setFont(Some(&NSFont::monospacedDigitSystemFontOfSize_weight(11.0, unsafe { NSFontWeightRegular })));
        position.setTextColor(Some(&NSColor::secondaryLabelColor()));
        let nav = NSStackView::stackViewWithViews(&NSArray::from_slice(&[prev.as_ref(), position.as_ref(), next.as_ref()]), mtm);
        nav.setOrientation(NSUserInterfaceLayoutOrientation::Horizontal);
        nav.setSpacing(2.0);
        nav.setHidden(true);
        // Top-right: "Not now", one pull-down that pauses every card for the
        // duration picked; they all come back, unanswered, when it ends.
        let snooze = snooze_menu(mtm, &target);
        let spacer = NSView::new(mtm);
        let header = NSStackView::stackViewWithViews(
            &NSArray::from_slice(&[&*badge_holder, who.as_ref(), &*spacer, nav.as_ref(), meta.as_ref(), snooze.as_ref()]),
            mtm,
        );
        header.setOrientation(NSUserInterfaceLayoutOrientation::Horizontal);
        header.setSpacing(8.0);
        header.setAlignment(NSLayoutAttribute::CenterY);

        // Greyed line above the message, hidden unless the question is superseded.
        let note = NSTextField::wrappingLabelWithString(&ns(""), mtm);
        note.setFont(Some(&NSFont::systemFontOfSize(12.0)));
        note.setTextColor(Some(&NSColor::secondaryLabelColor()));
        note.setPreferredMaxLayoutWidth(WIDTH - 2.0 * INSET);
        note.setHidden(true);

        let body = NSTextField::wrappingLabelWithString(&ns(""), mtm);
        body.setFont(Some(&NSFont::systemFontOfSize(14.0)));
        body.setTextColor(Some(&NSColor::labelColor()));
        body.setSelectable(true);
        body.setPreferredMaxLayoutWidth(WIDTH - 2.0 * INSET);

        // A decision card: title, subtitle, then its blocks in a scrolling area.
        let title = small_label(mtm, 15.0, &NSColor::labelColor());
        title.setFont(Some(&NSFont::systemFontOfSize_weight(15.0, unsafe { NSFontWeightBold })));
        let subtitle = small_label(mtm, 12.0, &NSColor::secondaryLabelColor());
        let blocks = NSStackView::new(mtm);
        blocks.setOrientation(NSUserInterfaceLayoutOrientation::Vertical);
        blocks.setAlignment(NSLayoutAttribute::Leading);
        blocks.setSpacing(10.0);
        blocks.setTranslatesAutoresizingMaskIntoConstraints(false);
        let document = {
            let this = FlippedView::alloc(mtm).set_ivars(());
            let view: Retained<FlippedView> = unsafe { msg_send![super(this), init] };
            view
        };
        document.setTranslatesAutoresizingMaskIntoConstraints(false);
        document.addSubview(&blocks);
        let scroll = NSScrollView::new(mtm);
        scroll.setHasVerticalScroller(true);
        scroll.setAutohidesScrollers(true);
        scroll.setDrawsBackground(false);
        scroll.setDocumentView(Some(&document));
        scroll.setHidden(true);
        let clip = scroll.contentView();
        let scroll_height = scroll.heightAnchor().constraintEqualToConstant(0.0);
        activate(&[
            blocks.leadingAnchor().constraintEqualToAnchor(&document.leadingAnchor()),
            blocks.trailingAnchor().constraintEqualToAnchor(&document.trailingAnchor()),
            blocks.topAnchor().constraintEqualToAnchor(&document.topAnchor()),
            blocks.bottomAnchor().constraintEqualToAnchor(&document.bottomAnchor()),
            document.leadingAnchor().constraintEqualToAnchor(&clip.leadingAnchor()),
            document.topAnchor().constraintEqualToAnchor(&clip.topAnchor()),
            document.widthAnchor().constraintEqualToAnchor(&clip.widthAnchor()),
            scroll_height.clone(),
        ]);
        let authorizes = small_label(mtm, 11.0, &NSColor::secondaryLabelColor());
        let error = small_label(mtm, 12.0, &NSColor::systemRedColor());

        // The answer field: borderless text inside a rounded, lightly tinted box,
        // so it reads as the place to type rather than a grey line on a grey
        // card. Wraps up to `MAX_LINES` lines; Enter sends.
        let line_height = NSLayoutManager::new().defaultLineHeightForFont(&field_font()).ceil();
        let field = NSTextField::new(mtm);
        field.setBordered(false);
        field.setDrawsBackground(false);
        field.setFocusRingType(NSFocusRingType::None);
        field.setFont(Some(&field_font()));
        field.setTextColor(Some(&NSColor::labelColor()));
        field.setUsesSingleLineMode(false);
        if let Some(cell) = field.cell() {
            cell.setWraps(true);
            cell.setScrollable(false);
        }
        field.setMaximumNumberOfLines(0);
        unsafe { field.setDelegate(Some(ProtocolObject::from_ref(&*target))) };
        field.setTranslatesAutoresizingMaskIntoConstraints(false);
        let field_box = NSView::new(mtm);
        field_box.setWantsLayer(true);
        if let Some(layer) = field_box.layer() {
            layer.setCornerRadius(10.0);
            layer.setBorderWidth(1.0);
        }
        field_box.addSubview(&field);
        let field_height = field.heightAnchor().constraintEqualToConstant(line_height);
        activate(&[
            field.leadingAnchor().constraintEqualToAnchor_constant(&field_box.leadingAnchor(), 12.0),
            field.trailingAnchor().constraintEqualToAnchor_constant(&field_box.trailingAnchor(), -12.0),
            field.topAnchor().constraintEqualToAnchor_constant(&field_box.topAnchor(), 9.0),
            field.bottomAnchor().constraintEqualToAnchor_constant(&field_box.bottomAnchor(), -9.0),
            field_height.clone(),
        ]);

        // One horizontal row per line of buttons, wrapped by `button_rows`.
        let buttons = NSStackView::new(mtm);
        buttons.setOrientation(NSUserInterfaceLayoutOrientation::Vertical);
        buttons.setAlignment(NSLayoutAttribute::Trailing);
        buttons.setSpacing(BUTTON_SPACING);
        let footer_spacer = NSView::new(mtm);
        let footer = NSStackView::stackViewWithViews(
            &NSArray::from_slice(&[&*footer_spacer, buttons.as_ref()]),
            mtm,
        );
        footer.setOrientation(NSUserInterfaceLayoutOrientation::Horizontal);

        let stack = NSStackView::stackViewWithViews(
            &NSArray::from_slice(&[
                header.as_ref(),
                note.as_ref(),
                body.as_ref(),
                title.as_ref(),
                subtitle.as_ref(),
                scroll.as_ref(),
                &*field_box,
                authorizes.as_ref(),
                error.as_ref(),
                footer.as_ref(),
            ]),
            mtm,
        );
        stack.setOrientation(NSUserInterfaceLayoutOrientation::Vertical);
        stack.setAlignment(NSLayoutAttribute::Leading);
        stack.setSpacing(12.0);
        stack.setCustomSpacing_afterView(10.0, &header);
        stack.setCustomSpacing_afterView(4.0, &title);
        stack.setCustomSpacing_afterView(6.0, &authorizes);
        stack.setEdgeInsets(NSEdgeInsets { top: INSET - 4.0, left: INSET, bottom: INSET - 4.0, right: INSET });
        stack.setTranslatesAutoresizingMaskIntoConstraints(false);
        background.addSubview(&stack);
        let width = stack.widthAnchor().constraintEqualToConstant(WIDTH);
        let inner = |view: &NSView| view.widthAnchor().constraintEqualToAnchor_constant(&stack.widthAnchor(), -2.0 * INSET);
        activate(&[
            stack.leadingAnchor().constraintEqualToAnchor(&background.leadingAnchor()),
            stack.trailingAnchor().constraintEqualToAnchor(&background.trailingAnchor()),
            stack.topAnchor().constraintEqualToAnchor(&background.topAnchor()),
            stack.bottomAnchor().constraintEqualToAnchor(&background.bottomAnchor()),
            width.clone(),
            // Every row spans the card's inner width: the field no longer runs past the edge.
            inner(&header),
            inner(&note),
            inner(&body),
            inner(&title),
            inner(&subtitle),
            inner(&scroll),
            inner(&field_box),
            inner(&authorizes),
            inner(&error),
            inner(&footer),
            badge_holder.widthAnchor().constraintEqualToConstant(28.0),
            badge_holder.heightAnchor().constraintEqualToConstant(28.0),
            badge.centerXAnchor().constraintEqualToAnchor(&badge_holder.centerXAnchor()),
            badge.centerYAnchor().constraintEqualToAnchor(&badge_holder.centerYAnchor()),
        ]);

        Card {
            panel,
            target,
            background,
            wash,
            badge_holder,
            who,
            meta,
            nav,
            prev,
            next,
            position,
            note,
            body,
            title,
            subtitle,
            scroll,
            scroll_height,
            blocks,
            authorizes,
            error,
            field,
            field_box,
            field_height,
            buttons,
            width,
            line_height,
            current: RefCell::new(None),
            shown: RefCell::new(None),
            drafts: RefCell::new(HashMap::new()),
            editors: RefCell::new(Vec::new()),
            edits: RefCell::new(HashMap::new()),
            rich: Cell::new(false),
            announced: RefCell::new(Vec::new()),
            tint: RefCell::new(NSColor::controlAccentColor()),
        }
    }

    fn hide(&self) {
        *self.current.borrow_mut() = None;
        *self.shown.borrow_mut() = None;
        self.panel.orderOut(None);
    }

    /// Queue the answer; the board decides, and the next `show` moves on (or
    /// keeps the card with an error line).
    fn answer(&self, action: String, trigger: Trigger) {
        let Some((id, card_version)) = self.current.borrow().as_ref().map(|c| (c.id.clone(), c.card_version)) else { return };
        let text = self.field.stringValue().to_string().trim().to_string();
        let edits: Vec<(usize, String)> =
            self.editors.borrow().iter().map(|(block, editor)| (*block, editor.view.string().to_string())).collect();
        log::info!("Ask card: answered {} with {:?} ({:?}, v{})", id, action, trigger, card_version);
        if let Ok(mut answers) = ANSWERS.lock() {
            answers.push(CardAnswer { id, action, text, card_version, trigger, edits });
        }
    }

    /// The primary action of a decision card, by key. A grant action answers
    /// only ⌘Return; a disabled one (needs text, field empty) nothing.
    fn trigger_primary(&self, trigger: Trigger) -> bool {
        let primary = self.current.borrow().as_ref().and_then(|c| c.primary.clone());
        let Some((action, grant)) = primary else { return false };
        if grant && trigger != Trigger::CmdReturn {
            return true;
        }
        let index = self.current.borrow().as_ref().and_then(|c| c.buttons.iter().position(|b| *b == action));
        if index.is_some_and(|i| self.button_enabled(i as isize)) {
            self.answer(action, trigger);
        }
        true
    }

    /// Whether button `index` answers: a `needs_input` one does not while the
    /// field is empty.
    fn button_enabled(&self, index: isize) -> bool {
        let empty = self.field.stringValue().to_string().trim().is_empty();
        let needs = self
            .current
            .borrow()
            .as_ref()
            .and_then(|c| usize::try_from(index).ok().and_then(|i| c.made.get(i)).map(|(_, needs)| *needs))
            .unwrap_or(false);
        !(needs && empty)
    }

    /// `needs_input` buttons are dimmed while the field is empty. Not
    /// `setEnabled(false)`: AppKit then erases a custom title entirely.
    fn update_enabled(&self) {
        let empty = self.field.stringValue().to_string().trim().is_empty();
        if let Some(current) = self.current.borrow().as_ref() {
            for (button, needs_input) in &current.made {
                let enabled = !(*needs_input && empty);
                button.setAlphaValue(if enabled { 1.0 } else { 0.4 });
            }
        }
    }

    fn show(&self, mtm: MainThreadMarker, question: &CardQuestion, total: usize, on_board: &[&str]) {
        let shown = Shown {
            id: question.id.to_string(),
            from: question.from.to_string(),
            message: question.message.to_string(),
            buttons: question.buttons.to_vec(),
            input: question.input,
            asked_at: question.asked_at,
            note: question.note.map(str::to_string),
            card: question.card.cloned(),
            card_version: question.card_version,
            position: question.position,
            total,
            error: question.error.map(str::to_string),
        };
        if self.shown.borrow().as_ref() == Some(&shown) && self.panel.isVisible() {
            return;
        }
        // The field's text belongs to its question: kept while browsing, dropped
        // once the question leaves the board.
        let previous = self.current.borrow().as_ref().map(|c| c.id.clone());
        let is_new = previous.as_deref() != Some(question.id);
        {
            let mut drafts = self.drafts.borrow_mut();
            if let Some(prev) = previous.filter(|_| is_new) {
                drafts.insert(prev, self.field.stringValue().to_string());
            }
            drafts.retain(|id, text| on_board.contains(&id.as_str()) && !text.is_empty());
        }
        self.edits.borrow_mut().retain(|id, (version, _)| {
            on_board.contains(&id.as_str()) && !(id == question.id && *version != question.card_version)
        });
        let announce = {
            let mut announced = self.announced.borrow_mut();
            announced.retain(|id| on_board.contains(&id.as_str()));
            let fresh = !announced.iter().any(|id| id == question.id);
            if fresh {
                announced.push(question.id.to_string());
            }
            fresh
        };
        *self.shown.borrow_mut() = Some(shown);
        let rich = question.card.is_some();
        self.rich.set(rich);

        self.who.setStringValue(&ns(question.from));
        let (r, g, b) = crate::ask::agent_rgb(question.from);
        let tint = NSColor::colorWithSRGBRed_green_blue_alpha(r, g, b, 1.0);
        if let Some(layer) = self.badge_holder.layer() {
            layer.setBackgroundColor(Some(&tint.CGColor()));
        }
        self.who.setTextColor(tint.blendedColorWithFraction_ofColor(0.35, &NSColor::whiteColor()).as_deref());
        if let Some(layer) = self.background.layer() {
            layer.setBorderColor(Some(&tint.colorWithAlphaComponent(0.55).CGColor()));
        }
        let colors = cg_colors(&[&tint.colorWithAlphaComponent(0.42), &tint.colorWithAlphaComponent(0.0)]);
        unsafe { self.wash.setColors(Some(&colors)) };
        self.meta.setStringValue(&ns(&short_time(question.asked_at)));
        self.nav.setHidden(total <= 1);
        self.position.setStringValue(&ns(&format!("{} / {}", question.position, total)));
        self.prev.setEnabled(question.position > 1);
        self.prev.setAlphaValue(if question.position > 1 { 1.0 } else { 0.3 });
        self.next.setEnabled(question.position < total);
        self.next.setAlphaValue(if question.position < total { 1.0 } else { 0.3 });
        self.note.setStringValue(&ns(question.note.unwrap_or("")));
        self.note.setHidden(question.note.is_none());
        self.note.setPreferredMaxLayoutWidth(if rich { CARD_WIDTH } else { WIDTH } - 2.0 * INSET);

        self.width.setConstant(if rich { CARD_WIDTH } else { WIDTH });
        self.body.setHidden(rich);
        self.body.setStringValue(&ns(if rich { "" } else { question.message }));
        self.title.setHidden(!rich);
        self.scroll.setHidden(!rich);
        self.subtitle.setHidden(true);
        self.authorizes.setHidden(true);
        if let Some(doc) = question.card {
            self.title.setStringValue(&ns(&doc.title));
            if let Some(sub) = &doc.subtitle {
                self.subtitle.setStringValue(&ns(sub));
                self.subtitle.setHidden(false);
            }
            if let Some(target) = doc.grant_action().and_then(|a| a.grant.as_ref()).map(|g| g.target.clone()) {
                self.authorizes.setStringValue(&ns(&format!("Authorizes: {}", target)));
                self.authorizes.setHidden(false);
            }
            self.draw_blocks(mtm, question.id, doc, &tint);
        }
        self.error.setStringValue(&ns(question.error.unwrap_or("")));
        self.error.setHidden(question.error.is_none());

        if is_new {
            let draft = self.drafts.borrow_mut().remove(question.id).unwrap_or_default();
            self.field.setStringValue(&ns(&draft));
            self.field_height.setConstant(self.line_height);
        }
        self.field_box.setHidden(!question.input);
        if let Some(layer) = self.field_box.layer() {
            let fill = NSColor::whiteColor()
                .colorWithAlphaComponent(0.10)
                .blendedColorWithFraction_ofColor(0.12, &tint);
            layer.setBackgroundColor(fill.map(|c| c.CGColor()).as_deref());
            layer.setBorderColor(Some(&tint.colorWithAlphaComponent(0.6).CGColor()));
        }
        let placeholder = question
            .card
            .and_then(|d| d.input.as_ref())
            .and_then(|i| i.placeholder.clone())
            .unwrap_or_else(|| PLACEHOLDER.to_string());
        self.field.setPlaceholderAttributedString(Some(&attributed(
            &placeholder,
            &NSFont::systemFontOfSize(14.0),
            &NSColor::whiteColor().colorWithAlphaComponent(0.45),
        )));
        *self.tint.borrow_mut() = tint;

        for view in self.buttons.arrangedSubviews().iter() {
            view.removeFromSuperview();
        }
        let card_width = if rich { CARD_WIDTH } else { WIDTH };
        // Right-aligned, the default (first action) last, as macOS dialogs do;
        // wrapped onto several rows when they do not fit the card's width.
        let made: Vec<(Retained<NSButton>, CGFloat, bool)> = question
            .buttons
            .iter()
            .enumerate()
            .map(|(index, id)| self.make_button(mtm, question, id, index, card_width))
            .collect();
        let widths: Vec<CGFloat> = made.iter().map(|(_, w, _)| *w).collect();
        for row in button_rows(&widths, card_width - 2.0 * INSET, BUTTON_SPACING) {
            let line = NSStackView::new(mtm);
            line.setOrientation(NSUserInterfaceLayoutOrientation::Horizontal);
            line.setSpacing(BUTTON_SPACING);
            for index in row {
                line.addArrangedSubview(&made[index].0);
            }
            self.buttons.addArrangedSubview(&line);
        }
        let primary = question.card.and_then(|d| d.primary()).map(|a| (a.id.clone(), a.grant.is_some()));
        *self.current.borrow_mut() = Some(Current {
            id: question.id.to_string(),
            buttons: question.buttons.to_vec(),
            card_version: question.card_version,
            primary,
            made: made.into_iter().map(|(b, _, needs)| (b, needs)).collect(),
        });
        self.update_enabled();

        self.place(mtm, false);
        self.panel.orderFrontRegardless();
        if announce {
            if let Some(sound) = NSSound::soundNamed(&ns("Glass")) {
                sound.play();
            }
            NSApplication::sharedApplication(mtm)
                .requestUserAttention(NSRequestUserAttentionType::InformationalRequest);
        }
    }

    /// The decision card's blocks, rebuilt.
    fn draw_blocks(&self, mtm: MainThreadMarker, id: &str, doc: &Doc, tint: &NSColor) {
        for view in self.blocks.arrangedSubviews().iter() {
            view.removeFromSuperview();
        }
        let mut editors = Vec::new();
        // Leave room for the vertical scroller: a long card scrolls, and a
        // legacy scroller takes its width out of the clip view, cutting the
        // right edge of every fixed-width block (an overlay one draws over it).
        let gutter = NSScroller::scrollerWidthForControlSize_scrollerStyle(
            NSControlSize::Regular,
            NSScroller::preferredScrollerStyle(mtm),
            mtm,
        );
        let width = CARD_WIDTH - 2.0 * INSET - gutter - 4.0;
        let edits = self.edits.borrow();
        let kept = edits.get(id).map(|(_, texts)| texts);
        for (index, block) in doc.blocks.iter().enumerate() {
            // A code block is what the asker will send: the user may change it,
            // and the answer carries his version (`crate::ask::final_blocks`).
            let view = match block {
                crate::decision_card::Block::Code { text, label } => {
                    let text = kept.and_then(|k| k.get(&index)).unwrap_or(text);
                    let (view, editor) = crate::card_view::code_block(mtm, text, label.as_deref(), width, true);
                    if let Some(editor) = editor {
                        editor.view.setDelegate(Some(ProtocolObject::from_ref(&*self.target)));
                        editors.push((index, editor));
                    }
                    view
                }
                _ => crate::card_view::block_view(mtm, block, width, tint),
            };
            self.blocks.addArrangedSubview(&view);
        }
        drop(edits);
        *self.editors.borrow_mut() = editors;
        // Scroll back to the top for a new card.
        if let Some(document) = self.scroll.documentView() {
            document.scrollPoint(CGPoint::new(0.0, 0.0));
        }
    }

    /// A code block was typed in: remember every editor's text for this
    /// question and version, and grow the edited box with its text.
    fn code_edited(&self, mtm: MainThreadMarker) {
        let Some((id, version)) = self.current.borrow().as_ref().map(|c| (c.id.clone(), c.card_version)) else { return };
        let texts: HashMap<usize, String> = self
            .editors
            .borrow()
            .iter()
            .map(|(block, editor)| {
                editor.height.setConstant(crate::card_view::text_height(&editor.view));
                (*block, editor.view.string().to_string())
            })
            .collect();
        self.edits.borrow_mut().insert(id, (version, texts));
        self.place(mtm, true);
    }

    /// Size the card to its content and pin it top-right of its screen (or keep
    /// its top edge where it is, `keep_top`, as the field grows). A decision
    /// card stops at `MAX_SCREEN_SHARE` of the screen: its blocks scroll.
    fn place(&self, mtm: MainThreadMarker, keep_top: bool) {
        let screen = self
            .panel
            .screen()
            .or_else(|| NSScreen::mainScreen(mtm))
            .map(|s| s.visibleFrame())
            .unwrap_or(CGRect::new(CGPoint::new(0.0, 0.0), CGSize::new(1440.0, 900.0)));
        if self.rich.get() {
            self.blocks.layoutSubtreeIfNeeded();
            let content = self.blocks.fittingSize().height;
            self.scroll_height.setConstant(content);
            self.background.layoutSubtreeIfNeeded();
            let total = self.background.fittingSize().height;
            let max = screen.size.height * MAX_SCREEN_SHARE;
            if total > max {
                self.scroll_height.setConstant((content - (total - max)).max(MIN_BLOCKS));
            }
        } else {
            self.scroll_height.setConstant(0.0);
        }
        self.background.layoutSubtreeIfNeeded();
        let fitting = self.background.fittingSize();
        let width = if self.rich.get() { CARD_WIDTH } else { WIDTH };
        let size = CGSize::new(width, fitting.height);
        let frame = if keep_top {
            let old = self.panel.frame();
            CGRect::new(CGPoint::new(old.origin.x, old.origin.y + old.size.height - size.height), size)
        } else {
            CGRect::new(
                CGPoint::new(
                    screen.origin.x + screen.size.width - size.width - 16.0,
                    screen.origin.y + screen.size.height - size.height - 16.0,
                ),
                size,
            )
        };
        self.panel.setFrame_display(frame, true);
        without_animation(|| self.wash.setFrame(CGRect::new(CGPoint::new(0.0, 0.0), size)));
    }

    /// Drawn by hand: macOS paints its default button blue only in the key
    /// window, and the card never takes the keyboard on its own, so system
    /// buttons all came out grey.
    /// Returns the button, the width it takes, and whether it needs text.
    fn make_button(&self, mtm: MainThreadMarker, question: &CardQuestion, id: &str, index: usize, card_width: CGFloat) -> (Retained<NSButton>, CGFloat, bool) {
        let action = question.card.and_then(|d| d.action(id));
        let dismiss = question.dismissable && id == crate::ask::DISMISS && action.is_none();
        let (title, primary, destructive, needs_input) = match (question.card, action) {
            (Some(_), Some(a)) => {
                let hint = if a.grant.is_some() && a.style == Style::Primary { "  ⌘↩" } else { "" };
                (format!("{}{}", a.label, hint), a.style == Style::Primary, a.style == Style::Destructive, a.needs_input)
            }
            (Some(_), None) => (id.to_string(), false, false, false),
            (None, _) => (id.to_string(), index == 0 && !dismiss, false, false),
        };
        let tint = self.tint.borrow();
        let button = unsafe {
            NSButton::buttonWithTitle_target_action(&ns(&title), Some(&self.target), Some(sel!(pressed:)), mtm)
        };
        button.setTag(index as isize);
        button.setBordered(false);
        button.setWantsLayer(true);
        if let Some(layer) = button.layer() {
            layer.setCornerRadius(8.0);
            let fill = if primary { tint.clone() } else { NSColor::whiteColor().colorWithAlphaComponent(0.12) };
            layer.setBackgroundColor(Some(&fill.CGColor()));
        }
        let weight = unsafe { if primary { NSFontWeightSemibold } else { NSFontWeightRegular } };
        let color = if primary {
            NSColor::whiteColor()
        } else if destructive {
            NSColor::systemRedColor()
        } else {
            NSColor::labelColor()
        };
        button.setAttributedTitle(&attributed(&title, &NSFont::systemFontOfSize_weight(13.0, weight), &color));
        // A plain card's default button carries Return; a decision card's keys
        // go through the panel (Return never fires a grant action).
        if primary && question.card.is_none() {
            button.setKeyEquivalent(&ns("\r"));
        }
        button.setTranslatesAutoresizingMaskIntoConstraints(false);
        // A title longer than the card is truncated rather than pushing the card wider.
        let width = (button.attributedTitle().size().width + 28.0).max(72.0).min(card_width - 2.0 * INSET);
        activate(&[
            button.widthAnchor().constraintEqualToConstant(width),
            button.heightAnchor().constraintEqualToConstant(30.0),
        ]);
        (button, width, needs_input)
    }

    /// Grows (or shrinks) the field to its text, up to `MAX_LINES`, and the card
    /// with it, its top edge fixed so the card extends downward like a chat input.
    fn fit_field(&self) {
        let mut text = match self.field.currentEditor() {
            Some(editor) => editor.string().to_string(),
            None => self.field.stringValue().to_string(),
        };
        // A trailing newline starts a line the measure would not count.
        if text.ends_with('\n') || text.is_empty() {
            text.push(' ');
        }
        let width = (self.field.bounds().size.width - 4.0).max(1.0);
        let measured = attributed(&text, &field_font(), &NSColor::labelColor())
            .boundingRectWithSize_options_context(
                CGSize::new(width, CGFloat::MAX),
                NSStringDrawingOptions::UsesLineFragmentOrigin | NSStringDrawingOptions::UsesFontLeading,
                None,
            )
            .size
            .height;
        let height = measured.ceil().max(self.line_height).min(self.line_height * MAX_LINES);
        if (height - self.field_height.constant()).abs() < 1.0 {
            return;
        }
        self.field_height.setConstant(height);
        self.place(MainThreadMarker::from(&*self.target), true);
    }
}

/// `asked_at` as the local short time (`14:05`, or `2:05 PM` in a 12-hour
/// locale), as Swift's `DateFormatter.localizedString(timeStyle: .short)`.
fn short_time(asked_at: u64) -> String {
    let date = NSDate::dateWithTimeIntervalSince1970(asked_at as f64);
    NSDateFormatter::localizedStringFromDate_dateStyle_timeStyle(
        &date,
        NSDateFormatterStyle::NoStyle,
        NSDateFormatterStyle::ShortStyle,
    )
    .to_string()
}

/// Show `question` on the card (replacing what it showed), or hide the card
/// when `None`. `total` is how many questions are on the board (`n / N`, and
/// the Dock badge); `on_board` their ids, to drop the typed text of the ones
/// that left. Calling it again with the same id keeps what the user is typing.
pub fn show(mtm: MainThreadMarker, question: Option<&CardQuestion>, total: usize, on_board: &[&str]) {
    let count = question.map(|_| total);
    if BADGE.with(|b| b.replace(count)) != count {
        let label = count.map(|n| ns(&n.to_string()));
        NSApplication::sharedApplication(mtm).dockTile().setBadgeLabel(label.as_deref());
    }
    match question {
        Some(question) => card_or_build(mtm).show(mtm, question, total, on_board),
        None => {
            if let Some(card) = card() {
                card.drafts.borrow_mut().clear();
                card.hide();
            }
        }
    }
}

/// The cards are paused: hide the card, keep what was typed on each question
/// for when it comes back, and leave the Dock badge counting what waits.
pub fn pause(mtm: MainThreadMarker, total: usize) {
    let count = (total > 0).then_some(total);
    if BADGE.with(|b| b.replace(count)) != count {
        let label = count.map(|n| ns(&n.to_string()));
        NSApplication::sharedApplication(mtm).dockTile().setBadgeLabel(label.as_deref());
    }
    if let Some(card) = card() {
        // The field of the question shown is saved like a browse away from it.
        if let Some(id) = card.current.borrow().as_ref().map(|c| c.id.clone()) {
            let text = card.field.stringValue().to_string();
            if !text.is_empty() {
                card.drafts.borrow_mut().insert(id, text);
            }
        }
        card.hide();
    }
}

/// The pause "Not now" asked since the last call, in seconds.
pub fn take_snooze() -> Option<u64> {
    SNOOZE.lock().ok().and_then(|mut s| s.take())
}

/// Answers given since the last call (button clicked, Return, ⌘Return).
pub fn take_answers() -> Vec<CardAnswer> {
    match ANSWERS.lock() {
        Ok(mut answers) => std::mem::take(&mut *answers),
        Err(_) => Vec::new(),
    }
}

/// ‹ / › presses since the last call: -1 or +1 each.
pub fn take_browse() -> Vec<i32> {
    match BROWSE.lock() {
        Ok(mut queue) => std::mem::take(&mut *queue),
        Err(_) => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snooze_menu_face_is_not_a_choice() {
        assert_eq!(snooze_choice(0), None, "item 0 is the moon face");
        assert_eq!(snooze_choice(-1), None, "nothing selected");
        assert_eq!(snooze_choice(1), Some(0), "the first duration is 20 min");
        assert_eq!(snooze_choice(crate::ask::DND_CHOICES.len() as isize), Some(crate::ask::DND_CHOICES.len() - 1));
        assert_eq!(snooze_choice(crate::ask::DND_CHOICES.len() as isize + 1), None);
    }
}
