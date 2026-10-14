//! The card an agent's question appears on (`crate::ask`).
//!
//! A port of Kova Float's `QuestionCard.swift`, whose look the user validated:
//! same classes, sizes, colours, fonts and corner radii — do not redesign it here.
//!
//! The card sits top-right of the screen, over everything, without taking the
//! keyboard until the user clicks into it. One question at a time; the others
//! wait behind it ("N more").
//!
//! Layout: a borderless rounded panel on a translucent HUD background, the
//! asker's colour washing down from the top edge and rimming the card. Header
//! (badge, who asks, time, how many wait), the message, the answer field, then
//! the buttons right-aligned with the first one as the default (tinted, Enter).
//!
//! Main thread only. The answers are queued here and drained by Kova's render
//! tick through [`take_answers`]: AppKit calls the button and field callbacks
//! from inside its event loop, where the window list may be borrowed.

use objc2::rc::Retained;
use objc2::runtime::{AnyObject, ProtocolObject, Sel};
use objc2::{define_class, msg_send, sel, MainThreadOnly};
use objc2_app_kit::{
    NSApplication, NSAttributedStringNSExtendedStringDrawing, NSAttributedStringNSStringDrawing,
    NSBackingStoreType, NSButton, NSColor, NSControl, NSControlTextEditingDelegate,
    NSEventModifierFlags, NSFloatingWindowLevel, NSFocusRingType, NSFont,
    NSFontAttributeName, NSFontWeightRegular, NSFontWeightSemibold,
    NSForegroundColorAttributeName, NSLayoutAttribute, NSLayoutConstraint,
    NSLayoutConstraintOrientation, NSLayoutManager, NSLayoutPriorityRequired, NSLineBreakMode,
    NSPanel, NSRequestUserAttentionType, NSResponder, NSScreen, NSSound, NSStackView,
    NSStringDrawingOptions, NSTextAlignment, NSTextField, NSTextFieldDelegate, NSTextView,
    NSUserInterfaceLayoutOrientation, NSView, NSVisualEffectBlendingMode,
    NSVisualEffectMaterial, NSVisualEffectState, NSWindow, NSWindowCollectionBehavior,
    NSWindowStyleMask,
};
use objc2_core_foundation::{CGFloat, CGPoint, CGRect, CGSize};
use objc2_foundation::{
    MainThreadMarker, NSArray, NSAttributedString, NSAttributedStringKey, NSDate,
    NSDateFormatter, NSDateFormatterStyle, NSDictionary, NSEdgeInsets, NSNotification, NSObject,
    NSObjectProtocol, NSString,
};
use objc2_quartz_core::{CAGradientLayer, CATransaction};
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::Mutex;

/// One question as the card shows it.
pub struct CardQuestion<'a> {
    pub id: &'a str,
    /// The agent's name, shown on the card and hashed into its colour.
    pub from: &'a str,
    pub message: &'a str,
    /// Never empty, in order; the first is the primary (drawn last, rightmost).
    pub buttons: &'a [String],
    /// Whether to show the answer field.
    pub input: bool,
    /// Epoch seconds; shown as the local short time.
    pub asked_at: u64,
}

/// What the user answered: the button (or the first one, for Enter) and the
/// field's text, trimmed.
pub struct CardAnswer {
    pub id: String,
    pub action: String,
    pub text: String,
}

/// The card's width, as in Swift.
const WIDTH: CGFloat = 400.0;
/// Horizontal inset of every row; the vertical one is 4 points less.
const INSET: CGFloat = 20.0;
/// Beyond this the field stops growing and scrolls.
const MAX_LINES: CGFloat = 8.0;
const FIELD_FONT_SIZE: CGFloat = 14.0;
const PLACEHOLDER: &str = "Type an answer, or just click a button";

/// Answers given since the last [`take_answers`]. A `Mutex` although only the
/// main thread touches it: `take_answers` takes no `MainThreadMarker`, so a
/// thread_local would silently hand another thread an empty queue.
static ANSWERS: Mutex<Vec<CardAnswer>> = Mutex::new(Vec::new());

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
    /// be typed into.
    #[unsafe(super(NSPanel, NSWindow, NSResponder, NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "KovaAskCardPanel"]
    struct CardPanel;

    impl CardPanel {
        #[unsafe(method(canBecomeKeyWindow))]
        fn can_become_key_window(&self) -> bool {
            true
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
            }
        }

        /// Enter in the field sends with the first button; Shift+Enter (or
        /// Option+Enter) adds a line.
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

    impl CardTarget {
        /// A button: its tag is its index in the question's buttons.
        #[unsafe(method(pressed:))]
        fn pressed(&self, sender: &NSButton) {
            let Some(card) = card() else { return };
            let index = sender.tag();
            let action = card.current.borrow().as_ref().and_then(|c| {
                usize::try_from(index).ok().and_then(|i| c.buttons.get(i).cloned())
            });
            if let Some(action) = action {
                card.answer(action);
            }
        }
    }
);

/// The body of `control:textView:doCommandBySelector:`, outside the macro:
/// an early `return` inside a `define_class!` method fights its `bool`
/// conversion.
fn enter_in_field(mtm: MainThreadMarker, text_view: &NSTextView, selector: Sel) -> bool {
    if selector != sel!(insertNewline:) {
        return false;
    }
    let Some(card) = card() else { return false };
    let Some(primary) = card.current.borrow().as_ref().map(|c| c.buttons[0].clone()) else {
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
    card.answer(primary);
    true
}

/// The question on screen, as much of it as the callbacks need.
struct Current {
    id: String,
    buttons: Vec<String>,
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
    waiting: usize,
}

struct Card {
    panel: Retained<CardPanel>,
    target: Retained<CardTarget>,
    background: Retained<objc2_app_kit::NSVisualEffectView>,
    wash: Retained<CAGradientLayer>,
    badge_holder: Retained<NSView>,
    who: Retained<NSTextField>,
    meta: Retained<NSTextField>,
    body: Retained<NSTextField>,
    field: Retained<NSTextField>,
    field_box: Retained<NSView>,
    /// The field's height, set from its text: it grows a line at a time as the
    /// answer gets longer.
    field_height: Retained<NSLayoutConstraint>,
    buttons: Retained<NSStackView>,
    line_height: CGFloat,
    current: RefCell<Option<Current>>,
    shown: RefCell<Option<Shown>>,
    /// The last question answered: it stays hidden even if `show` is called
    /// with it again before the caller has drained the answer.
    answered: RefCell<Option<String>>,
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

fn ns(s: &str) -> Retained<NSString> {
    NSString::from_str(s)
}

/// An attributed string with a font and a colour.
fn attributed(text: &str, font: &NSFont, color: &NSColor) -> Retained<NSAttributedString> {
    let keys: [&NSAttributedStringKey; 2] = unsafe { [NSFontAttributeName, NSForegroundColorAttributeName] };
    let values: [&AnyObject; 2] = [font.as_ref(), color.as_ref()];
    let attrs = NSDictionary::from_slices(&keys, &values);
    unsafe { NSAttributedString::new_with_attributes(&ns(text), &attrs) }
}

/// `[c1, c2]` as the untyped array `CAGradientLayer.colors` takes.
fn cg_colors(colors: &[&NSColor]) -> Retained<NSArray> {
    let cg: Vec<Retained<AnyObject>> = colors
        .iter()
        // A CGColor is an Objective-C object; the cast only erases its type.
        .map(|c| unsafe { Retained::cast_unchecked::<AnyObject>(c.CGColor()) })
        .collect();
    NSArray::from_retained_slice(&cg)
}

fn activate(constraints: &[Retained<NSLayoutConstraint>]) {
    NSLayoutConstraint::activateConstraints(&NSArray::from_retained_slice(constraints));
}

/// Set a layer property with Core Animation's implicit animation off, as
/// Swift's `CATransaction.setDisableActions(true)` blocks do.
fn without_animation(f: impl FnOnce()) {
    CATransaction::begin();
    CATransaction::setDisableActions(true);
    f();
    CATransaction::commit();
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

        // Header: a round badge, who asks, and on the right the time and the queue.
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
        let spacer = NSView::new(mtm);
        let header = NSStackView::stackViewWithViews(
            &NSArray::from_slice(&[&*badge_holder, who.as_ref(), &*spacer, meta.as_ref()]),
            mtm,
        );
        header.setOrientation(NSUserInterfaceLayoutOrientation::Horizontal);
        header.setSpacing(8.0);
        header.setAlignment(NSLayoutAttribute::CenterY);

        let body = NSTextField::wrappingLabelWithString(&ns(""), mtm);
        body.setFont(Some(&NSFont::systemFontOfSize(14.0)));
        body.setTextColor(Some(&NSColor::labelColor()));
        body.setSelectable(true);
        body.setPreferredMaxLayoutWidth(WIDTH - 2.0 * INSET);

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

        let buttons = NSStackView::new(mtm);
        buttons.setOrientation(NSUserInterfaceLayoutOrientation::Horizontal);
        buttons.setSpacing(8.0);
        let footer_spacer = NSView::new(mtm);
        let footer = NSStackView::stackViewWithViews(
            &NSArray::from_slice(&[&*footer_spacer, buttons.as_ref()]),
            mtm,
        );
        footer.setOrientation(NSUserInterfaceLayoutOrientation::Horizontal);

        let stack = NSStackView::stackViewWithViews(
            &NSArray::from_slice(&[header.as_ref(), body.as_ref(), &*field_box, footer.as_ref()]),
            mtm,
        );
        stack.setOrientation(NSUserInterfaceLayoutOrientation::Vertical);
        stack.setAlignment(NSLayoutAttribute::Leading);
        stack.setSpacing(12.0);
        stack.setCustomSpacing_afterView(10.0, &header);
        stack.setEdgeInsets(NSEdgeInsets { top: INSET - 4.0, left: INSET, bottom: INSET - 4.0, right: INSET });
        stack.setTranslatesAutoresizingMaskIntoConstraints(false);
        background.addSubview(&stack);
        activate(&[
            stack.leadingAnchor().constraintEqualToAnchor(&background.leadingAnchor()),
            stack.trailingAnchor().constraintEqualToAnchor(&background.trailingAnchor()),
            stack.topAnchor().constraintEqualToAnchor(&background.topAnchor()),
            stack.bottomAnchor().constraintEqualToAnchor(&background.bottomAnchor()),
            stack.widthAnchor().constraintEqualToConstant(WIDTH),
            // Every row spans the card's inner width: the field no longer runs past the edge.
            header.widthAnchor().constraintEqualToAnchor_constant(&stack.widthAnchor(), -2.0 * INSET),
            body.widthAnchor().constraintEqualToAnchor_constant(&stack.widthAnchor(), -2.0 * INSET),
            field_box.widthAnchor().constraintEqualToAnchor_constant(&stack.widthAnchor(), -2.0 * INSET),
            footer.widthAnchor().constraintEqualToAnchor_constant(&stack.widthAnchor(), -2.0 * INSET),
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
            body,
            field,
            field_box,
            field_height,
            buttons,
            line_height,
            current: RefCell::new(None),
            shown: RefCell::new(None),
            answered: RefCell::new(None),
            tint: RefCell::new(NSColor::controlAccentColor()),
        }
    }

    fn hide(&self) {
        *self.current.borrow_mut() = None;
        *self.shown.borrow_mut() = None;
        self.panel.orderOut(None);
    }

    /// Queue the answer and hide the card until the caller shows the next one.
    fn answer(&self, action: String) {
        let Some(id) = self.current.borrow().as_ref().map(|c| c.id.clone()) else { return };
        let text = self.field.stringValue().to_string().trim().to_string();
        log::info!("Ask card: answered {} with {:?}", id, action);
        if let Ok(mut answers) = ANSWERS.lock() {
            answers.push(CardAnswer { id: id.clone(), action, text });
        }
        *self.answered.borrow_mut() = Some(id);
        self.hide();
    }

    fn show(&self, mtm: MainThreadMarker, question: &CardQuestion, waiting: usize) {
        if self.answered.borrow().as_deref() == Some(question.id) {
            return;
        }
        let shown = Shown {
            id: question.id.to_string(),
            from: question.from.to_string(),
            message: question.message.to_string(),
            buttons: question.buttons.to_vec(),
            input: question.input,
            asked_at: question.asked_at,
            waiting,
        };
        if self.shown.borrow().as_ref() == Some(&shown) && self.panel.isVisible() {
            return;
        }
        let is_new = self.current.borrow().as_ref().map(|c| c.id.as_str()) != Some(question.id);
        *self.current.borrow_mut() = Some(Current { id: shown.id.clone(), buttons: shown.buttons.clone() });
        *self.shown.borrow_mut() = Some(shown);

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
        let time = short_time(question.asked_at);
        let meta = if waiting > 0 { format!("{} · {} more", time, waiting) } else { time };
        self.meta.setStringValue(&ns(&meta));
        self.body.setStringValue(&ns(question.message));
        if is_new {
            self.field.setStringValue(&ns(""));
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
        self.field.setPlaceholderAttributedString(Some(&attributed(
            PLACEHOLDER,
            &NSFont::systemFontOfSize(14.0),
            &NSColor::whiteColor().colorWithAlphaComponent(0.45),
        )));
        *self.tint.borrow_mut() = tint;

        for view in self.buttons.arrangedSubviews().iter() {
            view.removeFromSuperview();
        }
        // Right-aligned, the default (first action) last, as macOS dialogs do.
        for (index, title) in question.buttons.iter().enumerate().rev() {
            let button = self.make_button(mtm, title, index);
            self.buttons.addArrangedSubview(&button);
        }

        self.background.layoutSubtreeIfNeeded();
        let fitting = self.background.fittingSize();
        without_animation(|| self.wash.setFrame(CGRect::new(CGPoint::new(0.0, 0.0), fitting)));
        let size = CGSize::new(WIDTH, fitting.height);
        let screen = NSScreen::mainScreen(mtm)
            .map(|s| s.visibleFrame())
            .unwrap_or(CGRect::new(CGPoint::new(0.0, 0.0), CGSize::new(1440.0, 900.0)));
        let frame = CGRect::new(
            CGPoint::new(
                screen.origin.x + screen.size.width - size.width - 16.0,
                screen.origin.y + screen.size.height - size.height - 16.0,
            ),
            size,
        );
        self.panel.setFrame_display(frame, true);
        self.panel.orderFrontRegardless();
        if is_new {
            if let Some(sound) = NSSound::soundNamed(&ns("Glass")) {
                sound.play();
            }
            NSApplication::sharedApplication(mtm)
                .requestUserAttention(NSRequestUserAttentionType::InformationalRequest);
        }
    }

    /// Drawn by hand: macOS paints its default button blue only in the key
    /// window, and the card never takes the keyboard on its own, so system
    /// buttons all came out grey.
    fn make_button(&self, mtm: MainThreadMarker, title: &str, index: usize) -> Retained<NSButton> {
        let primary = index == 0;
        let tint = self.tint.borrow();
        let button = unsafe {
            NSButton::buttonWithTitle_target_action(&ns(title), Some(&self.target), Some(sel!(pressed:)), mtm)
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
        let color = if primary { NSColor::whiteColor() } else { NSColor::labelColor() };
        button.setAttributedTitle(&attributed(title, &NSFont::systemFontOfSize_weight(13.0, weight), &color));
        if primary {
            button.setKeyEquivalent(&ns("\r"));
        }
        button.setTranslatesAutoresizingMaskIntoConstraints(false);
        let width = button.attributedTitle().size().width + 28.0;
        activate(&[
            button.widthAnchor().constraintEqualToConstant(width.max(72.0)),
            button.heightAnchor().constraintEqualToConstant(30.0),
        ]);
        button
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
        self.background.layoutSubtreeIfNeeded();
        let new_height = self.background.fittingSize().height;
        let mut frame = self.panel.frame();
        frame.origin.y = frame.origin.y + frame.size.height - new_height;
        frame.size.height = new_height;
        self.panel.setFrame_display(frame, true);
        without_animation(|| self.wash.setFrame(CGRect::new(CGPoint::new(0.0, 0.0), frame.size)));
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
/// when `None`. `waiting` is how many more questions queue behind it.
///
/// Also sets the Dock badge to the total count (`waiting + 1`), or clears it.
/// Calling it again with the same id keeps what the user is typing.
pub fn show(mtm: MainThreadMarker, question: Option<&CardQuestion>, waiting: usize) {
    let count = question.map(|_| waiting + 1);
    if BADGE.with(|b| b.replace(count)) != count {
        let label = count.map(|n| ns(&n.to_string()));
        NSApplication::sharedApplication(mtm).dockTile().setBadgeLabel(label.as_deref());
    }
    match question {
        Some(question) => card_or_build(mtm).show(mtm, question, waiting),
        None => {
            if let Some(card) = card() {
                card.hide();
                *card.answered.borrow_mut() = None;
            }
        }
    }
}

/// Answers given since the last call (button clicked or Enter).
pub fn take_answers() -> Vec<CardAnswer> {
    match ANSWERS.lock() {
        Ok(mut answers) => std::mem::take(&mut *answers),
        Err(_) => Vec::new(),
    }
}
