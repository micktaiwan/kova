//! Native drawing of a card document's blocks (`crate::decision_card`),
//! shared by the corner card of `kova ask` (`crate::ask_card`) and the views
//! agents dock in panes (`crate::window::dock_view`), so both look the same.
//!
//! Main thread only.

use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{define_class, AllocAnyThread, MainThreadOnly, Message};
use objc2_app_kit::{
    NSAttributedStringNSStringDrawing, NSColor, NSFont, NSFontAttributeName, NSFontManager,
    NSFontTraitMask, NSFontWeightBold, NSFontWeightRegular, NSFontWeightSemibold,
    NSForegroundColorAttributeName, NSLayoutAttribute, NSLayoutConstraint, NSLinkAttributeName,
    NSMutableParagraphStyle, NSParagraphStyleAttributeName, NSResponder, NSScrollView, NSStackView,
    NSTextAlignment, NSTextField, NSTextTab, NSTextView, NSToolTipAttributeName,
    NSUserInterfaceLayoutOrientation, NSView,
};
use objc2_core_foundation::{CGFloat, CGPoint, CGRect, CGSize};
use objc2_foundation::{
    MainThreadMarker, NSArray, NSAttributedString, NSAttributedStringKey, NSDictionary,
    NSMutableAttributedString, NSObject, NSString,
};

use crate::decision_card::{format_number, parse_markup, BarItem, Block};

/// The key column of a `fields` block.
const KEY_COLUMN: CGFloat = 130.0;

define_class!(
    /// The scroll area's document view: flipped, so blocks start at the top.
    #[unsafe(super(NSView, NSResponder, NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "KovaAskCardFlipped"]
    pub struct FlippedView;

    impl FlippedView {
        #[unsafe(method(isFlipped))]
        fn is_flipped(&self) -> bool {
            true
        }
    }
);

pub fn ns(s: &str) -> Retained<NSString> {
    NSString::from_str(s)
}

/// An attributed string with a font and a colour.
pub fn attributed(text: &str, font: &NSFont, color: &NSColor) -> Retained<NSAttributedString> {
    let keys: [&NSAttributedStringKey; 2] = unsafe { [NSFontAttributeName, NSForegroundColorAttributeName] };
    let values: [&AnyObject; 2] = [font.as_ref(), color.as_ref()];
    let attrs = NSDictionary::from_slices(&keys, &values);
    unsafe { NSAttributedString::new_with_attributes(&ns(text), &attrs) }
}

/// A run of a block's text: font, colour, and optionally a link (with its URL
/// as tooltip, so a label cannot hide where it leads) and a paragraph style.
pub fn run(text: &str, font: &NSFont, color: &NSColor, link: Option<&str>, para: Option<&NSMutableParagraphStyle>) -> Retained<NSAttributedString> {
    let mut keys: Vec<&NSAttributedStringKey> = unsafe { vec![NSFontAttributeName, NSForegroundColorAttributeName] };
    let mut values: Vec<Retained<AnyObject>> = vec![
        unsafe { Retained::cast_unchecked::<AnyObject>(font.retain()) },
        unsafe { Retained::cast_unchecked::<AnyObject>(color.retain()) },
    ];
    if let Some(url) = link {
        keys.push(unsafe { NSLinkAttributeName });
        values.push(unsafe { Retained::cast_unchecked::<AnyObject>(ns(url)) });
        keys.push(unsafe { NSToolTipAttributeName });
        values.push(unsafe { Retained::cast_unchecked::<AnyObject>(ns(url)) });
    }
    if let Some(para) = para {
        keys.push(unsafe { NSParagraphStyleAttributeName });
        values.push(unsafe { Retained::cast_unchecked::<AnyObject>(para.retain()) });
    }
    let value_refs: Vec<&AnyObject> = values.iter().map(|v| &**v).collect();
    let attrs = NSDictionary::from_slices(&keys, &value_refs);
    unsafe { NSAttributedString::new_with_attributes(&ns(text), &attrs) }
}

/// `text` under the inline markup of § 4.1, appended to `out`.
pub fn append_markup(mtm: MainThreadMarker, out: &NSMutableAttributedString, text: &str, size: CGFloat, color: &NSColor, para: Option<&NSMutableParagraphStyle>) {
    let manager = NSFontManager::sharedFontManager(mtm);
    for span in parse_markup(text) {
        let weight = unsafe { if span.bold { NSFontWeightBold } else { NSFontWeightRegular } };
        let mut font = if span.code {
            NSFont::monospacedSystemFontOfSize_weight(size - 1.0, weight)
        } else {
            NSFont::systemFontOfSize_weight(size, weight)
        };
        if span.italic {
            font = manager.convertFont_toHaveTrait(&font, NSFontTraitMask::ItalicFontMask);
        }
        let link_color = NSColor::linkColor();
        let color = if span.link.is_some() { &*link_color } else { color };
        out.appendAttributedString(&run(&span.text, &font, color, span.link.as_deref(), para));
    }
}

/// A read-only, selectable text view of `text`, `width` wide, with its height
/// fixed from its laid-out text. Links in it open in the default browser.
pub fn text_view(mtm: MainThreadMarker, text: &NSAttributedString, width: CGFloat) -> Retained<NSTextView> {
    text_view_sized(mtm, text, width).0
}

/// [`text_view`] and its height constraint, for a view whose text changes.
fn text_view_sized(mtm: MainThreadMarker, text: &NSAttributedString, width: CGFloat) -> (Retained<NSTextView>, Retained<NSLayoutConstraint>) {
    let view = NSTextView::initWithFrame(NSTextView::alloc(mtm), CGRect::new(CGPoint::new(0.0, 0.0), CGSize::new(width, 10.0)));
    view.setEditable(false);
    view.setSelectable(true);
    view.setDrawsBackground(false);
    view.setRichText(true);
    view.setTextContainerInset(CGSize::new(0.0, 0.0));
    if let (Some(container), Some(storage)) = unsafe { (view.textContainer(), view.textStorage()) } {
        container.setLineFragmentPadding(0.0);
        container.setWidthTracksTextView(false);
        container.setContainerSize(CGSize::new(width, CGFloat::MAX));
        storage.setAttributedString(text);
    }
    let height = text_height(&view);
    view.setTranslatesAutoresizingMaskIntoConstraints(false);
    let height_constraint = view.heightAnchor().constraintEqualToConstant(height);
    activate(&[view.widthAnchor().constraintEqualToConstant(width), height_constraint.clone()]);
    (view, height_constraint)
}

/// The height of a text view's laid-out text, at least one line's worth of
/// points so an emptied editor stays clickable.
pub fn text_height(view: &NSTextView) -> CGFloat {
    let mut height = 16.0;
    if let (Some(container), Some(layout)) = unsafe { (view.textContainer(), view.layoutManager()) } {
        layout.ensureLayoutForTextContainer(&container);
        height = layout.usedRectForTextContainer(&container).size.height.ceil().max(1.0);
    }
    height.max(16.0)
}

/// An editable `code` block: the text view the user types in and its height
/// constraint, to grow it with the text.
pub struct CodeEditor {
    pub view: Retained<NSTextView>,
    pub height: Retained<NSLayoutConstraint>,
}

/// A `code` block: monospace text in a framed box, `caption` above it in grey.
/// `editable` makes the text a plain-text editor (no smart quotes, dashes or
/// replacements: the bytes typed are the bytes sent) and returns it.
pub fn code_block(mtm: MainThreadMarker, text: &str, caption: Option<&str>, width: CGFloat, editable: bool) -> (Retained<NSView>, Option<CodeEditor>) {
    let label = NSColor::labelColor();
    let grey = NSColor::secondaryLabelColor();
    let font = NSFont::monospacedSystemFontOfSize_weight(12.0, unsafe { NSFontWeightRegular });
    let s = mutable();
    s.appendAttributedString(&attributed(text, &font, &label));
    let (inner, height) = text_view_sized(mtm, &s, width - 20.0);
    let editor = editable.then(|| {
        inner.setRichText(false);
        inner.setEditable(true);
        inner.setAllowsUndo(true);
        inner.setAutomaticQuoteSubstitutionEnabled(false);
        inner.setAutomaticDashSubstitutionEnabled(false);
        inner.setAutomaticTextReplacementEnabled(false);
        inner.setAutomaticSpellingCorrectionEnabled(false);
        inner.setAutomaticLinkDetectionEnabled(false);
        inner.setAutomaticDataDetectionEnabled(false);
        inner.setSmartInsertDeleteEnabled(false);
        // Plain text: font and colour apply to what gets typed too.
        inner.setFont(Some(&font));
        inner.setTextColor(Some(&label));
        inner.setInsertionPointColor(Some(&label));
        CodeEditor { view: inner.clone(), height: height.clone() }
    });
    let frame = NSView::new(mtm);
    frame.setWantsLayer(true);
    if let Some(layer) = frame.layer() {
        layer.setCornerRadius(6.0);
        layer.setBorderWidth(1.0);
        let border = if editable { 0.32 } else { 0.2 };
        layer.setBorderColor(Some(&NSColor::whiteColor().colorWithAlphaComponent(border).CGColor()));
        layer.setBackgroundColor(Some(&NSColor::blackColor().colorWithAlphaComponent(0.18).CGColor()));
    }
    frame.addSubview(&inner);
    frame.setTranslatesAutoresizingMaskIntoConstraints(false);
    activate(&[
        inner.leadingAnchor().constraintEqualToAnchor_constant(&frame.leadingAnchor(), 10.0),
        inner.topAnchor().constraintEqualToAnchor_constant(&frame.topAnchor(), 8.0),
        inner.bottomAnchor().constraintEqualToAnchor_constant(&frame.bottomAnchor(), -8.0),
        frame.widthAnchor().constraintEqualToConstant(width),
    ]);
    let caption = match (caption, editable) {
        (Some(c), true) => Some(format!("{} · editable", c)),
        (Some(c), false) => Some(c.to_string()),
        (None, true) => Some("Editable".to_string()),
        (None, false) => None,
    };
    let view = match caption {
        Some(caption) => {
            let c = NSTextField::labelWithString(&ns(&caption), mtm);
            c.setFont(Some(&NSFont::systemFontOfSize(11.0)));
            c.setTextColor(Some(&grey));
            let column = NSStackView::stackViewWithViews(&NSArray::from_slice(&[c.as_ref(), &*frame]), mtm);
            column.setOrientation(NSUserInterfaceLayoutOrientation::Vertical);
            column.setAlignment(NSLayoutAttribute::Leading);
            column.setSpacing(4.0);
            Retained::into_super(column)
        }
        None => frame,
    };
    (view, editor)
}

pub fn mutable() -> Retained<NSMutableAttributedString> {
    NSMutableAttributedString::new()
}

/// `[c1, c2]` as the untyped array `CAGradientLayer.colors` takes.
pub fn cg_colors(colors: &[&NSColor]) -> Retained<NSArray> {
    let cg: Vec<Retained<AnyObject>> = colors
        .iter()
        // A CGColor is an Objective-C object; the cast only erases its type.
        .map(|c| unsafe { Retained::cast_unchecked::<AnyObject>(c.CGColor()) })
        .collect();
    NSArray::from_retained_slice(&cg)
}

pub fn activate(constraints: &[Retained<NSLayoutConstraint>]) {
    NSLayoutConstraint::activateConstraints(&NSArray::from_retained_slice(constraints));
}

/// One block of a card, `width` points wide. `tint` is the accent colour
/// (the quote bar, the charts).
pub fn block_view(mtm: MainThreadMarker, block: &Block, width: CGFloat, tint: &NSColor) -> Retained<NSView> {
    let label = NSColor::labelColor();
    let grey = NSColor::secondaryLabelColor();
    match block {
        Block::Text { text } => {
            let s = mutable();
            append_markup(mtm, &s, text, 14.0, &label, None);
            Retained::into_super(Retained::into_super(text_view(mtm, &s, width)))
        }
        Block::Context { text } => {
            let s = mutable();
            s.appendAttributedString(&attributed(text, &NSFont::systemFontOfSize(11.0), &grey));
            Retained::into_super(Retained::into_super(text_view(mtm, &s, width)))
        }
        Block::Quote { text, author, time } => {
            let s = mutable();
            let head: Vec<&str> = [author.as_deref(), time.as_deref()].into_iter().flatten().collect();
            if !head.is_empty() {
                s.appendAttributedString(&attributed(&format!("{}\n", head.join(" · ")), &NSFont::systemFontOfSize(11.0), &grey));
            }
            s.appendAttributedString(&attributed(text, &NSFont::systemFontOfSize(13.0), &label));
            let inner = text_view(mtm, &s, width - 12.0);
            let bar = NSView::new(mtm);
            bar.setWantsLayer(true);
            if let Some(layer) = bar.layer() {
                layer.setBackgroundColor(Some(&tint.colorWithAlphaComponent(0.7).CGColor()));
                layer.setCornerRadius(1.5);
            }
            bar.setTranslatesAutoresizingMaskIntoConstraints(false);
            activate(&[bar.widthAnchor().constraintEqualToConstant(3.0)]);
            let row = NSStackView::stackViewWithViews(&NSArray::from_slice(&[&*bar, inner.as_ref()]), mtm);
            row.setOrientation(NSUserInterfaceLayoutOrientation::Horizontal);
            row.setSpacing(9.0);
            row.setAlignment(NSLayoutAttribute::Top);
            activate(&[bar.heightAnchor().constraintEqualToAnchor(&inner.heightAnchor())]);
            Retained::into_super(row)
        }
        Block::Code { text, label: caption } => code_block(mtm, text, caption.as_deref(), width, false).0,
        Block::Fields { items } => {
            // Two columns from one text: a tab stop at the key column,
            // and wrapped values indented under it.
            let para = NSMutableParagraphStyle::new();
            let tab = unsafe {
                NSTextTab::initWithTextAlignment_location_options(NSTextTab::alloc(), NSTextAlignment::Left, KEY_COLUMN, &NSDictionary::new())
            };
            para.setTabStops(Some(&NSArray::from_retained_slice(&[tab])));
            para.setHeadIndent(KEY_COLUMN);
            para.setParagraphSpacing(3.0);
            let s = mutable();
            for (i, item) in items.iter().enumerate() {
                let sep = if i + 1 < items.len() { "\n" } else { "" };
                s.appendAttributedString(&run(&format!("{}\t", item.key), &NSFont::systemFontOfSize(12.0), &grey, None, Some(&para)));
                append_markup(mtm, &s, &item.value, 13.0, &label, Some(&para));
                if !sep.is_empty() {
                    s.appendAttributedString(&run(sep, &NSFont::systemFontOfSize(12.0), &grey, None, Some(&para)));
                }
            }
            Retained::into_super(Retained::into_super(text_view(mtm, &s, width)))
        }
        Block::Divider => {
            let line = NSView::new(mtm);
            line.setWantsLayer(true);
            if let Some(layer) = line.layer() {
                layer.setBackgroundColor(Some(&NSColor::separatorColor().CGColor()));
            }
            line.setTranslatesAutoresizingMaskIntoConstraints(false);
            activate(&[
                line.heightAnchor().constraintEqualToConstant(1.0),
                line.widthAnchor().constraintEqualToConstant(width),
            ]);
            line
        }
        Block::Table { columns, rows } => table_view(mtm, columns, rows, width),
        Block::Bars { items, unit } => bars_view(mtm, items, unit.as_deref(), width, tint),
        Block::Sparkline { values, label: caption } => sparkline_view(mtm, values, caption.as_deref(), width, tint),
        Block::Progress { value, label: caption } => progress_view(mtm, *value, caption.as_deref(), width, tint),
    }
}

fn fixed(view: &NSView, w: CGFloat, h: CGFloat) {
    view.setTranslatesAutoresizingMaskIntoConstraints(false);
    activate(&[view.widthAnchor().constraintEqualToConstant(w), view.heightAnchor().constraintEqualToConstant(h)]);
}

fn small(mtm: MainThreadMarker, text: &str, size: CGFloat, color: &NSColor) -> Retained<NSTextField> {
    let l = NSTextField::labelWithString(&ns(text), mtm);
    l.setFont(Some(&NSFont::systemFontOfSize(size)));
    l.setTextColor(Some(color));
    l
}

fn row(mtm: MainThreadMarker, views: &[&NSView], spacing: CGFloat) -> Retained<NSStackView> {
    let r = NSStackView::stackViewWithViews(&NSArray::from_slice(views), mtm);
    r.setOrientation(NSUserInterfaceLayoutOrientation::Horizontal);
    r.setSpacing(spacing);
    r.setAlignment(NSLayoutAttribute::CenterY);
    r
}

fn column(mtm: MainThreadMarker, views: &[&NSView], spacing: CGFloat) -> Retained<NSStackView> {
    let c = NSStackView::stackViewWithViews(&NSArray::from_slice(views), mtm);
    c.setOrientation(NSUserInterfaceLayoutOrientation::Vertical);
    c.setAlignment(NSLayoutAttribute::Leading);
    c.setSpacing(spacing);
    c
}

/// A filled bar of `fill` points inside a track `track` points wide.
fn bar(mtm: MainThreadMarker, track: CGFloat, fill: CGFloat, height: CGFloat, tint: &NSColor) -> Retained<NSView> {
    let t = NSView::new(mtm);
    t.setWantsLayer(true);
    if let Some(layer) = t.layer() {
        layer.setBackgroundColor(Some(&NSColor::whiteColor().colorWithAlphaComponent(0.08).CGColor()));
        layer.setCornerRadius(height / 2.0);
    }
    let f = NSView::initWithFrame(NSView::alloc(mtm), CGRect::new(CGPoint::new(0.0, 0.0), CGSize::new(fill.clamp(0.0, track), height)));
    f.setWantsLayer(true);
    if let Some(layer) = f.layer() {
        layer.setBackgroundColor(Some(&tint.CGColor()));
        layer.setCornerRadius(height / 2.0);
    }
    t.addSubview(&f);
    fixed(&t, track, height);
    t
}

/// Column widths of a table: each column's widest text, from 40 to 260 points.
fn table_widths(columns: &[String], rows: &[Vec<String>], font: &NSFont) -> Vec<CGFloat> {
    (0..columns.len())
        .map(|c| {
            let widest = std::iter::once(&columns[c])
                .chain(rows.iter().filter_map(|r| r.get(c)))
                .map(|t| attributed(&t.replace('\n', " "), font, &NSColor::labelColor()).size().width)
                .fold(0.0, CGFloat::max);
            (widest + 4.0).clamp(40.0, 260.0)
        })
        .collect()
}

/// A plain table: grey header, thin lines between rows, scrolling sideways
/// when wider than `width`.
fn table_view(mtm: MainThreadMarker, columns: &[String], rows: &[Vec<String>], width: CGFloat) -> Retained<NSView> {
    const GAP: CGFloat = 12.0;
    let font = NSFont::systemFontOfSize(12.0);
    let widths = table_widths(columns, rows, &font);
    let total: CGFloat = widths.iter().sum::<CGFloat>() + GAP * (widths.len().saturating_sub(1)) as CGFloat;
    let grey = NSColor::secondaryLabelColor();
    let label = NSColor::labelColor();
    let mut lines: Vec<Retained<NSView>> = Vec::new();
    let header: Vec<Retained<NSTextView>> = columns
        .iter()
        .zip(&widths)
        .map(|(c, w)| {
            let s = mutable();
            s.appendAttributedString(&attributed(c, &NSFont::systemFontOfSize_weight(11.0, unsafe { NSFontWeightSemibold }), &grey));
            text_view(mtm, &s, *w)
        })
        .collect();
    let refs: Vec<&NSView> = header.iter().map(|v| v.as_ref() as &NSView).collect();
    let h = row(mtm, &refs, GAP);
    h.setAlignment(NSLayoutAttribute::Top);
    lines.push(Retained::into_super(h));
    for r in rows {
        let rule = NSView::new(mtm);
        rule.setWantsLayer(true);
        if let Some(layer) = rule.layer() {
            layer.setBackgroundColor(Some(&NSColor::separatorColor().CGColor()));
        }
        fixed(&rule, total, 1.0);
        lines.push(rule);
        let cells: Vec<Retained<NSTextView>> = r
            .iter()
            .zip(&widths)
            .map(|(cell, w)| {
                let s = mutable();
                append_markup(mtm, &s, &cell.replace('\n', " "), 12.0, &label, None);
                text_view(mtm, &s, *w)
            })
            .collect();
        let refs: Vec<&NSView> = cells.iter().map(|v| v.as_ref() as &NSView).collect();
        let line = row(mtm, &refs, GAP);
        line.setAlignment(NSLayoutAttribute::Top);
        lines.push(Retained::into_super(line));
    }
    let refs: Vec<&NSView> = lines.iter().map(|v| v.as_ref()).collect();
    let table = column(mtm, &refs, 4.0);
    if total <= width {
        return Retained::into_super(table);
    }
    // Wider than the card: the table scrolls sideways inside it.
    table.setTranslatesAutoresizingMaskIntoConstraints(false);
    let height = table.fittingSize().height;
    let document = FlippedView::new_view(mtm);
    document.setFrame(CGRect::new(CGPoint::new(0.0, 0.0), CGSize::new(total, height)));
    document.addSubview(&table);
    activate(&[
        table.leadingAnchor().constraintEqualToAnchor(&document.leadingAnchor()),
        table.topAnchor().constraintEqualToAnchor(&document.topAnchor()),
    ]);
    let scroll = NSScrollView::new(mtm);
    scroll.setHasHorizontalScroller(true);
    scroll.setHasVerticalScroller(false);
    scroll.setDrawsBackground(false);
    scroll.setDocumentView(Some(&document));
    fixed(&scroll, width, height + 14.0);
    Retained::into_super(scroll)
}

/// A horizontal bar chart: label, bar scaled to the largest value, value and unit.
fn bars_view(mtm: MainThreadMarker, items: &[BarItem], unit: Option<&str>, width: CGFloat, tint: &NSColor) -> Retained<NSView> {
    const LABEL_W: CGFloat = 120.0;
    const VALUE_W: CGFloat = 80.0;
    let track = (width - LABEL_W - VALUE_W - 16.0).max(40.0);
    let max = items.iter().map(|i| i.value).fold(0.0, f64::max);
    let grey = NSColor::secondaryLabelColor();
    let lines: Vec<Retained<NSStackView>> = items
        .iter()
        .map(|item| {
            let l = small(mtm, &item.label, 12.0, &NSColor::labelColor());
            l.setLineBreakMode(objc2_app_kit::NSLineBreakMode::ByTruncatingTail);
            fixed(&l, LABEL_W, 16.0);
            let fill = if max > 0.0 { track * (item.value / max) as CGFloat } else { 0.0 };
            let b = bar(mtm, track, fill, 10.0, tint);
            let value = match unit {
                Some(u) => format!("{} {}", format_number(item.value), u),
                None => format_number(item.value),
            };
            let v = small(mtm, &value, 11.0, &grey);
            row(mtm, &[l.as_ref(), &b, v.as_ref()], 8.0)
        })
        .collect();
    let refs: Vec<&NSView> = lines.iter().map(|v| v.as_ref() as &NSView).collect();
    Retained::into_super(column(mtm, &refs, 6.0))
}

/// Where each value of a sparkline sits in a `w` × `h` box (origin
/// bottom-left): spread evenly, scaled between the lowest and highest value;
/// a constant series is a flat line at mid-height.
pub fn sparkline_points(values: &[f64], w: f64, h: f64) -> Vec<(f64, f64)> {
    let lo = values.iter().cloned().fold(f64::INFINITY, f64::min);
    let hi = values.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
    let n = values.len().max(2) as f64 - 1.0;
    values
        .iter()
        .enumerate()
        .map(|(i, v)| {
            let x = w * i as f64 / n;
            let y = if hi > lo { 1.0 + (h - 2.0) * (v - lo) / (hi - lo) } else { h / 2.0 };
            (x, y)
        })
        .collect()
}

/// A one-line chart, its last value printed after it.
fn sparkline_view(mtm: MainThreadMarker, values: &[f64], caption: Option<&str>, width: CGFloat, tint: &NSColor) -> Retained<NSView> {
    let height: CGFloat = 18.0;
    let grey = NSColor::secondaryLabelColor();
    let last = small(mtm, &format_number(*values.last().unwrap_or(&0.0)), 12.0, &NSColor::labelColor());
    let cap = caption.map(|c| small(mtm, c, 12.0, &grey));
    let reserved = 70.0 + if cap.is_some() { 110.0 } else { 0.0 };
    let w = (width - reserved).max(60.0);
    let chart = NSView::new(mtm);
    chart.setWantsLayer(true);
    let shape = objc2_quartz_core::CAShapeLayer::new();
    let path = objc2_core_graphics::CGMutablePath::new();
    for (i, (x, y)) in sparkline_points(values, w, height).into_iter().enumerate() {
        unsafe {
            if i == 0 {
                objc2_core_graphics::CGMutablePath::move_to_point(Some(&path), std::ptr::null(), x, y);
            } else {
                objc2_core_graphics::CGMutablePath::add_line_to_point(Some(&path), std::ptr::null(), x, y);
            }
        }
    }
    shape.setPath(Some(&path));
    shape.setFillColor(None);
    shape.setStrokeColor(Some(&tint.CGColor()));
    shape.setLineWidth(1.5);
    shape.setFrame(CGRect::new(CGPoint::new(0.0, 0.0), CGSize::new(w, height)));
    if let Some(layer) = chart.layer() {
        layer.addSublayer(&shape);
    }
    fixed(&chart, w, height);
    let r = match &cap {
        Some(c) => {
            c.setLineBreakMode(objc2_app_kit::NSLineBreakMode::ByTruncatingTail);
            fixed(c, 100.0, 16.0);
            row(mtm, &[c.as_ref(), &chart, last.as_ref()], 8.0)
        }
        None => row(mtm, &[&chart, last.as_ref()], 8.0),
    };
    Retained::into_super(r)
}

/// A thin progress bar with its label.
fn progress_view(mtm: MainThreadMarker, value: f64, caption: Option<&str>, width: CGFloat, tint: &NSColor) -> Retained<NSView> {
    let b = bar(mtm, width, width * value as CGFloat, 4.0, tint);
    match caption {
        Some(c) => {
            let l = small(mtm, c, 11.0, &NSColor::secondaryLabelColor());
            Retained::into_super(column(mtm, &[l.as_ref(), &b], 4.0))
        }
        None => b,
    }
}

impl FlippedView {
    pub fn new_view(mtm: MainThreadMarker) -> Retained<FlippedView> {
        let this = FlippedView::alloc(mtm).set_ivars(());
        unsafe { objc2::msg_send![super(this), init] }
    }
}

/// Splits buttons (their widths, in action order) into rows that fit `max`,
/// top row first, each row listed left to right. The first action sits
/// bottom-right, as macOS dialogs put the default; the rows fill from the
/// bottom up, each one growing leftward, so the next actions sit beside it.
pub fn button_rows(widths: &[CGFloat], max: CGFloat, spacing: CGFloat) -> Vec<Vec<usize>> {
    let mut rows: Vec<Vec<usize>> = Vec::new();
    let mut used: CGFloat = 0.0;
    for (index, &width) in widths.iter().enumerate() {
        match rows.last_mut() {
            Some(row) if used + spacing + width <= max + 0.5 => {
                row.push(index);
                used += spacing + width;
            }
            _ => {
                rows.push(vec![index]);
                used = width;
            }
        }
    }
    // Built bottom row first, rightmost first: flip both.
    rows.reverse();
    for row in &mut rows {
        row.reverse();
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::{button_rows, sparkline_points};

    #[test]
    fn buttons_that_fit_stay_on_one_row_with_the_default_rightmost() {
        assert_eq!(button_rows(&[100.0, 80.0, 72.0], 360.0, 8.0), vec![vec![2, 1, 0]]);
    }

    #[test]
    fn buttons_that_overflow_wrap_with_the_default_bottom_right() {
        // The case that squeezed one button to an ellipsis: 545 points of buttons in 360.
        let rows = button_rows(&[138.0, 188.0, 123.0, 72.0], 360.0, 8.0);
        assert_eq!(rows, vec![vec![3, 2], vec![1, 0]]);
    }

    #[test]
    fn a_button_as_wide_as_the_card_gets_its_own_row() {
        assert_eq!(button_rows(&[360.0, 72.0], 360.0, 8.0), vec![vec![1], vec![0]]);
        assert!(button_rows(&[], 360.0, 8.0).is_empty());
    }

    #[test]
    fn sparkline_spreads_and_scales_and_flattens_a_constant() {
        assert_eq!(sparkline_points(&[0.0, 5.0, 10.0], 100.0, 12.0), vec![(0.0, 1.0), (50.0, 6.0), (100.0, 11.0)]);
        assert_eq!(sparkline_points(&[3.0, 3.0], 10.0, 18.0), vec![(0.0, 9.0), (10.0, 9.0)]);
        assert_eq!(sparkline_points(&[-1.0, 1.0], 10.0, 12.0), vec![(0.0, 1.0), (10.0, 11.0)], "negative values");
    }
}
