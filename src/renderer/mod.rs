mod blink;
pub mod context;
pub mod glyph_atlas;
pub mod layer;
pub mod pane_renderer;
pub mod pipeline;
pub mod vertex;

/// Horizontal padding inside a pane, in LOGICAL points. Always go through
/// `RenderContext::h_padding()` (or `KovaView::h_padding()`), which scales it to
/// pixels — using the constant raw makes the pane's apparent padding, and
/// therefore its column count, depend on the display's scale factor.
pub const PANE_H_PADDING: f32 = 10.0;
const TOOLTIP_ANIM_FRAMES: u8 = 10; // ~166ms at 60fps

/// Synthetic italic slant: tan(~12°). The glyph quad's top edge is shifted this
/// fraction of the baseline height to the right, the descender edge to the left.
/// Fallback only — used when the font has no real italic face, or for a char
/// (box-drawing, etc.) with no italic form. Real italic glyphs are rasterized
/// from the font's italic face; see `GlyphAtlas::rasterize_italic_char`.
const ITALIC_SHEAR: f32 = 0.213;

/// A hoverable zone in a status bar, with associated tooltip text.
#[derive(Clone)]
struct TooltipZone {
    x: f32,
    y: f32,
    width: f32,
    height: f32,
    text: &'static str,
}

impl TooltipZone {
    fn contains(&self, px: f32, py: f32) -> bool {
        px >= self.x && px < self.x + self.width && py >= self.y && py < self.y + self.height
    }
}

/// Active tooltip state for rendering.
#[derive(Clone, PartialEq)]
pub struct ActiveTooltip {
    pub text: &'static str,
    pub anchor_x: f32,
    pub anchor_y: f32,
}

// The tab palette lives in `crate::tab_colors`, outside the renderer, so the
// IPC transport can validate `set-tab-color` indices without depending on it.
use crate::tab_colors::TAB_COLORS;

/// Saturation kept on an inactive colored tab (CSS `saturate(0.7)`).
const DIM_SATURATION: f32 = 0.7;
/// Brightness kept on an inactive colored tab (CSS `brightness(0.82)`).
const DIM_BRIGHTNESS: f32 = 0.82;

/// Dim a tab color so only the active tab shows its full hue, matching the
/// CSS filter chain `saturate(0.7) brightness(0.82)`: pull the channels toward
/// the luminance, then scale. The tint stays recognizable, but the active tab
/// is the only saturated block in the bar.
fn dim_inactive_tab(c: [f32; 3]) -> [f32; 3] {
    let lum = 0.213 * c[0] + 0.715 * c[1] + 0.072 * c[2];
    let mix = |v: f32| ((lum + (v - lum) * DIM_SATURATION) * DIM_BRIGHTNESS).clamp(0.0, 1.0);
    [mix(c[0]), mix(c[1]), mix(c[2])]
}

const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

/// Format a Unix timestamp (seconds) as "DD/MMM HH:mm" in local time.
fn format_last_activity(ts: u64) -> String {
    let t = ts as libc::time_t;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    unsafe { libc::localtime_r(&t, &mut tm) };
    let mon = MONTHS.get(tm.tm_mon as usize).copied().unwrap_or("???");
    format!(
        "{:02}/{} {:02}:{:02}",
        tm.tm_mday, mon, tm.tm_hour, tm.tm_min
    )
}

/// Format a count as human-readable string (e.g. "1.2K", "3.4M").
fn format_count(n: u64) -> String {
    if n < 1_000 {
        format!("{}", n)
    } else if n < 1_000_000 {
        format!("{:.1}K", n as f64 / 1_000.0)
    } else if n < 1_000_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else {
        format!("{:.1}G", n as f64 / 1_000_000_000.0)
    }
}

use blink::CursorBlink;
use context::RenderContext;
use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2_metal::*;
use objc2_quartz_core::{CAMetalDrawable, CAMetalLayer};
#[cfg(test)]
use pane_renderer::{PaneBuild, can_reuse_pane_cache};
use pane_renderer::{PaneDrawKey, PaneRenderer, PaneStyle};
use parking_lot::RwLock;
use std::ptr::NonNull;
use std::sync::Arc;
use std::time::SystemTime;
use vertex::Vertex;

#[cfg(test)]
use crate::config::DimMode;
use crate::config::{Config, KeysConfig};
use crate::pane::PaneId;

/// Color of the minimized-pane marker (status-bar counter and switcher ⊟ icon).
/// Violet: distinct from the bell (orange) and completion (green) dots.
const MINIMIZED_FG: [f32; 4] = [0.75, 0.55, 0.95, 1.0];

/// Color of the anchor mark drawn on a pane row whose conversation is one of
/// today's anchors. Gold, the colour the overlay's own title wears: an anchor
/// is a claim about the day, not another kind of attention.
const ANCHOR_FG: [f32; 4] = [1.0, 0.8, 0.25, 1.0];

/// Color of the "Claude Code is working" marker (status-bar ✳ counter and
/// switcher ✳ icon). Green: something is happening, nothing is owed.
const WORKING_FG: [f32; 4] = [0.6, 0.85, 0.6, 1.0];
/// Open todos at the end of a pane switcher row: warm, so a conversation with
/// work left in it is spotted without reading the dim tail of every row.
const RAF_TODO_FG: [f32; 4] = [0.95, 0.6, 0.3, 1.0];
/// Between the todo and idea counts of a switcher row.
const RAF_SEPARATOR: &str = " \u{b7} ";
/// Frame of a subject's box in the pane switcher: the tab headers' blue,
/// dimmed, so the box groups rows without competing with their text.
const SUBJECT_FRAME: [f32; 3] = [0.4, 0.52, 0.72];
/// Thickness of that frame, in drawable pixels.
const SUBJECT_FRAME_PX: f32 = 2.0;
/// A subject's name, on the tinted title bar of its box.
const SUBJECT_TITLE_FG: [f32; 4] = [0.95, 0.95, 1.0, 1.0];

/// One stretch of the subject strip: a subject's name over the columns its
/// panes occupy side by side, in its color.
#[derive(Clone, Debug, PartialEq)]
pub struct SubjectSegment {
    pub x: f32,
    pub width: f32,
    pub name: String,
    /// Index into `TAB_COLORS`; `None` falls back to the Cmd+P frame blue.
    pub color: Option<usize>,
    /// The focused pane is under it: drawn in full color, the others dimmed
    /// as inactive tabs are.
    pub focused: bool,
}

/// The subject strip of the active tab: where it sits and what it shows.
#[derive(Clone, Debug, Default)]
pub struct SubjectStrip {
    pub y: f32,
    pub height: f32,
    pub segments: Vec<SubjectSegment>,
}

/// The strip's segments from the tab's columns, left to right: for each,
/// `(x, width, subject, focused)` — the subject being the first one filed
/// among the column's panes, top first. Neighboring columns of the same
/// subject make one segment; a column in no subject breaks the run, so the
/// same name shows twice when a subject's panes are apart.
pub fn subject_strip_segments(
    columns: &[(f32, f32, Option<(String, Option<usize>)>, bool)],
) -> Vec<SubjectSegment> {
    let mut out: Vec<SubjectSegment> = Vec::new();
    let mut previous_filed = false;
    for (x, width, subject, focused) in columns {
        let Some((name, color)) = subject else {
            previous_filed = false;
            continue;
        };
        if *width <= 0.0 {
            // A fully minimized column takes no room: it neither shows nor
            // breaks the run it sits in.
            continue;
        }
        match out.last_mut() {
            Some(last) if previous_filed && last.name == *name => {
                last.width = x + width - last.x;
                last.focused |= *focused;
            }
            _ => out.push(SubjectSegment {
                x: *x,
                width: *width,
                name: name.clone(),
                color: *color,
                focused: *focused,
            }),
        }
        previous_filed = true;
    }
    out
}

/// Color of the status-bar unread counter. Same amber as the per-pane bell dot:
/// the counter and the dots it sums must read as one signal.
const UNREAD_FG: [f32; 4] = [0.9, 0.6, 0.2, 1.0];

/// Attention state for a non-focused pane (bell > completion > none).
#[derive(Clone, Copy, PartialEq, Debug)]
enum PaneAttention {
    None,
    Completion,
    Bell,
}

impl PaneAttention {
    fn from_flags(has_bell: bool, has_completion: bool) -> Self {
        if has_bell {
            Self::Bell
        } else if has_completion {
            Self::Completion
        } else {
            Self::None
        }
    }

    fn dot_color(self) -> Option<[f32; 4]> {
        match self {
            Self::Bell => Some([0.9, 0.6, 0.2, 1.0]),
            Self::Completion => Some([0.2, 0.8, 0.3, 1.0]),
            Self::None => None,
        }
    }

    fn bar_bg(self, default: [f32; 3]) -> [f32; 3] {
        match self {
            Self::Bell => [0.35, 0.22, 0.10],
            Self::Completion => [0.15, 0.30, 0.15],
            Self::None => default,
        }
    }
}
#[cfg(test)]
use crate::terminal::CursorShape;
use crate::terminal::{Cell, FilterMatch, TerminalState};

/// Data passed to the renderer for drawing filter overlay.
pub struct FilterRenderData {
    pub query: String,
    pub matches: Vec<FilterMatch>,
    /// The match ↑/↓ picked in the list: painted as the selected row, and the
    /// one Enter jumps to.
    pub selected: usize,
    /// Shown instead of the match count when there is nothing to count yet —
    /// how to get back a query filtered earlier in this run.
    pub hint: Option<String>,
}

/// First listed match the filter overlay draws, so the row at `selected` stays
/// in view. Stateless on purpose: the list shows `max_visible` rows starting
/// here, and both the drawing and the click hit-testing go through this one
/// function, so a click always lands on the row it was drawn over.
pub fn filter_list_scroll(selected: usize, max_visible: usize) -> usize {
    if max_visible == 0 {
        return 0;
    }
    selected.saturating_sub(max_visible - 1)
}

/// A single closed tab in the `Cmd+O` overlay.
pub struct RecentProjectEntry {
    /// Name the user gave the tab; `None` shows the path alone.
    pub title: Option<String>,
    /// Index into `TAB_COLORS` the tab had; painted as the row background.
    pub color: Option<usize>,
    pub path: String,
    pub time_ago: String,
    pub pane_count: usize,
    pub invalid: bool,
}

/// Data passed to the renderer for drawing recent projects overlay.
pub struct RecentProjectsRenderData<'a> {
    /// The rows the filter keeps.
    pub entries: &'a [&'a RecentProjectEntry],
    /// Filter typed in the input field, and the caret in it (in chars).
    pub query: &'a str,
    pub cursor: usize,
    /// Closed tabs before filtering, to tell "none at all" from "none match".
    pub total: usize,
    pub selected: usize,
    pub scroll: usize,
}

/// One row of the search palette result list: either a non-selectable group
/// header (a tab name, or the "Tabs" section divider) or a selectable hit.
pub struct SearchRowRender<'a> {
    pub text: &'a str,
    pub is_header: bool,
}

/// Data passed to the renderer for drawing the search palette overlay.
pub struct SearchPaletteRenderData<'a> {
    /// Current input string.
    pub query: &'a str,
    /// Caret position in `query`, in chars.
    pub cursor: usize,
    /// Last submitted query (used to label the result list while the user
    /// continues typing — drives the "Results for: …" header).
    pub submitted_query: &'a str,
    /// True while a worker thread is still running for the submitted query.
    pub searching: bool,
    /// Result rows (headers + hits) to display.
    pub rows: &'a [SearchRowRender<'a>],
    /// Selected index into `rows` — always a hit row when one exists.
    pub selected: usize,
    /// First visible row in the result list (vertical scroll offset).
    pub scroll: usize,
}

/// Data passed to the renderer for drawing a list-selection overlay
/// (used by "Send Tab to Window" and "Merge Tab").
pub struct SendToWindowRenderData<'a> {
    pub title: &'a str,
    pub entries: &'a [String],
    pub selected: usize,
    /// Whether the last entry is a special "New Window" option (gets a distinct color).
    pub has_new_entry: bool,
}

/// One row of the pane-switcher overlay: either a non-selectable tab header
/// or a selectable pane entry.
pub struct PaneSwitcherRowRender<'a> {
    pub text: &'a str,
    pub is_header: bool,
    /// Pending bell on this pane (unread) — drives an attention dot.
    pub has_bell: bool,
    /// A command completed on this pane (unread) — drives an attention dot.
    pub has_completion: bool,
    /// Pane is minimized (hidden from the layout) — drives a colored ⊟ icon.
    pub minimized: bool,
    /// Claude Code is actively working in this pane — drives a ✳ marker.
    pub working: bool,
    /// This pane is waiting for the user — drives a ? marker.
    /// Binary running in the pane ("claude 2.1.226"), shown dim at the right
    /// end of the row. `None` on headers and at a bare shell prompt.
    pub process: Option<&'a str>,
    /// This pane holds a bookmarked conversation — the row is painted light
    /// blue on black text so a tracked project is spotted without reading.
    pub bookmarked: bool,
    /// This pane holds one of today's anchors — the row wears an \u{2693} in the
    /// anchor colour, and the anchors section leaves it out: one row per
    /// conversation, where the conversation actually is.
    pub anchored: bool,
    /// Open todos of the pane's Claude conversation (`"3 todo"`), drawn in
    /// colour at the very end of the row, after the binary.
    pub raf_todo: Option<&'a str>,
    /// Open ideas of the same conversation (`"1 idea"`), drawn dim after the
    /// todos (` · ` between them), or alone when there are no todos.
    pub raf_idea: Option<&'a str>,
    /// A subject's header (`is_header` is set too): its name on a tinted
    /// title bar, first line of the subject's box.
    pub subject_header: bool,
    /// A pane filed under a subject: drawn inside the box.
    pub in_subject: bool,
    /// The box's frame opens above this row (a subject header).
    pub box_top: bool,
    /// The box's frame closes under this row (its last member, or the header
    /// of an empty subject).
    pub box_bottom: bool,
    /// The subject's color, an index into `TAB_COLORS`: the box's frame and
    /// title bar, as the subject strip draws it. `None` = the default blue.
    pub subject_color: Option<usize>,
}

/// One column of the pane-switcher overlay: a vertical run of rows holding
/// whole tabs (a tab header followed by its panes), never split mid-tab.
pub struct PaneSwitcherColumnRender<'a> {
    pub rows: &'a [PaneSwitcherRowRender<'a>],
    /// First visible row in this column (vertical scroll offset).
    pub scroll: usize,
}

/// Data passed to the renderer for drawing the tab/pane switcher overlay.
pub struct PaneSwitcherRenderData<'a> {
    pub columns: &'a [PaneSwitcherColumnRender<'a>],
    /// Selected column index.
    pub selected_col: usize,
    /// Selected row within `columns[selected_col]` — always a pane row.
    pub selected_row: usize,
    /// Attention-only list: every pane row shown is asking for something, and
    /// the panes that are not have been left out. Changes the title and the
    /// hint, so the list never looks like a truncated version of the full one.
    pub filtered: bool,
    /// The bookmark half is up: the saved conversations in place of the open
    /// panes (`b`). Changes the title and the hint line, for the same reason.
    pub show_bookmarks: bool,
    /// The memory list is up (`m`): every pane, biggest first. Carries what
    /// they hold together ("13.8 GB") and how many there are, for the title.
    pub memory_total: Option<(String, usize)>,
}

/// Vertical geometry of a list overlay (title + subtitle + scrolling rows),
/// shared between the renderer and mouse hit-testing so a click maps to the
/// exact row that was drawn. Computed purely from cell size + viewport height.
/// First row the search palette shows: its scroll, moved down just enough to
/// keep the selected row in view.
pub fn search_palette_scroll(selected: usize, scroll: usize, max_visible: usize) -> usize {
    if selected >= scroll + max_visible {
        selected + 1 - max_visible
    } else {
        scroll
    }
}

pub struct OverlayListGeometry {
    pub content_top: f32,
    pub row_height: f32,
    pub max_visible: usize,
}

/// Geometry of the big directory label drawn over a flashing pane.
#[derive(Debug, Clone, Copy, PartialEq)]
struct FlashLabelLayout {
    name_scale: f32,
    name_x: f32,
    name_y: f32,
    parent_scale: f32,
    parent_x: f32,
    parent_y: f32,
    session_scale: f32,
    session_x: f32,
    session_y: f32,
    box_x: f32,
    box_y: f32,
    box_w: f32,
    box_h: f32,
}

/// Lay out the flash label inside a pane rectangle: the directory name is
/// blown up as far as it fits (never past `FLASH_NAME_MAX_SCALE`, where the
/// upscaled atlas bitmap starts to smear), the path above it sits underneath
/// at a fixed small scale, the conversation name under that, and the whole
/// block is centered behind a padded backdrop. `session_chars` is 0 when the
/// pane holds no agent conversation, and that line then takes no space. Pure
/// geometry, so it can be checked without a GPU.
fn flash_label_layout(
    pane: (f32, f32, f32, f32),
    name_chars: usize,
    parent_chars: usize,
    session_chars: usize,
    cell_w: f32,
    cell_h: f32,
) -> FlashLabelLayout {
    const FLASH_NAME_MAX_SCALE: f32 = 3.0;
    const FLASH_NAME_MIN_SCALE: f32 = 1.0;
    const FLASH_PARENT_SCALE: f32 = 1.1;
    const FLASH_SESSION_SCALE: f32 = 1.4;
    /// Fraction of the pane width the name is allowed to span.
    const FLASH_WIDTH_RATIO: f32 = 0.8;

    let (px, py, pw, ph) = pane;
    let name_chars = name_chars.max(1) as f32;
    let name_scale = ((pw * FLASH_WIDTH_RATIO) / (name_chars * cell_w))
        .clamp(FLASH_NAME_MIN_SCALE, FLASH_NAME_MAX_SCALE);
    let name_w = name_chars * cell_w * name_scale;
    let name_h = cell_h * name_scale;

    // The path line shrinks below its nominal scale rather than being clipped
    // in a narrow pane.
    let parent_scale = if parent_chars == 0 {
        FLASH_PARENT_SCALE
    } else {
        FLASH_PARENT_SCALE
            .min((pw * FLASH_WIDTH_RATIO) / (parent_chars as f32 * cell_w))
            .max(0.6)
    };
    let parent_w = parent_chars as f32 * cell_w * parent_scale;
    let parent_h = if parent_chars == 0 {
        0.0
    } else {
        cell_h * parent_scale
    };
    let gap = if parent_chars == 0 {
        0.0
    } else {
        name_h * 0.15
    };

    // Same shrink-rather-than-clip rule as the path line.
    let session_scale = if session_chars == 0 {
        FLASH_SESSION_SCALE
    } else {
        FLASH_SESSION_SCALE
            .min((pw * FLASH_WIDTH_RATIO) / (session_chars as f32 * cell_w))
            .max(0.6)
    };
    let session_w = session_chars as f32 * cell_w * session_scale;
    let session_h = if session_chars == 0 {
        0.0
    } else {
        cell_h * session_scale
    };
    let session_gap = if session_chars == 0 {
        0.0
    } else {
        name_h * 0.15
    };

    let block_h = name_h + gap + parent_h + session_gap + session_h;
    let top = py + (ph - block_h) / 2.0;
    let pad_x = cell_w * name_scale;
    let pad_y = name_h * 0.35;
    let box_w = (name_w.max(parent_w).max(session_w) + pad_x * 2.0).min(pw);

    FlashLabelLayout {
        name_scale,
        name_x: px + (pw - name_w) / 2.0,
        name_y: top,
        parent_scale,
        parent_x: px + (pw - parent_w) / 2.0,
        parent_y: top + name_h + gap,
        session_scale,
        session_x: px + (pw - session_w) / 2.0,
        session_y: top + name_h + gap + parent_h + session_gap,
        box_x: px + (pw - box_w) / 2.0,
        box_y: top - pad_y,
        box_w,
        box_h: block_h + pad_y * 2.0,
    }
}

/// How far past `max_x` a glyph cell may end and still count as fitting.
///
/// A right-aligned run starts at `max_x - n * cell_w`, so its last cell ends
/// exactly on `max_x` — in exact arithmetic. In f32 the subtraction and the
/// n additions that walk the run back rarely cancel to the bit, and an
/// overshoot of one ulp used to drop the last glyph: the pane switcher showed
/// "claude 2.1.22" in one column and "claude 2.1.228" in the next, the two
/// columns differing only by their right margin. A quarter pixel swamps that
/// rounding and is invisible on screen.
const GLYPH_FIT_EPSILON: f32 = 0.25;

/// First char of a single-line input to draw when only `visible` chars fit:
/// zero until the caret would leave the box, then just enough to keep it on
/// the last visible column.
fn input_scroll(cursor: usize, visible: usize) -> usize {
    (cursor + 1).saturating_sub(visible.max(1))
}

/// Where the session name starts in the attention banner: flush right, unless
/// that would run into the tier label, in which case it starts two cells after
/// the label and gets clipped at the right edge instead.
fn banner_title_x(title_chars: usize, label_end: f32, right_edge: f32, cell_w: f32) -> f32 {
    let flush_right = right_edge - title_chars as f32 * cell_w;
    flush_right.max(label_end + cell_w * 2.0)
}

/// Whether a glyph cell starting at `x` still fits before `max_x`.
fn glyph_fits(x: f32, cell_w: f32, max_x: f32) -> bool {
    x + cell_w <= max_x + GLYPH_FIT_EPSILON
}

/// Horizontal split of a pane switcher row between its title (left) and the
/// binary running in the pane (right).
struct SwitcherRowSplit {
    /// Where the title stops being drawn, so it never runs into the binary.
    title_limit: f32,
    /// Where the binary starts, or `None` when the column is too narrow to
    /// carry both — a half-written program name is worse than none.
    process_x: Option<f32>,
}

/// Width, in cells, of what a pane switcher row parks flush right: the binary,
/// then two cells, then the todo count, ` · `, the idea count. Each part is
/// optional and the gaps only appear between parts that exist.
fn switcher_row_tail_chars(
    process: Option<&str>,
    raf_todo: Option<&str>,
    raf_idea: Option<&str>,
) -> usize {
    let n = |s: Option<&str>| s.map_or(0, |s| s.chars().count());
    let raf = match (raf_todo, raf_idea) {
        (Some(_), Some(_)) => n(raf_todo) + RAF_SEPARATOR.chars().count() + n(raf_idea),
        _ => n(raf_todo) + n(raf_idea),
    };
    let gap = if process.is_some() && raf > 0 { 2 } else { 0 };
    n(process) + gap + raf
}

/// Lay out one pane switcher row. Pure geometry, so it can be checked without
/// a GPU: the title keeps the left, the binary is parked flush right with a
/// two-cell gap, and the binary is dropped entirely when that would leave the
/// title less than a third of the row.
fn switcher_row_split(
    left_margin: f32,
    right_margin: f32,
    process_chars: usize,
    cell_w: f32,
) -> SwitcherRowSplit {
    if process_chars == 0 {
        return SwitcherRowSplit {
            title_limit: right_margin,
            process_x: None,
        };
    }
    let process_x = right_margin - process_chars as f32 * cell_w;
    let title_limit = process_x - cell_w * 2.0;
    let min_title = left_margin + (right_margin - left_margin) / 3.0;
    if title_limit < min_title {
        SwitcherRowSplit {
            title_limit: right_margin,
            process_x: None,
        }
    } else {
        SwitcherRowSplit {
            title_limit,
            process_x: Some(process_x),
        }
    }
}

/// Per-pane data passed from window to renderer.
pub struct PaneRenderData {
    pub terminal: Arc<RwLock<TerminalState>>,
    pub viewport: PaneViewport,
    pub shell_ready: bool,
    pub is_focused: bool,
    /// Draw the focus outline around this pane. Only the caller knows whether
    /// the outline tells anything: Kova.app sets it on the focused pane of a
    /// tab with more than one visible pane, the library never (the host app
    /// shows which of its views has the keyboard).
    pub outlined: bool,
    /// Fade this pane while it is not focused (`splits.dim_opacity`). The
    /// caller decides, like `outlined`: Kova.app fades every pane that is not
    /// its tab's focused one, a host app never (it manages its own panes, and
    /// a pane alone on its layer has nothing to be told apart from).
    pub dim_unfocused: bool,
    pub pane_id: PaneId,
    pub custom_title: Option<String>,
    pub has_completion: bool,
    pub has_bell: bool,
    pub minimized: bool,
    pub input_chars: Arc<std::sync::atomic::AtomicU64>,
    /// Foreground binary running in the pane (`claude`, `nvim`…), shown in the
    /// status bar. `None` at a bare shell prompt.
    pub fg_process: Option<String>,
    /// This pane's conversation is bookmarked: its status bar is painted blue,
    /// so a saved conversation is recognisable without opening the switcher.
    pub bookmarked: bool,
}

/// Sub-region of the drawable where a pane is rendered (in pixels).
#[derive(Clone, Copy)]
pub struct PaneViewport {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

impl PaneViewport {
    /// Entirely outside the screen's horizontal span `[0, screen_w)` (a tab
    /// wider than the screen, scrolled). Such a pane is neither drawn nor
    /// resized until it comes back.
    pub fn off_screen(&self, screen_w: f32) -> bool {
        self.x + self.width <= 0.0 || self.x >= screen_w
    }
}

const INITIAL_VERTEX_BYTES: usize = 8 * 1024 * 1024; // 8MB

/// Kova's window renderer: one per window, the host of that window's panes.
/// Owns the shared `RenderContext`, one `PaneRenderer` per pane, the drawable
/// side (vertex buffers, one `nextDrawable` / `presentDrawable` per frame), the
/// clock (cursor blink) and everything that is window chrome: tab bar, global
/// status bar, separators, banners, flashes, overlays, tooltips.
pub struct Renderer {
    /// Command queue, pipeline and glyph atlas, shared by every pane.
    ctx: RenderContext,
    /// How panes are drawn (colors, dim, focus outline, status bar).
    style: PaneStyle,
    // Pre-allocated buffers
    viewport_buf: Retained<ProtocolObject<dyn MTLBuffer>>,
    vertex_bufs: [Retained<ProtocolObject<dyn MTLBuffer>>; 2],
    /// Capacity of each vertex buffer; per-Renderer because each window has
    /// its own buffers (a process-global value would skip reallocation when
    /// another window already grew, overflowing this window's buffers).
    vertex_buf_capacity: usize,
    vertex_buf_idx: usize,
    last_viewport: [f32; 2],
    /// Phase of the focused pane's cursor blink, in real time.
    blink: CursorBlink,
    cursor_blink_frames: u32,
    /// Rate of the run-loop timer that calls `render_panes` (ticks per
    /// second). `cursor_blink_frames` counts in these ticks. Taken from the
    /// config this window was built with and never reloaded: the timer is
    /// started once, at launch, and keeps its rate.
    frame_rate: u32,
    global_bar_bg: [f32; 3],
    global_bar_time_color: [f32; 3],
    global_bar_scroll_color: [f32; 3],
    last_minute: u32,
    cached_time_str: String,
    last_rss_epoch: u32,
    cached_rss_str: String,
    cached_proc_count: u32,
    cached_proc_str: String,
    cached_io_str: String,
    /// Cached memory report lines for overlay (set by window on Cmd+Shift+I).
    cached_mem_report: Vec<String>,
    tab_bar_bg: [f32; 3],
    tab_bar_fg: [f32; 3],
    tab_bar_active_bg: [f32; 3],
    switcher_selected_bg: [f32; 3],
    /// Hovered URL: per-row segments [(visible_row, col_start, col_end)]
    pub hovered_url: Option<Vec<(usize, u16, u16)>>,
    /// Hovered URL text (for status bar display)
    pub hovered_url_text: Option<String>,
    /// Resize mode feedback text, displayed on the left of the global status bar.
    pub resize_feedback_text: Option<String>,
    /// Banner to paint across the focused pane's status bar (text, background):
    /// which attention tier the last Cmd+J landed in. `None` most of the time.
    pub pane_banner: Option<(String, [f32; 3])>,
    /// The active tab's subject strip, set by the window every frame.
    pub subject_strip: SubjectStrip,
    /// Boundary flash: (edge_x, top_y, bottom_y, alpha, is_right_edge). Set by window when navigation hits tab edge.
    pub boundary_flash: Option<(f32, f32, f32, f32, bool)>,
    /// Pane flash for search-palette jumps: (x, y, width, height, alpha).
    /// A pulsing rectangle drawn around a pane's viewport for ~half a second.
    pub pane_flash: Option<(f32, f32, f32, f32, f32)>,
    /// Big label drawn inside the flashing pane: (directory name, path above
    /// it). Set on Cmd+J jumps so a landing far from the eye names itself.
    pub pane_flash_label: Option<(String, String, String)>,
    /// Loading progress: (ready_panes, total_panes). None when all loaded.
    pub loading_progress: Option<(u32, u32)>,
    /// Pane ID of the hovered URL (to show URL only in that pane's status bar)
    pub hovered_url_pane_id: Option<PaneId>,
    /// Cached help hint text for status bar (avoid per-frame allocation).
    cached_help_hint: String,
    /// Cached permanent shortcuts reminder for the global status bar
    /// (help + overlay combos), built once from the key config.
    cached_shortcuts_hint: String,
    /// Cached help overlay rows, pre-split into two display columns.
    cached_help_columns: [Vec<HelpRow>; 2],
    /// Hoverable zones in status bars (rebuilt each frame).
    tooltip_zones: Vec<TooltipZone>,
    /// Clickable rect of the minimized-panes counter in the global status bar
    /// (x, y, w, h); None when the counter is hidden. Click opens the switcher.
    pub minimized_counter_zone: Option<(f32, f32, f32, f32)>,
    /// One renderer per drawn pane, holding its last built vertices. While a
    /// pane defers rendering inside a synchronized-output burst (DEC 2026), its
    /// previous frame is drawn from this cache instead of rebuilding from the
    /// mid-update grid — rebuilding would present exactly the torn frames mode
    /// 2026 exists to hide. A pane absent from the map has no cached build.
    pane_renderers: std::collections::HashMap<u32, PaneRenderer>,
    /// Active tooltip to render. Set by window on mouse hover.
    pub active_tooltip: Option<ActiveTooltip>,
    /// Tooltip being animated (kept for fade-out after active_tooltip becomes None).
    tooltip_visible: Option<ActiveTooltip>,
    /// Animation progress: 0 = hidden, TOOLTIP_ANIM_FRAMES = fully visible.
    tooltip_anim: u8,
}

/// Runs of consecutive cells sharing a background other than `default_bg`,
/// as (first column, one past the last column, colour).
fn bg_runs(line: &[Cell], default_bg: [u8; 3]) -> Vec<(usize, usize, [u8; 3])> {
    let mut runs: Vec<(usize, usize, [u8; 3])> = Vec::new();
    for (col, cell) in line.iter().enumerate() {
        if cell.bg == default_bg {
            continue;
        }
        match runs.last_mut() {
            Some((_, end, bg)) if *end == col && *bg == cell.bg => *end = col + 1,
            _ => runs.push((col, col + 1, cell.bg)),
        }
    }
    runs
}

/// Width, in cells, of the switcher's hint line: each pair is drawn as
/// "<key> <what>" with two spaces before the next one, and the trailing gap
/// after the last pair is not part of the line. Kept apart from the drawing so
/// the centring can be checked without a GPU.
fn hint_line_width(hints: &[(&str, &str)]) -> usize {
    let total: usize = hints
        .iter()
        .map(|(k, w)| k.chars().count() + 1 + w.chars().count() + 2)
        .sum();
    total.saturating_sub(2)
}

impl Renderer {
    pub fn new(
        device: &ProtocolObject<dyn MTLDevice>,
        layer: &CAMetalLayer,
        _terminal: Arc<RwLock<TerminalState>>,
        scale: f64,
        config: &Config,
    ) -> Self {
        let ctx = RenderContext::new(device, layer.pixelFormat(), scale, config);

        let make_vertex_buf = || {
            device
                .newBufferWithLength_options(
                    INITIAL_VERTEX_BYTES,
                    MTLResourceOptions(
                        MTLResourceOptions::CPUCacheModeDefaultCache.0
                            | MTLResourceOptions::StorageModeShared.0,
                    ),
                )
                .expect("failed to allocate vertex buffer")
        };

        let viewport = [0.0f32; 2];
        let viewport_buf = unsafe {
            device.newBufferWithBytes_length_options(
                NonNull::new(viewport.as_ptr() as *mut _).unwrap(),
                std::mem::size_of_val(&viewport),
                MTLResourceOptions::CPUCacheModeDefaultCache,
            )
        }
        .unwrap();

        Renderer {
            ctx,
            style: PaneStyle::from_config(config),
            viewport_buf,
            vertex_bufs: [make_vertex_buf(), make_vertex_buf()],
            vertex_buf_capacity: INITIAL_VERTEX_BYTES,
            vertex_buf_idx: 0,
            last_viewport: [0.0; 2],
            blink: CursorBlink::new(),
            cursor_blink_frames: config.terminal.cursor_blink_frames,
            frame_rate: config.terminal.fps,
            global_bar_bg: config.global_status_bar.bg_color,
            global_bar_time_color: config.global_status_bar.time_color,
            global_bar_scroll_color: config.global_status_bar.scroll_indicator_color,
            last_minute: u32::MAX,
            cached_time_str: String::new(),
            last_rss_epoch: u32::MAX,
            cached_rss_str: String::new(),
            cached_proc_count: 0,
            cached_proc_str: String::from("▶0"),
            cached_io_str: String::new(),
            cached_mem_report: Vec::new(),
            tab_bar_bg: config.tab_bar.bg_color,
            tab_bar_fg: config.tab_bar.fg_color,
            tab_bar_active_bg: config.tab_bar.active_bg,
            switcher_selected_bg: config.switcher.selected_bg,
            hovered_url: None,
            hovered_url_text: None,
            resize_feedback_text: None,
            pane_banner: None,
            subject_strip: SubjectStrip::default(),
            boundary_flash: None,
            pane_flash: None,
            pane_flash_label: None,
            loading_progress: None,
            hovered_url_pane_id: None,
            cached_help_hint: String::new(),
            cached_shortcuts_hint: String::new(),
            cached_help_columns: [Vec::new(), Vec::new()],
            tooltip_zones: Vec::new(),
            minimized_counter_zone: None,
            pane_renderers: std::collections::HashMap::new(),
            active_tooltip: None,
            tooltip_visible: None,
            tooltip_anim: 0,
        }
    }

    /// Re-read the appearance half of a freshly loaded config into this
    /// renderer. Only what can change between two frames: colors, the fade of
    /// unfocused panes, the focus outline. Font, keybindings and terminal
    /// geometry are not touched — they are read once at startup and need a
    /// restart (the atlas and every PTY are sized from them).
    pub fn apply_config(&mut self, config: &Config) {
        self.style = PaneStyle::from_config(config);
        self.cursor_blink_frames = config.terminal.cursor_blink_frames;
        self.global_bar_bg = config.global_status_bar.bg_color;
        self.global_bar_time_color = config.global_status_bar.time_color;
        self.global_bar_scroll_color = config.global_status_bar.scroll_indicator_color;
        self.tab_bar_bg = config.tab_bar.bg_color;
        self.tab_bar_fg = config.tab_bar.fg_color;
        self.tab_bar_active_bg = config.tab_bar.active_bg;
        self.switcher_selected_bg = config.switcher.selected_bg;
        // Every pane's vertices carry the old colors; drop the cache so the
        // next frame rebuilds them.
        self.pane_renderers.clear();
    }

    /// The hovered URL as `pane` sees it: its segments and text when the URL
    /// is in that pane, nothing otherwise.
    fn pane_hover(&self, pane_id: PaneId) -> (Option<&[(usize, u16, u16)]>, Option<&str>) {
        if self.hovered_url_pane_id == Some(pane_id) {
            (
                self.hovered_url.as_deref(),
                self.hovered_url_text.as_deref(),
            )
        } else {
            (None, None)
        }
    }

    /// Everything `pane`'s build reads besides its grid (see `PaneDrawKey`).
    fn pane_draw_key(&self, pane: &PaneRenderData, blink_on: bool) -> PaneDrawKey {
        let (hovered_url, hovered_url_text) = self.pane_hover(pane.pane_id);
        PaneRenderer::draw_key(
            pane,
            hovered_url,
            hovered_url_text,
            blink_on,
            self.ctx.atlas.generation,
        )
    }

    /// Build one pane outside of a frame, the way `rebuild_pane_draws` does,
    /// with the renderer's hover as is. Lets the vertex snapshots pin the
    /// output of a pane build without a drawable.
    #[cfg(test)]
    fn build_vertices(
        &mut self,
        term: &TerminalState,
        vp: &PaneViewport,
        blink_on: bool,
        is_focused: bool,
        outlined: bool,
        custom_title: Option<&str>,
        attention: PaneAttention,
        pane_input_chars: u64,
        pane_id: PaneId,
        fg_process: Option<&str>,
        bookmarked: bool,
    ) -> Vec<Vertex> {
        PaneBuild {
            ctx: &mut self.ctx,
            style: &self.style,
            dim_unfocused: true,
            hovered_url: self.hovered_url.as_deref(),
            hovered_url_text: self.hovered_url_text.as_deref(),
            zones: &mut self.tooltip_zones,
        }
        .build_vertices(
            term,
            vp,
            blink_on,
            is_focused,
            outlined,
            custom_title,
            attention,
            pane_input_chars,
            pane_id,
            fg_process,
            bookmarked,
        )
    }

    /// Build per-pane vertex lists into the cache and return the draw list
    /// (pane id + scissor). Retried (max 3 passes) when rasterizing new
    /// glyphs changes the atlas generation mid-loop: UVs computed before the
    /// change point into the wrong texture location for this frame.
    fn rebuild_pane_draws(
        &mut self,
        panes: &[PaneRenderData],
        viewport_w: f32,
        viewport_h: f32,
        has_filter: bool,
        blink_on: bool,
        dirty_panes: &[u32],
        sync_panes: &[u32],
    ) -> Vec<(u32, MTLScissorRect)> {
        let mut pane_draws: Vec<(u32, MTLScissorRect)> = Vec::new();
        for _pass in 0..3 {
            let atlas_gen = self.ctx.atlas.generation;
            pane_draws.clear();
            self.tooltip_zones.clear();
            for pane in panes {
                let vp = &pane.viewport;
                // Minimized panes take no layout space and are never drawn;
                // they surface via the status-bar counter and the pane switcher.
                if pane.minimized {
                    self.pane_renderers.remove(&pane.pane_id);
                    continue;
                }
                // Skip panes entirely off-screen (hidden by horizontal scroll).
                // Their dirty flag was consumed this frame without a rebuild:
                // drop the cache entry so they are rebuilt when they come back.
                if vp.off_screen(viewport_w) {
                    self.pane_renderers.remove(&pane.pane_id);
                    continue;
                }
                if pane.is_focused && has_filter {
                    // Filter overlay covers the focused pane; same as above.
                    self.pane_renderers.remove(&pane.pane_id);
                    continue;
                }
                let in_sync = sync_panes.contains(&pane.pane_id);
                let key = self.pane_draw_key(pane, blink_on);
                // Scope hovered URL to the pane that owns it
                let is_hover_pane = self.hovered_url_pane_id == Some(pane.pane_id);
                let hovered_url = if is_hover_pane {
                    self.hovered_url.as_deref()
                } else {
                    None
                };
                let hovered_url_text = if is_hover_pane {
                    self.hovered_url_text.as_deref()
                } else {
                    None
                };
                self.pane_renderers
                    .entry(pane.pane_id)
                    .or_default()
                    .prepare(
                        &mut self.ctx,
                        &self.style,
                        pane,
                        hovered_url,
                        hovered_url_text,
                        key,
                        in_sync,
                        dirty_panes.contains(&pane.pane_id),
                        &mut self.tooltip_zones,
                    );
                // Compute scissor rect clamped to drawable bounds. Ceil the
                // extent: truncation shaves up to 1px off the right/bottom
                // painted edge at fractional split boundaries.
                let sx = (vp.x.max(0.0)) as usize;
                let sy = (vp.y.max(0.0)) as usize;
                let sw = ((vp.width).min(viewport_w - sx as f32).max(0.0)).ceil() as usize;
                let sh = ((vp.height).min(viewport_h - sy as f32).max(0.0)).ceil() as usize;
                let has_verts = self
                    .pane_renderers
                    .get(&pane.pane_id)
                    .is_some_and(|p| !p.vertices().is_empty());
                if has_verts && sw > 0 && sh > 0 {
                    pane_draws.push((
                        pane.pane_id,
                        MTLScissorRect {
                            x: sx,
                            y: sy,
                            width: sw,
                            height: sh,
                        },
                    ));
                }
            }
            if self.ctx.atlas.generation == atlas_gen {
                break;
            }
            // Atlas grew or was repacked while building: every cached vertex
            // list has stale UVs — drop them all and rebuild (the needed
            // glyphs are rasterized now, so the next pass is stable).
            self.pane_renderers.clear();
        }
        // Drop cache entries of panes that no longer exist
        self.pane_renderers
            .retain(|id, _| panes.iter().any(|p| p.pane_id == *id));
        pane_draws
    }

    /// Render multiple panes. Each entry: (terminal, viewport, shell_ready, is_focused, pane_id, custom_title, has_completion, has_bell, minimized).
    /// `separators` are line segments (x1, y1, x2, y2) drawn between splits.
    pub fn render_panes(
        &mut self,
        layer: &CAMetalLayer,
        panes: &[PaneRenderData],
        separators: &[(f32, f32, f32, f32)],
        tab_titles: &[(String, bool, Option<usize>, bool, bool, bool, bool)],
        filter: Option<&FilterRenderData>,
        tab_bar_left_inset: f32,
        hidden_left: usize,
        hidden_right: usize,
        focused_column: usize,
        total_columns: usize,
        active_tab: usize,
        total_tabs: usize,
        active_tab_name: &str,
        working_agents: usize,
        unread_panes: usize,
        sleeping_panes: usize,
        minimized_counts: (usize, usize),
        show_help: bool,
        show_mem_report: bool,
        recent_projects: Option<&RecentProjectsRenderData<'_>>,
        send_to_window: Option<&SendToWindowRenderData<'_>>,
        search_palette: Option<&SearchPaletteRenderData<'_>>,
        pane_switcher: Option<&PaneSwitcherRenderData<'_>>,
        help_hint_remaining: u32,
        keys_config: Option<&KeysConfig>,
    ) {
        // Reset blink on cursor movement of focused pane (see `CursorBlink`).
        let focused_epoch = panes.iter().find(|p| p.is_focused).map(|p| {
            p.terminal
                .read()
                .cursor_move_epoch
                .load(std::sync::atomic::Ordering::Relaxed)
        });
        let (blink_on, blink_changed) = self.blink.sample(
            std::time::Instant::now(),
            focused_epoch,
            self.cursor_blink_frames,
            self.frame_rate,
        );

        // Shared timestamp for time + RSS checks
        let now_secs = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        // Check if minute changed for global status bar time
        let minute_changed = {
            let current_minute = (now_secs / 60) as u32;
            if current_minute != self.last_minute {
                self.last_minute = current_minute;
                let t = now_secs as libc::time_t;
                let mut tm: libc::tm = unsafe { std::mem::zeroed() };
                unsafe { libc::localtime_r(&t, &mut tm) };
                self.cached_time_str = format!("{:02}:{:02}", tm.tm_hour, tm.tm_min);
                true
            } else {
                false
            }
        };

        // Update RSS + process count every 2 seconds
        let rss_changed = {
            let epoch_2s = (now_secs / 2) as u32;
            if epoch_2s != self.last_rss_epoch {
                self.last_rss_epoch = epoch_2s;
                let rss_mb = crate::get_rss_mb();
                self.cached_rss_str = if rss_mb >= 0.0 {
                    format!("{:.1}M", rss_mb)
                } else {
                    String::new()
                };
                self.cached_proc_count = crate::terminal::pty::foreground_process_count();
                self.cached_proc_str = format!("▶{}", self.cached_proc_count);
                true
            } else {
                false
            }
        };

        // Check if any pane is dirty. Only peeked here: the flags are
        // consumed once a drawable is in hand, so a frame dropped for lack of
        // one cannot lose them (a clean pane is now drawn from its cache).
        // Sync-deferred panes keep their dirty flag and are drawn from the
        // vertex cache below (their previous, coherent frame).
        let mut any_dirty = false;
        let mut any_not_ready = false;
        let mut any_sync_deferred = false;
        for pane in panes {
            if !pane.shell_ready {
                any_not_ready = true;
            }
            if PaneRenderer::in_sync_window(pane) {
                any_sync_deferred = true;
                continue; // Don't consume dirty flag — pane will render later
            }
            if pane
                .terminal
                .read()
                .dirty
                .load(std::sync::atomic::Ordering::Relaxed)
            {
                any_dirty = true;
            }
        }
        // A clean pane whose status-bar inputs moved (input counter, fg
        // process, bookmark…) must be redrawn too: those never mark it dirty.
        let any_key_changed = panes.iter().any(|p| {
            !p.minimized
                && self
                    .pane_renderers
                    .get(&p.pane_id)
                    .is_some_and(|r| r.key_changed(&self.pane_draw_key(p, blink_on)))
        });
        // If only sync-deferred panes were dirty, still need to render the others
        let all_ready = !any_not_ready;
        let has_filter = filter.is_some();
        let has_recent_projects = recent_projects.is_some();
        let has_search_palette = search_palette.is_some();
        let tooltip_animating = self.tooltip_anim > 0 && self.tooltip_anim < TOOLTIP_ANIM_FRAMES;
        let has_loading = self.loading_progress.is_some();
        let has_pane_flash = self.pane_flash.is_some();
        let has_status_text = self.resize_feedback_text.is_some();
        if all_ready
            && !any_dirty
            && !any_key_changed
            && !any_sync_deferred
            && !blink_changed
            && !minute_changed
            && !rss_changed
            && !has_filter
            && !show_help
            && !show_mem_report
            && !has_recent_projects
            && !has_search_palette
            && !has_pane_flash
            && !has_status_text
            && help_hint_remaining == 0
            && !tooltip_animating
            && !has_loading
        {
            return;
        }

        let drawable = match layer.nextDrawable() {
            Some(d) => d,
            None => return,
        };

        let drawable_size = layer.drawableSize();
        let viewport_w = drawable_size.width as f32;
        let viewport_h = drawable_size.height as f32;

        // Sync state is read once per frame: evaluated twice, a burst ending
        // in between would leave a pane neither consumed nor sync-deferred.
        let sync_panes: Vec<u32> = panes
            .iter()
            .filter(|p| PaneRenderer::in_sync_window(p))
            .map(|p| p.pane_id)
            .collect();
        // Consume the dirty flags now that this frame will be drawn; the
        // panes that had one are rebuilt, the others may reuse their cache.
        let dirty_panes: Vec<u32> = panes
            .iter()
            .filter(|p| !sync_panes.contains(&p.pane_id))
            .filter(|p| {
                p.terminal
                    .read()
                    .dirty
                    .swap(false, std::sync::atomic::Ordering::Relaxed)
            })
            .map(|p| p.pane_id)
            .collect();

        // Build vertices for each pane with its own scissor rect for clipping.
        // pane_draws references the vertex cache by pane id (vertices live in
        // self.pane_renderers).
        let mut pane_draws = self.rebuild_pane_draws(
            panes,
            viewport_w,
            viewport_h,
            filter.is_some(),
            blink_on,
            &dirty_panes,
            &sync_panes,
        );
        let atlas_gen_after_panes = self.ctx.atlas.generation;
        let pane_zone_count = self.tooltip_zones.len();
        let mut overlay_vertices = Vec::new();

        // Build overlay vertices (separators, tab bar, status bar, filter, help)
        // These are drawn with a global scissor rect (no per-pane clipping needed)

        // Draw split separators (1px lines)
        if !separators.is_empty() {
            let no_tex = [0.0_f32, 0.0];
            let white = [1.0_f32, 1.0, 1.0, 0.0]; // unused (bg_color path)
            let sep_bg = [1.0_f32, 1.0, 1.0, 0.15]; // light grey via bg_color
            let thickness = 1.0_f32;
            for &(x1, y1, x2, y2) in separators {
                if (x1 - x2).abs() < 0.1 {
                    // Vertical line
                    let lx = x1 - thickness * 0.5;
                    let rx = x1 + thickness * 0.5;
                    overlay_vertices.push(Vertex {
                        position: [lx, y1],
                        tex_coords: no_tex,
                        color: white,
                        bg_color: sep_bg,
                    });
                    overlay_vertices.push(Vertex {
                        position: [rx, y1],
                        tex_coords: no_tex,
                        color: white,
                        bg_color: sep_bg,
                    });
                    overlay_vertices.push(Vertex {
                        position: [lx, y2],
                        tex_coords: no_tex,
                        color: white,
                        bg_color: sep_bg,
                    });
                    overlay_vertices.push(Vertex {
                        position: [rx, y1],
                        tex_coords: no_tex,
                        color: white,
                        bg_color: sep_bg,
                    });
                    overlay_vertices.push(Vertex {
                        position: [rx, y2],
                        tex_coords: no_tex,
                        color: white,
                        bg_color: sep_bg,
                    });
                    overlay_vertices.push(Vertex {
                        position: [lx, y2],
                        tex_coords: no_tex,
                        color: white,
                        bg_color: sep_bg,
                    });
                } else {
                    // Horizontal line
                    let ty = y1 - thickness * 0.5;
                    let by = y1 + thickness * 0.5;
                    overlay_vertices.push(Vertex {
                        position: [x1, ty],
                        tex_coords: no_tex,
                        color: white,
                        bg_color: sep_bg,
                    });
                    overlay_vertices.push(Vertex {
                        position: [x2, ty],
                        tex_coords: no_tex,
                        color: white,
                        bg_color: sep_bg,
                    });
                    overlay_vertices.push(Vertex {
                        position: [x1, by],
                        tex_coords: no_tex,
                        color: white,
                        bg_color: sep_bg,
                    });
                    overlay_vertices.push(Vertex {
                        position: [x2, ty],
                        tex_coords: no_tex,
                        color: white,
                        bg_color: sep_bg,
                    });
                    overlay_vertices.push(Vertex {
                        position: [x2, by],
                        tex_coords: no_tex,
                        color: white,
                        bg_color: sep_bg,
                    });
                    overlay_vertices.push(Vertex {
                        position: [x1, by],
                        tex_coords: no_tex,
                        color: white,
                        bg_color: sep_bg,
                    });
                }
            }
        }

        // Draw boundary flash (red line at tab edge)
        if let Some((edge_x, top_y, bottom_y, alpha, is_right)) = self.boundary_flash {
            let no_tex = [0.0_f32, 0.0];
            let white = [1.0_f32, 1.0, 1.0, 0.0];
            let flash_color = [0.9_f32, 0.2, 0.2, alpha];
            let thickness = 5.0_f32;
            // Draw inside the pane, flush against the edge
            let (lx, rx) = if is_right {
                (edge_x - thickness, edge_x)
            } else {
                (edge_x, edge_x + thickness)
            };
            overlay_vertices.push(Vertex {
                position: [lx, top_y],
                tex_coords: no_tex,
                color: white,
                bg_color: flash_color,
            });
            overlay_vertices.push(Vertex {
                position: [rx, top_y],
                tex_coords: no_tex,
                color: white,
                bg_color: flash_color,
            });
            overlay_vertices.push(Vertex {
                position: [lx, bottom_y],
                tex_coords: no_tex,
                color: white,
                bg_color: flash_color,
            });
            overlay_vertices.push(Vertex {
                position: [rx, top_y],
                tex_coords: no_tex,
                color: white,
                bg_color: flash_color,
            });
            overlay_vertices.push(Vertex {
                position: [rx, bottom_y],
                tex_coords: no_tex,
                color: white,
                bg_color: flash_color,
            });
            overlay_vertices.push(Vertex {
                position: [lx, bottom_y],
                tex_coords: no_tex,
                color: white,
                bg_color: flash_color,
            });
        }

        // Draw tab bar
        if tab_titles.len() > 0 {
            self.build_tab_bar_vertices(
                &mut overlay_vertices,
                viewport_w,
                tab_titles,
                tab_bar_left_inset,
            );
        }
        self.build_subject_strip_vertices(&mut overlay_vertices, viewport_w);

        // Update I/O char counters (global totals, persist across pane closures)
        if rss_changed {
            let total_in =
                crate::terminal::pty::GLOBAL_INPUT_CHARS.load(std::sync::atomic::Ordering::Relaxed);
            let total_out = crate::terminal::pty::GLOBAL_PRINTABLE_CHARS
                .load(std::sync::atomic::Ordering::Relaxed);
            self.cached_io_str =
                format!("↑{} ↓{}", format_count(total_in), format_count(total_out));
        }

        // Draw global status bar
        self.build_global_status_bar_vertices(
            &mut overlay_vertices,
            viewport_w,
            viewport_h,
            hidden_left,
            hidden_right,
            focused_column,
            total_columns,
            active_tab,
            total_tabs,
            active_tab_name,
            working_agents,
            unread_panes,
            sleeping_panes,
            minimized_counts.0,
            minimized_counts.1,
            help_hint_remaining,
            keys_config,
        );

        // Attention banner over the focused pane's status bar. Drawn in the
        // overlay pass rather than inside the pane's own vertices: those are
        // cached per pane and would keep a stale banner alive across frames.
        if let (Some((text, color)), Some(focused)) = (
            self.pane_banner.clone(),
            panes.iter().find(|p| p.is_focused),
        ) {
            // The banner hides the bar's right-hand title, which is the only
            // place an unrenamed session shows its name: carry it over.
            let title = focused
                .custom_title
                .clone()
                .or_else(|| focused.terminal.read().title.clone());
            self.build_pane_banner_vertices(
                &mut overlay_vertices,
                &focused.viewport,
                &text,
                title.as_deref(),
                color,
            );
        }

        // Draw filter overlay on focused pane
        if let Some(filter_data) = filter {
            if let Some(focused_pane) = panes.iter().find(|p| p.is_focused) {
                self.build_filter_overlay_vertices(
                    &mut overlay_vertices,
                    &focused_pane.viewport,
                    filter_data,
                );
            }
        }

        // Draw help overlay (on top of everything)
        if show_help {
            if let Some(keys_config) = keys_config {
                self.build_help_overlay_vertices(
                    &mut overlay_vertices,
                    viewport_w,
                    viewport_h,
                    keys_config,
                );
            }
        }

        // Draw memory report overlay
        if show_mem_report {
            self.build_mem_report_overlay_vertices(&mut overlay_vertices, viewport_w, viewport_h);
        }

        // Draw recent projects overlay
        if let Some(rp) = recent_projects {
            self.build_recent_projects_overlay_vertices(
                &mut overlay_vertices,
                viewport_w,
                viewport_h,
                rp,
            );
        }

        // Draw send-to-window overlay
        if let Some(stw) = send_to_window {
            self.build_send_to_window_overlay_vertices(
                &mut overlay_vertices,
                viewport_w,
                viewport_h,
                stw,
            );
        }

        // Draw tab/pane switcher overlay
        if let Some(ps) = pane_switcher {
            self.build_pane_switcher_overlay_vertices(
                &mut overlay_vertices,
                viewport_w,
                viewport_h,
                ps,
            );
        }

        // Draw pane flash border (search-palette jump highlight) — drawn before
        // the search palette so the palette overlay would cover it if both were
        // somehow active simultaneously (palette closes before flash starts).
        if let Some((x, y, w, h, alpha)) = self.pane_flash {
            let no_tex = [0.0_f32, 0.0];
            let white = [1.0_f32, 1.0, 1.0, 0.0];
            let color = [1.0_f32, 0.85, 0.3, alpha];
            let thickness = 4.0_f32;
            let mut push_quad = |x: f32, y: f32, w: f32, h: f32| {
                overlay_vertices.push(Vertex {
                    position: [x, y],
                    tex_coords: no_tex,
                    color: white,
                    bg_color: color,
                });
                overlay_vertices.push(Vertex {
                    position: [x + w, y],
                    tex_coords: no_tex,
                    color: white,
                    bg_color: color,
                });
                overlay_vertices.push(Vertex {
                    position: [x, y + h],
                    tex_coords: no_tex,
                    color: white,
                    bg_color: color,
                });
                overlay_vertices.push(Vertex {
                    position: [x + w, y],
                    tex_coords: no_tex,
                    color: white,
                    bg_color: color,
                });
                overlay_vertices.push(Vertex {
                    position: [x + w, y + h],
                    tex_coords: no_tex,
                    color: white,
                    bg_color: color,
                });
                overlay_vertices.push(Vertex {
                    position: [x, y + h],
                    tex_coords: no_tex,
                    color: white,
                    bg_color: color,
                });
            };
            // Top, bottom, left, right edges as four thin quads.
            push_quad(x, y, w, thickness);
            push_quad(x, y + h - thickness, w, thickness);
            push_quad(x, y, thickness, h);
            push_quad(x + w - thickness, y, thickness, h);

            // The directory name in big over the pane, when the jump asked for
            // it (Cmd+J): the border alone does not say where the eye landed.
            if let Some((name, parent, session)) = self.pane_flash_label.clone() {
                self.build_flash_label_vertices(
                    &mut overlay_vertices,
                    (x, y, w, h),
                    alpha,
                    &name,
                    &parent,
                    &session,
                );
            }
        }

        // Draw search palette overlay (on top of everything except tooltip)
        if let Some(sp) = search_palette {
            self.build_search_palette_overlay_vertices(
                &mut overlay_vertices,
                viewport_w,
                viewport_h,
                sp,
            );
        }

        // Tooltip animation: update visible state and animation counter
        if self.active_tooltip.is_some() {
            self.tooltip_visible = self.active_tooltip.clone();
            self.tooltip_anim = self.tooltip_anim.saturating_add(1).min(TOOLTIP_ANIM_FRAMES);
        } else {
            self.tooltip_anim = self.tooltip_anim.saturating_sub(1);
            if self.tooltip_anim == 0 {
                self.tooltip_visible = None;
            }
        }
        self.build_tooltip_vertices(&mut overlay_vertices, viewport_w);

        // Lay every pane's vertices + the overlay end to end in the GPU
        // buffer, tracking draw ranges. Copied straight from the cache into the
        // buffer below: no intermediate Vec holding a second copy of the frame.
        // (start, count, scissor, source pane — None for the overlay)
        let mut draw_calls: Vec<(usize, usize, MTLScissorRect, Option<u32>)> = Vec::new();
        let global_scissor = MTLScissorRect {
            x: 0,
            y: 0,
            width: viewport_w as usize,
            height: viewport_h as usize,
        };

        // Overlay text (tab bar, global bar, palettes) may have rasterized new
        // glyphs and grown/repacked the atlas AFTER the pane build — pane UVs
        // would point into the wrong texture. Rebuild the panes once; the
        // overlay vertices themselves are at worst stale for this one frame.
        if self.ctx.atlas.generation != atlas_gen_after_panes {
            let overlay_zones: Vec<TooltipZone> = self.tooltip_zones.split_off(pane_zone_count);
            self.pane_renderers.clear();
            pane_draws = self.rebuild_pane_draws(
                panes,
                viewport_w,
                viewport_h,
                filter.is_some(),
                blink_on,
                &dirty_panes,
                &sync_panes,
            );
            self.tooltip_zones.extend(overlay_zones);
        }

        let mut total_vertices = 0usize;
        for (pane_id, scissor) in &pane_draws {
            let Some(pane) = self.pane_renderers.get(pane_id) else {
                continue;
            };
            let verts = pane.vertices();
            draw_calls.push((total_vertices, verts.len(), *scissor, Some(*pane_id)));
            total_vertices += verts.len();
        }
        if !overlay_vertices.is_empty() {
            draw_calls.push((total_vertices, overlay_vertices.len(), global_scissor, None));
            total_vertices += overlay_vertices.len();
        }

        // Update viewport buffer if changed
        let viewport = [viewport_w, viewport_h];
        if viewport != self.last_viewport {
            self.last_viewport = viewport;
            unsafe {
                let ptr = self.viewport_buf.contents().as_ptr() as *mut [f32; 2];
                *ptr = viewport;
            }
        }

        // Update atlas size buffer if atlas grew
        self.ctx.sync_atlas_size();

        let pass_desc = {
            let desc = MTLRenderPassDescriptor::new();
            let color = unsafe { desc.colorAttachments().objectAtIndexedSubscript(0) };
            let tex = drawable.texture();
            color.setTexture(Some(&tex));
            color.setLoadAction(MTLLoadAction::Clear);
            color.setClearColor(MTLClearColor {
                red: self.style.bg_color[0] as f64,
                green: self.style.bg_color[1] as f64,
                blue: self.style.bg_color[2] as f64,
                alpha: 1.0,
            });
            color.setStoreAction(MTLStoreAction::Store);
            desc
        };

        let cmd_buf = match self.ctx.command_queue.commandBuffer() {
            Some(buf) => buf,
            None => {
                log::error!("Metal: failed to create command buffer, skipping frame");
                return;
            }
        };
        let encoder = match cmd_buf.renderCommandEncoderWithDescriptor(&pass_desc) {
            Some(enc) => enc,
            None => {
                log::error!("Metal: failed to create render encoder, skipping frame");
                return;
            }
        };

        if total_vertices > 0 {
            let total_bytes = total_vertices * std::mem::size_of::<Vertex>();

            let buf_idx = self.vertex_buf_idx;
            self.vertex_buf_idx = 1 - buf_idx;

            let current_capacity = self.vertex_buf_capacity;
            if total_bytes > current_capacity {
                // Grow to next power-of-two that fits
                let new_capacity = total_bytes.next_power_of_two();
                log::warn!(
                    "Vertex data ({} bytes) exceeds buffer ({} bytes), growing to {} bytes",
                    total_bytes,
                    current_capacity,
                    new_capacity
                );
                let device = layer.device().expect("no Metal device on layer");
                for i in 0..2 {
                    self.vertex_bufs[i] = device
                        .newBufferWithLength_options(
                            new_capacity,
                            MTLResourceOptions(
                                MTLResourceOptions::CPUCacheModeDefaultCache.0
                                    | MTLResourceOptions::StorageModeShared.0,
                            ),
                        )
                        .expect("failed to reallocate vertex buffer");
                }
                self.vertex_buf_capacity = new_capacity;
            }

            let vertex_buf = &self.vertex_bufs[buf_idx];
            let dst = vertex_buf.contents().as_ptr() as *mut Vertex;
            for &(start, count, _, source) in &draw_calls {
                let src: &[Vertex] = match source {
                    Some(pane_id) => match self.pane_renderers.get(&pane_id) {
                        Some(pane) => pane.vertices(),
                        None => continue,
                    },
                    None => &overlay_vertices,
                };
                debug_assert_eq!(src.len(), count);
                // SAFETY: the buffer holds at least total_vertices vertices
                // (grown above) and the ranges in draw_calls are disjoint.
                unsafe { std::ptr::copy_nonoverlapping(src.as_ptr(), dst.add(start), count) };
            }

            encoder.setRenderPipelineState(&self.ctx.pipeline);
            unsafe {
                encoder.setVertexBuffer_offset_atIndex(Some(vertex_buf), 0, 0);
                encoder.setVertexBuffer_offset_atIndex(Some(&self.viewport_buf), 0, 1);
                encoder.setVertexBuffer_offset_atIndex(Some(&self.ctx.atlas_size_buf), 0, 2);
                encoder.setFragmentTexture_atIndex(Some(&*self.ctx.atlas.texture), 0);
            }

            // Draw each group with its own scissor rect
            for &(start, count, ref scissor, _) in &draw_calls {
                encoder.setScissorRect(MTLScissorRect {
                    x: scissor.x,
                    y: scissor.y,
                    width: scissor.width,
                    height: scissor.height,
                });
                unsafe {
                    encoder.drawPrimitives_vertexStart_vertexCount(
                        MTLPrimitiveType::Triangle,
                        start,
                        count,
                    );
                }
            }
        }

        encoder.endEncoding();
        let mtl_drawable: &ProtocolObject<dyn MTLDrawable> = ProtocolObject::from_ref(&*drawable);
        cmd_buf.presentDrawable(mtl_drawable);
        cmd_buf.commit();
    }

    /// Paint `text` over the whole status-bar row of `vp`, on a solid `color`
    /// background — the pane's own bar is hidden underneath for as long as it
    /// lasts, so the message cannot be mistaken for one more field in the bar.
    /// The text sits one cell in from the left, and is clipped at the right edge
    /// like every other status-bar string. `title` — the pane's session name —
    /// goes at the right end in bold, where the bar it hides would show it.
    fn build_pane_banner_vertices(
        &mut self,
        vertices: &mut Vec<Vertex>,
        vp: &PaneViewport,
        text: &str,
        title: Option<&str>,
        color: [f32; 3],
    ) {
        let cell_h = self.ctx.atlas.cell_height;
        let cell_w = self.ctx.atlas.cell_width;
        let bar_y = vp.y + vp.height - cell_h;
        push_bg_quad(vertices, vp.x, bar_y, vp.width, cell_h, color);
        let fg = [1.0, 1.0, 1.0, 1.0];
        let no_bg = [0.0, 0.0, 0.0, 0.0];
        let text_x = vp.x + self.ctx.h_padding() + cell_w;
        let right_edge = vp.x + vp.width - cell_w;
        let label_end = self
            .ctx
            .render_status_text(vertices, text, text_x, bar_y, right_edge, fg, no_bg);
        if let Some(title) = title.filter(|t| !t.is_empty()) {
            let title_x = banner_title_x(title.chars().count(), label_end, right_edge, cell_w);
            // Faux-bold, as for terminal cells: the same glyphs again, 1px right.
            for dx in [0.0, 1.0] {
                self.ctx.render_status_text(
                    vertices,
                    title,
                    title_x + dx,
                    bar_y,
                    right_edge + dx,
                    fg,
                    no_bg,
                );
            }
        }
    }

    fn build_global_status_bar_vertices(
        &mut self,
        vertices: &mut Vec<Vertex>,
        viewport_w: f32,
        viewport_h: f32,
        hidden_left: usize,
        hidden_right: usize,
        focused_column: usize,
        total_columns: usize,
        active_tab: usize,
        total_tabs: usize,
        active_tab_name: &str,
        working_agents: usize,
        unread_panes: usize,
        sleeping_panes: usize,
        minimized_current: usize,
        minimized_total: usize,
        help_hint_remaining: u32,
        keys_config: Option<&KeysConfig>,
    ) {
        let cell_w = self.ctx.atlas.cell_width;
        let cell_h = self.ctx.atlas.cell_height;
        let bar_y = viewport_h - cell_h;
        self.minimized_counter_zone = None;

        // Background quad
        push_bg_quad(vertices, 0.0, bar_y, viewport_w, cell_h, self.global_bar_bg);

        let no_bg = [0.0, 0.0, 0.0, 0.0];
        let time_fg = [
            self.global_bar_time_color[0],
            self.global_bar_time_color[1],
            self.global_bar_time_color[2],
            1.0,
        ];
        let scroll_fg = [
            self.global_bar_scroll_color[0],
            self.global_bar_scroll_color[1],
            self.global_bar_scroll_color[2],
            1.0,
        ];
        let tab_fg = [
            self.global_bar_time_color[0],
            self.global_bar_time_color[1],
            self.global_bar_time_color[2],
            0.5,
        ];

        // Center: [tab/total] - col/total, combined with scroll arrows
        // Pre-compute the full text to measure total width for centering
        {
            let tab_name_text = format!("{} ", active_tab_name);
            let tab_text = format!("[{}/{}]", active_tab, total_tabs);
            let sep = " - ";
            let col_text = format!("{}/{}", focused_column, total_columns);

            // Build optional left/right scroll parts
            let left_arrow = if hidden_left > 0 {
                format!("⟵ {} | ", hidden_left)
            } else {
                String::new()
            };
            let right_arrow = if hidden_right > 0 {
                format!(" | {} ⟶", hidden_right)
            } else {
                String::new()
            };

            // Total char width for centering
            let total_chars = left_arrow.chars().count()
                + tab_name_text.chars().count()
                + tab_text.chars().count()
                + sep.chars().count()
                + col_text.chars().count()
                + right_arrow.chars().count();
            let text_w = total_chars as f32 * cell_w;
            let center_start = (viewport_w - text_w) / 2.0;
            let mut x = center_start;

            if hidden_left > 0 {
                x = self.ctx.render_status_text(
                    vertices,
                    &left_arrow,
                    x,
                    bar_y,
                    viewport_w,
                    scroll_fg,
                    no_bg,
                );
            }
            x = self.ctx.render_status_text(
                vertices,
                &tab_name_text,
                x,
                bar_y,
                viewport_w,
                tab_fg,
                no_bg,
            );
            x = self
                .ctx
                .render_status_text(vertices, &tab_text, x, bar_y, viewport_w, tab_fg, no_bg);
            x = self
                .ctx
                .render_status_text(vertices, sep, x, bar_y, viewport_w, tab_fg, no_bg);
            x = self
                .ctx
                .render_status_text(vertices, &col_text, x, bar_y, viewport_w, scroll_fg, no_bg);
            if hidden_right > 0 {
                x = self.ctx.render_status_text(
                    vertices,
                    &right_arrow,
                    x,
                    bar_y,
                    viewport_w,
                    scroll_fg,
                    no_bg,
                );
            }
            self.push_tooltip_zone(
                center_start,
                bar_y,
                x - center_start,
                cell_h,
                "Tab / total — focused column / total columns",
            );
        }

        // Left: loading progress (highest priority)
        if let Some((ready, total)) = self.loading_progress {
            let text = format!("Loading {}/{}...", ready, total);
            let loading_fg = [0.6, 0.85, 0.6, 1.0];
            self.ctx.render_status_text(
                vertices, &text, cell_w, bar_y, viewport_w, loading_fg, no_bg,
            );
        }
        // Left: resize feedback (takes priority over help hint)
        else if let Some(text) = self.resize_feedback_text.clone() {
            let info_fg = [0.6, 0.85, 0.6, 1.0];
            self.ctx
                .render_status_text(vertices, &text, cell_w, bar_y, viewport_w, info_fg, no_bg);
        }
        // Left: permanent shortcuts reminder (help + overlays), unless loading
        // or resize feedback is shown. Brighter for the first few seconds after
        // startup (help_hint_remaining), then a dim steady state.
        else {
            if self.cached_shortcuts_hint.is_empty() {
                if let Some(kc) = keys_config {
                    self.cached_shortcuts_hint = format!(
                        "{} help   {} panes   {} search   {} closed tabs",
                        format_key_combo(&kc.toggle_help),
                        format_key_combo(&kc.open_pane_switcher),
                        format_key_combo(&kc.open_search),
                        format_key_combo(&kc.open_recent_project),
                    );
                }
            }
            if !self.cached_shortcuts_hint.is_empty() {
                let alpha = if help_hint_remaining > 0 { 0.85 } else { 0.4 };
                let hint_fg = [0.6, 0.75, 1.0, alpha];
                let hint = self.cached_shortcuts_hint.clone();
                self.ctx
                    .render_status_text(vertices, &hint, cell_w, bar_y, viewport_w, hint_fg, no_bg);
            }
        }

        // Right: proc count + RSS + time (e.g. "▶2  14.2M  17:42")
        if !self.cached_time_str.is_empty() {
            let time_str = self.cached_time_str.clone();
            let rss_str = self.cached_rss_str.clone();
            let time_w = time_str.chars().count() as f32 * cell_w;
            let right_x = viewport_w - time_w - cell_w;
            self.ctx.render_status_text(
                vertices, &time_str, right_x, bar_y, viewport_w, time_fg, no_bg,
            );

            let mut left_edge = right_x;
            let gap = cell_w * 2.0;

            if !rss_str.is_empty() {
                let rss_fg = [
                    self.global_bar_time_color[0],
                    self.global_bar_time_color[1],
                    self.global_bar_time_color[2],
                    0.6,
                ];
                let rss_w = rss_str.chars().count() as f32 * cell_w;
                left_edge = left_edge - rss_w - gap;
                self.ctx.render_status_text(
                    vertices, &rss_str, left_edge, bar_y, viewport_w, rss_fg, no_bg,
                );
                self.push_tooltip_zone(left_edge, bar_y, rss_w, cell_h, "Memory usage (RSS)");
            }

            if !self.cached_io_str.is_empty() {
                let io_fg = [
                    self.global_bar_time_color[0],
                    self.global_bar_time_color[1],
                    self.global_bar_time_color[2],
                    0.5,
                ];
                let io_w = self.cached_io_str.chars().count() as f32 * cell_w;
                left_edge = left_edge - io_w - gap;
                let io_str = self.cached_io_str.clone();
                self.ctx.render_status_text(
                    vertices, &io_str, left_edge, bar_y, viewport_w, io_fg, no_bg,
                );
                self.push_tooltip_zone(
                    left_edge,
                    bar_y,
                    io_w,
                    cell_h,
                    "Total I/O — ↑ chars sent  ↓ chars received",
                );
            }

            let proc_fg = if self.cached_proc_count > 0 {
                [0.6, 0.85, 0.6, 1.0]
            } else {
                [
                    self.global_bar_time_color[0],
                    self.global_bar_time_color[1],
                    self.global_bar_time_color[2],
                    0.4,
                ]
            };
            let proc_w = self.cached_proc_str.chars().count() as f32 * cell_w;
            left_edge = left_edge - proc_w - gap;
            let proc_str = self.cached_proc_str.clone();
            self.ctx.render_status_text(
                vertices, &proc_str, left_edge, bar_y, viewport_w, proc_fg, no_bg,
            );
            self.push_tooltip_zone(
                left_edge,
                bar_y,
                proc_w,
                cell_h,
                "Programs running in panes (an asleep agent pane starts its own when first shown)",
            );

            // Number of panes whose app is actively working — Claude Code's
            // spinner in the OSC title, or a `working` record from the Program
            // Status Protocol (Pi's way in). Hidden when none are busy.
            if working_agents > 0 {
                let agents_str = format!("\u{2733}{}", working_agents);
                let agents_w = agents_str.chars().count() as f32 * cell_w;
                left_edge = left_edge - agents_w - gap;
                self.ctx.render_status_text(
                    vertices,
                    &agents_str,
                    left_edge,
                    bar_y,
                    viewport_w,
                    WORKING_FG,
                    no_bg,
                );
                self.push_tooltip_zone(
                    left_edge,
                    bar_y,
                    agents_w,
                    cell_h,
                    "Panes working right now (Claude Code's spinner or a program's OSC 7501 status)",
                );
            }

            // Panes carrying output nobody has looked at yet — a bell, or a
            // command that finished while the eye was elsewhere. Sits right of
            // the working counter: together they read as "N running, M unread".
            // Hidden when everything has been read.
            if unread_panes > 0 {
                let unread_str = format!("\u{25cf}{}", unread_panes);
                let unread_w = unread_str.chars().count() as f32 * cell_w;
                left_edge = left_edge - unread_w - gap;
                self.ctx.render_status_text(
                    vertices,
                    &unread_str,
                    left_edge,
                    bar_y,
                    viewport_w,
                    UNREAD_FG,
                    no_bg,
                );
                self.push_tooltip_zone(
                    left_edge,
                    bar_y,
                    unread_w,
                    cell_h,
                    "Panes with unread output (bell or finished command)",
                );
            }

            // Agent panes restored asleep: their conversation resumes the first
            // time each is on screen, so the ▶ count leaves them out. Hidden
            // once every one has woken.
            if sleeping_panes > 0 {
                let sleep_str = format!("\u{25cc}{}", sleeping_panes);
                let sleep_w = sleep_str.chars().count() as f32 * cell_w;
                left_edge = left_edge - sleep_w - gap;
                let sleep_fg = [
                    self.global_bar_time_color[0],
                    self.global_bar_time_color[1],
                    self.global_bar_time_color[2],
                    0.6,
                ];
                self.ctx.render_status_text(
                    vertices, &sleep_str, left_edge, bar_y, viewport_w, sleep_fg, no_bg,
                );
                self.push_tooltip_zone(left_edge, bar_y, sleep_w, cell_h, "Agent panes still asleep — each resumes its conversation when first shown, and is saved on quit");
            }

            // Minimized panes: "⊟ current/total" (current tab / all windows).
            // Hidden when there are none anywhere; dimmed when none in this tab.
            // Clickable — opens the pane switcher (zone stored for hit-testing).
            if minimized_total > 0 {
                let min_str = format!("\u{229f} {}/{}", minimized_current, minimized_total);
                let min_fg = if minimized_current > 0 {
                    MINIMIZED_FG
                } else {
                    [MINIMIZED_FG[0], MINIMIZED_FG[1], MINIMIZED_FG[2], 0.4]
                };
                let min_w = min_str.chars().count() as f32 * cell_w;
                left_edge = left_edge - min_w - gap;
                self.ctx.render_status_text(
                    vertices, &min_str, left_edge, bar_y, viewport_w, min_fg, no_bg,
                );
                self.push_tooltip_zone(
                    left_edge,
                    bar_y,
                    min_w,
                    cell_h,
                    "Minimized panes — this tab / all windows (click: switcher)",
                );
                self.minimized_counter_zone = Some((left_edge, bar_y, min_w, cell_h));
            }
        }
    }

    /// The subject strip under the tab bar: each segment a band of its
    /// subject's color with the name on it, full color over the focused pane
    /// and dimmed elsewhere, like the tabs. The name stays on screen when the
    /// segment starts left of it (horizontal scroll).
    fn build_subject_strip_vertices(&mut self, vertices: &mut Vec<Vertex>, viewport_w: f32) {
        if self.subject_strip.height <= 0.0 {
            return;
        }
        let cell_w = self.ctx.atlas.cell_width;
        let cell_h = self.ctx.atlas.cell_height;
        let strip = std::mem::take(&mut self.subject_strip);
        push_bg_quad(
            vertices,
            0.0,
            strip.y,
            viewport_w,
            strip.height,
            self.tab_bar_bg,
        );
        let no_bg = [0.0, 0.0, 0.0, 0.0];
        for seg in &strip.segments {
            let x0 = seg.x.max(0.0);
            let x1 = (seg.x + seg.width).min(viewport_w);
            if x1 <= x0 {
                continue;
            }
            let base = seg
                .color
                .map_or(SUBJECT_FRAME, |i| TAB_COLORS[i % TAB_COLORS.len()]);
            let bg = if seg.focused {
                base
            } else {
                dim_inactive_tab(base)
            };
            // One pixel apart: two subjects sharing a color stay two bands.
            push_bg_quad(
                vertices,
                x0,
                strip.y,
                (x1 - x0 - 1.0).max(0.0),
                strip.height,
                bg,
            );
            let fg = if seg.focused {
                [1.0, 1.0, 1.0, 1.0]
            } else {
                [DIM_BRIGHTNESS, DIM_BRIGHTNESS, DIM_BRIGHTNESS, 1.0]
            };
            let text_y = strip.y + (strip.height - cell_h) / 2.0;
            self.ctx.render_status_text(
                vertices,
                &seg.name,
                x0 + cell_w,
                text_y,
                x1 - cell_w,
                fg,
                no_bg,
            );
        }
        self.subject_strip = strip;
    }

    fn build_tab_bar_vertices(
        &mut self,
        vertices: &mut Vec<Vertex>,
        viewport_w: f32,
        tab_titles: &[(String, bool, Option<usize>, bool, bool, bool, bool)],
        left_inset: f32,
    ) {
        let cell_w = self.ctx.atlas.cell_width;
        let cell_h = self.ctx.atlas.cell_height;
        let bar_h = (cell_h * 2.0).round();
        let tab_count = tab_titles.len();

        // Full-width background
        push_bg_quad(vertices, 0.0, 0.0, viewport_w, bar_h, self.tab_bar_bg);

        // Version label — always visible, doubles as window drag handle
        let version_label = format!("Kova v{}", env!("CARGO_PKG_VERSION"));
        let version_chars = version_label.chars().count() as f32;
        let right_inset = cell_w * (version_chars + 3.5);

        // Fixed width per tab, capped at cell_w * 20, reserving right inset
        let max_tab_w = cell_w * 20.0;
        let full_available_w = viewport_w - left_inset - right_inset;
        let tab_width = (full_available_w / tab_count as f32)
            .max(cell_w * 4.0)
            .min(max_tab_w);
        let no_bg = [0.0, 0.0, 0.0, 0.0];

        for (
            i,
            (title, is_active, color_idx, is_renaming, has_bell, has_completion, has_running),
        ) in tab_titles.iter().enumerate()
        {
            let x = left_inset + i as f32 * tab_width;

            // Tab background color. Inactive colored tabs are dimmed: with
            // every tab colored, a brighter marker on the active one drowns in
            // the surrounding saturation — the contrast has to come from the
            // others stepping back.
            let tab_bg: Option<[f32; 3]> = if let Some(idx) = color_idx {
                let c = TAB_COLORS[*idx % TAB_COLORS.len()];
                Some(if *is_active { c } else { dim_inactive_tab(c) })
            } else if *is_active {
                Some(self.tab_bar_active_bg)
            } else {
                None // transparent, shows bar bg
            };

            if let Some(bg) = tab_bg {
                push_bg_quad(vertices, x, 0.0, tab_width, bar_h, bg);
            }

            // Active tab: white border at bottom, the same on every tab color
            if *is_active {
                let border_h = 4.0_f32;
                push_bg_quad(
                    vertices,
                    x,
                    bar_h - border_h,
                    tab_width,
                    border_h,
                    [1.0, 1.0, 1.0],
                );
            }

            // Tab indicator: bell (orange ●) > completion (green ●) > running
            // (yellow ▶). Bell/completion only on non-active tabs (the active
            // tab's content is visible); running shows everywhere — the
            // churning pane may be minimized or scrolled out of view.
            // Computed before the title so the title can reserve its slot.
            let indicator: Option<([f32; 3], &str)> = if *has_bell && !is_active {
                Some(([1.0_f32, 0.45, 0.1], "●"))
            } else if *has_completion && !is_active {
                Some(([0.2_f32, 0.8, 0.3], "●"))
            } else if *has_running {
                Some(([1.0_f32, 0.8, 0.2], "▶"))
            } else {
                None
            };

            // Tab number + title: "1: title"
            // When renaming, show the end of the text so the cursor is visible
            let truncated: String;
            let max_title_chars = 25;
            let display_title = if title.chars().count() > max_title_chars {
                if *is_renaming {
                    // Show last N chars to keep cursor visible
                    let skip = title.chars().count() - max_title_chars;
                    truncated = title.chars().skip(skip).collect();
                    &truncated
                } else {
                    truncated = title.chars().take(max_title_chars).collect();
                    &truncated
                }
            } else {
                title
            };
            let label = format!("{}:{}", i + 1, display_title);
            // White text on the active tab; on a dimmed colored tab the label
            // dims by the same brightness factor as its background.
            let fg = if *is_active {
                [1.0, 1.0, 1.0, 1.0]
            } else if color_idx.is_some() {
                [DIM_BRIGHTNESS, DIM_BRIGHTNESS, DIM_BRIGHTNESS, 1.0]
            } else {
                [
                    self.tab_bar_fg[0],
                    self.tab_bar_fg[1],
                    self.tab_bar_fg[2],
                    1.0,
                ]
            };

            // Center text vertically and horizontally in the tab.
            // When an indicator is shown, clip the title before its slot
            // (indicator at tab_width - 2 cells) instead of running under it.
            let text_w = label.chars().count() as f32 * cell_w;
            let text_x = x + (tab_width - text_w) / 2.0;
            let text_y = (bar_h - cell_h) / 2.0;
            let max_x = if indicator.is_some() {
                x + tab_width - cell_w * 2.5
            } else {
                x + tab_width - cell_w
            };
            self.ctx.render_status_text(
                vertices,
                &label,
                text_x.max(x + cell_w * 0.5),
                text_y,
                max_x,
                fg,
                no_bg,
            );

            if let Some((color, glyph)) = indicator {
                let dot_x = x + tab_width - cell_w * 2.0;
                let dot_y = (bar_h - cell_h) / 2.0;
                let dot_color = if let Some(bg) = tab_bg {
                    let lum = |c: [f32; 3]| 0.299 * c[0] + 0.587 * c[1] + 0.114 * c[2];
                    if (lum(color) - lum(bg)).abs() < 0.25 {
                        [1.0, 1.0, 1.0, 1.0]
                    } else {
                        [color[0], color[1], color[2], 1.0]
                    }
                } else {
                    [color[0], color[1], color[2], 1.0]
                };
                self.ctx.render_status_text(
                    vertices,
                    glyph,
                    dot_x,
                    dot_y,
                    x + tab_width,
                    dot_color,
                    no_bg,
                );
            }
        }

        // Render drag grip + version label on the right (always visible — drag handle zone)
        {
            let grip_fg = [
                self.tab_bar_fg[0],
                self.tab_bar_fg[1],
                self.tab_bar_fg[2],
                1.0,
            ];
            let version_fg = [
                self.tab_bar_fg[0],
                self.tab_bar_fg[1],
                self.tab_bar_fg[2],
                0.5,
            ];
            let grip_x = viewport_w - right_inset + cell_w * 0.25;
            let version_x = grip_x + cell_w * 1.5;
            let version_y = (bar_h - cell_h) / 2.0;
            self.ctx
                .render_status_text(vertices, "⠿", grip_x, version_y, viewport_w, grip_fg, no_bg);
            self.ctx.render_status_text(
                vertices,
                &version_label,
                version_x,
                version_y,
                viewport_w - cell_w * 0.5,
                version_fg,
                no_bg,
            );
        }
    }

    fn build_filter_overlay_vertices(
        &mut self,
        vertices: &mut Vec<Vertex>,
        vp: &PaneViewport,
        filter: &FilterRenderData,
    ) {
        let selected_bg = self.switcher_selected_bg;
        build_filter_overlay(&mut self.ctx, vertices, vp, filter, selected_bg);
    }

    pub fn rebuild_atlas(&mut self, scale: f64) {
        self.ctx.rebuild_atlas(scale);
        // A new atlas restarts its generation count: cached UVs would point
        // into the old texture with a key that still matches.
        self.pane_renderers.clear();
    }

    /// Memory report for the renderer (atlas + vertex buffers).
    pub fn mem_report(&self) -> (usize, (u32, u32), usize, usize) {
        let atlas_bytes = self.ctx.atlas.mem_bytes();
        let atlas_dims = self.ctx.atlas.texture_size();
        let glyph_count = self.ctx.atlas.glyphs.len() + self.ctx.atlas.cluster_glyphs.len();
        let vertex_bytes = self.vertex_buf_capacity * 2; // double-buffered
        (atlas_bytes, atlas_dims, glyph_count, vertex_bytes)
    }

    fn build_help_overlay_vertices(
        &mut self,
        vertices: &mut Vec<Vertex>,
        viewport_w: f32,
        viewport_h: f32,
        keys_config: &KeysConfig,
    ) {
        // Overlay atlas: glyphs rasterized natively at ~1.3x the terminal font,
        // so they render sharp instead of bilinearly upscaling the base atlas.
        let ocw = self.ctx.atlas.overlay_cell_width;
        let och = self.ctx.atlas.overlay_cell_height;
        let base_ch = self.ctx.atlas.cell_height;

        // Semi-transparent dark overlay
        push_bg_quad_alpha(
            vertices,
            0.0,
            0.0,
            viewport_w,
            viewport_h,
            [0.0, 0.0, 0.0],
            0.9,
        );

        let no_bg = [0.0, 0.0, 0.0, 0.0];
        let title_fg = [1.0, 0.85, 0.3, 1.0]; // accent yellow
        let label_fg = [0.7, 0.7, 0.75, 1.0];
        let key_fg = [1.0, 1.0, 1.0, 1.0];
        let dim_fg = [0.55, 0.55, 0.6, 1.0];

        // Title lightly upscaled from the overlay atlas (kept near 1.0 to stay crisp).
        let title_scale = 1.4_f32;

        // Title centered
        let title = "Keyboard Shortcuts";
        let title_chars = title.chars().count() as f32;
        let title_x = (viewport_w - title_chars * ocw * title_scale) / 2.0;
        let mut y = och * 2.0;
        self.ctx.render_overlay_text(
            vertices,
            title,
            title_x,
            y,
            viewport_w,
            title_fg,
            no_bg,
            title_scale,
        );
        y += och * title_scale * 1.6;

        // Subtitle
        if self.cached_help_hint.is_empty() {
            self.cached_help_hint = format_key_combo(&keys_config.toggle_help);
        }
        let subtitle = format!("Press {} or Esc to close", &self.cached_help_hint);
        let sub_chars = subtitle.chars().count() as f32;
        let sub_x = (viewport_w - sub_chars * ocw) / 2.0;
        self.ctx.render_overlay_text(
            vertices, &subtitle, sub_x, y, viewport_w, label_fg, no_bg, 1.0,
        );
        drop(subtitle);
        y += och * 1.4;

        // Actually-resolved font (CoreText may substitute / fall back silently).
        let font_line = {
            let actual = self.ctx.atlas.actual_font_name.clone();
            let configured = &self.ctx.font_name;
            if actual.to_lowercase().contains(&configured.to_lowercase()) {
                format!("Font: {} {:.1}pt", actual, self.ctx.font_size)
            } else {
                format!(
                    "Font: {} {:.1}pt  (\"{}\" not found, using fallback)",
                    actual, self.ctx.font_size, configured
                )
            }
        };
        let fl_chars = font_line.chars().count() as f32;
        let fl_x = (viewport_w - fl_chars * ocw) / 2.0;
        self.ctx.render_overlay_text(
            vertices, &font_line, fl_x, y, viewport_w, dim_fg, no_bg, 1.0,
        );
        drop(font_line);
        y += och * 2.2;

        // Build the sectioned shortcut list (cached to avoid per-frame allocation).
        // Each entry is (label, key combo, one-line "what it does / when it works").
        // The two column groups are laid out left / right, sections kept intact.
        if self.cached_help_columns[0].is_empty() && self.cached_help_columns[1].is_empty() {
            let kc = keys_config;
            type Section<'a> = (&'a str, Vec<(&'a str, &'a str, &'a str)>);
            let left: Vec<Section> = vec![
                (
                    "TABS & WINDOWS",
                    vec![
                        ("New Tab", kc.new_tab.as_str(), ""),
                        (
                            "Close Pane/Tab",
                            kc.close_pane_or_tab.as_str(),
                            "pane, or tab if last one",
                        ),
                        ("Close Tab", kc.close_tab.as_str(), "whole tab at once"),
                        (
                            "Reopen Pane",
                            kc.reopen_closed_pane.as_str(),
                            "undo a close, conversation included",
                        ),
                        ("Previous Tab", kc.prev_tab.as_str(), ""),
                        ("Next Tab", kc.next_tab.as_str(), ""),
                        ("Jump to Tab", "cmd+1-9", ""),
                        ("Rename Tab", kc.rename_tab.as_str(), ""),
                        ("New Window", kc.new_window.as_str(), ""),
                        ("Close Window", kc.close_window.as_str(), ""),
                        ("Kill Window", kc.kill_window.as_str(), "force, no prompt"),
                        ("Reopen Tab", kc.open_recent_project.as_str(), "closed tabs"),
                    ],
                ),
                (
                    "SPLITS",
                    vec![
                        ("Vertical Split", kc.vsplit.as_str(), "side by side"),
                        ("Horizontal Split", kc.hsplit.as_str(), "stacked"),
                        (
                            "V Split (Root)",
                            kc.vsplit_root.as_str(),
                            "full column height",
                        ),
                        ("H Split (Root)", kc.hsplit_root.as_str(), "full row width"),
                        ("Equalize", kc.equalize.as_str(), "even out sizes"),
                    ],
                ),
                (
                    "MOVE PANES & TABS",
                    vec![
                        (
                            "Break Pane",
                            kc.break_pane.as_str(),
                            "pane → new tab (needs 2+ panes)",
                        ),
                        (
                            "Merge Tab",
                            kc.merge_tab.as_str(),
                            "fold this tab into another",
                        ),
                        ("Detach Tab", kc.detach_tab.as_str(), "tab → new window"),
                        (
                            "Merge Window",
                            kc.merge_window.as_str(),
                            "fold window into another",
                        ),
                        ("Swap Pane", kc.swap_up.as_str(), "trade two panes"),
                        (
                            "Reparent Pane",
                            kc.reparent_up.as_str(),
                            "move across the split tree",
                        ),
                    ],
                ),
                (
                    "MISC",
                    vec![
                        ("Memory Report", "cmd+shift+i", ""),
                        ("Help", kc.toggle_help.as_str(), "this screen"),
                    ],
                ),
            ];
            let right: Vec<Section> = vec![
                (
                    "PANES",
                    vec![
                        ("Navigate", kc.navigate_up.as_str(), "move focus"),
                        ("Resize Pane", kc.resize_up.as_str(), "adjust split ratio"),
                        ("Edge Grow", kc.edge_grow_right.as_str(), "grow one edge"),
                        (
                            "Virtual Width",
                            "ctrl+option+right",
                            "widen / narrow the tab",
                        ),
                        ("Minimize Pane", kc.minimize_pane.as_str(), ""),
                        ("Restore Minimized", kc.restore_minimized.as_str(), ""),
                        ("Rename Pane", kc.rename_pane.as_str(), "sticky title"),
                        (
                            "Repaint Pane",
                            kc.repaint_pane.as_str(),
                            "redraw / fix winsize",
                        ),
                        (
                            "Next Unread",
                            kc.next_attention.as_str(),
                            "unread, else idle agent session",
                        ),
                        (
                            "Back / Forward",
                            kc.history_back.as_str(),
                            "panes you visited",
                        ),
                    ],
                ),
                (
                    "EDIT & SEARCH",
                    vec![
                        ("Copy", kc.copy.as_str(), ""),
                        ("Copy Raw", kc.copy_raw.as_str(), ""),
                        ("Paste", kc.paste.as_str(), ""),
                        ("Find", kc.toggle_filter.as_str(), "search in this pane"),
                        (
                            "Global Search",
                            kc.open_search.as_str(),
                            "panes + closed sessions",
                        ),
                        (
                            "Switch Tab/Pane",
                            kc.open_pane_switcher.as_str(),
                            "switcher + bookmarks; \u{2318}\u{2190}\u{2192} file into subjects",
                        ),
                        (
                            "Bookmark Pane",
                            kc.toggle_bookmark.as_str(),
                            "keep this conversation",
                        ),
                        (
                            "Anchor Pane",
                            kc.toggle_anchor.as_str(),
                            "what today is for",
                        ),
                        (
                            "Go To Anchor",
                            kc.focus_anchor.as_str(),
                            "back to the first anchor",
                        ),
                        (
                            "Unread Panes",
                            kc.open_unread_switcher.as_str(),
                            "switcher, attention only",
                        ),
                    ],
                ),
                (
                    "TERMINAL",
                    vec![
                        (
                            "Line Start / End",
                            kc.terminal.home.as_str(),
                            "\u{2190} / \u{2192}",
                        ),
                        (
                            "Word Jump",
                            kc.terminal.word_back.as_str(),
                            "\u{2190} / \u{2192}",
                        ),
                        ("Kill Line", kc.terminal.kill_line.as_str(), ""),
                        (
                            "Newline",
                            kc.terminal.shift_enter.as_str(),
                            "without running",
                        ),
                        ("Clear Scrollback", "ctrl+l", "passed to the app too"),
                        ("Open URL", "cmd+click", ""),
                    ],
                ),
            ];
            let build = |sections: Vec<Section>| -> Vec<HelpRow> {
                let mut rows = Vec::new();
                for (header, items) in sections {
                    rows.push(HelpRow::Header(header.to_string()));
                    for (label, key, desc) in items {
                        rows.push(HelpRow::Item {
                            label: label.to_string(),
                            key: format_key_combo_arrows(key),
                            desc: desc.to_string(),
                        });
                    }
                }
                rows
            };
            self.cached_help_columns = [build(left), build(right)];
        }

        // Take columns out of self to avoid borrow conflict with render_overlay_text.
        let columns = std::mem::replace(&mut self.cached_help_columns, [Vec::new(), Vec::new()]);

        // Global column alignment: line up the key combo and description across all rows.
        let mut max_label = 0usize;
        let mut max_key = 0usize;
        for col in &columns {
            for row in col {
                if let HelpRow::Item { label, key, .. } = row {
                    max_label = max_label.max(label.chars().count());
                    max_key = max_key.max(key.chars().count());
                }
            }
        }
        let col_width = viewport_w / 2.0;
        let label_off = ocw * 2.0;
        let key_off = label_off + (max_label as f32 + 1.0) * ocw;
        let desc_off = key_off + (max_key as f32 + 2.0) * ocw;
        // Tighten the spacing when the tallest column would run past the
        // status bar, so a short window still shows every row.
        let avail = viewport_h - base_ch - y;
        let scale = columns
            .iter()
            .map(|col| {
                let headers = col
                    .iter()
                    .filter(|r| matches!(r, HelpRow::Header(_)))
                    .count();
                help_row_scale(headers, col.len() - headers, avail, och)
            })
            .fold(1.0_f32, f32::min);
        let row_h = och * HELP_ROW_STEP * scale;
        let header_gap = och * HELP_HEADER_GAP * scale;

        for (ci, col) in columns.iter().enumerate() {
            let base_x = ci as f32 * col_width;
            let mut row_y = y;
            for row in col {
                if row_y + och > viewport_h - base_ch {
                    break; // Don't overflow past global status bar
                }
                match row {
                    HelpRow::Header(h) => {
                        row_y += header_gap; // breathing room above each section
                        self.ctx.render_overlay_text(
                            vertices,
                            h,
                            base_x + label_off,
                            row_y,
                            base_x + col_width - ocw,
                            title_fg,
                            no_bg,
                            1.0,
                        );
                        row_y += row_h;
                    }
                    HelpRow::Item { label, key, desc } => {
                        self.ctx.render_overlay_text(
                            vertices,
                            label,
                            base_x + label_off,
                            row_y,
                            base_x + key_off - ocw,
                            label_fg,
                            no_bg,
                            1.0,
                        );
                        self.ctx.render_overlay_text(
                            vertices,
                            key,
                            base_x + key_off,
                            row_y,
                            base_x + desc_off - ocw,
                            key_fg,
                            no_bg,
                            1.0,
                        );
                        if !desc.is_empty() {
                            self.ctx.render_overlay_text(
                                vertices,
                                desc,
                                base_x + desc_off,
                                row_y,
                                base_x + col_width - ocw,
                                dim_fg,
                                no_bg,
                                1.0,
                            );
                        }
                        row_y += row_h;
                    }
                }
            }
        }

        // Put columns back.
        self.cached_help_columns = columns;
    }

    fn build_mem_report_overlay_vertices(
        &mut self,
        vertices: &mut Vec<Vertex>,
        viewport_w: f32,
        viewport_h: f32,
    ) {
        let overlay_cw = self.ctx.atlas.overlay_cell_width;
        let overlay_ch = self.ctx.atlas.overlay_cell_height;

        // Semi-transparent dark overlay
        push_bg_quad_alpha(
            vertices,
            0.0,
            0.0,
            viewport_w,
            viewport_h,
            [0.0, 0.0, 0.0],
            0.9,
        );

        let no_bg = [0.0, 0.0, 0.0, 0.0];
        let title_fg = [1.0, 0.85, 0.3, 1.0];
        let label_fg = [0.8, 0.85, 0.9, 1.0];
        let dim_fg = [0.55, 0.6, 0.65, 1.0];
        let estimated_fg = [0.9, 0.4, 0.35, 1.0];

        // Title — rendered natively at overlay font size, no stretching
        let title = "Memory Report";
        let title_chars = title.chars().count() as f32;
        let title_x = (viewport_w - title_chars * overlay_cw) / 2.0;
        let mut y = overlay_ch * 2.0;
        self.ctx.render_overlay_text(
            vertices, title, title_x, y, viewport_w, title_fg, no_bg, 1.0,
        );
        y += overlay_ch * 2.0;

        // Subtitle
        let subtitle = "Press Esc to close";
        let sub_chars = subtitle.chars().count() as f32;
        let sub_x = (viewport_w - sub_chars * overlay_cw) / 2.0;
        self.ctx
            .render_overlay_text(vertices, subtitle, sub_x, y, viewport_w, dim_fg, no_bg, 1.0);
        y += overlay_ch * 2.5;

        // Report lines
        let report = std::mem::take(&mut self.cached_mem_report);
        let left_margin = overlay_cw * 3.0;
        for line in &report {
            if y + overlay_ch > viewport_h - overlay_ch {
                break;
            }
            let (text, fg) = if let Some(stripped) = line.strip_prefix('~') {
                (stripped, estimated_fg)
            } else if line.starts_with("===") || line.starts_with("RSS") {
                (line.as_str(), title_fg)
            } else if line.starts_with("  ") {
                (line.as_str(), dim_fg)
            } else {
                (line.as_str(), label_fg)
            };
            self.ctx.render_overlay_text(
                vertices,
                text,
                left_margin,
                y,
                viewport_w - overlay_cw,
                fg,
                no_bg,
                1.0,
            );
            y += overlay_ch * 1.3;
        }
        self.cached_mem_report = report;
    }

    /// Vertical geometry of the scrolling list overlays (recent projects, pane
    /// switcher). Kept as a method so mouse hit-testing maps clicks to the same
    /// rows the renderer drew. The constants below MUST match the layout used in
    /// `build_pane_switcher_overlay_vertices` / `build_recent_projects_overlay_vertices`.
    pub fn overlay_list_geometry(&self, viewport_h: f32) -> OverlayListGeometry {
        let cell_h = self.ctx.atlas.cell_height;
        // title at y=3·cell_h, +title_scale(1.8)·2 lines, +the hint line and its gap.
        let content_top = cell_h * (3.0 + 1.8 * 2.0 + 1.3 * 2.0);
        // Rows sized for the terminal's own glyphs (the list draws them at 1.0).
        let row_height = cell_h * 1.7;
        let content_bottom = viewport_h - cell_h * 2.0;
        let max_visible = (((content_bottom - content_top) / row_height)
            .floor()
            .max(0.0)) as usize;
        OverlayListGeometry {
            content_top,
            row_height,
            max_visible,
        }
    }

    fn build_pane_switcher_overlay_vertices(
        &mut self,
        vertices: &mut Vec<Vertex>,
        viewport_w: f32,
        viewport_h: f32,
        data: &PaneSwitcherRenderData<'_>,
    ) {
        let cell_w = self.ctx.atlas.cell_width;
        let cell_h = self.ctx.atlas.cell_height;

        // Opaque backdrop: terminal text showing through competed with the list.
        push_bg_quad_alpha(
            vertices,
            0.0,
            0.0,
            viewport_w,
            viewport_h,
            [0.04, 0.04, 0.05],
            1.0,
        );

        let no_bg = [0.0, 0.0, 0.0, 0.0];
        let title_fg = [1.0, 0.85, 0.3, 1.0];
        let header_fg = [0.55, 0.75, 1.0, 1.0];
        let label_fg = [0.85, 0.85, 0.9, 1.0];
        let dim_fg = [0.45, 0.45, 0.5, 1.0];
        // The keys of the hint line, warmer than the words they go with.
        let key_fg = [0.95, 0.7, 0.35, 1.0];
        let selected_bg = self.switcher_selected_bg;
        // Bookmarked panes wear a band of their own, from `[bookmarks]` in the
        // config — the selected variant included, so selection still reads on a
        // row that already has a background.
        let bookmark_bg = self.style.bookmarks.row_bg;
        let bookmark_selected_bg = self.style.bookmarks.row_selected_bg;
        let bookmark_fg = self.style.bookmarks.row_fg;
        let bookmark_dim_fg = self.style.bookmarks.row_dim_fg;

        let title_scale = 1.8_f32;
        // 1.0: the terminal's glyphs at their own size. Any other factor
        // stretches the bitmaps and blurs them, worst on a 1x display.
        let body_scale = 1.0_f32;
        let scaled_cell_w = cell_w * body_scale;

        // Title centered. The unread count rides along so the number is read
        // before the eye starts scanning columns for the dots.
        let unread: usize = data
            .columns
            .iter()
            .flat_map(|c| c.rows.iter())
            .filter(|r| !r.is_header && (r.has_bell || r.has_completion))
            .count();
        let title = if data.filtered {
            // Filtered: the count is of everything the list holds, not just the
            // waiting ones — a bell and a finished command are in here too, and
            // an empty list has to say so rather than look like a bad draw.
            let panes = data
                .columns
                .iter()
                .flat_map(|c| c.rows.iter())
                .filter(|r| !r.is_header)
                .count();
            match panes {
                0 => "Nothing Unread".to_string(),
                1 => "Unread Panes  —  1".to_string(),
                n => format!("Unread Panes  —  {}", n),
            }
        } else if let Some((total, panes)) = &data.memory_total {
            format!("Memory  —  {} in {} panes", total, panes)
        } else if data.show_bookmarks {
            let saved = data
                .columns
                .iter()
                .flat_map(|c| c.rows.iter())
                .filter(|r| !r.is_header)
                .count();
            match saved {
                0 => "Bookmarks  —  none saved".to_string(),
                n => format!("Bookmarks  —  {}", n),
            }
        } else {
            match unread {
                0 => "Switch Tab / Pane".to_string(),
                1 => "Switch Tab / Pane  —  1 unread".to_string(),
                n => format!("Switch Tab / Pane  —  {} unread", n),
            }
        };
        // The title in the overlay font, rasterized at its own size: the
        // terminal's glyphs stretched by `title_scale` came out blurred.
        let overlay_cw = self.ctx.atlas.overlay_cell_width;
        let title_chars = title.chars().count() as f32;
        let title_x = ((viewport_w - title_chars * overlay_cw) / 2.0).max(0.0);
        let mut y = cell_h * 3.0;
        self.ctx.render_overlay_text(
            vertices, &title, title_x, y, viewport_w, title_fg, no_bg, 1.0,
        );
        y += cell_h * title_scale * 2.0;

        // Subtitle: the keys the overlay answers, each one a (key, what it does)
        // pair. The key is drawn in its own colour — read as a line of grey the
        // hints ran together, and the eye had to parse where each one started.
        let hints: &[(&str, &str)] = if data.filtered {
            &[
                ("\u{2191}\u{2193}\u{2190}\u{2192}", "Navigate"),
                ("\u{23ce}", "Focus"),
                ("u", "All panes"),
                ("esc", "Cancel"),
            ]
        } else if data.memory_total.is_some() {
            &[
                ("\u{2191}\u{2193}\u{2190}\u{2192}", "Navigate"),
                ("\u{23ce}", "Focus"),
                ("m", "Open panes"),
                ("b", "Bookmarks"),
                ("esc", "Cancel"),
            ]
        } else if data.show_bookmarks {
            &[
                ("\u{2191}\u{2193}\u{2190}\u{2192}", "Navigate"),
                ("\u{23ce}", "Open"),
                ("b", "Open panes"),
                ("m", "Memory"),
                ("\u{2318}\u{21e7}A", "Anchor"),
                ("\u{2318}\u{232b}", "Remove bookmark"),
                ("esc", "Cancel"),
            ]
        } else {
            &[
                ("\u{2191}\u{2193}\u{2190}\u{2192}", "Navigate"),
                ("\u{21e5}", "Next unread"),
                ("\u{23ce}", "Focus"),
                ("\u{2318}\u{2191}\u{2193}", "Move"),
                ("\u{2318}\u{2190}\u{2192}", "Subject"),
                ("\u{2318}\u{21e7}\u{2192}", "New subject"),
                ("n", "New pane"),
                ("\u{23ce}/\u{2318}\u{232b}", "Rename/delete"),
                ("c", "Subject color"),
                ("b", "Bookmarks"),
                ("m", "Memory"),
                ("\u{2318}B", "Bookmark"),
                ("\u{2318}\u{21e7}A", "Anchor"),
                ("u", "Unread only"),
                ("esc", "Cancel"),
            ]
        };
        let sub_chars = hint_line_width(hints) as f32;
        // Clamped: this hint line grew with each key added to the overlay, and a
        // centred line wider than the window would start off its left edge.
        let mut sub_x = ((viewport_w - sub_chars * scaled_cell_w) / 2.0).max(0.0);
        for (key, what) in hints {
            self.ctx.render_text(
                vertices, key, sub_x, y, viewport_w, key_fg, no_bg, body_scale,
            );
            sub_x += (key.chars().count() + 1) as f32 * scaled_cell_w;
            self.ctx.render_text(
                vertices, what, sub_x, y, viewport_w, dim_fg, no_bg, body_scale,
            );
            sub_x += (what.chars().count() + 2) as f32 * scaled_cell_w;
        }

        let geom = self.overlay_list_geometry(viewport_h);
        let content_top = geom.content_top;
        let row_height = geom.row_height;
        let max_visible = geom.max_visible.max(1);
        let scaled_cell_h = cell_h * body_scale;

        let ncols = data.columns.len().max(1);
        let col_w = viewport_w / ncols as f32;
        // Padding inside each column; pane labels indent a bit more than headers.
        let pad = scaled_cell_w * 1.5;

        for (c, column) in data.columns.iter().enumerate() {
            let col_x = c as f32 * col_w;
            let left_margin = col_x + pad;
            let right_margin = col_x + col_w - pad;

            let scroll = column.scroll.min(column.rows.len().saturating_sub(1));
            let end = (scroll + max_visible).min(column.rows.len());

            for (vis_i, i) in (scroll..end).enumerate() {
                let row = &column.rows[i];
                let row_y = content_top + vis_i as f32 * row_height;
                let text_y = row_y + (row_height - scaled_cell_h) / 2.0;

                // A subject's header is selectable (Enter renames, Cmd+Backspace
                // deletes); a tab's header never is.
                let is_selected = c == data.selected_col
                    && i == data.selected_row
                    && (!row.is_header || row.subject_header);
                let band = match (row.bookmarked, is_selected) {
                    (true, true) => Some(bookmark_selected_bg),
                    (true, false) => Some(bookmark_bg),
                    (false, true) => Some(selected_bg),
                    (false, false) => None,
                };
                // A subject's box: a thin frame around its header and members,
                // drawn a row at a time so it scrolls with them. Its edges sit
                // where the selection band's would, and the top and bottom lines
                // are pulled in a little so two boxes in a row never touch.
                let (x0, x1) = (left_margin - pad * 0.5, right_margin + pad * 0.5);
                let inset = row_height * 0.12;
                let y0 = row_y + if row.box_top { inset } else { 0.0 };
                let y1 = row_y + row_height - if row.box_bottom { inset } else { 0.0 };
                if let Some(color) = band {
                    if row.in_subject || row.subject_header {
                        // Inside a box the band stays within the frame, a
                        // couple of pixels clear of it, rather than covering it.
                        let gap = SUBJECT_FRAME_PX + 2.0;
                        let top = if row.box_top { y0 + gap } else { y0 };
                        let bottom = if row.box_bottom { y1 - gap } else { y1 };
                        push_bg_quad_alpha(
                            vertices,
                            x0 + gap,
                            top,
                            x1 - x0 - 2.0 * gap,
                            bottom - top,
                            color,
                            0.8,
                        );
                    } else {
                        push_bg_quad_alpha(vertices, x0, row_y, x1 - x0, row_height, color, 0.8);
                    }
                }

                // The subject's own color, the one its strip wears above the panes.
                let frame = row
                    .subject_color
                    .map_or(SUBJECT_FRAME, |i| TAB_COLORS[i % TAB_COLORS.len()]);
                if row.subject_header || row.in_subject {
                    push_bg_quad_alpha(vertices, x0, y0, SUBJECT_FRAME_PX, y1 - y0, frame, 0.9);
                    push_bg_quad_alpha(
                        vertices,
                        x1 - SUBJECT_FRAME_PX,
                        y0,
                        SUBJECT_FRAME_PX,
                        y1 - y0,
                        frame,
                        0.9,
                    );
                    if row.box_top {
                        push_bg_quad_alpha(vertices, x0, y0, x1 - x0, SUBJECT_FRAME_PX, frame, 0.9);
                    }
                    if row.box_bottom {
                        push_bg_quad_alpha(
                            vertices,
                            x0,
                            y1 - SUBJECT_FRAME_PX,
                            x1 - x0,
                            SUBJECT_FRAME_PX,
                            frame,
                            0.9,
                        );
                    }
                }

                if row.subject_header {
                    // The overlay font has no bold face, and drawing the glyphs
                    // twice blurs them: the name stands out by a brighter colour
                    // and the rule that closes its line.
                    let gap = SUBJECT_FRAME_PX;
                    let bar_bottom = if row.box_bottom { y1 - gap } else { y1 };
                    // No fill: a filled bar read like a bookmarked row's band.
                    // A rule under the name separates it from its panes.
                    if !row.box_bottom {
                        push_bg_quad_alpha(
                            vertices,
                            x0 + gap,
                            bar_bottom - 1.0,
                            x1 - x0 - 2.0 * gap,
                            1.0,
                            frame,
                            0.6,
                        );
                    }
                    self.ctx.render_text(
                        vertices,
                        row.text,
                        left_margin + scaled_cell_w,
                        text_y,
                        right_margin,
                        SUBJECT_TITLE_FG,
                        no_bg,
                        body_scale,
                    );
                } else if row.is_header {
                    self.ctx.render_text(
                        vertices,
                        row.text,
                        left_margin,
                        text_y,
                        right_margin,
                        header_fg,
                        no_bg,
                        body_scale,
                    );
                } else {
                    // No current-pane marker: the focused pane renders like any
                    // other row (the blue selection highlight is the only cursor).
                    // Attention (unread) dot for non-current panes, mirroring the
                    // per-pane status-bar dot (bell > completion). is_current panes
                    // are already suppressed upstream (has_bell/has_completion = false).
                    let attention = PaneAttention::from_flags(row.has_bell, row.has_completion);
                    // The anchor mark is a wide glyph: on a row that wears one
                    // the title starts a cell further right, or the two touch.
                    // A subject's members keep the same lead as any pane: the
                    // frame and the title above already say they belong together.
                    let lead = if row.anchored { "     " } else { "    " };
                    let text = format!("{}{}", lead, row.text);
                    let (row_fg, row_dim_fg) = if row.bookmarked {
                        (
                            if row.minimized {
                                bookmark_dim_fg
                            } else {
                                bookmark_fg
                            },
                            bookmark_dim_fg,
                        )
                    } else {
                        (if row.minimized { dim_fg } else { label_fg }, dim_fg)
                    };
                    // The running binary is parked at the right end of the row,
                    // dim: it says what the pane *is* without competing with the
                    // title, which is what the eye scans. The title is clipped
                    // before it rather than drawn under it.
                    // The conversation's open todos and ideas follow it, same
                    // tail: todos in colour, ideas dim.
                    let split = switcher_row_split(
                        left_margin,
                        right_margin,
                        switcher_row_tail_chars(row.process, row.raf_todo, row.raf_idea),
                        scaled_cell_w,
                    );
                    self.ctx.render_text(
                        vertices,
                        &text,
                        left_margin,
                        text_y,
                        split.title_limit,
                        row_fg,
                        no_bg,
                        body_scale,
                    );
                    if let Some(mut tail_x) = split.process_x {
                        if let Some(process) = row.process {
                            self.ctx.render_text(
                                vertices,
                                process,
                                tail_x,
                                text_y,
                                right_margin,
                                row_dim_fg,
                                no_bg,
                                body_scale,
                            );
                            tail_x += (process.chars().count() + 2) as f32 * scaled_cell_w;
                        }
                        if let Some(todo) = row.raf_todo {
                            let todo_fg = if row.bookmarked { row_fg } else { RAF_TODO_FG };
                            self.ctx.render_text(
                                vertices,
                                todo,
                                tail_x,
                                text_y,
                                right_margin,
                                todo_fg,
                                no_bg,
                                body_scale,
                            );
                            tail_x += todo.chars().count() as f32 * scaled_cell_w;
                            if row.raf_idea.is_some() {
                                self.ctx.render_text(
                                    vertices,
                                    RAF_SEPARATOR,
                                    tail_x,
                                    text_y,
                                    right_margin,
                                    row_dim_fg,
                                    no_bg,
                                    body_scale,
                                );
                                tail_x += RAF_SEPARATOR.chars().count() as f32 * scaled_cell_w;
                            }
                        }
                        if let Some(idea) = row.raf_idea {
                            self.ctx.render_text(
                                vertices,
                                idea,
                                tail_x,
                                text_y,
                                right_margin,
                                row_dim_fg,
                                no_bg,
                                body_scale,
                            );
                        }
                    }
                    if row.anchored {
                        // Anchor mark in the 4th char slot, right before the
                        // title: the three slots before it are taken by the
                        // minimized, working and attention markers.
                        let anchor_x = left_margin + 3.0 * scaled_cell_w;
                        self.ctx.render_text(
                            vertices,
                            "\u{2693}",
                            anchor_x,
                            text_y,
                            right_margin,
                            ANCHOR_FG,
                            no_bg,
                            body_scale,
                        );
                    }
                    if row.minimized {
                        // Minimized marker in the 1st char slot, in a color of
                        // its own so hidden panes stand out in the list.
                        self.ctx.render_text(
                            vertices,
                            "\u{229f}",
                            left_margin,
                            text_y,
                            right_margin,
                            MINIMIZED_FG,
                            no_bg,
                            body_scale,
                        );
                    }
                    if let Some(color) = attention.dot_color() {
                        // Dot occupies the 3rd char slot (after the "    " lead-in).
                        let dot_x = left_margin + 2.0 * scaled_cell_w;
                        self.ctx.render_text(
                            vertices,
                            "\u{25cf}",
                            dot_x,
                            text_y,
                            right_margin,
                            color,
                            no_bg,
                            body_scale,
                        );
                    }
                    // Claude Code state in the 2nd char slot: "✳" while the
                    // session works.
                    if row.working {
                        let state_x = left_margin + scaled_cell_w;
                        self.ctx.render_text(
                            vertices,
                            "\u{2733}",
                            state_x,
                            text_y,
                            right_margin,
                            WORKING_FG,
                            no_bg,
                            body_scale,
                        );
                    }
                }
            }

            // Per-column scroll indicators (centered in the column)
            let ind_x = col_x + col_w / 2.0;
            if scroll > 0 {
                self.ctx.render_text(
                    vertices,
                    "\u{25b2}",
                    ind_x,
                    content_top - scaled_cell_h,
                    right_margin,
                    dim_fg,
                    no_bg,
                    body_scale,
                );
            }
            if end < column.rows.len() {
                self.ctx.render_text(
                    vertices,
                    "\u{25bc}",
                    ind_x,
                    content_top + max_visible as f32 * row_height,
                    right_margin,
                    dim_fg,
                    no_bg,
                    body_scale,
                );
            }
        }
    }

    fn build_send_to_window_overlay_vertices(
        &mut self,
        vertices: &mut Vec<Vertex>,
        viewport_w: f32,
        viewport_h: f32,
        data: &SendToWindowRenderData<'_>,
    ) {
        let cell_w = self.ctx.atlas.cell_width;
        let cell_h = self.ctx.atlas.cell_height;

        // Semi-transparent dark overlay
        push_bg_quad_alpha(
            vertices,
            0.0,
            0.0,
            viewport_w,
            viewport_h,
            [0.0, 0.0, 0.0],
            0.9,
        );

        let no_bg = [0.0, 0.0, 0.0, 0.0];
        let title_fg = [1.0, 0.85, 0.3, 1.0];
        let label_fg = [0.85, 0.85, 0.9, 1.0];
        let dim_fg = [0.45, 0.45, 0.5, 1.0];
        let selected_bg = self.switcher_selected_bg;
        let new_window_fg = [0.5, 0.8, 0.5, 1.0];

        let title_scale = 1.8_f32;
        let body_scale = 1.3_f32;
        let scaled_cell_w = cell_w * body_scale;
        let scaled_cell_h = cell_h * body_scale;
        let row_height = scaled_cell_h * 1.6;

        // Title centered
        let title = data.title;
        let title_chars = title.chars().count() as f32;
        let title_x = (viewport_w - title_chars * cell_w * title_scale) / 2.0;
        let mut y = cell_h * 3.0;
        self.ctx.render_text(
            vertices,
            title,
            title_x,
            y,
            viewport_w,
            title_fg,
            no_bg,
            title_scale,
        );
        y += cell_h * title_scale * 2.0;

        // Subtitle
        let subtitle = "\u{2191}\u{2193} Navigate  \u{23ce} Send  esc Cancel";
        let sub_chars = subtitle.chars().count() as f32;
        let sub_x = (viewport_w - sub_chars * scaled_cell_w) / 2.0;
        self.ctx.render_text(
            vertices, subtitle, sub_x, y, viewport_w, dim_fg, no_bg, body_scale,
        );
        y += scaled_cell_h * 2.0;

        let left_margin = scaled_cell_w * 3.0;
        let right_margin = viewport_w - scaled_cell_w * 3.0;

        for (i, label) in data.entries.iter().enumerate() {
            let row_y = y + i as f32 * row_height;
            let text_y = row_y + (row_height - scaled_cell_h) / 2.0;

            // Selected row background
            if i == data.selected {
                push_bg_quad_alpha(
                    vertices,
                    left_margin - scaled_cell_w,
                    row_y,
                    right_margin - left_margin + scaled_cell_w * 2.0,
                    row_height,
                    selected_bg,
                    0.8,
                );
            }

            // "New Window" gets a distinct color (only if flagged)
            let is_new_window = data.has_new_entry && i == data.entries.len() - 1;
            let fg = if is_new_window {
                new_window_fg
            } else {
                label_fg
            };
            let prefix = if is_new_window { "+ " } else { "" };
            let text = format!("{}{}", prefix, label);
            self.ctx.render_text(
                vertices,
                &text,
                left_margin,
                text_y,
                right_margin,
                fg,
                no_bg,
                body_scale,
            );
        }
    }

    fn build_recent_projects_overlay_vertices(
        &mut self,
        vertices: &mut Vec<Vertex>,
        viewport_w: f32,
        viewport_h: f32,
        data: &RecentProjectsRenderData<'_>,
    ) {
        let cell_w = self.ctx.atlas.cell_width;
        let cell_h = self.ctx.atlas.cell_height;

        // Semi-transparent dark overlay
        push_bg_quad_alpha(
            vertices,
            0.0,
            0.0,
            viewport_w,
            viewport_h,
            [0.0, 0.0, 0.0],
            0.9,
        );

        let no_bg = [0.0, 0.0, 0.0, 0.0];
        let title_fg = [1.0, 0.85, 0.3, 1.0];
        let label_fg = [0.85, 0.85, 0.9, 1.0];
        let dim_fg = [0.45, 0.45, 0.5, 1.0];
        let time_fg = [0.5, 0.5, 0.55, 1.0];
        let selected_bg = self.switcher_selected_bg;
        let invalid_fg = [0.4, 0.4, 0.42, 1.0];

        let title_scale = 1.8_f32;
        let body_scale = 1.3_f32;
        let scaled_cell_w = cell_w * body_scale;
        let scaled_cell_h = cell_h * body_scale;
        let row_height = scaled_cell_h * 1.6;

        // Title centered
        let title = "Reopen Closed Tab";
        let title_chars = title.chars().count() as f32;
        let title_x = (viewport_w - title_chars * cell_w * title_scale) / 2.0;
        let mut y = cell_h * 3.0;
        self.ctx.render_text(
            vertices,
            title,
            title_x,
            y,
            viewport_w,
            title_fg,
            no_bg,
            title_scale,
        );
        y += cell_h * title_scale * 2.0;

        // Subtitle
        let subtitle = "↑↓ Navigate  ⏎ Open  ⌘⌫ Remove  esc Cancel";
        let sub_chars = subtitle.chars().count() as f32;
        let sub_x = (viewport_w - sub_chars * scaled_cell_w) / 2.0;
        self.ctx.render_text(
            vertices, subtitle, sub_x, y, viewport_w, dim_fg, no_bg, body_scale,
        );
        y += scaled_cell_h * 2.0;

        let left_margin = scaled_cell_w * 3.0;
        let right_margin = viewport_w - scaled_cell_w * 3.0;

        // Filter input: same box as the search palette's, focused on open.
        let input_h = row_height;
        push_bg_quad(
            vertices,
            left_margin - scaled_cell_w,
            y,
            right_margin - left_margin + scaled_cell_w * 2.0,
            input_h,
            [0.18, 0.18, 0.22],
        );
        let prompt = "› ";
        let input_text_y = y + (input_h - scaled_cell_h) / 2.0;
        self.ctx.render_text(
            vertices,
            prompt,
            left_margin,
            input_text_y,
            right_margin,
            dim_fg,
            no_bg,
            body_scale,
        );
        let prompt_w = prompt.chars().count() as f32 * scaled_cell_w;
        let visible_chars = ((right_margin - left_margin - prompt_w) / scaled_cell_w)
            .floor()
            .max(1.0) as usize;
        let input_scroll = input_scroll(data.cursor, visible_chars);
        if data.query.is_empty() {
            self.ctx.render_text(
                vertices,
                "Type to filter by name or path",
                left_margin + prompt_w,
                input_text_y,
                right_margin,
                dim_fg,
                no_bg,
                body_scale,
            );
        } else {
            let shown: String = data.query.chars().skip(input_scroll).collect();
            self.ctx.render_text(
                vertices,
                &shown,
                left_margin + prompt_w,
                input_text_y,
                right_margin,
                label_fg,
                no_bg,
                body_scale,
            );
        }
        let caret_x =
            left_margin + prompt_w + ((data.cursor - input_scroll) as f32) * scaled_cell_w;
        push_bg_quad(
            vertices,
            caret_x,
            input_text_y,
            1.5,
            scaled_cell_h,
            [1.0, 1.0, 1.0],
        );
        y += input_h + scaled_cell_h;

        let content_top = y;
        let content_bottom = viewport_h - cell_h * 2.0;
        let max_visible = ((content_bottom - content_top) / row_height) as usize;

        // Compute scroll to keep selected visible
        let scroll = if data.selected >= data.scroll + max_visible {
            data.selected - max_visible + 1
        } else {
            data.scroll
        };

        if data.entries.is_empty() {
            let msg = if data.total == 0 {
                "No closed tabs to reopen"
            } else {
                "No closed tab matches"
            };
            let msg_w = msg.chars().count() as f32 * scaled_cell_w;
            let msg_x = (viewport_w - msg_w) / 2.0;
            self.ctx.render_text(
                vertices,
                msg,
                msg_x,
                content_top + row_height,
                viewport_w,
                dim_fg,
                no_bg,
                body_scale,
            );
            return;
        }

        for (i, entry) in data
            .entries
            .iter()
            .enumerate()
            .skip(scroll)
            .take(max_visible)
        {
            let row_y = content_top + (i - scroll) as f32 * row_height;
            let text_y = row_y + (row_height - scaled_cell_h) / 2.0;

            // Row background: the tab's own color, dimmed like an inactive tab
            // in the bar and at full hue when selected — the same contrast rule
            // as the tab bar. An uncolored row only gets the selection color.
            let is_selected = i == data.selected;
            let row_x = left_margin - scaled_cell_w;
            let row_w = right_margin - left_margin + scaled_cell_w * 2.0;
            let tab_color = entry.color.map(|idx| TAB_COLORS[idx % TAB_COLORS.len()]);
            match tab_color {
                Some(c) => {
                    let bg = if is_selected { c } else { dim_inactive_tab(c) };
                    push_bg_quad(vertices, row_x, row_y, row_w, row_height, bg);
                }
                None if is_selected => {
                    push_bg_quad_alpha(vertices, row_x, row_y, row_w, row_height, selected_bg, 0.8);
                }
                None => {}
            }
            // Selection marker: on a colored list the background alone does not
            // say which row is picked (a dimmed blue reads like the selection).
            if is_selected {
                push_bg_quad(
                    vertices,
                    row_x,
                    row_y,
                    scaled_cell_w * 0.3,
                    row_height,
                    [1.0, 1.0, 1.0],
                );
            }

            // Text on a colored row follows the tab bar: white when selected,
            // dimmed by the background's brightness otherwise.
            let (fg, dim_fg, time_fg) = match (tab_color, entry.invalid) {
                (_, true) => (invalid_fg, dim_fg, time_fg),
                (Some(_), false) => {
                    let v = if is_selected { 1.0 } else { DIM_BRIGHTNESS };
                    let sub = [v * 0.8, v * 0.8, v * 0.8, 1.0];
                    ([v, v, v, 1.0], sub, sub)
                }
                (None, false) => (label_fg, dim_fg, time_fg),
            };

            // Tab name, then its directory dimmed; an unnamed tab shows the
            // directory alone.
            let text_max_x = right_margin - scaled_cell_w * 12.0;
            match entry.title {
                Some(ref title) => {
                    self.ctx.render_text(
                        vertices,
                        title,
                        left_margin,
                        text_y,
                        text_max_x,
                        fg,
                        no_bg,
                        body_scale,
                    );
                    let path_x = left_margin + (title.chars().count() as f32 + 3.0) * scaled_cell_w;
                    self.ctx.render_text(
                        vertices,
                        &entry.path,
                        path_x,
                        text_y,
                        text_max_x,
                        dim_fg,
                        no_bg,
                        body_scale,
                    );
                }
                None => {
                    self.ctx.render_text(
                        vertices,
                        &entry.path,
                        left_margin,
                        text_y,
                        text_max_x,
                        fg,
                        no_bg,
                        body_scale,
                    );
                }
            }

            // Pane count (if > 1)
            let info = if entry.pane_count > 1 {
                format!("{}p  {}", entry.pane_count, entry.time_ago)
            } else {
                entry.time_ago.clone()
            };
            let info_w = info.chars().count() as f32 * scaled_cell_w;
            let info_x = right_margin - info_w;
            self.ctx.render_text(
                vertices,
                &info,
                info_x,
                text_y,
                right_margin,
                time_fg,
                no_bg,
                body_scale,
            );
        }

        // Scroll indicators
        if scroll > 0 {
            let arrow = "▲";
            let ax = (viewport_w - scaled_cell_w) / 2.0;
            self.ctx.render_text(
                vertices,
                arrow,
                ax,
                content_top - scaled_cell_h,
                viewport_w,
                dim_fg,
                no_bg,
                body_scale,
            );
        }
        if scroll + max_visible < data.entries.len() {
            let arrow = "▼";
            let ax = (viewport_w - scaled_cell_w) / 2.0;
            self.ctx.render_text(
                vertices,
                arrow,
                ax,
                content_bottom,
                viewport_w,
                dim_fg,
                no_bg,
                body_scale,
            );
        }
    }

    /// Vertical geometry of the search palette's result list, shared by the
    /// renderer and the mouse hit-testing so a click lands on the row drawn.
    pub fn search_palette_list_geometry(&self, viewport_h: f32) -> OverlayListGeometry {
        let cell_h = self.ctx.atlas.cell_height;
        let scaled_cell_h = cell_h * 1.3;
        let row_height = scaled_cell_h * 1.6;
        // Title, subtitle, input box, status line: see the build function.
        let content_top = cell_h * 3.0
            + cell_h * 1.8 * 1.5
            + scaled_cell_h * 2.0
            + row_height
            + scaled_cell_h * 0.75
            + scaled_cell_h * 1.5;
        let content_bottom = viewport_h - cell_h * 2.0;
        let max_visible = ((content_bottom - content_top) / row_height).max(0.0) as usize;
        OverlayListGeometry {
            content_top,
            row_height,
            max_visible,
        }
    }

    fn build_search_palette_overlay_vertices(
        &mut self,
        vertices: &mut Vec<Vertex>,
        viewport_w: f32,
        viewport_h: f32,
        data: &SearchPaletteRenderData<'_>,
    ) {
        let cell_w = self.ctx.atlas.cell_width;
        let cell_h = self.ctx.atlas.cell_height;

        // Dim the world behind the palette.
        push_bg_quad_alpha(
            vertices,
            0.0,
            0.0,
            viewport_w,
            viewport_h,
            [0.0, 0.0, 0.0],
            0.85,
        );

        let no_bg = [0.0, 0.0, 0.0, 0.0];
        let title_fg = [1.0, 0.85, 0.3, 1.0];
        let label_fg = [0.85, 0.85, 0.9, 1.0];
        let dim_fg = [0.55, 0.55, 0.6, 1.0];
        let input_bg = [0.18, 0.18, 0.22];
        let selected_bg = self.switcher_selected_bg;
        let caret_fg = [1.0, 1.0, 1.0];

        let title_scale = 1.8_f32;
        let body_scale = 1.3_f32;
        let scaled_cell_w = cell_w * body_scale;
        let scaled_cell_h = cell_h * body_scale;
        let row_height = scaled_cell_h * 1.6;

        // Title
        let title = "Search";
        let title_chars = title.chars().count() as f32;
        let title_x = (viewport_w - title_chars * cell_w * title_scale) / 2.0;
        let mut y = cell_h * 3.0;
        self.ctx.render_text(
            vertices,
            title,
            title_x,
            y,
            viewport_w,
            title_fg,
            no_bg,
            title_scale,
        );
        y += cell_h * title_scale * 1.5;

        // Subtitle
        let subtitle = "↑↓ Navigate  ⏎ Open  esc Cancel";
        let sub_chars = subtitle.chars().count() as f32;
        let sub_x = (viewport_w - sub_chars * scaled_cell_w) / 2.0;
        self.ctx.render_text(
            vertices, subtitle, sub_x, y, viewport_w, dim_fg, no_bg, body_scale,
        );
        y += scaled_cell_h * 2.0;

        // Input box: a single full-width row with the query and caret.
        let left_margin = scaled_cell_w * 3.0;
        let right_margin = viewport_w - scaled_cell_w * 3.0;
        let input_h = row_height;
        push_bg_quad(
            vertices,
            left_margin - scaled_cell_w,
            y,
            right_margin - left_margin + scaled_cell_w * 2.0,
            input_h,
            input_bg,
        );

        let prompt = "› ";
        let text_y = y + (input_h - scaled_cell_h) / 2.0;
        self.ctx.render_text(
            vertices,
            prompt,
            left_margin,
            text_y,
            right_margin,
            dim_fg,
            no_bg,
            body_scale,
        );
        let prompt_w = prompt.chars().count() as f32 * scaled_cell_w;
        // A query wider than the box scrolls so the caret stays in view.
        let visible_chars = ((right_margin - left_margin - prompt_w) / scaled_cell_w)
            .floor()
            .max(1.0) as usize;
        let scroll = input_scroll(data.cursor, visible_chars);
        let shown: String = data.query.chars().skip(scroll).collect();
        self.ctx.render_text(
            vertices,
            &shown,
            left_margin + prompt_w,
            text_y,
            right_margin,
            label_fg,
            no_bg,
            body_scale,
        );

        // Caret as a thin vertical bar at the cursor position.
        let caret_x = left_margin + prompt_w + ((data.cursor - scroll) as f32) * scaled_cell_w;
        push_bg_quad(vertices, caret_x, text_y, 1.5, scaled_cell_h, caret_fg);

        y += input_h + scaled_cell_h * 0.75;

        // Status line: searching / "N results" / hint when query empty.
        let hit_count = data.rows.iter().filter(|r| !r.is_header).count();
        let status = if data.searching {
            format!("Searching for \"{}\"...", data.submitted_query)
        } else if data.submitted_query.is_empty() {
            "Type to search across all panes and closed sessions.".to_string()
        } else if hit_count == 0 {
            format!("No matches for \"{}\".", data.submitted_query)
        } else {
            format!(
                "{} match{} for \"{}\"",
                hit_count,
                if hit_count == 1 { "" } else { "es" },
                data.submitted_query
            )
        };
        self.ctx.render_text(
            vertices,
            &status,
            left_margin,
            y,
            right_margin,
            dim_fg,
            no_bg,
            body_scale,
        );
        y += scaled_cell_h * 1.5;

        if data.rows.is_empty() {
            return;
        }

        let geom = self.search_palette_list_geometry(viewport_h);
        debug_assert!(
            (geom.content_top - y).abs() < 0.01,
            "geometry out of sync with the layout"
        );
        let content_top = geom.content_top;
        let content_bottom = viewport_h - cell_h * 2.0;
        let max_visible = geom.max_visible;
        if max_visible == 0 {
            return;
        }

        let scroll = search_palette_scroll(data.selected, data.scroll, max_visible);

        let header_fg = [1.0, 0.85, 0.3, 1.0];
        let hit_indent = scaled_cell_w * 2.0;
        for (i, row) in data.rows.iter().enumerate().skip(scroll).take(max_visible) {
            let row_y = content_top + (i - scroll) as f32 * row_height;
            let text_y = row_y + (row_height - scaled_cell_h) / 2.0;

            if row.is_header {
                // Group header: tab name (section 1) or the "Tabs" divider.
                self.ctx.render_text(
                    vertices,
                    row.text,
                    left_margin,
                    text_y,
                    right_margin,
                    header_fg,
                    no_bg,
                    body_scale,
                );
            } else {
                if i == data.selected {
                    push_bg_quad_alpha(
                        vertices,
                        left_margin - scaled_cell_w,
                        row_y,
                        right_margin - left_margin + scaled_cell_w * 2.0,
                        row_height,
                        selected_bg,
                        0.8,
                    );
                }
                self.ctx.render_text(
                    vertices,
                    row.text,
                    left_margin + hit_indent,
                    text_y,
                    right_margin,
                    label_fg,
                    no_bg,
                    body_scale,
                );
            }
        }

        if scroll > 0 {
            let arrow = "▲";
            let ax = (viewport_w - scaled_cell_w) / 2.0;
            self.ctx.render_text(
                vertices,
                arrow,
                ax,
                content_top - scaled_cell_h,
                viewport_w,
                dim_fg,
                no_bg,
                body_scale,
            );
        }
        if scroll + max_visible < data.rows.len() {
            let arrow = "▼";
            let ax = (viewport_w - scaled_cell_w) / 2.0;
            self.ctx.render_text(
                vertices,
                arrow,
                ax,
                content_bottom,
                viewport_w,
                dim_fg,
                no_bg,
                body_scale,
            );
        }
    }

    pub fn cell_size(&self) -> (f32, f32) {
        self.ctx.cell_size()
    }

    pub fn set_mem_report(&mut self, report: Vec<String>) {
        self.cached_mem_report = report;
    }

    pub fn status_bar_enabled(&self) -> bool {
        self.style.status_bar_enabled
    }

    /// Hit-test mouse position against tooltip zones. Returns an ActiveTooltip if hovering.
    pub fn hit_test_tooltip(&self, px: f32, py: f32) -> Option<ActiveTooltip> {
        self.tooltip_zones
            .iter()
            .find(|z| z.contains(px, py))
            .map(|z| ActiveTooltip {
                text: z.text,
                anchor_x: z.x + z.width / 2.0,
                anchor_y: z.y,
            })
    }

    fn push_tooltip_zone(&mut self, x: f32, y: f32, width: f32, height: f32, text: &'static str) {
        self.tooltip_zones.push(TooltipZone {
            x,
            y,
            width,
            height,
            text,
        });
    }

    fn build_tooltip_vertices(&mut self, vertices: &mut Vec<Vertex>, viewport_w: f32) {
        let tt = match &self.tooltip_visible {
            Some(t) => t,
            None => return,
        };
        // Smoothstep ease-in/ease-out: t²(3 - 2t)
        let t = self.tooltip_anim as f32 / TOOLTIP_ANIM_FRAMES as f32;
        let alpha = t * t * (3.0 - 2.0 * t);

        let cell_w = self.ctx.atlas.cell_width;
        let cell_h = self.ctx.atlas.cell_height;
        let padding_x = cell_w;
        let padding_y = cell_h * 0.25;
        let text_w = tt.text.chars().count() as f32 * cell_w;
        let box_w = text_w + padding_x * 2.0;
        let box_h = cell_h + padding_y * 2.0;

        // Center tooltip horizontally on anchor, clamp to viewport
        let box_x = (tt.anchor_x - box_w / 2.0).clamp(0.0, (viewport_w - box_w).max(0.0));

        // Position above the anchor, with slight slide-up during animation
        let slide = (1.0 - alpha) * cell_h * 0.3;
        let box_y = tt.anchor_y - box_h - 2.0 + slide;

        let bg = [0.15, 0.15, 0.18];
        push_bg_quad_alpha(vertices, box_x, box_y, box_w, box_h, bg, 0.95 * alpha);

        let text_str = tt.text;
        let text_x = box_x + padding_x;
        let text_y = box_y + padding_y;
        let fg = [0.85, 0.85, 0.85, alpha];
        let no_bg = [0.0, 0.0, 0.0, 0.0];
        self.ctx.render_status_text(
            vertices,
            text_str,
            text_x,
            text_y,
            text_x + text_w + cell_w,
            fg,
            no_bg,
        );
    }

    /// Draw the big directory label of a pane flash: the directory name, the
    /// path above it, the name of the agent conversation living in the pane
    /// (empty when there is none), and a padded backdrop so the terminal
    /// content underneath does not fight the text. Every line fades with
    /// `alpha`.
    fn build_flash_label_vertices(
        &mut self,
        vertices: &mut Vec<Vertex>,
        pane: (f32, f32, f32, f32),
        alpha: f32,
        name: &str,
        parent: &str,
        session: &str,
    ) {
        let cell_w = self.ctx.atlas.overlay_cell_width;
        let cell_h = self.ctx.atlas.overlay_cell_height;
        let layout = flash_label_layout(
            pane,
            name.chars().count(),
            parent.chars().count(),
            session.chars().count(),
            cell_w,
            cell_h,
        );

        let no_bg = [0.0, 0.0, 0.0, 0.0];
        push_bg_quad_alpha(
            vertices,
            layout.box_x,
            layout.box_y,
            layout.box_w,
            layout.box_h,
            self.style.bg_color,
            alpha * 0.92,
        );
        let name_fg = [1.0, 0.85, 0.3, alpha];
        let parent_fg = [0.75, 0.75, 0.75, alpha * 0.8];
        let right = pane.0 + pane.2;
        self.ctx.render_overlay_text(
            vertices,
            name,
            layout.name_x,
            layout.name_y,
            right,
            name_fg,
            no_bg,
            layout.name_scale,
        );
        if !parent.is_empty() {
            self.ctx.render_overlay_text(
                vertices,
                parent,
                layout.parent_x,
                layout.parent_y,
                right,
                parent_fg,
                no_bg,
                layout.parent_scale,
            );
        }
        if !session.is_empty() {
            // Blue, so the conversation name never reads as part of the path.
            let session_fg = [0.55, 0.78, 1.0, alpha];
            self.ctx.render_overlay_text(
                vertices,
                session,
                layout.session_x,
                layout.session_y,
                right,
                session_fg,
                no_bg,
                layout.session_scale,
            );
        }
    }
}

/// The colour of an OSC 7501 badge, by the hue key `program_status::badge`
/// returns: working, waiting for the user, finished, failed. Picked to stay
/// legible on the status bar over the default dark background.
pub(super) fn program_status_color(hue: u8) -> [f32; 3] {
    match hue {
        1 => [1.00, 0.72, 0.28],
        2 => [0.45, 0.85, 0.55],
        3 => [1.00, 0.42, 0.42],
        _ => [0.45, 0.72, 1.00],
    }
}

/// Fade a colour toward `bg`. `t` is the dim amount: 0.0 leaves the colour
/// alone, 1.0 makes it the background. Used by `text` dim mode, which fades
/// glyphs instead of laying a veil over the pane.
pub(super) fn fade_toward(c: [f32; 3], bg: [f32; 3], t: f32) -> [f32; 3] {
    if t <= 0.0 {
        return c;
    }
    let t = t.min(1.0);
    [
        c[0] + (bg[0] - c[0]) * t,
        c[1] + (bg[1] - c[1]) * t,
        c[2] + (bg[2] - c[2]) * t,
    ]
}

/// The Ctrl+F filter overlay over the pane at `vp`: a dark veil, the query
/// bar with the match count (or the recall hint), then one row per match,
/// the query highlighted and the picked one banded. Kova.app draws it over its
/// focused pane, the library over its pane alone (`layer::LayerRenderer`).
pub(super) fn build_filter_overlay(
    ctx: &mut RenderContext,
    vertices: &mut Vec<Vertex>,
    vp: &PaneViewport,
    filter: &FilterRenderData,
    selected_bg: [f32; 3],
) {
    let cell_w = ctx.atlas.cell_width;
    let cell_h = ctx.atlas.cell_height;

    // 1. Semi-transparent dark overlay covering the entire pane
    push_bg_quad_alpha(
        vertices,
        vp.x,
        vp.y,
        vp.width,
        vp.height,
        [0.0, 0.0, 0.0],
        0.85,
    );

    let no_bg = [0.0, 0.0, 0.0, 0.0];

    // 2. Search bar background
    let bar_bg = [0.2, 0.2, 0.25];
    push_bg_quad(vertices, vp.x, vp.y, vp.width, cell_h, bar_bg);

    // 3. Search bar text: "/ query▏"
    let bar_text = format!("/ {}▏", &filter.query);
    let bar_fg = [1.0, 0.8, 0.2, 1.0]; // accent yellow
    ctx.render_status_text(
        vertices,
        &bar_text,
        vp.x + ctx.h_padding(),
        vp.y,
        vp.x + vp.width - cell_w,
        bar_fg,
        no_bg,
    );

    // Match count — or, on an empty query, how to recall an earlier one.
    let count_text = match &filter.hint {
        Some(h) => h.clone(),
        None => format!("{} matches", filter.matches.len()),
    };
    let count_fg = [0.6, 0.6, 0.6, 1.0];
    let count_w = count_text.chars().count() as f32 * cell_w;
    ctx.render_status_text(
        vertices,
        &count_text,
        vp.x + vp.width - count_w - ctx.h_padding(),
        vp.y,
        vp.x + vp.width,
        count_fg,
        no_bg,
    );

    // 4. List matched lines — truncate text to visible columns to limit vertices
    let max_visible = ((vp.height / cell_h).floor() as usize).saturating_sub(1);
    let scroll = filter_list_scroll(filter.selected, max_visible);
    let match_fg = [0.85, 0.85, 0.85, 1.0];
    let highlight_fg = [1.0, 0.8, 0.2, 1.0];
    let query_lower = filter.query.to_lowercase();
    let max_chars = ((vp.width - 2.0 * ctx.h_padding()) / cell_w) as usize;

    for (i, m) in filter
        .matches
        .iter()
        .enumerate()
        .skip(scroll)
        .take(max_visible)
    {
        let y = vp.y + (i - scroll + 1) as f32 * cell_h;
        let max_x = vp.x + vp.width - ctx.h_padding();

        // The row ↑/↓ sits on, banded as the Cmd+P list bands its own: without
        // it the list never says what Enter would jump to.
        if i == filter.selected {
            push_bg_quad_alpha(vertices, vp.x, y, vp.width, cell_h, selected_bg, 0.8);
        }

        // Line number prefix
        let prefix = format!("{:>6}: ", m.abs_line);
        let prefix_fg = [0.5, 0.5, 0.5, 1.0];
        let after_prefix = ctx.render_status_text(
            vertices,
            &prefix,
            vp.x + ctx.h_padding(),
            y,
            max_x,
            prefix_fg,
            no_bg,
        );

        // Truncate line text to what fits on screen
        let prefix_chars = prefix.chars().count();
        let text_limit = max_chars.saturating_sub(prefix_chars);
        let display_text: String = m.text.chars().take(text_limit).collect();

        if query_lower.is_empty() {
            ctx.render_status_text(
                vertices,
                &display_text,
                after_prefix,
                y,
                max_x,
                match_fg,
                no_bg,
            );
        } else {
            // Split text into spans: alternating normal/highlighted.
            // to_lowercase() is not byte-length-preserving ('İ' is 2
            // bytes, lowercases to 3), so byte offsets found in the
            // lowercased copy can't slice display_text directly — build
            // a map from each text_lower byte back to the byte offset of
            // its source char in display_text.
            let mut text_lower = String::with_capacity(display_text.len());
            let mut lower_to_orig: Vec<usize> = Vec::with_capacity(display_text.len() + 1);
            for (orig_idx, ch) in display_text.char_indices() {
                for lc in ch.to_lowercase() {
                    text_lower.push(lc);
                    lower_to_orig.resize(text_lower.len(), orig_idx);
                }
            }
            lower_to_orig.push(display_text.len());

            let mut spans: Vec<(&str, bool)> = Vec::new();
            let mut pos = 0; // byte position in text_lower
            while pos < text_lower.len() {
                if let Some(found) = text_lower[pos..].find(&query_lower) {
                    let m_start = pos + found;
                    let m_end = m_start + query_lower.len();
                    let o_pos = lower_to_orig[pos];
                    let o_start = lower_to_orig[m_start];
                    let o_end = lower_to_orig[m_end];
                    if o_start > o_pos {
                        spans.push((&display_text[o_pos..o_start], false));
                    }
                    spans.push((&display_text[o_start..o_end], true));
                    pos = m_end;
                } else {
                    spans.push((&display_text[lower_to_orig[pos]..], false));
                    break;
                }
            }

            let mut x = after_prefix;
            for (span, is_hl) in spans {
                let fg = if is_hl { highlight_fg } else { match_fg };
                x = ctx.render_status_text(vertices, span, x, y, max_x, fg, no_bg);
            }
        }
    }
}

pub(super) fn push_bg_quad(
    vertices: &mut Vec<Vertex>,
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    bg: [f32; 3],
) {
    push_bg_quad_alpha(vertices, x, y, w, h, bg, 1.0);
}

pub(super) fn push_bg_quad_alpha(
    vertices: &mut Vec<Vertex>,
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    bg: [f32; 3],
    alpha: f32,
) {
    let bg4 = [bg[0], bg[1], bg[2], alpha];
    let no_tex = [0.0, 0.0];
    let white = [1.0, 1.0, 1.0, 0.0];

    vertices.push(Vertex {
        position: [x, y],
        tex_coords: no_tex,
        color: white,
        bg_color: bg4,
    });
    vertices.push(Vertex {
        position: [x + w, y],
        tex_coords: no_tex,
        color: white,
        bg_color: bg4,
    });
    vertices.push(Vertex {
        position: [x, y + h],
        tex_coords: no_tex,
        color: white,
        bg_color: bg4,
    });
    vertices.push(Vertex {
        position: [x + w, y],
        tex_coords: no_tex,
        color: white,
        bg_color: bg4,
    });
    vertices.push(Vertex {
        position: [x + w, y + h],
        tex_coords: no_tex,
        color: white,
        bg_color: bg4,
    });
    vertices.push(Vertex {
        position: [x, y + h],
        tex_coords: no_tex,
        color: white,
        bg_color: bg4,
    });
}

/// One row of the help overlay: either a section header or a shortcut entry.
enum HelpRow {
    Header(String),
    Item {
        label: String,
        key: String,
        desc: String,
    },
}

/// Help overlay spacing, in overlay cell heights: the step from one row to the
/// next, and the extra gap above each section header.
const HELP_ROW_STEP: f32 = 1.4;
const HELP_HEADER_GAP: f32 = 0.5;

/// Factor applied to the help overlay spacing so a column of `headers` +
/// `items` rows fits in `avail` pixels: 1.0 when it already fits, less when it
/// would run past the status bar, never so tight that rows touch — past that
/// point the overflow is still cut.
fn help_row_scale(headers: usize, items: usize, avail: f32, och: f32) -> f32 {
    let rows = headers + items;
    if rows < 2 {
        return 1.0;
    }
    // From the first row's top to the last row's bottom: a gap per header,
    // a step per row but the last, and the last row's own height.
    let spacing = headers as f32 * HELP_HEADER_GAP + (rows - 1) as f32 * HELP_ROW_STEP;
    // One pixel of slack keeps f32 rounding from cutting the last row at an
    // exact fit.
    let scale = (avail - och - 1.0) / (spacing * och);
    scale.clamp(1.0 / HELP_ROW_STEP, 1.0)
}

/// Format a key combo string like "cmd+shift+d" into "⌘⇧D" for display.
/// Like `format_key_combo` but replaces a trailing arrow direction with "Arrows".
fn format_key_combo_arrows(s: &str) -> String {
    let parts: Vec<&str> = s.split('+').collect();
    if let Some(last) = parts.last() {
        match last.trim().to_ascii_lowercase().as_str() {
            "up" | "down" | "left" | "right" => {
                let prefix: Vec<&str> = parts[..parts.len() - 1].to_vec();
                let rebuilt = if prefix.is_empty() {
                    "arrows".to_string()
                } else {
                    format!("{}+arrows", prefix.join("+"))
                };
                return format_key_combo(&rebuilt);
            }
            _ => {}
        }
    }
    format_key_combo(s)
}

fn format_key_combo(s: &str) -> String {
    let mut result = String::new();
    let parts: Vec<&str> = s.split('+').collect();
    // Same split rule as the keybinding parser: a trailing '+' means the key
    // itself is '+' (e.g. "cmd+shift++" → modifiers=[cmd,shift], key='+').
    let (modifier_parts, key_str) = if parts.last() == Some(&"") && parts.len() >= 2 {
        (&parts[..parts.len() - 1], "+")
    } else {
        (&parts[..parts.len() - 1], parts[parts.len() - 1])
    };
    for part in modifier_parts {
        let trimmed = part.trim();
        if trimmed.is_empty() {
            // Artifact of splitting a literal '+' key on '+' — not a modifier.
            continue;
        }
        match trimmed.to_ascii_lowercase().as_str() {
            "cmd" | "command" => result.push('\u{2318}'),
            "ctrl" | "control" => result.push('\u{2303}'),
            "option" | "alt" | "opt" => result.push('\u{2325}'),
            "shift" => result.push('\u{21E7}'),
            _ => {
                result.push_str(trimmed);
            }
        }
    }
    let key_trimmed = key_str.trim();
    match key_trimmed.to_ascii_lowercase().as_str() {
        "up" => result.push('\u{2191}'),
        "down" => result.push('\u{2193}'),
        "left" => result.push('\u{2190}'),
        "right" => result.push('\u{2192}'),
        "backspace" | "delete" => result.push('\u{232B}'),
        "enter" | "return" => result.push('\u{21A9}'),
        "/" => result.push('/'),
        "[" => result.push('['),
        "]" => result.push(']'),
        "+" => result.push('+'),
        k => result.push_str(&k.to_ascii_uppercase()),
    }
    result
}

#[cfg(test)]
mod tests {
    #[test]
    fn viewport_off_screen_only_when_no_column_is_visible() {
        let vp = |x: f32, width: f32| super::PaneViewport {
            x,
            y: 0.0,
            width,
            height: 100.0,
        };
        assert!(
            vp(-500.0, 500.0).off_screen(1000.0),
            "ends exactly at the left edge"
        );
        assert!(
            !vp(-499.0, 500.0).off_screen(1000.0),
            "one pixel still visible on the left"
        );
        assert!(
            vp(1000.0, 500.0).off_screen(1000.0),
            "starts exactly at the right edge"
        );
        assert!(
            !vp(999.0, 500.0).off_screen(1000.0),
            "one pixel still visible on the right"
        );
        assert!(
            !vp(-100.0, 2000.0).off_screen(1000.0),
            "wider than the screen"
        );
    }

    fn draw_key() -> PaneDrawKey {
        PaneDrawKey {
            vp: [0.0, 0.0, 800.0, 600.0],
            is_focused: false,
            dim_unfocused: true,
            outlined: false,
            show_blink: true,
            hover: None,
            custom_title: None,
            has_bell: false,
            has_completion: false,
            input_chars: 0,
            printable_chars: 0,
            last_activity: 0,
            fg_process: None,
            bookmarked: false,
            atlas_generation: 0,
        }
    }

    fn bg_line(bgs: &[[u8; 3]]) -> Vec<Cell> {
        bgs.iter()
            .map(|&bg| Cell {
                bg,
                ..Cell::default()
            })
            .collect()
    }

    #[test]
    fn bg_runs_merge_neighbours_and_skip_the_default() {
        let d = [0, 0, 0];
        let (r, g) = ([255, 0, 0], [0, 255, 0]);
        let line = bg_line(&[d, r, r, r, g, d, d, r]);
        assert_eq!(bg_runs(&line, d), vec![(1, 4, r), (4, 5, g), (7, 8, r)]);
    }

    #[test]
    fn bg_runs_of_a_default_line_is_empty() {
        let d = [1, 2, 3];
        assert!(bg_runs(&bg_line(&[d, d, d]), d).is_empty());
    }

    #[test]
    fn a_clean_unchanged_pane_is_drawn_from_its_cache() {
        let k = draw_key();
        assert!(can_reuse_pane_cache(
            Some((&k, false, true)),
            &k,
            true,
            false,
            false
        ));
    }

    #[test]
    fn a_dirty_pane_is_rebuilt() {
        let k = draw_key();
        assert!(!can_reuse_pane_cache(
            Some((&k, false, true)),
            &k,
            true,
            false,
            true
        ));
    }

    #[test]
    fn a_pane_with_no_cache_is_built() {
        assert!(!can_reuse_pane_cache(None, &draw_key(), true, false, false));
    }

    #[test]
    fn anything_shown_outside_the_grid_forces_a_rebuild() {
        // Each of these changes without the pane being marked dirty.
        let cached = draw_key();
        let changes: Vec<fn(&mut PaneDrawKey)> = vec![
            |k| k.show_blink = false,
            |k| k.is_focused = true,
            |k| k.hover = Some((vec![(0, 1, 5)], Some("https://x".into()))),
            |k| k.custom_title = Some("t".into()),
            |k| k.has_bell = true,
            |k| k.has_completion = true,
            |k| k.input_chars = 1,
            |k| k.printable_chars = 1,
            |k| k.last_activity = 1,
            |k| k.fg_process = Some("claude".into()),
            |k| k.bookmarked = true,
            |k| k.atlas_generation = 1,
            |k| k.vp[2] = 801.0,
        ];
        for change in changes {
            let mut now = cached.clone();
            change(&mut now);
            assert!(!can_reuse_pane_cache(
                Some((&cached, false, true)),
                &now,
                true,
                false,
                false
            ));
        }
    }

    #[test]
    fn inside_a_sync_burst_only_the_viewport_matters() {
        let cached = draw_key();
        let mut now = cached.clone();
        now.input_chars = 42;
        assert!(can_reuse_pane_cache(
            Some((&cached, false, true)),
            &now,
            true,
            true,
            true
        ));
        now.vp[3] = 500.0;
        assert!(!can_reuse_pane_cache(
            Some((&cached, false, true)),
            &now,
            true,
            true,
            true
        ));
    }

    #[test]
    fn a_torn_or_placeholder_build_is_never_reused() {
        let k = draw_key();
        assert!(!can_reuse_pane_cache(
            Some((&k, true, true)),
            &k,
            true,
            false,
            false
        ));
        assert!(!can_reuse_pane_cache(
            Some((&k, false, false)),
            &k,
            true,
            false,
            false
        ));
        assert!(!can_reuse_pane_cache(
            Some((&k, false, true)),
            &k,
            false,
            false,
            false
        ));
    }

    #[test]
    fn banner_title_sits_flush_right_unless_it_would_hit_the_label() {
        use super::banner_title_x;
        // Room to spare: ends exactly on the right edge.
        assert_eq!(banner_title_x(5, 100.0, 500.0, 10.0), 450.0);
        // Too long: starts two cells after the label, clipped later.
        assert_eq!(banner_title_x(50, 100.0, 500.0, 10.0), 120.0);
    }

    #[test]
    fn input_scroll_keeps_the_caret_in_the_box() {
        use super::input_scroll;
        assert_eq!(input_scroll(0, 10), 0);
        assert_eq!(input_scroll(9, 10), 0); // caret on the last column
        assert_eq!(input_scroll(10, 10), 1);
        assert_eq!(input_scroll(150, 10), 141);
        assert_eq!(input_scroll(3, 0), 3);
    }

    #[test]
    fn the_hint_line_measures_what_it_draws() {
        // "\u{23ce} Focus  esc Cancel" — 7 + 2 + 10, no trailing gap.
        assert_eq!(
            super::hint_line_width(&[("\u{23ce}", "Focus"), ("esc", "Cancel")]),
            19
        );
        assert_eq!(super::hint_line_width(&[]), 0);
    }
    use super::*;

    /// Bottom of the last row of a help column laid out at `scale`, walking
    /// the rows the way `build_help_overlay_vertices` does.
    fn help_column_bottom(headers: usize, items: usize, och: f32, scale: f32) -> f32 {
        let rows = headers + items;
        headers as f32 * HELP_HEADER_GAP * och * scale
            + (rows - 1) as f32 * HELP_ROW_STEP * och * scale
            + och
    }

    #[test]
    fn help_spacing_is_untouched_when_the_column_fits() {
        assert_eq!(help_row_scale(4, 24, 5000.0, 40.0), 1.0);
    }

    #[test]
    fn help_spacing_tightens_until_the_column_fits() {
        let (headers, items, och, avail) = (4, 24, 40.0, 1500.0);
        let scale = help_row_scale(headers, items, avail, och);
        assert!(scale < 1.0);
        assert!(help_column_bottom(headers, items, och, scale) <= avail);
    }

    #[test]
    fn help_rows_never_overlap_however_short_the_window() {
        let scale = help_row_scale(4, 24, 200.0, 40.0);
        assert!(HELP_ROW_STEP * scale >= 1.0 - f32::EPSILON);
    }

    #[test]
    fn text_dim_fades_toward_the_background_and_never_past_it() {
        let bg = [0.1, 0.1, 0.12];
        let fg = [1.0, 0.5, 0.0];
        assert_eq!(fade_toward(fg, bg, 0.0), fg);
        assert_eq!(
            fade_toward(fg, bg, -1.0),
            fg,
            "a negative amount is a no-op"
        );
        let close = |a: [f32; 3], b: [f32; 3]| (0..3).all(|i| (a[i] - b[i]).abs() < 1e-6);
        assert!(close(fade_toward(fg, bg, 1.0), bg));
        assert!(
            close(fade_toward(fg, bg, 2.0), bg),
            "clamped, never past the background"
        );
        let half = fade_toward(fg, bg, 0.5);
        for i in 0..3 {
            assert!((half[i] - (fg[i] + bg[i]) * 0.5).abs() < 1e-6);
        }
    }

    #[test]
    fn a_hollow_cursor_leaves_the_cell_centre_untouched() {
        // The unfocused-pane cursor is four edge quads; the middle of the cell
        // must stay clear, otherwise it reads as filled and the focused pane
        // loses its only unique mark.
        let (cell_w, cell_h) = (10.0_f32, 20.0_f32);
        let t = (cell_h * 0.08_f32).max(1.0).round();
        let edges = [
            (0.0, 0.0, cell_w, t),
            (0.0, cell_h - t, cell_w, t),
            (0.0, t, t, cell_h - 2.0 * t),
            (cell_w - t, t, t, cell_h - 2.0 * t),
        ];
        let (cx, cy) = (cell_w * 0.5, cell_h * 0.5);
        for (x, y, w, h) in edges {
            let inside = cx >= x && cx < x + w && cy >= y && cy < y + h;
            assert!(
                !inside,
                "edge quad {:?} covers the cell centre",
                (x, y, w, h)
            );
        }
        let covered: f32 = edges.iter().map(|(_, _, w, h)| w * h).sum();
        assert!(
            covered < cell_w * cell_h,
            "the outline must not fill the cell"
        );
    }

    #[test]
    fn a_right_aligned_run_keeps_its_last_glyph() {
        // Geometry read off the pane switcher: two columns of a two-window
        // screen, same text, same cell width, different right margins. Both
        // must draw all seven characters.
        let cell_w = 10.2_f32;
        let draw = |max_x: f32, n: usize| {
            let mut x = max_x - n as f32 * cell_w;
            let mut drawn = 0;
            for _ in 0..n {
                if !glyph_fits(x, cell_w, max_x) {
                    break;
                }
                drawn += 1;
                x += cell_w;
            }
            drawn
        };
        assert_eq!(draw(861.4, 7), 7);
        assert_eq!(draw(1713.4, 7), 7);
    }

    #[test]
    fn a_glyph_past_the_limit_is_still_dropped() {
        assert!(!glyph_fits(100.0, 10.0, 109.0));
        assert!(glyph_fits(100.0, 10.0, 110.0));
    }

    #[test]
    fn flash_label_fills_the_pane_without_overflowing_it() {
        // Wide pane, short name: capped at the max scale, centered, and the
        // backdrop stays inside the pane.
        let l = flash_label_layout((100.0, 200.0, 800.0, 400.0), 4, 16, 0, 10.0, 20.0);
        assert_eq!(l.name_scale, 3.0);
        let name_w = 4.0 * 10.0 * l.name_scale;
        assert!((l.name_x - (100.0 + (800.0 - name_w) / 2.0)).abs() < 0.01);
        assert!(l.box_x >= 100.0 && l.box_x + l.box_w <= 900.0 + 0.01);
        // The path line sits under the name, never over it.
        assert!(l.parent_y > l.name_y + 20.0 * l.name_scale - 0.01);
    }

    #[test]
    fn flash_label_stacks_the_session_name_under_the_path() {
        let l = flash_label_layout((0.0, 0.0, 800.0, 400.0), 4, 16, 12, 10.0, 20.0);
        assert!(l.session_y > l.parent_y + 20.0 * l.parent_scale - 0.01);
        // The block stays centered: session line included, it fits the pane.
        assert!(l.box_y >= 0.0 && l.box_y + l.box_h <= 400.0 + 0.01);
    }

    #[test]
    fn flash_label_without_a_session_takes_no_room_for_it() {
        let with_session = flash_label_layout((0.0, 0.0, 800.0, 400.0), 4, 16, 12, 10.0, 20.0);
        let without = flash_label_layout((0.0, 0.0, 800.0, 400.0), 4, 16, 0, 10.0, 20.0);
        assert!(without.box_h < with_session.box_h);
        assert_eq!(
            without.session_y,
            without.parent_y + 20.0 * without.parent_scale
        );
    }

    #[test]
    fn flash_label_shrinks_a_long_name_to_the_pane_width() {
        let l = flash_label_layout((0.0, 0.0, 300.0, 200.0), 40, 0, 0, 10.0, 20.0);
        assert_eq!(l.name_scale, 1.0, "never shrinks below the overlay size");
        let l = flash_label_layout((0.0, 0.0, 300.0, 200.0), 12, 0, 0, 10.0, 20.0);
        assert!(l.name_scale < 3.0 && l.name_scale > 1.0);
        assert!(
            12.0 * 10.0 * l.name_scale <= 300.0,
            "name must fit the pane"
        );
    }

    #[test]
    fn flash_label_without_a_path_line_centers_the_name_alone() {
        let l = flash_label_layout((0.0, 0.0, 400.0, 100.0), 1, 0, 0, 10.0, 20.0);
        let name_h = 20.0 * l.name_scale;
        assert!((l.name_y - (100.0 - name_h) / 2.0).abs() < 0.01);
    }

    #[test]
    fn switcher_row_without_a_binary_gives_the_title_the_whole_row() {
        let split = switcher_row_split(0.0, 300.0, 0, 10.0);
        assert_eq!(split.title_limit, 300.0);
        assert_eq!(split.process_x, None);
    }

    #[test]
    fn switcher_row_parks_the_binary_flush_right() {
        // "claude 2.1.226" = 14 chars → starts 140px before the right margin,
        // and the title stops two cells earlier.
        let split = switcher_row_split(0.0, 300.0, 14, 10.0);
        assert_eq!(split.process_x, Some(160.0));
        assert_eq!(split.title_limit, 140.0);
    }

    #[test]
    fn switcher_row_tail_is_the_binary_alone_without_a_raf() {
        assert_eq!(
            switcher_row_tail_chars(Some("claude 2.1.226"), None, None),
            14
        );
        assert_eq!(switcher_row_tail_chars(None, None, None), 0);
    }

    #[test]
    fn switcher_row_tail_adds_the_raf_after_a_two_cell_gap() {
        // "claude 2.1.226" + 2 + "3 todo" + " · " + "1 idea"
        assert_eq!(
            switcher_row_tail_chars(Some("claude 2.1.226"), Some("3 todo"), Some("1 idea")),
            14 + 2 + 6 + 3 + 6
        );
        assert_eq!(
            switcher_row_tail_chars(Some("claude 2.1.226"), Some("3 todo"), None),
            14 + 2 + 6
        );
        assert_eq!(
            switcher_row_tail_chars(Some("claude 2.1.226"), None, Some("2 ideas")),
            14 + 2 + 7
        );
        assert_eq!(switcher_row_tail_chars(None, Some("3 todo"), None), 6);
    }

    #[test]
    fn switcher_row_drops_the_binary_rather_than_squeeze_the_title() {
        // A column barely wider than the binary itself: the title would be left
        // with almost nothing, so the binary goes instead.
        let split = switcher_row_split(0.0, 160.0, 14, 10.0);
        assert_eq!(split.process_x, None);
        assert_eq!(split.title_limit, 160.0);
    }

    #[test]
    fn format_count_below_thousand_is_verbatim() {
        assert_eq!(format_count(0), "0");
        assert_eq!(format_count(7), "7");
        assert_eq!(format_count(999), "999");
    }

    #[test]
    fn format_count_scales_with_suffixes() {
        assert_eq!(format_count(1_000), "1.0K");
        assert_eq!(format_count(1_234), "1.2K");
        assert_eq!(format_count(1_000_000), "1.0M");
        assert_eq!(format_count(3_400_000), "3.4M");
        assert_eq!(format_count(1_000_000_000), "1.0G");
    }

    #[test]
    fn format_last_activity_has_expected_shape() {
        // Timezone-dependent value, so assert the "DD/MMM HH:mm" shape, not the value.
        let s = format_last_activity(1_700_000_000);
        let bytes = s.as_bytes();
        assert_eq!(s.len(), 12, "got {s:?}");
        assert_eq!(bytes[2], b'/');
        assert_eq!(bytes[6], b' ');
        assert_eq!(bytes[9], b':');
        // Month is one of the known abbreviations.
        let mon = &s[3..6];
        assert!(MONTHS.contains(&mon), "unexpected month {mon:?}");
    }

    #[test]
    fn format_key_combo_renders_modifier_glyphs() {
        assert_eq!(format_key_combo("cmd+shift+d"), "\u{2318}\u{21E7}D");
        assert_eq!(format_key_combo("ctrl+a"), "\u{2303}A");
        assert_eq!(format_key_combo("option+left"), "\u{2325}\u{2190}");
        assert_eq!(format_key_combo("cmd+p"), "\u{2318}P");
    }

    #[test]
    fn format_key_combo_special_keys() {
        assert_eq!(format_key_combo("enter"), "\u{21A9}");
        assert_eq!(format_key_combo("backspace"), "\u{232B}");
        assert_eq!(format_key_combo("cmd+["), "\u{2318}[");
        // A trailing '+' is the literal '+' key, not an empty modifier.
        assert_eq!(format_key_combo("cmd+shift++"), "\u{2318}\u{21E7}+");
        assert_eq!(format_key_combo_arrows("cmd+shift++"), "\u{2318}\u{21E7}+");
    }

    #[test]
    fn format_key_combo_arrows_collapses_directions() {
        // A trailing arrow direction is replaced with the word "Arrows".
        assert_eq!(format_key_combo_arrows("cmd+up"), "\u{2318}ARROWS");
        assert_eq!(format_key_combo_arrows("up"), "ARROWS");
        // Non-arrow keys fall through to the normal formatter.
        assert_eq!(format_key_combo_arrows("cmd+d"), "\u{2318}D");
    }

    #[test]
    fn tooltip_zone_contains_is_half_open() {
        let z = TooltipZone {
            x: 10.0,
            y: 20.0,
            width: 30.0,
            height: 8.0,
            text: "x",
        };
        // Inside.
        assert!(z.contains(10.0, 20.0)); // top-left corner is inclusive
        assert!(z.contains(25.0, 24.0));
        assert!(z.contains(39.9, 27.9));
        // Outside: right/bottom edges are exclusive.
        assert!(!z.contains(40.0, 24.0));
        assert!(!z.contains(25.0, 28.0));
        assert!(!z.contains(9.9, 24.0));
        assert!(!z.contains(25.0, 19.9));
    }

    #[test]
    fn pane_attention_bell_wins_over_completion() {
        assert_eq!(PaneAttention::from_flags(true, true), PaneAttention::Bell);
        assert_eq!(PaneAttention::from_flags(true, false), PaneAttention::Bell);
        assert_eq!(
            PaneAttention::from_flags(false, true),
            PaneAttention::Completion
        );
        assert_eq!(PaneAttention::from_flags(false, false), PaneAttention::None);
    }

    #[test]
    fn pane_attention_dot_color_only_when_attention() {
        assert!(PaneAttention::Bell.dot_color().is_some());
        assert!(PaneAttention::Completion.dot_color().is_some());
        assert!(PaneAttention::None.dot_color().is_none());
    }

    #[test]
    fn pane_attention_bar_bg_falls_back_to_default() {
        let default = [0.1, 0.2, 0.3];
        assert_eq!(PaneAttention::None.bar_bg(default), default);
        assert_ne!(PaneAttention::Bell.bar_bg(default), default);
        assert_ne!(PaneAttention::Completion.bar_bg(default), default);
    }

    #[test]
    fn neighboring_columns_of_a_subject_make_one_segment() {
        let col = |x: f32, name: Option<&str>, focused: bool| {
            (x, 10.0, name.map(|n| (n.to_string(), Some(1))), focused)
        };
        let segs = subject_strip_segments(&[
            col(0.0, Some("auth"), false),
            col(10.0, Some("auth"), true),
            col(20.0, Some("api"), false),
            col(30.0, None, false),
            col(40.0, Some("auth"), false),
        ]);
        let spans: Vec<(f32, f32, &str, bool)> = segs
            .iter()
            .map(|s| (s.x, s.width, s.name.as_str(), s.focused))
            .collect();
        assert_eq!(
            spans,
            [
                (0.0, 20.0, "auth", true),
                (20.0, 10.0, "api", false),
                (40.0, 10.0, "auth", false)
            ],
            "a column in no subject breaks the run: the name shows twice"
        );
        // A minimized column (no width) neither shows nor breaks the run.
        let segs = subject_strip_segments(&[
            col(0.0, Some("a"), false),
            (10.0, 0.0, Some(("a".to_string(), Some(1))), false),
            col(10.0, Some("a"), false),
        ]);
        assert_eq!(segs.len(), 1);
        assert_eq!(segs[0].width, 20.0);
    }

    #[test]
    fn dim_inactive_tab_keeps_hue_but_steps_back() {
        for c in TAB_COLORS {
            let d = dim_inactive_tab(c);
            let lum = |v: [f32; 3]| 0.213 * v[0] + 0.715 * v[1] + 0.072 * v[2];
            // Always darker than the full-color version the active tab keeps.
            assert!(lum(d) < lum(c), "{c:?} -> {d:?} should be darker");
            // Still tinted: the widest channel gap survives the desaturation.
            let spread = |v: [f32; 3]| {
                v.iter().cloned().fold(f32::MIN, f32::max)
                    - v.iter().cloned().fold(f32::MAX, f32::min)
            };
            assert!(spread(d) > spread(c) * 0.4, "{c:?} -> {d:?} lost its hue");
            assert!(
                d.iter().all(|v| (0.0..=1.0).contains(v)),
                "{d:?} out of range"
            );
        }
    }
}

/// Characterization of `build_vertices` as of 13/10/2026, before the embedding
/// refactor (docs/embedding.md). The renderer is the real one on the real Metal
/// device, but its atlas carries fixed metrics and synthetic glyphs, so the
/// snapshots depend on neither the installed font nor the macOS version.
///
/// A snapshot that moves means the pixels a pane draws moved. To accept a change
/// on purpose: `KOVA_UPDATE_SNAPSHOTS=1 cargo test vertex_snapshots`, then read the
/// diff of `src/renderer/snapshots/`.
#[cfg(test)]
mod vertex_snapshots {
    use super::*;
    use crate::terminal::{GridPos, Selection, SelectionMode};
    use glyph_atlas::GlyphInfo;

    const CELL_W: f32 = 10.0;
    const CELL_H: f32 = 20.0;

    fn renderer() -> Renderer {
        let device = MTLCreateSystemDefaultDevice().expect("no Metal device");
        let layer = CAMetalLayer::new();
        layer.setDevice(Some(&device));
        layer.setPixelFormat(MTLPixelFormat::BGRA8Unorm);
        let mut config = Config::default();
        config.status_bar.enabled = false;
        let term = Arc::new(RwLock::new(TerminalState::new(4, 2, 10, [255; 3], [0; 3])));
        let mut r = Renderer::new(&device, &layer, term, 2.0, &config);
        r.ctx.atlas.fake_metrics(CELL_W, CELL_H, 4.0, 1024, 512);
        r
    }

    /// A deterministic atlas slot per char: a 16-px grid indexed by code point.
    fn slot(code: u32, wide: bool) -> GlyphInfo {
        GlyphInfo {
            x: (code % 32) * 32,
            y: (code / 32 % 16) * 32,
            width: if wide {
                2 * CELL_W as u32
            } else {
                CELL_W as u32 - 2
            },
            height: CELL_H as u32 - 4,
            is_color: false,
        }
    }

    /// Register every glyph the visible cells need, so nothing is rasterized.
    fn fill_atlas(r: &mut Renderer, t: &TerminalState) {
        for line in t.visible_lines() {
            for cell in line.iter() {
                if let Some(ref cl) = cell.cluster {
                    let code: u32 = cl.chars().map(|c| c as u32).sum();
                    r.ctx.atlas.fake_cluster_glyph(cl, slot(code, false));
                } else if !cell.is_blank() {
                    let wide = unicode_width::UnicodeWidthChar::width(cell.c) == Some(2);
                    r.ctx.atlas.fake_glyph(cell.c, slot(cell.c as u32, wide));
                }
            }
        }
    }

    /// A terminal with the default config colors, as Kova builds them.
    fn term(cols: u16, rows: u16) -> TerminalState {
        let c = Config::default();
        TerminalState::new(
            cols,
            rows,
            10,
            crate::terminal::color_to_u8(c.colors.foreground),
            crate::terminal::color_to_u8(c.colors.background),
        )
    }

    fn type_lines(t: &mut TerminalState, lines: &[&str]) {
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

    fn vp() -> PaneViewport {
        PaneViewport {
            x: 100.0,
            y: 50.0,
            width: 140.0,
            height: 90.0,
        }
    }

    fn build(r: &mut Renderer, t: &TerminalState, focused: bool) -> Vec<Vertex> {
        fill_atlas(r, t);
        r.build_vertices(
            t,
            &vp(),
            true,
            focused,
            focused,
            None,
            PaneAttention::None,
            0,
            1,
            None,
            false,
        )
    }

    fn dump(vs: &[Vertex]) -> String {
        let f = |x: f32| format!("{:.4}", x);
        let mut out = String::new();
        for (i, v) in vs.iter().enumerate() {
            if i % 6 == 0 {
                out.push_str(&format!("# quad {}\n", i / 6));
            }
            out.push_str(&format!(
                "pos {} {} | uv {} {} | fg {} {} {} {} | bg {} {} {} {}\n",
                f(v.position[0]),
                f(v.position[1]),
                f(v.tex_coords[0]),
                f(v.tex_coords[1]),
                f(v.color[0]),
                f(v.color[1]),
                f(v.color[2]),
                f(v.color[3]),
                f(v.bg_color[0]),
                f(v.bg_color[1]),
                f(v.bg_color[2]),
                f(v.bg_color[3]),
            ));
        }
        out
    }

    fn check(name: &str, vs: &[Vertex]) {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("src/renderer/snapshots")
            .join(format!("{name}.snap"));
        let got = dump(vs);
        let update = std::env::var_os("KOVA_UPDATE_SNAPSHOTS").is_some();
        match std::fs::read_to_string(&path) {
            Ok(want) if !update => {
                if want != got {
                    let first = want.lines().zip(got.lines()).position(|(a, b)| a != b);
                    panic!(
                        "vertex snapshot {name} moved ({} lines expected, {} got); first difference at line {:?}:\n  want: {:?}\n  got:  {:?}",
                        want.lines().count(),
                        got.lines().count(),
                        first.map(|i| i + 1),
                        first.and_then(|i| want.lines().nth(i)),
                        first.and_then(|i| got.lines().nth(i)),
                    );
                }
            }
            _ => std::fs::write(&path, got).expect("write snapshot"),
        }
    }

    #[test]
    fn plain_text_with_a_focused_block_cursor() {
        let mut r = renderer();
        let mut t = term(12, 3);
        type_lines(&mut t, &["ab", "cd"]);
        check("plain_focused", &build(&mut r, &t, true));
    }

    #[test]
    fn unfocused_pane_full_dim_and_hollow_cursor() {
        let mut r = renderer();
        let mut t = term(12, 3);
        type_lines(&mut t, &["ab"]);
        check("unfocused_full_dim", &build(&mut r, &t, false));
    }

    #[test]
    fn unfocused_pane_text_dim() {
        let mut r = renderer();
        r.style.dim_mode = DimMode::Text;
        let mut t = term(12, 3);
        type_lines(&mut t, &["ab"]);
        check("unfocused_text_dim", &build(&mut r, &t, false));
    }

    #[test]
    fn colors_and_background_runs() {
        let mut r = renderer();
        let mut t = term(12, 3);
        t.set_sgr(&[31]);
        t.put_char('R');
        t.set_sgr(&[0, 44]);
        t.put_char('B');
        t.put_char(' ');
        t.put_char('B');
        t.set_sgr(&[0, 38, 2, 10, 200, 30]);
        t.put_char('g');
        t.set_sgr(&[0]);
        check("colors_bg_runs", &build(&mut r, &t, true));
    }

    #[test]
    fn bold_italic_underline_strikethrough() {
        let mut r = renderer();
        let mut t = term(12, 3);
        for (sgr, c) in [(1u16, 'b'), (3, 'i'), (4, 'u'), (9, 's')] {
            t.set_sgr(&[sgr]);
            t.put_char(c);
            t.set_sgr(&[0]);
        }
        t.set_sgr(&[4]);
        t.put_char(' ');
        t.set_sgr(&[0]);
        check("attributes", &build(&mut r, &t, true));
    }

    #[test]
    fn selection_highlight() {
        let mut r = renderer();
        let mut t = term(12, 3);
        type_lines(&mut t, &["hello", "world"]);
        t.selection = Some(Selection {
            anchor: GridPos { line: 0, col: 3 },
            end: GridPos { line: 1, col: 1 },
            mode: SelectionMode::Normal,
        });
        check("selection", &build(&mut r, &t, true));
    }

    #[test]
    fn wide_chars_and_clusters() {
        let mut r = renderer();
        let mut t = term(12, 3);
        type_lines(&mut t, &["前x", "e\u{301}!"]);
        check("wide_and_cluster", &build(&mut r, &t, true));
    }

    #[test]
    fn paste_block_rows() {
        let mut r = renderer();
        let mut t = term(12, 5);
        type_lines(&mut t, &["say:", "```slack", "body", "```", "```/slack"]);
        check("paste_block", &build(&mut r, &t, true));
    }

    #[test]
    fn scrolled_back_hides_the_cursor_and_shows_scrollback() {
        let mut r = renderer();
        let mut t = term(12, 2);
        type_lines(&mut t, &["one", "two", "three", "four"]);
        t.scroll(2);
        check("scrolled_back", &build(&mut r, &t, true));
    }

    #[test]
    fn bottom_gravity_on_a_short_screen() {
        let mut r = renderer();
        let mut t = term(12, 4);
        type_lines(&mut t, &["$ ls"]);
        assert!(t.y_offset_rows() > 0);
        check("bottom_gravity", &build(&mut r, &t, true));
    }

    #[test]
    fn hovered_url_underline() {
        let mut r = renderer();
        let mut t = term(12, 3);
        type_lines(&mut t, &["http://a.b/c"]);
        r.hovered_url = Some(vec![(0, 0, 12), (1, 0, 3)]);
        check("hovered_url", &build(&mut r, &t, true));
    }

    /// Register a synthetic glyph for every char of `texts` (overlay text goes
    /// through the same atlas as the cells).
    fn fake_text(r: &mut Renderer, texts: &[&str]) {
        for t in texts {
            for c in t.chars() {
                let wide = unicode_width::UnicodeWidthChar::width(c) == Some(2);
                r.ctx.atlas.fake_glyph(c, slot(c as u32, wide));
            }
        }
    }

    /// The Ctrl+F filter overlay over a pane: the bar, the count (or the
    /// recall hint), the listed lines with the query highlighted
    /// case-insensitively — Kova.app's and the library pane's.
    #[test]
    fn filter_overlay() {
        let mut r = renderer();
        let vp = PaneViewport {
            x: 100.0,
            y: 50.0,
            width: 300.0,
            height: 90.0,
        };
        let filter = FilterRenderData {
            query: "err".into(),
            matches: vec![
                FilterMatch {
                    abs_line: 3,
                    text: "an error here".into(),
                },
                FilterMatch {
                    abs_line: 12,
                    text: "ERR \u{130}x err".into(),
                },
                FilterMatch {
                    abs_line: 40,
                    text: "third err".into(),
                },
                FilterMatch {
                    abs_line: 41,
                    text: "not shown".into(),
                },
            ],
            selected: 1,
            hint: None,
        };
        fake_text(
            &mut r,
            &[
                "/ err\u{258F}",
                "4 matches 0123456789: ",
                "an error here",
                "ERR \u{130}x err",
                "third",
                "not shown",
                "\u{2191} recall",
            ],
        );
        let mut vs = Vec::new();
        r.build_filter_overlay_vertices(&mut vs, &vp, &filter);
        check("filter_overlay", &vs);
        let hinted = FilterRenderData {
            query: String::new(),
            matches: Vec::new(),
            selected: 0,
            hint: Some("\u{2191} recall".into()),
        };
        let mut vs = Vec::new();
        r.build_filter_overlay_vertices(&mut vs, &vp, &hinted);
        check("filter_overlay_hint", &vs);
    }

    #[test]
    fn underline_and_bar_cursors() {
        let mut r = renderer();
        let mut t = term(12, 3);
        type_lines(&mut t, &["ab"]);
        t.cursor_shape = CursorShape::Underline;
        check("cursor_underline", &build(&mut r, &t, true));
        t.cursor_shape = CursorShape::Bar;
        check("cursor_bar", &build(&mut r, &t, true));
    }

    #[test]
    fn hidden_cursor_and_blink_off_draw_no_cursor() {
        let mut r = renderer();
        let mut t = term(12, 3);
        type_lines(&mut t, &["ab"]);
        fill_atlas(&mut r, &t);
        let on = r.build_vertices(
            &t,
            &vp(),
            true,
            true,
            true,
            None,
            PaneAttention::None,
            0,
            1,
            None,
            false,
        );
        let off = r.build_vertices(
            &t,
            &vp(),
            false,
            true,
            true,
            None,
            PaneAttention::None,
            0,
            1,
            None,
            false,
        );
        assert_eq!(
            on.len() - off.len(),
            6,
            "blink off drops exactly the cursor quad"
        );
        t.cursor_visible = false;
        let hidden = r.build_vertices(
            &t,
            &vp(),
            true,
            true,
            true,
            None,
            PaneAttention::None,
            0,
            1,
            None,
            false,
        );
        assert_eq!(hidden.len(), off.len());
    }

    #[test]
    fn a_focused_pane_without_the_outline_flag_draws_no_outline() {
        let mut r = renderer();
        let mut t = term(12, 3);
        type_lines(&mut t, &["ab"]);
        fill_atlas(&mut r, &t);
        let outlined = r.build_vertices(
            &t,
            &vp(),
            true,
            true,
            true,
            None,
            PaneAttention::None,
            0,
            1,
            None,
            false,
        );
        let plain = r.build_vertices(
            &t,
            &vp(),
            true,
            true,
            false,
            None,
            PaneAttention::None,
            0,
            1,
            None,
            false,
        );
        assert_eq!(
            outlined.len() - plain.len(),
            4 * 6,
            "the outline is exactly four quads"
        );
        assert_eq!(
            dump(&outlined[..plain.len()]),
            dump(&plain),
            "nothing else changes"
        );
    }

    #[test]
    fn the_working_counter_paints_the_global_bar() {
        // The status bar's `✳N` is the count `window::tick` makes with
        // `p.is_working()` — the spinner in the OSC title, or a program's
        // `state=working` record (OSC 7501). This closes the other end: the
        // count has to reach the bar, with the ✳ glyphs and its tooltip.
        let mut r = renderer();
        r.cached_time_str = "12:00".into();
        // Every glyph the bar draws gets a fake slot, `✳` included: nothing is
        // rasterized, so the test needs no atlas texture.
        for c in (0x20u8..0x7f)
            .map(char::from)
            .chain(['✳', '●', '◌', '▶', '↑', '↓', '⟵', '⟶'])
        {
            r.ctx.atlas.fake_glyph(c, slot(c as u32, false));
        }
        let bar = |r: &mut Renderer, working: usize| {
            let mut vs = Vec::new();
            r.tooltip_zones.clear();
            r.build_global_status_bar_vertices(
                &mut vs, 1400.0, 800.0, 0, 0, 1, 1, 1, 1, "tab", working, 0, 0, 0, 0, 0, None,
            );
            let zones: Vec<&'static str> = r.tooltip_zones.iter().map(|z| z.text).collect();
            (dump(&vs), zones)
        };
        let (idle_bar, idle_zones) = bar(&mut r, 0);
        let (working_bar, working_zones) = bar(&mut r, 1);
        let (busy_bar, _) = bar(&mut r, 2);
        let working_zone = "Panes working right now";
        assert!(
            !idle_zones.iter().any(|t| t.starts_with(working_zone)),
            "nothing busy, nothing shown"
        );
        assert!(
            working_zones.iter().any(|t| t.starts_with(working_zone)),
            "one pane working: the counter is on the bar"
        );
        assert_ne!(
            idle_bar, working_bar,
            "and its glyphs are painted, not just a tooltip"
        );
        assert_ne!(
            working_bar, busy_bar,
            "the count itself is painted: ✳1 is not ✳2"
        );
    }

    #[test]
    fn snapshots_ignore_the_installed_font() {
        // Two renderers built independently produce the same bytes: nothing in
        // the output comes from the atlas the font would have filled.
        let mut t = term(12, 3);
        type_lines(&mut t, &["Wy!", "前"]);
        let a = dump(&build(&mut renderer(), &t, true));
        let b = dump(&build(&mut renderer(), &t, true));
        assert_eq!(a, b);
    }
}
