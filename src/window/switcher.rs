//! The tab/pane switcher overlay (`Cmd+P`, and `Cmd+Shift+J` for the
//! attention-only list): the row model it is built from, and the keys, scroll
//! and clicks it answers while it is open.

use super::*;

/// One row of the tab/pane switcher overlay.
pub(super) enum SwitcherRow {
    /// A tab name — not selectable.
    TabHeader(String),
    /// A subject of the tab above: the first line of its box. Selectable:
    /// Enter renames the subject, Cmd+Backspace deletes it. `empty` when no
    /// pane is filed under it: the box then holds this line alone.
    SubjectHeader { name: String, empty: bool, tab: TabId },
    /// A pane entry — selectable, focuses the pane on Enter/click.
    /// `minimized` panes are restored (unhidden) when selected.
    Pane {
        pane_id: PaneId,
        title: String,
        is_current: bool,
        has_bell: bool,
        has_completion: bool,
        minimized: bool,
        /// The agent in this pane is working: Claude Code generating or running a
        /// tool, or any program reporting `state=working` (OSC 7501, Pi's way).
        /// Drawn as ✳.
        working: bool,
        /// The binary running in the pane, with its version when known
        /// ("claude 2.1.226"). `None` at a bare shell prompt, and also when the
        /// title already *is* that name — no row should say "vim … vim".
        process: Option<String>,
        /// This pane holds a bookmarked conversation. The row is painted so it
        /// stands out, and the bookmark groups leave the conversation out:
        /// a bookmark that is already open is this row, not a second one.
        bookmarked: bool,
        /// This pane holds an anchored conversation. The anchor is listed in its
        /// own section whether or not a pane holds it, so this row is dropped
        /// before the list is shown — see `drop_anchored_panes`.
        anchored: bool,
        /// What the pane's Claude conversation still has to do, read from
        /// `~/.claude/raf/` when the list is built (`crate::raf`): `"3 todo"`,
        /// drawn in colour at the row end, and `"1 idea"`, drawn dim. `None`
        /// when the count is zero or the pane holds no Claude conversation.
        raf_todo: Option<String>,
        raf_idea: Option<String>,
        /// The subject box this row is drawn in, under the header that names
        /// it. `None` outside a box — and always on the
        /// attention-only and memory lists, which have no subjects.
        subject: Option<String>,
        /// Last row of its subject's box: the frame closes under it.
        last_in_subject: bool,
    },
    /// An anchored conversation — selectable, and always listed, even when a
    /// pane still holds it: the anchors are what today is for, so the section
    /// stays the same whether or not the work is currently open.
    Anchor {
        /// Index into the saved anchors.
        index: usize,
        title: String,
        /// Agent holding the conversation, shown dim at the end of the row.
        detail: Option<String>,
    },
    /// A bookmarked conversation — selectable. Enter jumps to the pane that
    /// still holds it, or reopens it where it belongs when nothing does.
    Bookmark {
        /// Index into the saved list, so the row can act on it without carrying
        /// the whole bookmark around.
        index: usize,
        title: String,
        /// Project name and agent, shown dim at the end of the row.
        detail: Option<String>,
    },
}

impl SwitcherRow {
    fn is_pane(&self) -> bool {
        matches!(self, SwitcherRow::Pane { .. })
    }

    /// Rows Enter and the arrow keys can land on: panes, bookmarks, anchors,
    /// and subject headers (renamed or deleted from there). Tab headers are
    /// the only thing the selection skips.
    fn is_selectable(&self) -> bool {
        !matches!(self, SwitcherRow::TabHeader(_))
    }

    /// Is this row asking for something? A bell, or a command that finished
    /// while the eye was elsewhere — the two markers the switcher draws, and the
    /// ones the attention-only list keeps. `working` is deliberately not one of
    /// them: a session still chewing has nothing to hand over yet, exactly as in
    /// `Cmd+J`'s tiers.
    pub(super) fn needs_attention(&self) -> bool {
        match self {
            SwitcherRow::Pane { has_bell, has_completion, .. } => *has_bell || *has_completion,
            _ => false,
        }
    }
}

/// Keep only the rows that ask for something, tab by tab.
///
/// A tab header survives as long as one of its panes does — the filtered list
/// still has to say *where* the pane lives — and a tab whose panes all fall out
/// disappears whole, header included, rather than naming a tab the list has
/// nothing to say about. Groups are per-tab (one header followed by its panes),
/// which is what makes that decision local.
pub(super) fn retain_attention_rows(groups: Vec<Vec<SwitcherRow>>) -> Vec<Vec<SwitcherRow>> {
    groups
        .into_iter()
        .filter_map(|group| {
            // Subject headers go too: this list has no subjects.
            let kept: Vec<SwitcherRow> = group
                .into_iter()
                .filter(|r| match r {
                    SwitcherRow::SubjectHeader { .. } => false,
                    r => !r.is_pane() || r.needs_attention(),
                })
                .collect();
            kept.iter().any(|r| r.is_pane()).then_some(kept)
        })
        .collect()
}

/// One line of a tab's outline in Cmd+P: a subject's header, or one of the
/// tab's panes by its index in the tab's order (`Tab::pane_subjects`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum OutlineItem {
    /// The header of a subject's box. `empty`: no pane is filed under it.
    Subject { name: String, empty: bool },
    /// A pane, drawn inside `subject`'s box when it has one; `last` closes
    /// the box under it.
    Pane { index: usize, subject: Option<String>, last: bool },
}

/// The order Cmd+P draws a tab in, from the tab's subjects and each pane's
/// subject (`panes`, in the tab's order).
///
/// Empty subjects come first, each a box of its own header. Then the panes in
/// order, a pane without a subject exactly where it is; the first member of a
/// subject met opens its box — header, then every member of that subject in
/// order. The gestures keep a subject's members next to each other in the
/// layout, but a split at the end or an IPC move can tear them apart: the
/// list gathers them under the first one rather than drawing two boxes, and
/// the panes on screen are left where they are.
pub(super) fn subject_outline(subjects: &[String], panes: &[Option<String>]) -> Vec<OutlineItem> {
    let mut out = Vec::new();
    for name in subjects {
        if !panes.iter().any(|s| s.as_deref() == Some(name.as_str())) {
            out.push(OutlineItem::Subject { name: name.clone(), empty: true });
        }
    }
    let mut opened: Vec<&str> = Vec::new();
    for (i, s) in panes.iter().enumerate() {
        match s.as_deref() {
            None => out.push(OutlineItem::Pane { index: i, subject: None, last: false }),
            Some(name) if opened.contains(&name) => {}
            Some(name) => {
                opened.push(name);
                out.push(OutlineItem::Subject { name: name.to_string(), empty: false });
                let members: Vec<usize> =
                    (i..panes.len()).filter(|&j| panes[j].as_deref() == Some(name)).collect();
                let n = members.len();
                for (k, index) in members.into_iter().enumerate() {
                    out.push(OutlineItem::Pane { index, subject: Some(name.to_string()), last: k + 1 == n });
                }
            }
        }
    }
    out
}

/// A tab's pane rows laid out as its outline: subject headers inserted, each
/// pane row tagged with the box it is drawn in. `rows[i]` is the pane whose
/// subject is `panes[i]`; `tab` is the tab they all belong to.
pub(super) fn subject_rows(
    tab: TabId,
    subjects: &[String],
    rows: Vec<SwitcherRow>,
    panes: &[Option<String>],
) -> Vec<SwitcherRow> {
    let mut slots: Vec<Option<SwitcherRow>> = rows.into_iter().map(Some).collect();
    subject_outline(subjects, panes)
        .into_iter()
        .filter_map(|item| match item {
            OutlineItem::Subject { name, empty } => Some(SwitcherRow::SubjectHeader { name, empty, tab }),
            OutlineItem::Pane { index, subject, last } => {
                let mut row = slots.get_mut(index)?.take()?;
                if let SwitcherRow::Pane { subject: s, last_in_subject, .. } = &mut row {
                    *s = subject;
                    *last_in_subject = last;
                }
                Some(row)
            }
        })
        .collect()
}

/// The subject of the line drawn right above the pane at `p` — its header, or
/// one of its members — which Cmd+Right files the pane under. `None` when that
/// line is the tab's header or a pane without a subject: Cmd+Right then asks
/// for a name.
pub(super) fn subject_above(subjects: &[String], panes: &[Option<String>], p: usize) -> Option<String> {
    let outline = subject_outline(subjects, panes);
    let pos = outline.iter().position(|it| matches!(it, OutlineItem::Pane { index, .. } if *index == p))?;
    match outline.get(pos.checked_sub(1)?)? {
        OutlineItem::Subject { name, .. } => Some(name.clone()),
        OutlineItem::Pane { subject, .. } => subject.clone(),
    }
}

/// How many steps forward Cmd+Left moves the pane at `p` as it leaves its
/// subject: past the subject's last member, so it lands right under the box.
/// Zero when it already is the last one, or has no subject.
pub(super) fn outdent_steps(panes: &[Option<String>], p: usize) -> usize {
    let Some(name) = panes.get(p).and_then(|s| s.as_deref()) else { return 0 };
    panes
        .iter()
        .enumerate()
        .skip(p + 1)
        .filter(|(_, s)| s.as_deref() == Some(name))
        .map(|(j, _)| j - p)
        .last()
        .unwrap_or(0)
}

/// File the pane under `name` in its tab: Cmd+Right's effect, and the
/// `set-pane-subject` command's. A name the tab lacks is added to its list.
/// Nothing moves on screen. Returns whether the subject was created, `None`
/// when the tab holds no such pane.
pub(super) fn file_under_subject(tab: &mut Tab, pane_id: PaneId, name: &str) -> Option<bool> {
    tab.pane(pane_id)?;
    let created = tab.add_subject(name);
    tab.pane_mut(pane_id)?.subject = Some(name.to_string());
    Some(created)
}

/// Take the pane out of its subject: Cmd+Left's effect, and `set-pane-subject`
/// with a null subject's. A pane from the middle of the box moves past its
/// last member (`outdent_steps`); the subject stays, even left empty. Returns
/// whether the layout moved, `None` when the tab holds no such pane or the
/// pane is in no subject.
pub(super) fn take_out_of_subject(tab: &mut Tab, pane_id: PaneId) -> Option<bool> {
    let order = tab.pane_subjects();
    let p = order.iter().position(|(id, _)| *id == pane_id)?;
    order[p].1.as_ref()?;
    let subjects: Vec<Option<String>> = order.into_iter().map(|(_, s)| s).collect();
    let steps = outdent_steps(&subjects, p);
    tab.pane_mut(pane_id)?.subject = None;
    let mut moved = false;
    for _ in 0..steps {
        if !tab.move_pane_in_order(pane_id, true) {
            break;
        }
        moved = true;
    }
    Some(moved)
}

/// Every subject of a tab with the panes filed under it, in the order Cmd+P
/// draws the boxes (`subject_outline`): empty ones first, then each where its
/// first member sits, members in the tab's order. `list-subjects` answers
/// with it, so a client sees the boxes the user sees.
pub(super) fn subject_listing(subjects: &[String], panes: &[(PaneId, Option<String>)]) -> Vec<(String, Vec<PaneId>)> {
    let names: Vec<Option<String>> = panes.iter().map(|(_, s)| s.clone()).collect();
    let mut out: Vec<(String, Vec<PaneId>)> = Vec::new();
    for item in subject_outline(subjects, &names) {
        match item {
            OutlineItem::Subject { name, .. } => out.push((name, Vec::new())),
            // The outline puts every member right under its header.
            OutlineItem::Pane { index, subject: Some(_), .. } => {
                if let Some((_, members)) = out.last_mut() {
                    members.push(panes[index].0);
                }
            }
            OutlineItem::Pane { subject: None, .. } => {}
        }
    }
    out
}

/// What Cmd+Up/Down does to the pane at `p` in a tab with subjects.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SubjectMove {
    /// Nothing to do: the edge of the tab, or of the pane's box.
    Stay,
    /// Trade places with the pane at this index: the previous or next member
    /// of the same subject.
    Swap(usize),
    /// Step this many times in the direction asked (`Tab::move_pane_in_order`).
    Steps(usize),
}

/// Cmd+Up/Down with subjects. A pane in a subject moves among its members only,
/// and stops at the edge of its box (leaving it is Cmd+Left). A pane outside
/// moves one line of the list, and a box in the way is jumped over whole. The
/// empty boxes at the top are not places in the layout: a pane stops under
/// them, at the first position, as it always did.
///
/// Without any subject this is exactly one step, as before subjects existed.
pub(super) fn subject_move(subjects: &[String], panes: &[Option<String>], p: usize, forward: bool) -> SubjectMove {
    let Some(own) = panes.get(p) else { return SubjectMove::Stay };
    if let Some(name) = own.as_deref() {
        let same = |j: &usize| panes[*j].as_deref() == Some(name);
        let other = if forward { (p + 1..panes.len()).find(same) } else { (0..p).rev().find(same) };
        return other.map_or(SubjectMove::Stay, SubjectMove::Swap);
    }
    let outline = subject_outline(subjects, panes);
    let Some(pos) = outline.iter().position(|it| matches!(it, OutlineItem::Pane { index, .. } if *index == p)) else {
        return SubjectMove::Stay;
    };
    let members = |name: &str| -> Vec<usize> {
        (0..panes.len()).filter(|&j| panes[j].as_deref() == Some(name)).collect()
    };
    let target = if forward {
        match outline.get(pos + 1) {
            Some(OutlineItem::Pane { index, subject: None, .. }) => Some(*index),
            Some(OutlineItem::Pane { subject: Some(name), .. } | OutlineItem::Subject { name, .. }) => members(name).last().copied(),
            None => None,
        }
    } else {
        match pos.checked_sub(1).and_then(|i| outline.get(i)) {
            Some(OutlineItem::Pane { index, subject: None, .. }) => Some(*index),
            Some(OutlineItem::Subject { empty: true, .. }) | None => None,
            Some(OutlineItem::Pane { subject: Some(name), .. } | OutlineItem::Subject { name, .. }) => members(name).first().copied(),
        }
    };
    let steps = match target {
        Some(t) if forward && t > p => t - p,
        Some(t) if !forward && t < p => p - t,
        _ => 0,
    };
    if steps == 0 { SubjectMove::Stay } else { SubjectMove::Steps(steps) }
}

/// What Cmd+Up/Down does with a subject's header selected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum SubjectBlockMove {
    /// The edge of the tab, or of the empty boxes at its top.
    Stay,
    /// The tab's panes in their new order, as indices into the old one: the
    /// box traded places with the line or box next to it.
    Panes(Vec<usize>),
    /// An empty subject: the tab's subject list in its new order.
    Subjects(Vec<String>),
}

/// Cmd+Up/Down on the header of `name`: the whole box moves one line of the
/// list, over the pane or the box next to it, its members coming out next to
/// each other. An empty box moves among the other empty ones at the top of the
/// tab — they are not places in the layout, so a box with panes never goes
/// above them and an empty one never goes below them.
pub(super) fn subject_block_move(
    subjects: &[String],
    panes: &[Option<String>],
    name: &str,
    forward: bool,
) -> SubjectBlockMove {
    let filed = |s: &str| panes.iter().any(|p| p.as_deref() == Some(s));
    if !filed(name) {
        let empties: Vec<&String> = subjects.iter().filter(|s| !filed(s)).collect();
        let Some(i) = empties.iter().position(|s| *s == name) else { return SubjectBlockMove::Stay };
        let j = if forward { i + 1 } else { match i.checked_sub(1) { Some(j) => j, None => return SubjectBlockMove::Stay } };
        let Some(other) = empties.get(j) else { return SubjectBlockMove::Stay };
        let mut out = subjects.to_vec();
        let (a, b) = (
            out.iter().position(|s| s == name).unwrap(),
            out.iter().position(|s| s == *other).unwrap(),
        );
        out.swap(a, b);
        return SubjectBlockMove::Subjects(out);
    }
    // The list as blocks: a lone pane, or a box with its members.
    let mut blocks: Vec<Vec<usize>> = Vec::new();
    let mut own = None;
    for item in subject_outline(subjects, panes) {
        match item {
            OutlineItem::Subject { empty: true, .. } => {}
            OutlineItem::Subject { name: n, .. } => {
                if n == name {
                    own = Some(blocks.len());
                }
                blocks.push(Vec::new());
            }
            OutlineItem::Pane { index, subject: None, .. } => blocks.push(vec![index]),
            OutlineItem::Pane { index, .. } => blocks.last_mut().unwrap().push(index),
        }
    }
    let Some(i) = own else { return SubjectBlockMove::Stay };
    let j = if forward { i + 1 } else { match i.checked_sub(1) { Some(j) => j, None => return SubjectBlockMove::Stay } };
    if j >= blocks.len() {
        return SubjectBlockMove::Stay;
    }
    blocks.swap(i, j);
    SubjectBlockMove::Panes(blocks.into_iter().flatten().collect())
}

/// The conversation a pane is *about* to reopen: the id that follows `--resume`
/// (Claude) or `resume` (Codex) in the line waiting in it.
///
/// A restored pane wears its conversation's title from the first frame, while
/// the resume line sits pre-typed at its prompt and no agent is running yet —
/// so it has no session id to match an anchor on. This is what links the two
/// until Enter is pressed.
pub(super) fn pending_session_id(last_command: Option<&str>) -> Option<&str> {
    let cmd = last_command?;
    let mut tokens = cmd.split_whitespace();
    while let Some(tok) = tokens.next() {
        if tok == "--resume" || tok == "resume" {
            return tokens.next();
        }
    }
    None
}

/// What a pane switcher row says about the binary running in the pane.
///
/// Titles come from the app itself (Claude Code names the session, an editor
/// names the file), so the program behind them is invisible in the list — this
/// is what puts it back. It stays out of the way when the title *is* the
/// program name, which is what a pane at a bare `vim` already shows.
pub(super) fn switcher_process_label(process: Option<&ProcessInfo>, title: &str) -> Option<String> {
    let label = process?.label();
    (!label.is_empty() && label != title).then_some(label)
}

/// Next pane row with unread output — a bell, or a command that finished while
/// the eye was elsewhere — scanning forward from just after `(col, row)` in
/// column-major order and wrapping around the whole grid. `None` when nothing is
/// unread: the caller then leaves the selection where it is, so pressing Tab in
/// a quiet switcher does nothing rather than jumping somewhere arbitrary.
pub(super) fn next_unread_row(
    columns: &[Vec<SwitcherRow>],
    col: usize,
    row: usize,
) -> Option<(usize, usize)> {
    let flat: Vec<(usize, usize)> = columns
        .iter()
        .enumerate()
        .flat_map(|(c, rows)| (0..rows.len()).map(move |r| (c, r)))
        .collect();
    let start = flat.iter().position(|&p| p == (col, row)).map_or(0, |i| i + 1);
    flat.iter()
        .cycle()
        .skip(start)
        .take(flat.len())
        .find(|&&(c, r)| {
            matches!(
                columns[c][r],
                SwitcherRow::Pane { has_bell: true, .. } | SwitcherRow::Pane { has_completion: true, .. }
            )
        })
        .copied()
}

/// Index of the pane row whose position is closest to `target` within `col`.
/// Every column holds at least one pane (each tab has ≥1 pane), so this always
/// returns a valid pane index; falls back to 0 only for a degenerate empty column.
fn nearest_pane_row(col: &[SwitcherRow], target: usize) -> usize {
    col.iter()
        .enumerate()
        .filter(|(_, r)| r.is_selectable())
        .min_by_key(|(i, _)| (*i as isize - target as isize).unsigned_abs())
        .map(|(i, _)| i)
        .unwrap_or(0)
}

/// What activating a switcher row does.
enum SwitcherTarget {
    Pane(PaneId),
    Bookmark(usize),
    Anchor(usize),
    /// A subject header: its tab and name, and where its row sits.
    Subject(TabId, String, usize, usize),
}

/// Which list the full switcher shows. The attention-only list (`u`) has no
/// such choice: it answers one question, and ignores this.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SwitcherView {
    /// The open panes, tab by tab — what Cmd+P opens on.
    Panes,
    /// The saved conversations in place of the open panes (`b`).
    Bookmarks,
    /// Every pane in one list, biggest resident memory first (`m`): which
    /// sessions to close when the Mac runs short.
    Memory,
}

/// `b` and `m` each flip between their own list and the open panes; from the
/// other one, they go straight to theirs.
pub(super) fn toggled_view(current: SwitcherView, wanted: SwitcherView) -> SwitcherView {
    if current == wanted { SwitcherView::Panes } else { wanted }
}

/// The list a letter asks for, read from the typed character
/// (`charactersIgnoringModifiers`) and not the keycode: on AZERTY the `m` key
/// is QWERTY's `;`, and QWERTY's `m` key types `,`.
pub(super) fn view_for_key(unmodified: Option<&str>) -> Option<SwitcherView> {
    match unmodified?.to_lowercase().as_str() {
        "b" => Some(SwitcherView::Bookmarks),
        "m" => Some(SwitcherView::Memory),
        _ => None,
    }
}

/// A byte count the way the memory list shows it: "312 MB", "1.4 GB".
pub(super) fn format_memory(bytes: u64) -> String {
    const MB: f64 = 1024.0 * 1024.0;
    let mb = bytes as f64 / MB;
    if mb >= 1024.0 {
        format!("{:.1} GB", mb / 1024.0)
    } else {
        format!("{:.0} MB", mb)
    }
}

/// The memory list: pane rows, biggest first, one group each so the column
/// packing spreads them over the columns in reading order. Ties keep the tab
/// order they came in.
pub(super) fn memory_groups(mut panes: Vec<(u64, SwitcherRow)>) -> Vec<Vec<SwitcherRow>> {
    panes.sort_by(|a, b| b.0.cmp(&a.0));
    panes.into_iter().map(|(_, row)| vec![row]).collect()
}

pub(super) struct PaneSwitcherState {
    /// Columns of rows. Each column holds whole tabs (a tab header followed by
    /// its pane rows); a tab is never split across two columns.
    pub(super) columns: Vec<Vec<SwitcherRow>>,
    /// Selected column index.
    pub(super) selected_col: usize,
    /// Selected row within `columns[selected_col]`; always points at a `Pane` row.
    pub(super) selected_row: usize,
    /// Per-column first-visible-row offset (vertical scroll), one entry per column.
    pub(super) scroll: Vec<usize>,
    /// Fractional accumulator for trackpad/wheel scroll (sub-row deltas).
    pub(super) scroll_acc: f64,
    /// Attention-only list: every row shown is a pane asking for something.
    /// The toggle rebuilds the overlay, so this is only what the current
    /// snapshot was built with — what the title says and what the toggle flips.
    pub(super) filtered: bool,
    /// Which list is up: the open panes, the bookmarks (`b`), or the panes by
    /// memory (`m`). The list opens on the panes — what is running is what
    /// Cmd+P is asked for nine times out of ten, and the bookmarks used to push
    /// it off the screen.
    pub(super) view: SwitcherView,
    /// The memory list's total and pane count, for its title; `None` on the
    /// other lists.
    pub(super) memory_total: Option<(u64, usize)>,
    /// Every pane's (bell, unread completion) when this snapshot was built,
    /// so the tick can tell when a row's dot has gone stale.
    pub(super) attention: Vec<(PaneId, bool, bool)>,
}

/// The dim right-hand half of a bookmark row: the agent holding the
/// conversation. The directory is not repeated here — it is the header of the
/// group the row sits in. `None` for a bookmarked shell.
pub(super) fn bookmark_detail(bm: &crate::bookmarks::Bookmark) -> Option<String> {
    bm.agent.map(|a| a.as_str().to_string())
}

/// The header naming a directory of bookmarks: its last component ("api"),
/// or the whole path with `~` for home when another saved directory ends the
/// same way, so two `app` folders never share one name.
pub(super) fn bookmark_dir_header(cwd: &str, all_cwds: &[&str], home: Option<&str>) -> String {
    let base = |p: &str| {
        std::path::Path::new(p)
            .file_name()
            .and_then(|n| n.to_str())
            .filter(|n| !n.is_empty())
            .map(str::to_string)
    };
    let name = base(cwd);
    let clashes = name.is_some() && all_cwds.iter().any(|&other| other != cwd && base(other) == name);
    match name {
        Some(n) if !clashes => n,
        _ => match home.filter(|h| !h.is_empty()).and_then(|h| cwd.strip_prefix(h)) {
            Some(rest) if rest.is_empty() || rest.starts_with('/') => format!("~{}", rest),
            _ => cwd.to_string(),
        },
    }
}

/// The anchors that open the list, one group per directory — like the bookmark
/// groups below, and for the same reason: the anchors of a day span several
/// projects, and a flat list says nothing about which one a row belongs to.
/// Each header carries the anchor mark and the directory name.
///
/// `hidden` leaves out the anchors a pane already holds, exactly as the
/// bookmark groups leave out an open bookmark: that pane row wears the anchor
/// mark itself, so the conversation is listed once, where it actually is.
///
/// Nothing left to list, no group: a section with nothing under it would take a
/// line to say nothing.
pub(super) fn anchor_groups(
    saved: &[crate::anchors::Anchor],
    hidden: impl Fn(&crate::anchors::Anchor) -> bool,
) -> Vec<Vec<SwitcherRow>> {
    let home = std::env::var("HOME").ok();
    let mut dirs: Vec<&str> = Vec::new();
    let mut groups: Vec<Vec<SwitcherRow>> = Vec::new();
    for (index, anchor) in saved.iter().enumerate() {
        if hidden(anchor) {
            continue;
        }
        let row = SwitcherRow::Anchor {
            index,
            title: anchor.label.clone(),
            detail: bookmark_detail(anchor),
        };
        match dirs.iter().position(|d| *d == anchor.cwd) {
            Some(g) => groups[g].push(row),
            None => {
                dirs.push(&anchor.cwd);
                groups.push(vec![row]);
            }
        }
    }
    let all_cwds: &[&str] = &dirs;
    for (dir, group) in dirs.iter().zip(groups.iter_mut()) {
        let header = bookmark_dir_header(dir, all_cwds, home.as_deref());
        group.insert(0, SwitcherRow::TabHeader(format!("\u{2693} {}", header)));
    }
    groups
}

/// The whole list, in the order it is read: the anchors, then the tabs with
/// their panes, then the bookmarks. Today's work opens the list — whatever is
/// running below is what happened to the day, not what it was for.
///
/// `hidden` is the bookmark filter (open, or anchored); the anchors are not
/// filtered at all, which is the difference between the two sections.
pub(super) fn saved_sections(
    anchors: &[crate::anchors::Anchor],
    bookmarks: &[crate::bookmarks::Bookmark],
    tabs: Vec<Vec<SwitcherRow>>,
    hidden: impl Fn(&crate::bookmarks::Bookmark) -> bool,
    anchor_hidden: impl Fn(&crate::anchors::Anchor) -> bool,
) -> Vec<Vec<SwitcherRow>> {
    let mut groups = anchor_groups(anchors, anchor_hidden);
    groups.extend(tabs);
    groups.extend(bookmark_groups(bookmarks, hidden));
    groups
}

/// The bookmark part of the list: one group per directory, each headed by that
/// directory, holding the saved conversations that are *not* already open. A
/// bookmark whose pane is open is up in its tab, painted as a bookmark —
/// repeating it here would offer two rows for one conversation.
///
/// Groups come in the order of their newest bookmark, and rows keep the saved
/// order (newest first) inside a group. A directory whose bookmarks are all
/// open has no group; with nothing left to reopen the result is empty.
/// `hidden` also covers the anchored conversations: an anchor is already a row
/// up in its own section, and one conversation never gets two rows.
pub(super) fn bookmark_groups(
    saved: &[crate::bookmarks::Bookmark],
    hidden: impl Fn(&crate::bookmarks::Bookmark) -> bool,
) -> Vec<Vec<SwitcherRow>> {
    let home = std::env::var("HOME").ok();
    let mut dirs: Vec<&str> = Vec::new();
    let mut groups: Vec<Vec<SwitcherRow>> = Vec::new();
    for (index, bm) in saved.iter().enumerate() {
        if hidden(bm) {
            continue;
        }
        let row = SwitcherRow::Bookmark {
            index,
            title: bm.label.clone(),
            detail: bookmark_detail(bm),
        };
        match dirs.iter().position(|d| *d == bm.cwd) {
            Some(g) => groups[g].push(row),
            None => {
                dirs.push(&bm.cwd);
                groups.push(vec![row]);
            }
        }
    }
    // Only the directories that end up on screen: a name is ambiguous when two
    // *visible* groups share it, and spelling out a path to avoid a clash with a
    // row nobody sees reads as a bug.
    let all_cwds: &[&str] = &dirs;
    for (dir, group) in dirs.iter().zip(groups.iter_mut()) {
        let header = bookmark_dir_header(dir, &all_cwds, home.as_deref());
        group.insert(0, SwitcherRow::TabHeader(format!("★ {}", header)));
    }
    groups
}

/// Where the selection lands once the bookmark at `removed` (its index in the
/// saved list) is gone and the list rebuilt: on the row that took its place,
/// else the last bookmark row, so a run of Cmd+Backspace walks down the section.
/// `None` when no bookmark row is left — the caller keeps the default selection.
pub(super) fn bookmark_row_after_removal(
    columns: &[Vec<SwitcherRow>],
    removed: usize,
) -> Option<(usize, usize)> {
    let rows: Vec<(usize, usize, usize)> = columns
        .iter()
        .enumerate()
        .flat_map(|(c, col)| {
            col.iter().enumerate().filter_map(move |(r, row)| match row {
                SwitcherRow::Bookmark { index, .. } => Some((c, r, *index)),
                _ => None,
            })
        })
        .collect();
    rows.iter()
        .find(|&&(_, _, i)| i >= removed)
        .or(rows.last())
        .map(|&(c, r, _)| (c, r))
}

impl KovaView {
    /// The pane of this window still running a bookmarked conversation, if one
    /// does — jumping to it beats opening a second pane on the same session.
    pub(super) fn pane_holding_session(&self, bm: &crate::bookmarks::Bookmark) -> Option<PaneId> {
        let session_id = bm.session_id.as_deref()?;
        // A bookmark written before agents were named is a Claude one.
        let agent = bm.agent.unwrap_or(crate::agent_session::Agent::Claude);
        let tabs = self.ivars().tabs.borrow();
        let mut found = None;
        for tab in tabs.iter() {
            tab.for_each_pane(&mut |pane| {
                // A pane still asleep since launch holds it too: focusing it
                // wakes that one instead of resuming the session a second time.
                let live = pane.agent_kind() == Some(agent)
                    && pane.agent_session_id().as_deref() == Some(session_id);
                let asleep = pane
                    .dormant_session()
                    .is_some_and(|(owner, id)| owner == agent && id == session_id);
                if found.is_none() && (live || asleep) {
                    found = Some(pane.id);
                }
            });
        }
        found
    }

    /// Act on a bookmark row: focus the pane that still holds the conversation,
    /// or put it back where it belongs and run its resume line — picking the
    /// bookmark already said which conversation to bring back.
    fn open_bookmark(&self, index: usize) {
        let saved = crate::bookmarks::load();
        let Some(bm) = saved.items.get(index) else { return };
        self.open_saved_conversation(bm);
    }

    /// Act on an anchor row: same gesture as a bookmark row, on the other list.
    fn open_anchor(&self, index: usize) {
        let saved = crate::anchors::load();
        let Some(anchor) = saved.items.get(index) else { return };
        self.open_saved_conversation(anchor);
    }

    /// The pane of this window this saved conversation is already in: the one
    /// running its session, or — for a shell, which has no session to match on —
    /// a bare shell sitting in its directory. Without the second case, going
    /// back to a shell anchor would split a new pane every single time.
    pub(super) fn pane_holding_saved(&self, bm: &crate::bookmarks::Bookmark) -> Option<PaneId> {
        if bm.session_id.is_some() {
            return self.pane_holding_session(bm);
        }
        let tabs = self.ivars().tabs.borrow();
        let mut found = None;
        for tab in tabs.iter() {
            tab.for_each_pane(&mut |pane| {
                if found.is_none()
                    && pane.agent_session_id().is_none()
                    && pane.dormant_session_id().is_none()
                    && pane.cwd().as_deref() == Some(bm.cwd.as_str())
                {
                    found = Some(pane.id);
                }
            });
        }
        found
    }

    /// Bring a saved conversation back: focus the pane that still holds it,
    /// wherever it lives, or put it where it belongs and run its resume line.
    /// Shared by the bookmark rows, the anchor rows and Cmd+A — picking any of
    /// them already said which conversation to bring back.
    ///
    /// Every window is searched, not just this one: a conversation open in
    /// another window is open, and reopening it would run a second `resume` on
    /// a live session.
    pub(super) fn open_saved_conversation(&self, bm: &crate::bookmarks::Bookmark) {
        let mtm = unsafe { MainThreadMarker::new_unchecked() };
        let app = objc2_app_kit::NSApplication::sharedApplication(mtm);
        let windows = app.windows();
        for i in 0..windows.count() {
            let win = windows.objectAtIndex(i);
            let Some(view) = crate::app::kova_view(&win) else { continue };
            let Some(pane_id) = view.pane_holding_saved(bm) else { continue };
            view.ipc_focus_pane(pane_id);
            win.makeKeyAndOrderFront(None);
            view.set_pane_flash(pane_id, 30, None);
            return;
        }
        self.open_conversation_in_project(bm.resume_command(), &bm.cwd);
    }

    /// Cmd+Backspace on a bookmark row: drop it from the saved list once an
    /// alert naming it is confirmed. The list is rebuilt in place, so several
    /// dead bookmarks can go in a row without reopening Cmd+P.
    fn pane_switcher_remove_selected_bookmark(&self) {
        let index = {
            let guard = self.ivars().pane_switcher.borrow();
            match guard
                .as_ref()
                .and_then(|s| s.columns.get(s.selected_col).and_then(|c| c.get(s.selected_row)))
            {
                Some(SwitcherRow::Bookmark { index, .. }) => *index,
                _ => return,
            }
        };
        let Some(bm) = crate::bookmarks::load().items.get(index).cloned() else { return };
        // No borrow is held past this point: the alert pumps events.
        let mtm = unsafe { MainThreadMarker::new_unchecked() };
        let informative = format!(
            "\u{ab}{}\u{bb} will no longer appear in Cmd+P. The conversation itself is not deleted.",
            bm.label
        );
        if !confirm_action(mtm, "Remove this bookmark?", &informative, "Remove") {
            return;
        }
        self.pane_switcher_drop_bookmark(index, &bm);
    }

    /// Drop a bookmark and rebuild the open switcher around the gap. Shared by
    /// Cmd+Backspace (after its alert) and Cmd+B (no alert, like on a pane).
    fn pane_switcher_drop_bookmark(&self, index: usize, bm: &crate::bookmarks::Bookmark) {
        // Read the file again rather than reuse the copy from before the alert:
        // another window may have changed the list meanwhile, which is also why
        // the bookmark is found by its key and not by its old index.
        let mut saved = crate::bookmarks::load();
        crate::bookmarks::remove(&mut saved.items, bm.key());
        crate::bookmarks::save(&saved);
        *self.ivars().bookmark_keys.borrow_mut() = crate::bookmarks::keys(&saved.items);
        self.set_transient_status(&format!("Removed bookmark {}", bm.label));

        if self.ivars().pane_switcher.borrow().is_none() {
            return;
        }
        let (_, view) = self.pane_switcher_modes();
        self.open_pane_switcher(false, view);
        if let Some(state) = self.ivars().pane_switcher.borrow_mut().as_mut() {
            if let Some((c, r)) = bookmark_row_after_removal(&state.columns, index) {
                state.selected_col = c;
                state.selected_row = r;
            }
        }
        self.pane_switcher_clamp_scroll();
        self.mark_dirty();
    }

    /// What the open overlay is showing: the attention-only flag, and which
    /// list is up. Off and the open panes when no overlay is open.
    pub(super) fn pane_switcher_modes(&self) -> (bool, SwitcherView) {
        let guard = self.ivars().pane_switcher.borrow();
        guard.as_ref().map_or((false, SwitcherView::Panes), |s| (s.filtered, s.view))
    }

    /// Each pane's (bell, unread completion), as the switcher rows show them:
    /// the focused pane never asks for attention.
    fn pane_attention(&self) -> Vec<(PaneId, bool, bool)> {
        let mut out = Vec::new();
        let tabs = self.ivars().tabs.borrow();
        let active = self.ivars().active_tab.get();
        for (ti, tab) in tabs.iter().enumerate() {
            let focused_pane = tab.focused_pane;
            tab.for_each_pane(&mut |pane| {
                if ti == active && pane.id == focused_pane {
                    out.push((pane.id, false, false));
                } else {
                    let term = pane.terminal.read();
                    out.push((
                        pane.id,
                        term.bell.load(std::sync::atomic::Ordering::Relaxed),
                        term.unread_completion(),
                    ));
                }
            });
        }
        out
    }

    /// Rebuild the pane switcher if it is open, keeping the selected pane — or
    /// subject header — selected: its rows are a snapshot taken when it
    /// opened, so a change that arrives meanwhile would not show.
    pub(super) fn refresh_open_pane_switcher(&self) {
        let selected = {
            let guard = self.ivars().pane_switcher.borrow();
            let Some(state) = guard.as_ref() else { return };
            match state.columns.get(state.selected_col).and_then(|c| c.get(state.selected_row)) {
                Some(SwitcherRow::Pane { pane_id, .. }) => Some(Ok(*pane_id)),
                Some(SwitcherRow::SubjectHeader { name, tab, .. }) => Some(Err((*tab, name.clone()))),
                _ => None,
            }
        };
        let (filtered, view) = self.pane_switcher_modes();
        self.open_pane_switcher(filtered, view);
        let Some(target) = selected else { return };
        {
            let mut guard = self.ivars().pane_switcher.borrow_mut();
            let Some(state) = guard.as_mut() else { return };
            let spot = state.columns.iter().enumerate().find_map(|(c, col)| {
                col.iter()
                    .position(|r| match (r, &target) {
                        (SwitcherRow::Pane { pane_id, .. }, Ok(id)) => pane_id == id,
                        (SwitcherRow::SubjectHeader { name, tab, .. }, Err((t, n))) => tab == t && name == n,
                        _ => false,
                    })
                    .map(|r| (c, r))
            });
            if let Some((c, r)) = spot {
                state.selected_col = c;
                state.selected_row = r;
            }
        }
        self.pane_switcher_clamp_scroll();
    }

    /// Called on each tick: rebuild an open switcher whose dots no longer
    /// match the panes. A bell or a finished command arriving from a pane's
    /// output (BEL, OSC 133;D) would otherwise stay invisible until reopening.
    pub fn poll_pane_switcher(&self) {
        let stale = {
            let guard = self.ivars().pane_switcher.borrow();
            let Some(state) = guard.as_ref() else { return };
            state.attention != self.pane_attention()
        };
        if stale {
            self.refresh_open_pane_switcher();
        }
    }

    /// Open the tab/pane switcher overlay: every tab with its panes, click or
    /// Enter to focus. Selection starts on the currently-focused pane.
    ///
    /// With `filtered`, the same list keeps only the panes asking for something
    /// — the unread and waiting ones. It is the sit-down counterpart of `Cmd+J`,
    /// which walks the same panes one jump at a time without ever showing how
    /// many there are; `u` flips between the two lists once open. Two things it
    /// does not share with `Cmd+J`: this list is one window's tabs (`Cmd+J`
    /// crosses windows), and its last tier — an idle Claude session — is not an
    /// unread pane, so it is not in here either.
    ///
    /// `view` picks the full list's content. `Bookmarks` swaps the tab groups
    /// for the saved conversations — the `b` toggle. The anchors stay in both
    /// of those: they are what today is for, and they sit in the same place
    /// whatever the rest of the list shows. `Memory` (`m`) is every pane in one
    /// list, biggest first, each row ending with what its process tree holds.
    pub(super) fn open_pane_switcher(&self, filtered: bool, view: SwitcherView) {
        let show_bookmarks = view == SwitcherView::Bookmarks;
        let memory_view = !filtered && view == SwitcherView::Memory;
        let mut memory_rows: Vec<(u64, SwitcherRow)> = Vec::new();
        // Read once, up front: the same list marks the open panes below and
        // fills the bookmark groups at the end.
        let saved = crate::bookmarks::load();
        let anchors = crate::anchors::load();
        // Also the moment to refresh both caches the status bars read: another
        // window — or track, through `set-anchor` — may have changed either list
        // since this one last did.
        *self.ivars().bookmark_keys.borrow_mut() = crate::bookmarks::keys(&saved.items);
        *self.ivars().anchor_keys.borrow_mut() = crate::anchors::keys(&anchors.items);
        let anchored = crate::anchors::keys(&anchors.items);
        // Anchored conversations count as saved here too: the pane's own status
        // bar paints them, and a row that contradicted the pane it points at
        // would be the worse of the two answers.
        let anchored_sessions: std::collections::HashSet<&str> =
            anchors.items.iter().filter_map(|a| a.session_id.as_deref()).collect();
        let bookmarked_sessions: std::collections::HashSet<&str> = saved
            .items
            .iter()
            .filter_map(|b| b.session_id.as_deref())
            .chain(anchors.items.iter().filter_map(|a| a.session_id.as_deref()))
            .collect();
        // Build one row group per tab (header followed by its pane rows).
        // The conversations the panes hold, live or pre-typed: an anchor among
        // them is left out of the anchors section, since its pane row carries
        // the anchor mark.
        let mut open_sessions: std::collections::HashSet<String> = Default::default();
        let mut groups: Vec<Vec<SwitcherRow>> = Vec::new();
        // Subject boxes are drawn on the full list of open panes only: the
        // attention-only and memory lists answer other questions.
        let show_subjects = !filtered && view == SwitcherView::Panes;
        {
            let tabs = self.ivars().tabs.borrow();
            let active = self.ivars().active_tab.get();
            for (ti, tab) in tabs.iter().enumerate() {
                let mut rows: Vec<SwitcherRow> = Vec::new();
                rows.push(SwitcherRow::TabHeader(format!("{}  {}", ti + 1, tab.title())));
                let focused_pane = tab.focused_pane;
                let mut pane_rows: Vec<SwitcherRow> = Vec::new();
                let mut pane_subjects: Vec<Option<String>> = Vec::new();
                tab.for_each_pane(&mut |pane| {
                    let is_current = ti == active && pane.id == focused_pane;
                    // Attention (unread) mirrors the per-pane status-bar dot: bell
                    // or a completed command, but never on the currently-focused pane.
                    let (has_bell, has_completion) = if is_current {
                        (false, false)
                    } else {
                        let term = pane.terminal.read();
                        (
                            term.bell.load(std::sync::atomic::Ordering::Relaxed),
                            term.unread_completion(),
                        )
                    };
                    let title = pane.display_title("shell");
                    let mut process = switcher_process_label(pane.fg_process().as_ref(), &title);
                    // Probed only for the memory list: a process-tree walk per
                    // pane is not worth paying on every Cmd+P.
                    let memory = memory_view.then(|| pane.pty.tree_resident_bytes());
                    if let Some(bytes) = memory {
                        process = Some(match process {
                            Some(p) => format!("{}  {}", format_memory(bytes), p),
                            None => format_memory(bytes),
                        });
                    }
                    let session = pane.agent_session_id();
                    let bookmarked =
                        session.as_ref().is_some_and(|id| bookmarked_sessions.contains(id.as_str()));
                    let last_command = pane.last_command();
                    let anchored_pane = match session.as_ref() {
                        Some(id) => anchored_sessions.contains(id.as_str()),
                        // No agent running yet: the pane was restored and its
                        // resume line is still waiting at the prompt.
                        None => pending_session_id(last_command.as_deref())
                            .is_some_and(|id| anchored_sessions.contains(id)),
                    };
                    match session.as_ref() {
                        Some(id) => {
                            open_sessions.insert(id.clone());
                        }
                        None => {
                            if let Some(id) = pending_session_id(last_command.as_deref()) {
                                open_sessions.insert(id.to_string());
                            }
                        }
                    }
                    // The conversation's open items. A restored pane whose
                    // resume line still waits at the prompt holds it too.
                    let raf = pane
                        .claude_session_id()
                        .or_else(|| pending_session_id(last_command.as_deref()).map(str::to_string))
                        .and_then(|id| crate::raf::load(&id))
                        .unwrap_or_default();
                    let (raf_todo, raf_idea) = crate::raf::marker_parts(raf.todos.len(), raf.ideas.len());
                    let row = SwitcherRow::Pane {
                        pane_id: pane.id,
                        title,
                        is_current,
                        has_bell,
                        has_completion,
                        minimized: pane.minimized,
                        working: pane.is_working(),
                        process,
                        bookmarked,
                        anchored: anchored_pane,
                        raf_todo,
                        raf_idea,
                        subject: None,
                        last_in_subject: false,
                    };
                    match memory {
                        Some(bytes) => memory_rows.push((bytes, row)),
                        None => {
                            pane_rows.push(row);
                            pane_subjects.push(pane.subject.clone());
                        }
                    }
                });
                if show_subjects {
                    rows.extend(subject_rows(tab.id, &tab.subjects, pane_rows, &pane_subjects));
                } else {
                    rows.extend(pane_rows);
                }
                groups.push(rows);
            }
        }
        if !filtered && memory_rows.is_empty() && groups.iter().all(|g| g.iter().all(|r| !r.is_pane())) {
            return; // nothing to switch to
        }
        let memory_total = memory_view
            .then(|| (memory_rows.iter().map(|(b, _)| *b).sum::<u64>(), memory_rows.len()));
        // Saved conversations, one group per directory at the end of the list.
        // Only on the full list: the attention-only one answers "what is asking
        // for something", and a bookmark never asks for anything. Neither does
        // an anchor — same reason it is left out of the filtered list.
        if memory_view {
            groups = memory_groups(memory_rows);
        } else if !filtered {
            let tabs = if show_bookmarks { Vec::new() } else { groups };
            // On the bookmark half, an open conversation keeps its row: the pane
            // that holds it is not on screen to stand for it. Only an anchored
            // one is still left out, since its own section is right above.
            let bookmarks: &[crate::bookmarks::Bookmark] =
                if show_bookmarks { &saved.items } else { &[] };
            groups = saved_sections(
                &anchors.items,
                bookmarks,
                tabs,
                |bm| {
                    anchored.contains(bm.key())
                        || (!show_bookmarks && self.pane_holding_session(bm).is_some())
                },
                |a| a.session_id.as_deref().is_some_and(|id| open_sessions.contains(id)),
            );
        }
        // An empty filtered list still opens: the answer "nothing is unread" is
        // one the overlay has to give out loud, and `u` from there shows all the
        // panes again. Only the unfiltered list can refuse to open.
        let groups = if filtered { retain_attention_rows(groups) } else { groups };

        // Partition the tab groups into ≤3 contiguous columns, balanced by row
        // count. A group joins the current column unless closing the column now
        // (without it) lands closer to the per-column target than including it.
        let ncols = groups.len().min(3).max(1);
        let total: usize = groups.iter().map(|g| g.len()).sum();
        let mut columns: Vec<Vec<SwitcherRow>> = Vec::new();
        let mut cur: Vec<SwitcherRow> = Vec::new();
        let mut cur_w = 0usize;
        let mut placed_w = 0usize;
        for g in groups {
            let w = g.len();
            let cols_left = ncols - columns.len();
            if cols_left > 1 && !cur.is_empty() {
                let target = (total - placed_w) as f64 / cols_left as f64;
                if (cur_w as f64 - target).abs() <= ((cur_w + w) as f64 - target).abs() {
                    placed_w += cur_w;
                    columns.push(std::mem::take(&mut cur));
                    cur_w = 0;
                }
            }
            cur.extend(g);
            cur_w += w;
        }
        columns.push(cur);

        // Land on the currently-focused pane; otherwise the first pane row.
        // The memory list starts at its top: the biggest pane is what it is
        // opened for.
        let mut selected_col = 0usize;
        let mut selected_row = 0usize;
        let mut found = memory_view;
        'outer: for (c, col) in columns.iter().enumerate() {
            for (r, row) in col.iter().enumerate() {
                if matches!(row, SwitcherRow::Pane { is_current: true, .. }) {
                    selected_col = c;
                    selected_row = r;
                    found = true;
                    break 'outer;
                }
            }
        }
        if !found {
            for (c, col) in columns.iter().enumerate() {
                if let Some(r) = col.iter().position(|x| x.is_selectable()) {
                    selected_col = c;
                    selected_row = r;
                    break;
                }
            }
        }

        let scroll = vec![0usize; columns.len()];
        let attention = self.pane_attention();
        *self.ivars().pane_switcher.borrow_mut() = Some(PaneSwitcherState {
            columns,
            selected_col,
            selected_row,
            scroll,
            scroll_acc: 0.0,
            filtered,
            view,
            memory_total,
            attention,
        });
        self.pane_switcher_clamp_scroll();
        self.mark_dirty();
    }

    /// Adjust the selected column's scroll offset so the selected row stays visible.
    fn pane_switcher_clamp_scroll(&self) {
        let max_visible = {
            let renderer = match self.ivars().renderer.get() { Some(r) => r, None => return };
            let vh = self.drawable_viewport().height;
            renderer.read().overlay_list_geometry(vh).max_visible.max(1)
        };
        let mut guard = self.ivars().pane_switcher.borrow_mut();
        if let Some(state) = guard.as_mut() {
            let col = state.selected_col;
            let sel = state.selected_row;
            if let Some(sc) = state.scroll.get_mut(col) {
                if sel < *sc {
                    *sc = sel;
                } else if sel >= *sc + max_visible {
                    *sc = sel + 1 - max_visible;
                }
            }
        }
    }

    /// Handle key events in the tab/pane switcher overlay.
    pub(super) fn handle_pane_switcher_key(&self, event: &NSEvent) {
        let keycode = event.keyCode();
        log::debug!("pane switcher key: keycode={:#x} flags={:#x}", keycode, event.modifierFlags().0);

        // Escape → close
        if keycode == 0x35 {
            *self.ivars().pane_switcher.borrow_mut() = None;
            self.mark_dirty();
            return;
        }

        // Enter → focus selected pane; on a subject header, rename the subject.
        if keycode == 0x24 {
            if let Some((tab, name)) = self.pane_switcher_selected_subject() {
                self.pane_switcher_rename_subject(tab, &name);
            } else {
                self.pane_switcher_focus_selected();
            }
            return;
        }

        // `u`, or the shortcut that opens the attention-only list, flips
        // between "every pane" and "only the ones asking for something".
        // Flipping rebuilds the overlay rather than hiding rows in place: the
        // list is a snapshot of the panes either way, and rebuilding is what
        // lands the selection back where each list wants it — the focused pane
        // on the full list, the first pane that wants something on the other.
        // `b` swaps the open panes for the bookmarks, and back. Not on the
        // attention-only list, which answers a different question and has no
        // bookmark half.
        // `m` does the same with the memory list.
        let unmodified = event.charactersIgnoringModifiers().map(|s| s.to_string());

        // `c` on a subject's header: the subject's next color, box and strip
        // band at once. Read from the typed letter (AZERTY), and never with
        // Cmd held: Cmd+C reaches this handler too.
        if unmodified.as_deref().is_some_and(|c| c.eq_ignore_ascii_case("c"))
            && !event.modifierFlags().contains(NSEventModifierFlags::Command)
        {
            if let Some((tab, name)) = self.pane_switcher_selected_subject() {
                if let Some(t) = self.ivars().tabs.borrow_mut().iter_mut().find(|t| t.id == tab) {
                    t.cycle_subject_color(&name);
                }
                self.mark_dirty();
                return;
            }
        }

        let wanted = view_for_key(unmodified.as_deref());
        if let Some(wanted) = wanted.filter(|_| !event.modifierFlags().contains(NSEventModifierFlags::Command)) {
            let (filtered, view) = self.pane_switcher_modes();
            if !filtered {
                self.open_pane_switcher(false, toggled_view(view, wanted));
                return;
            }
        }

        let opens_filtered = KeyCombo::from_event(event);
        let opens_filtered = self.ivars().keybindings.get().is_some_and(|kb| {
            matches!(kb.window_map.get(&opens_filtered), Some(Action::OpenUnreadSwitcher))
        });
        if unmodified.as_deref().is_some_and(|c| c.eq_ignore_ascii_case("u")) || opens_filtered {
            let (filtered, view) = self.pane_switcher_modes();
            self.open_pane_switcher(!filtered, view);
            return;
        }

        // Cmd+↑ / Cmd+↓ → move the selected pane one step in its tab's order
        // instead of moving the selection. Minimized panes are steps like any
        // other, so the selected row travels exactly one line of the list per
        // press, and the selection follows the pane it moved. Only on the full
        // list: the attention-only one hides rows, so the pane the move steps
        // over is often not the row above or below, and the list would look
        // unchanged while the layout moved underneath. Not on the memory list
        // either, whose order is the memory and not the tab's.
        let full_list = self
            .ivars()
            .pane_switcher
            .borrow()
            .as_ref()
            .is_some_and(|s| !s.filtered && s.view != SwitcherView::Memory);
        if full_list
            && event.modifierFlags().contains(NSEventModifierFlags::Command)
            && (keycode == 0x7E || keycode == 0x7D)
        {
            // On a subject's header the whole box moves.
            if let Some((tab, name)) = self.pane_switcher_selected_subject() {
                self.pane_switcher_move_subject(tab, &name, keycode == 0x7D);
            } else {
                self.pane_switcher_move_selected(keycode == 0x7D);
            }
            return;
        }

        // Cmd+→ files the selected pane under the subject drawn right above it
        // (or asks for a new one's name); Cmd+← takes it out. Only where the
        // boxes are drawn: the full list of open panes.
        let subjects_shown = self
            .ivars()
            .pane_switcher
            .borrow()
            .as_ref()
            .is_some_and(|s| !s.filtered && s.view == SwitcherView::Panes);
        // Cmd alone: Cmd+Option+←/→ is pane navigation, and must not file or
        // unfile anything on its way through.
        let flags = event.modifierFlags();
        let cmd_only = flags.contains(NSEventModifierFlags::Command)
            && !flags.intersects(
                NSEventModifierFlags::Option | NSEventModifierFlags::Control | NSEventModifierFlags::Shift,
            );
        if subjects_shown
            && cmd_only
            && (keycode == 0x7C || keycode == 0x7B)
        {
            if keycode == 0x7C {
                self.pane_switcher_indent_selected(false);
            } else {
                self.pane_switcher_outdent_selected();
            }
            return;
        }
        // Cmd+Shift+→ always asks for a name: the way to open a new subject
        // right under a box, where Cmd+→ would join that box.
        let cmd_shift = flags.contains(NSEventModifierFlags::Command | NSEventModifierFlags::Shift)
            && !flags.intersects(NSEventModifierFlags::Option | NSEventModifierFlags::Control);
        if subjects_shown && cmd_shift && keycode == 0x7C {
            self.pane_switcher_indent_selected(true);
            return;
        }

        // Cmd+Backspace on a bookmark row → remove it, after confirmation. Same
        // key as the closed-tabs list (Cmd+O). On a subject header it deletes
        // the subject. Pane rows ignore it.
        if keycode == 0x33 && event.modifierFlags().contains(NSEventModifierFlags::Command) {
            if let Some((tab, name)) = self.pane_switcher_selected_subject() {
                self.pane_switcher_delete_subject(tab, &name);
            } else {
                self.pane_switcher_remove_selected_bookmark();
            }
            return;
        }

        // Cmd+B (the toggle-bookmark binding) → same toggle as in a pane: a pane
        // row gains or loses its bookmark, a bookmark row is dropped.
        let toggles_bookmark = KeyCombo::from_event(event);
        let toggles_bookmark = self.ivars().keybindings.get().is_some_and(|kb| {
            matches!(kb.window_map.get(&toggles_bookmark), Some(Action::ToggleBookmark))
        });
        if toggles_bookmark {
            self.pane_switcher_toggle_selected_bookmark();
            return;
        }

        // Cmd+Shift+A (the toggle-anchor binding) → the same gesture on the
        // other list: a pane row is anchored or un-anchored, an anchor row is
        // dropped. No alert on either: an anchor is a claim about today, and
        // taking it back costs one keystroke, unlike a bookmark you may have
        // been keeping for weeks.
        let toggles_anchor = KeyCombo::from_event(event);
        let toggles_anchor = self.ivars().keybindings.get().is_some_and(|kb| {
            matches!(kb.window_map.get(&toggles_anchor), Some(Action::ToggleAnchor))
        });
        if toggles_anchor {
            self.pane_switcher_toggle_selected_anchor();
            return;
        }

        // Arrow keys: ↑↓ move within a column (headers skipped), ←→ between columns.
        {
            let mut guard = self.ivars().pane_switcher.borrow_mut();
            let state = match guard.as_mut() {
                Some(s) => s,
                None => return,
            };
            match keycode {
                0x7E => { // Up
                    let col = &state.columns[state.selected_col];
                    if let Some(i) = col[..state.selected_row].iter().rposition(|r| r.is_selectable()) {
                        state.selected_row = i;
                    }
                }
                0x7D => { // Down
                    let col = &state.columns[state.selected_col];
                    if let Some(off) = col.get(state.selected_row + 1..)
                        .and_then(|tail| tail.iter().position(|r| r.is_selectable()))
                    {
                        state.selected_row = state.selected_row + 1 + off;
                    }
                }
                0x7B => { // Left
                    if state.selected_col > 0 {
                        state.selected_col -= 1;
                        state.selected_row =
                            nearest_pane_row(&state.columns[state.selected_col], state.selected_row);
                    }
                }
                0x7C => { // Right
                    if state.selected_col + 1 < state.columns.len() {
                        state.selected_col += 1;
                        state.selected_row =
                            nearest_pane_row(&state.columns[state.selected_col], state.selected_row);
                    }
                }
                0x30 => { // Tab → jump to the next pane with unread output
                    if let Some((c, r)) = next_unread_row(
                        &state.columns,
                        state.selected_col,
                        state.selected_row,
                    ) {
                        state.selected_col = c;
                        state.selected_row = r;
                    }
                }
                _ => return,
            }
        }
        self.pane_switcher_clamp_scroll();
        self.mark_dirty();
    }

    /// Cmd+B inside the switcher. A pane row toggles its bookmark and the list
    /// is rebuilt with the selection kept on that pane, so the band appears or
    /// goes away under the eye; a bookmark row is removed without an alert.
    fn pane_switcher_toggle_selected_bookmark(&self) {
        let target = {
            let guard = self.ivars().pane_switcher.borrow();
            match guard.as_ref().and_then(|s| s.columns.get(s.selected_col).and_then(|c| c.get(s.selected_row))) {
                Some(SwitcherRow::Pane { pane_id, .. }) => SwitcherTarget::Pane(*pane_id),
                Some(SwitcherRow::Bookmark { index, .. }) => SwitcherTarget::Bookmark(*index),
                _ => return,
            }
        };
        match target {
            SwitcherTarget::Bookmark(index) => {
                let Some(bm) = crate::bookmarks::load().items.get(index).cloned() else { return };
                self.pane_switcher_drop_bookmark(index, &bm);
            }
            // An anchor row has its own key (Cmd+Shift+A); Cmd+B on it would
            // bookmark what is already saved under a stronger promise.
            SwitcherTarget::Anchor(_) | SwitcherTarget::Subject(..) => {}
            SwitcherTarget::Pane(pane_id) => {
                self.toggle_pane_bookmark(pane_id);
                let (filtered, view) = self.pane_switcher_modes();
                self.open_pane_switcher(filtered, view);
                if let Some(state) = self.ivars().pane_switcher.borrow_mut().as_mut() {
                    for (c, col) in state.columns.iter().enumerate() {
                        if let Some(r) = col.iter().position(
                            |row| matches!(row, SwitcherRow::Pane { pane_id: id, .. } if *id == pane_id),
                        ) {
                            state.selected_col = c;
                            state.selected_row = r;
                            break;
                        }
                    }
                }
                self.pane_switcher_clamp_scroll();
                self.mark_dirty();
            }
        }
    }

    /// Cmd+Shift+A inside the switcher: anchor or un-anchor the pane on the
    /// selected row, or drop the selected anchor. The list is rebuilt either
    /// way, so the Anchors section grows or shrinks under the eye.
    fn pane_switcher_toggle_selected_anchor(&self) {
        let target = {
            let guard = self.ivars().pane_switcher.borrow();
            match guard.as_ref().and_then(|s| s.columns.get(s.selected_col).and_then(|c| c.get(s.selected_row))) {
                Some(SwitcherRow::Pane { pane_id, .. }) => SwitcherTarget::Pane(*pane_id),
                Some(SwitcherRow::Anchor { index, .. }) => SwitcherTarget::Anchor(*index),
                Some(SwitcherRow::Bookmark { index, .. }) => SwitcherTarget::Bookmark(*index),
                _ => return,
            }
        };
        // The conversation this ran on, by the key both lists store it under:
        // the rebuild below looks the row up again, and it may have changed
        // section in between.
        let mut acted_key: Option<String> = None;
        match target {
            SwitcherTarget::Anchor(index) => {
                let mut anchors = crate::anchors::load();
                let Some(anchor) = anchors.items.get(index).cloned() else { return };
                crate::anchors::remove(&mut anchors.items, anchor.key());
                crate::anchors::save(&anchors);
                self.refresh_anchor_keys(&anchors);
                acted_key = Some(anchor.key().to_string());
                self.set_transient_status(&format!("Removed anchor {}", anchor.label));
            }
            SwitcherTarget::Pane(pane_id) => self.toggle_pane_anchor(pane_id),
            // Never built from a subject header, which this key ignores.
            SwitcherTarget::Subject(..) => return,
            // A bookmark row is promoted rather than ignored: it is the row you
            // are most likely to be pointing at when you decide what today is
            // for. It stays bookmarked, and moves up under the anchors.
            SwitcherTarget::Bookmark(index) => {
                let Some(bm) = crate::bookmarks::load().items.get(index).cloned() else { return };
                let mut anchors = crate::anchors::load();
                let label = bm.label.clone();
                acted_key = Some(bm.key().to_string());
                // The row on screen can be one push behind the file — another
                // window, or track, may have anchored this conversation since
                // Cmd+P was opened — so the message follows what the toggle did,
                // not what the row led us to expect.
                match crate::anchors::toggle(&mut anchors.items, bm) {
                    crate::anchors::Toggled::Full => {
                        self.set_transient_status(&format!(
                            "Already {} anchors — drop one first",
                            crate::anchors::MAX_ANCHORS
                        ));
                        return;
                    }
                    outcome => {
                        crate::anchors::save(&anchors);
                        self.refresh_anchor_keys(&anchors);
                        self.set_transient_status(&if outcome == crate::anchors::Toggled::Added {
                            format!("Anchored {}", label)
                        } else {
                            format!("Removed anchor {}", label)
                        });
                    }
                }
            }
        }
        if self.ivars().pane_switcher.borrow().is_none() {
            return;
        }
        let (filtered, view) = self.pane_switcher_modes();
        self.open_pane_switcher(filtered, view);
        // Put the selection back where the eye is: the rebuild moved every row
        // below the Anchors section, which just gained or lost a line. A pane is
        // found again by its id; an anchored or un-anchored conversation by the
        // key it is saved under, since its row changed section on the way.
        if let Some(state) = self.ivars().pane_switcher.borrow_mut().as_mut() {
            let anchors = crate::anchors::load();
            let bookmarks = crate::bookmarks::load();
            let row_matches = |row: &SwitcherRow| match (row, &target) {
                (SwitcherRow::Pane { pane_id, .. }, SwitcherTarget::Pane(id)) => pane_id == id,
                // Un-anchoring a conversation a pane still runs: its row is that
                // pane's, up in its tab, since the bookmark section leaves out
                // what is open.
                (SwitcherRow::Pane { pane_id, .. }, _) => {
                    acted_key.is_some()
                        && self.pane_as_saved(*pane_id).map(|s| s.key().to_string()) == acted_key
                }
                (SwitcherRow::Anchor { index, .. }, _) => {
                    anchors.items.get(*index).map(|a| a.key()) == acted_key.as_deref()
                }
                (SwitcherRow::Bookmark { index, .. }, _) => {
                    bookmarks.items.get(*index).map(|b| b.key()) == acted_key.as_deref()
                }
                _ => false,
            };
            for (c, col) in state.columns.iter().enumerate() {
                if let Some(r) = col.iter().position(row_matches) {
                    state.selected_col = c;
                    state.selected_row = r;
                    break;
                }
            }
        }
        self.pane_switcher_clamp_scroll();
        self.mark_dirty();
    }

    /// The pane on the selected switcher row, if the selection is on one.
    fn pane_switcher_selected_pane(&self) -> Option<PaneId> {
        let guard = self.ivars().pane_switcher.borrow();
        match guard.as_ref().and_then(|s| s.columns.get(s.selected_col).and_then(|c| c.get(s.selected_row))) {
            Some(SwitcherRow::Pane { pane_id, .. }) => Some(*pane_id),
            _ => None,
        }
    }

    /// Rebuild the open switcher and put the selection back on `pane_id`,
    /// wherever the rebuild landed it.
    fn pane_switcher_reselect(&self, pane_id: PaneId) {
        let (filtered, view) = self.pane_switcher_modes();
        self.open_pane_switcher(filtered, view);
        if let Some(state) = self.ivars().pane_switcher.borrow_mut().as_mut() {
            for (c, col) in state.columns.iter().enumerate() {
                if let Some(r) = col.iter().position(
                    |row| matches!(row, SwitcherRow::Pane { pane_id: id, .. } if *id == pane_id),
                ) {
                    state.selected_col = c;
                    state.selected_row = r;
                    break;
                }
            }
        }
        self.pane_switcher_clamp_scroll();
        self.mark_dirty();
    }

    /// Move the pane on the selected switcher row one step in its tab's order.
    ///
    /// With subjects, a step is one line of the list (`subject_move`): a pane
    /// in a box trades places with the previous or next member, a pane outside
    /// jumps over a whole box. Without any, it is one step of the tab's order.
    ///
    /// The overlay is rebuilt afterwards rather than patched in place — the
    /// list is a snapshot of the panes, and the tab groups it packs into
    /// columns depend on the order — then the selection is put back on the pane
    /// that moved, wherever the rebuild landed it.
    fn pane_switcher_move_selected(&self, forward: bool) {
        let Some(pane_id) = self.pane_switcher_selected_pane() else { return };
        let moved = {
            let mut tabs = self.ivars().tabs.borrow_mut();
            match tabs.iter_mut().find(|t| t.pane(pane_id).is_some()) {
                Some(tab) => {
                    let order = tab.pane_subjects();
                    let subjects: Vec<Option<String>> = order.iter().map(|(_, s)| s.clone()).collect();
                    let p = order.iter().position(|(id, _)| *id == pane_id).unwrap_or(0);
                    match subject_move(&tab.subjects, &subjects, p, forward) {
                        SubjectMove::Stay => false,
                        SubjectMove::Swap(q) => tab.swap_pane_positions(pane_id, order[q].0),
                        SubjectMove::Steps(n) => {
                            let mut moved = false;
                            for _ in 0..n {
                                if !tab.move_pane_in_order(pane_id, forward) {
                                    break;
                                }
                                moved = true;
                            }
                            moved
                        }
                    }
                }
                None => false,
            }
        };
        if !moved {
            return;
        }
        self.resize_all_panes();
        self.pane_switcher_reselect(pane_id);
    }

    /// Cmd+Right: file the selected pane under the subject drawn right above
    /// it. With none there — the tab's header, or a pane outside any box — ask
    /// for a name: an existing subject of the tab is joined, a new one is
    /// created. A pane already in a subject stays put. Nothing moves on screen.
    ///
    /// With `ask` (Cmd+Shift+Right), the name is always asked for, even under a
    /// box, and a pane already in a subject moves to the one typed.
    fn pane_switcher_indent_selected(&self, ask: bool) {
        let selected = self.pane_switcher_selected_pane();
        log::debug!("pane switcher indent: ask={} selected={:?}", ask, selected);
        let Some(pane_id) = selected else { return };
        let above = {
            let tabs = self.ivars().tabs.borrow();
            let Some(tab) = tabs.iter().find(|t| t.pane(pane_id).is_some()) else { return };
            let order = tab.pane_subjects();
            let p = order.iter().position(|(id, _)| *id == pane_id).unwrap_or(0);
            if ask {
                None
            } else if order[p].1.is_some() {
                return;
            } else {
                let subjects: Vec<Option<String>> = order.into_iter().map(|(_, s)| s).collect();
                subject_above(&tab.subjects, &subjects, p)
            }
        };
        let name = match above {
            Some(name) => name,
            None => {
                // No borrow is held past this point: the alert pumps events.
                let mtm = unsafe { MainThreadMarker::new_unchecked() };
                let Some(typed) = prompt_text(
                    mtm,
                    "New subject",
                    "Name the subject this pane is filed under in Cmd+P. A name the tab already has joins that subject.",
                    "Create",
                    "Subject name",
                    "",
                ) else {
                    return;
                };
                let typed = typed.trim().to_string();
                if typed.is_empty() {
                    return;
                }
                typed
            }
        };
        {
            let mut tabs = self.ivars().tabs.borrow_mut();
            // Found again: the alert may have let the pane move or close.
            let Some(tab) = tabs.iter_mut().find(|t| t.pane(pane_id).is_some()) else { return };
            file_under_subject(tab, pane_id, &name);
        }
        if self.ivars().pane_switcher.borrow().is_none() {
            return;
        }
        self.pane_switcher_reselect(pane_id);
    }

    /// Cmd+Left: take the selected pane out of its subject. A pane from the
    /// middle of the box moves past its last member, so it lands right under
    /// the box. The subject stays, even left empty.
    fn pane_switcher_outdent_selected(&self) {
        let Some(pane_id) = self.pane_switcher_selected_pane() else { return };
        let moved = {
            let mut tabs = self.ivars().tabs.borrow_mut();
            let Some(tab) = tabs.iter_mut().find(|t| t.pane(pane_id).is_some()) else { return };
            let Some(moved) = take_out_of_subject(tab, pane_id) else { return };
            moved
        };
        if moved {
            self.resize_all_panes();
        }
        self.pane_switcher_reselect(pane_id);
    }

    /// The subject whose header is selected, with the tab it belongs to.
    fn pane_switcher_selected_subject(&self) -> Option<(TabId, String)> {
        let guard = self.ivars().pane_switcher.borrow();
        match guard.as_ref().and_then(|s| s.columns.get(s.selected_col).and_then(|c| c.get(s.selected_row))) {
            Some(SwitcherRow::SubjectHeader { name, tab, .. }) => Some((*tab, name.clone())),
            _ => None,
        }
    }

    /// Rebuild the open switcher and put the selection on the header of
    /// `name` in `tab`.
    fn pane_switcher_reselect_subject(&self, tab: TabId, name: &str) {
        let (filtered, view) = self.pane_switcher_modes();
        self.open_pane_switcher(filtered, view);
        if let Some(state) = self.ivars().pane_switcher.borrow_mut().as_mut() {
            let spot = state.columns.iter().enumerate().find_map(|(c, col)| {
                col.iter()
                    .position(|row| {
                        matches!(row, SwitcherRow::SubjectHeader { name: n, tab: t, .. } if *t == tab && n == name)
                    })
                    .map(|r| (c, r))
            });
            if let Some((c, r)) = spot {
                state.selected_col = c;
                state.selected_row = r;
            }
        }
        self.pane_switcher_clamp_scroll();
        self.mark_dirty();
    }

    /// Cmd+Up/Down on a subject header: move the whole box one line of the
    /// list (`subject_block_move`), and keep the header selected.
    fn pane_switcher_move_subject(&self, tab: TabId, name: &str, forward: bool) {
        let moved_layout = {
            let mut tabs = self.ivars().tabs.borrow_mut();
            let Some(t) = tabs.iter_mut().find(|t| t.id == tab) else { return };
            let order = t.pane_subjects();
            let subjects: Vec<Option<String>> = order.iter().map(|(_, s)| s.clone()).collect();
            match subject_block_move(&t.subjects, &subjects, name, forward) {
                SubjectBlockMove::Stay => return,
                SubjectBlockMove::Subjects(list) => {
                    t.subjects = list;
                    false
                }
                SubjectBlockMove::Panes(indices) => {
                    let ids: Vec<PaneId> = indices.iter().map(|&i| order[i].0).collect();
                    t.reorder_panes(&ids)
                }
            }
        };
        if moved_layout {
            self.resize_all_panes();
        }
        self.pane_switcher_reselect_subject(tab, name);
    }

    /// Enter on a subject header: ask for a new name, the current one filled
    /// in. A name the tab already has merges the two subjects.
    fn pane_switcher_rename_subject(&self, tab: TabId, old: &str) {
        // No borrow is held past this point: the alert pumps events.
        let mtm = unsafe { MainThreadMarker::new_unchecked() };
        let Some(typed) = prompt_text(
            mtm,
            "Rename subject",
            "A name this tab already has merges the two subjects.",
            "Rename",
            "Subject name",
            old,
        ) else {
            return;
        };
        let new = typed.trim().to_string();
        if new.is_empty() || new == old {
            return;
        }
        {
            let mut tabs = self.ivars().tabs.borrow_mut();
            let Some(t) = tabs.iter_mut().find(|t| t.id == tab) else { return };
            t.rename_subject(old, &new);
        }
        self.set_transient_status(&format!("Renamed subject {} to {}", old, new));
        if self.ivars().pane_switcher.borrow().is_some() {
            self.pane_switcher_reselect_subject(tab, &new);
        }
    }

    /// Cmd+Backspace on a subject header: delete the subject. Its panes stay
    /// open where they are, out of the box. An empty subject goes at once; one
    /// that holds panes asks first.
    fn pane_switcher_delete_subject(&self, tab: TabId, name: &str) {
        let members = {
            let tabs = self.ivars().tabs.borrow();
            let Some(t) = tabs.iter().find(|t| t.id == tab) else { return };
            t.pane_subjects().iter().filter(|(_, s)| s.as_deref() == Some(name)).count()
        };
        if members > 0 {
            // No borrow is held past this point: the alert pumps events.
            let mtm = unsafe { MainThreadMarker::new_unchecked() };
            let informative = format!(
                "\u{ab}{}\u{bb} holds {} pane{}. They stay open, out of the box.",
                name,
                members,
                if members == 1 { "" } else { "s" }
            );
            if !confirm_action(mtm, "Delete this subject?", &informative, "Delete") {
                return;
            }
        }
        {
            let mut tabs = self.ivars().tabs.borrow_mut();
            let Some(t) = tabs.iter_mut().find(|t| t.id == tab) else { return };
            t.delete_subject(name);
        }
        self.set_transient_status(&format!("Deleted subject {}", name));
        if self.ivars().pane_switcher.borrow().is_none() {
            return;
        }
        // The selection goes to the tab's first pane: the rows are rebuilt and
        // may change columns, so no saved position would still be right.
        let first = {
            let tabs = self.ivars().tabs.borrow();
            tabs.iter().find(|t| t.id == tab).and_then(|t| t.pane_subjects().first().map(|(id, _)| *id))
        };
        match first {
            Some(pane_id) => self.pane_switcher_reselect(pane_id),
            None => {
                let (filtered, view) = self.pane_switcher_modes();
                self.open_pane_switcher(filtered, view);
            }
        }
    }

    /// Act on the currently-selected switcher row and close the overlay: focus
    /// a pane, or reopen a bookmarked conversation.
    fn pane_switcher_focus_selected(&self) {
        let target = {
            let guard = self.ivars().pane_switcher.borrow();
            guard.as_ref().and_then(|s| {
                match s.columns.get(s.selected_col).and_then(|c| c.get(s.selected_row)) {
                    Some(SwitcherRow::Pane { pane_id, .. }) => Some(SwitcherTarget::Pane(*pane_id)),
                    Some(SwitcherRow::Bookmark { index, .. }) => {
                        Some(SwitcherTarget::Bookmark(*index))
                    }
                    Some(SwitcherRow::Anchor { index, .. }) => {
                        Some(SwitcherTarget::Anchor(*index))
                    }
                    _ => None,
                }
            })
        };
        *self.ivars().pane_switcher.borrow_mut() = None;
        match target {
            Some(SwitcherTarget::Pane(pid)) => {
                self.ipc_focus_pane(pid);
            }
            Some(SwitcherTarget::Bookmark(index)) => self.open_bookmark(index),
            Some(SwitcherTarget::Anchor(index)) => self.open_anchor(index),
            Some(SwitcherTarget::Subject(..)) | None => {}
        }
        self.mark_dirty();
    }

    /// Scroll the switcher column under the cursor with the mouse wheel / trackpad.
    /// Adjusts only the vertical row offset; selection is unchanged.
    pub(super) fn handle_pane_switcher_scroll(&self, event: &NSEvent, is_trackpad: bool) {
        let (px, _py) = self.event_to_pixel(event);
        let max_visible = {
            let renderer = match self.ivars().renderer.get() { Some(r) => r, None => return };
            let vh = self.drawable_viewport().height;
            renderer.read().overlay_list_geometry(vh).max_visible.max(1)
        };
        let vw = self.drawable_viewport().width;

        let mut guard = self.ivars().pane_switcher.borrow_mut();
        let state = match guard.as_mut() {
            Some(s) => s,
            None => return,
        };
        let ncols = state.columns.len().max(1);
        let col = ((px / (vw / ncols as f32)).floor() as usize).min(ncols - 1);

        // Natural scrolling: dragging content up (negative deltaY) moves the list down.
        let dy = event.scrollingDeltaY();
        let lines = if is_trackpad {
            let acc = state.scroll_acc - dy / 8.0;
            let discrete = acc.trunc();
            state.scroll_acc = acc - discrete;
            discrete as i32
        } else {
            state.scroll_acc = 0.0;
            -dy as i32
        };
        if lines == 0 {
            return;
        }

        let col_len = state.columns[col].len();
        let max_scroll = col_len.saturating_sub(max_visible);
        if let Some(sc) = state.scroll.get_mut(col) {
            let next = (*sc as i64 + lines as i64).clamp(0, max_scroll as i64) as usize;
            if next != *sc {
                *sc = next;
                drop(guard);
                self.mark_dirty();
            }
        }
    }

    /// Handle a click in the tab/pane switcher overlay. A click on a pane row
    /// focuses it; a click anywhere else dismisses the overlay.
    pub(super) fn handle_pane_switcher_click(&self, px: f32, py: f32) {
        let target = {
            let renderer = match self.ivars().renderer.get() { Some(r) => r, None => return };
            let vp = self.drawable_viewport();
            let geom = renderer.read().overlay_list_geometry(vp.height);
            let guard = self.ivars().pane_switcher.borrow();
            let state = match guard.as_ref() {
                Some(s) => s,
                None => return,
            };
            let ncols = state.columns.len().max(1);
            let col = ((px / (vp.width / ncols as f32)).floor() as usize).min(ncols - 1);
            if py < geom.content_top {
                None
            } else {
                let vis = ((py - geom.content_top) / geom.row_height).floor() as usize;
                if vis >= geom.max_visible {
                    None
                } else {
                    let idx = state.scroll.get(col).copied().unwrap_or(0) + vis;
                    match state.columns[col].get(idx) {
                        Some(SwitcherRow::SubjectHeader { name, tab, .. }) => {
                            Some(SwitcherTarget::Subject(*tab, name.clone(), col, idx))
                        }
                        Some(SwitcherRow::Pane { pane_id, .. }) => {
                            Some(SwitcherTarget::Pane(*pane_id))
                        }
                        Some(SwitcherRow::Bookmark { index, .. }) => {
                            Some(SwitcherTarget::Bookmark(*index))
                        }
                        Some(SwitcherRow::Anchor { index, .. }) => {
                            Some(SwitcherTarget::Anchor(*index))
                        }
                        _ => None,
                    }
                }
            }
        };
        // A subject header is not something to go to: a click on it does what
        // Enter does there, and the list stays open.
        if let Some(SwitcherTarget::Subject(tab, name, c, r)) = target {
            if let Some(state) = self.ivars().pane_switcher.borrow_mut().as_mut() {
                state.selected_col = c;
                state.selected_row = r;
            }
            self.mark_dirty();
            self.pane_switcher_rename_subject(tab, &name);
            return;
        }
        *self.ivars().pane_switcher.borrow_mut() = None;
        match target {
            Some(SwitcherTarget::Pane(pid)) => {
                self.ipc_focus_pane(pid);
            }
            Some(SwitcherTarget::Bookmark(index)) => self.open_bookmark(index),
            Some(SwitcherTarget::Anchor(index)) => self.open_anchor(index),
            Some(SwitcherTarget::Subject(..)) | None => {}
        }
        self.mark_dirty();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a switcher grid from a compact spec: one string per column, one
    /// char per row — 'h' header, '.' plain pane, 'b' pane with a pending bell,
    /// 'c' pane with an unread completion.
    fn switcher_grid(spec: &[&str]) -> Vec<Vec<SwitcherRow>> {
        spec.iter()
            .map(|col| {
                col.chars()
                    .map(|c| match c {
                        'h' => SwitcherRow::TabHeader("tab".into()),
                        'k' => SwitcherRow::Bookmark {
                            index: 0,
                            title: "saved".into(),
                            detail: None,
                        },
                        _ => SwitcherRow::Pane {
                            pane_id: 0,
                            title: "p".into(),
                            is_current: false,
                            has_bell: c == 'b',
                            has_completion: c == 'c',
                            minimized: false,
                            working: false,
                            process: None,
                            bookmarked: false,
                            anchored: c == 'a',
                            raf_todo: None,
                            raf_idea: None,
                            subject: None,
                            last_in_subject: false,
                        },
                    })
                    .collect()
            })
            .collect()
    }

    /// Render a grid back to the compact spec, so a filtered grid can be
    /// compared row by row: 'h' header, 'b' bell, 'c' completion, '.' a pane
    /// asking for nothing.
    fn switcher_spec(groups: &[Vec<SwitcherRow>]) -> Vec<String> {
        groups
            .iter()
            .map(|col| {
                col.iter()
                    .map(|r| match r {
                        SwitcherRow::TabHeader(_) => 'h',
                        SwitcherRow::SubjectHeader { .. } => 's',
                        SwitcherRow::Pane { has_bell: true, .. } => 'b',
                        SwitcherRow::Pane { has_completion: true, .. } => 'c',
                        SwitcherRow::Pane { .. } => '.',
                        SwitcherRow::Bookmark { .. } => 'k',
                        SwitcherRow::Anchor { .. } => 'a',
                    })
                    .collect()
            })
            .collect()
    }

    fn saved(id: &str) -> crate::bookmarks::Bookmark {
        crate::bookmarks::Bookmark {
            config_dir: None,
            launcher: None,
            agent: Some(crate::agent_session::Agent::Claude),
            session_id: Some(id.into()),
            cwd: "/Users/x/projects/kova".into(),
            label: format!("conv {}", id),
        }
    }

    #[test]
    fn the_list_reads_anchors_then_panes_then_bookmarks() {
        // The order is the whole point of the section: today's work first, what
        // is actually running second, the rest after.
        let tabs = switcher_grid(&["h.."]);
        let groups = saved_sections(&[saved("a")], &[saved("b")], tabs, |_| false, |_| false);
        assert_eq!(switcher_spec(&groups), vec!["ha".to_string(), "h..".into(), "hk".into()]);
    }

    #[test]
    fn an_open_conversation_leaves_both_saved_sections() {
        // Same conversation in both lists, and open in a pane: no row in either
        // section — the pane row up in its tab is the one, and it wears the
        // anchor mark.
        let open = |id: Option<&str>| id == Some("a");
        let groups = saved_sections(
            &[saved("a")],
            &[saved("a")],
            Vec::new(),
            |bm| open(bm.session_id.as_deref()),
            |a| open(a.session_id.as_deref()),
        );
        assert!(groups.is_empty());
    }

    #[test]
    fn the_anchor_section_lists_every_anchor_no_pane_holds() {
        // What the section is for: today's work that is not on screen. The open
        // ones are up in their tabs, under the anchor mark.
        let groups = anchor_groups(&[saved("a"), saved("b")], |_| false);
        assert_eq!(switcher_spec(&groups), vec!["haa".to_string()]);
        match &groups[0][0] {
            SwitcherRow::TabHeader(h) => {
                assert_eq!(h, "\u{2693} kova", "the anchor mark, then the directory");
            }
            _ => panic!("the section opens with its header"),
        }
    }

    #[test]
    fn no_anchor_means_no_section_at_all() {
        assert!(anchor_groups(&[], |_| false).is_empty(), "an empty section would cost a line to say nothing");
    }

    #[test]
    fn an_anchored_conversation_is_not_offered_a_second_time_as_a_bookmark() {
        let items = vec![saved("a"), saved("b")];
        let anchored = crate::anchors::keys(&[saved("a")]);
        let groups = bookmark_groups(&items, |bm| anchored.contains(bm.key()));
        assert_eq!(switcher_spec(&groups), vec!["hk".to_string()]);
        match &groups[0][1] {
            SwitcherRow::Bookmark { index, .. } => assert_eq!(*index, 1, "the row left is the other one"),
            _ => panic!("expected the bookmark row"),
        }
    }

    #[test]
    fn a_restored_pane_is_matched_on_the_resume_line_waiting_in_it() {
        assert_eq!(
            pending_session_id(Some("claude --dangerously-skip-permissions --resume abc-123")),
            Some("abc-123")
        );
        assert_eq!(pending_session_id(Some("codex resume xyz")), Some("xyz"));
        assert_eq!(pending_session_id(Some("npm run dev")), None);
        assert_eq!(pending_session_id(None), None);
    }

    #[test]
    fn an_anchor_row_is_selectable_but_is_not_a_pane() {
        let row = SwitcherRow::Anchor { index: 0, title: "conv".into(), detail: None };
        assert!(row.is_selectable());
        assert!(!row.is_pane());
        assert!(!row.needs_attention(), "an anchor asks for nothing, like a bookmark");
    }

    #[test]
    fn an_open_bookmark_leaves_its_directory_group() {
        let items = vec![saved("a"), saved("b")];
        let groups = bookmark_groups(&items, |bm| bm.session_id.as_deref() == Some("a"));
        assert_eq!(switcher_spec(&groups), vec!["hk".to_string()]);
        // The row still points at the bookmark's own index in the saved list,
        // not at its position in the filtered rows.
        match &groups[0][1] {
            SwitcherRow::Bookmark { index, .. } => assert_eq!(*index, 1),
            _ => panic!("expected a bookmark row"),
        }
    }

    #[test]
    fn bookmarks_are_grouped_by_directory_in_order_of_their_newest() {
        let at = |id: &str, cwd: &str| crate::bookmarks::Bookmark { cwd: cwd.into(), ..saved(id) };
        let items = vec![at("a", "/h/api"), at("b", "/h/web"), at("c", "/h/api")];
        let groups = bookmark_groups(&items, |_| false);
        assert_eq!(switcher_spec(&groups), vec!["hkk".to_string(), "hk".to_string()]);
        let indexes: Vec<usize> = groups[0]
            .iter()
            .filter_map(|r| match r {
                SwitcherRow::Bookmark { index, .. } => Some(*index),
                _ => None,
            })
            .collect();
        assert_eq!(indexes, vec![0, 2]);
        match (&groups[0][0], &groups[1][0]) {
            (SwitcherRow::TabHeader(first), SwitcherRow::TabHeader(second)) => {
                assert_eq!(first, "★ api");
                assert_eq!(second, "★ web");
            }
            _ => panic!("each group starts with its directory"),
        }
    }

    #[test]
    fn a_directory_header_shows_the_path_only_when_names_clash() {
        let cwds = ["/Users/x/a/app", "/Users/x/b/app", "/Users/x/api"];
        assert_eq!(bookmark_dir_header("/Users/x/api", &cwds, Some("/Users/x")), "api");
        assert_eq!(bookmark_dir_header("/Users/x/a/app", &cwds, Some("/Users/x")), "~/a/app");
        assert_eq!(bookmark_dir_header("/opt/app", &["/opt/app", "/srv/app"], Some("/Users/x")), "/opt/app");
        // "/Users/xy" is not under "/Users/x".
        assert_eq!(bookmark_dir_header("/Users/xy/app", &["/Users/xy/app", "/app"], Some("/Users/x")), "/Users/xy/app");
    }

    #[test]
    fn after_a_removal_the_selection_takes_the_next_bookmark_then_the_last() {
        let bookmark = |index: usize| SwitcherRow::Bookmark { index, title: "b".into(), detail: None };
        // Saved list after removing index 1: what was 2 and 3 are now 1 and 2,
        // and the bookmark at 0 is open in a pane, so it has no row.
        let columns = vec![
            switcher_grid(&["h."]).remove(0),
            vec![SwitcherRow::TabHeader("Bookmarks".into()), bookmark(1), bookmark(2)],
        ];
        assert_eq!(bookmark_row_after_removal(&columns, 1), Some((1, 1)));
        // The last one removed: fall back on the row above it.
        assert_eq!(bookmark_row_after_removal(&columns, 3), Some((1, 2)));
        // No bookmark left: the caller keeps its own selection.
        assert_eq!(bookmark_row_after_removal(&switcher_grid(&["h."]), 0), None);
    }

    #[test]
    fn bookmark_groups_disappear_when_every_bookmark_is_open() {
        let items = vec![saved("a"), saved("b")];
        assert!(bookmark_groups(&items, |_| true).is_empty(), "no header without rows");
        assert!(bookmark_groups(&[], |_| false).is_empty());
    }

    #[test]
    fn the_attention_list_drops_the_bookmark_section_whole() {
        // Bookmarks never ask for anything, so the filtered list must not keep
        // them — header included.
        let grid = switcher_grid(&["hb.", "hkk"]);
        assert_eq!(switcher_spec(&retain_attention_rows(grid)), vec!["hb".to_string()]);
    }

    #[test]
    fn a_bookmark_row_is_selectable_but_is_not_a_pane() {
        let col = &switcher_grid(&["hk"])[0];
        assert!(col[1].is_selectable());
        assert!(!col[1].is_pane());
        assert!(!col[0].is_selectable(), "a header is never selected");
    }

    #[test]
    fn a_bookmark_row_names_its_agent_only() {
        let bm = crate::bookmarks::Bookmark {
            config_dir: None,
            launcher: None,
            agent: Some(crate::agent_session::Agent::Codex),
            session_id: Some("id".into()),
            cwd: "/Users/x/projects/kova".into(),
            label: "the split refactor".into(),
        };
        assert_eq!(bookmark_detail(&bm).as_deref(), Some("codex"));
        let shell = crate::bookmarks::Bookmark {
            config_dir: None,
            launcher: None,
            agent: None,
            session_id: None,
            cwd: "/Users/x/projects/kova".into(),
            label: "shell".into(),
        };
        assert_eq!(bookmark_detail(&shell), None);
    }

    #[test]
    fn nearest_pane_row_lands_on_the_closest_selectable_row() {
        // Column of one header then three panes. A target already on a pane
        // stays there; a target on the header slides down to the pane below.
        let col = &switcher_grid(&["h..."])[0];
        assert_eq!(nearest_pane_row(col, 2), 2);
        assert_eq!(nearest_pane_row(col, 0), 1);
    }

    #[test]
    fn nearest_pane_row_prefers_the_row_above_on_a_tie() {
        // Header sandwiched between two panes: rows 0 and 2 are both one step
        // from the target, and `min_by_key` keeps the first it meets.
        let col = vec![
            switcher_grid(&["."])[0].pop().unwrap(),
            SwitcherRow::TabHeader("tab".into()),
            switcher_grid(&["."])[0].pop().unwrap(),
        ];
        assert_eq!(nearest_pane_row(&col, 1), 0);
    }

    #[test]
    fn nearest_pane_row_falls_back_to_zero_without_a_pane() {
        // A column of headers only has nothing to select: row 0, not a panic.
        let col = &switcher_grid(&["hh"])[0];
        assert_eq!(nearest_pane_row(col, 1), 0);
    }

    #[test]
    fn nearest_pane_row_clamps_a_target_past_the_end() {
        // The selection can outlive the rebuild that shortened the column.
        let col = &switcher_grid(&["h.."])[0];
        assert_eq!(nearest_pane_row(col, 99), 2);
    }

    #[test]
    fn attention_filter_keeps_the_markers_and_drops_quiet_panes() {
        let groups = switcher_grid(&["h.b.b", "h..c"]);
        assert_eq!(switcher_spec(&retain_attention_rows(groups)), vec!["hbb", "hc"]);
    }

    #[test]
    fn attention_filter_drops_a_whole_tab_whose_panes_are_all_quiet() {
        // Second tab has nothing to say: its header goes with its panes rather
        // than standing alone over an empty group.
        let groups = switcher_grid(&["h.b", "h..", "hc"]);
        assert_eq!(switcher_spec(&retain_attention_rows(groups)), vec!["hb", "hc"]);
    }

    #[test]
    fn attention_filter_can_end_up_with_nothing_at_all() {
        // Every pane quiet: the caller opens an empty overlay that says so,
        // instead of a list that looks like the full one minus a few rows.
        let groups = switcher_grid(&["h..", "h."]);
        assert!(retain_attention_rows(groups).is_empty());
    }

    #[test]
    fn a_working_pane_is_not_asking_for_anything() {
        // Mirrors Cmd+J: a session still chewing has nothing to hand over, so
        // it stays out of the attention list even though the row draws a ✳.
        let working = SwitcherRow::Pane {
            pane_id: 0,
            title: "p".into(),
            is_current: false,
            has_bell: false,
            has_completion: false,
            minimized: false,
            working: true,
            process: None,
            bookmarked: false,
            anchored: false,
            raf_todo: None,
            raf_idea: None,
            subject: None,
            last_in_subject: false,
        };
        assert!(!working.needs_attention());
    }

    #[test]
    fn switcher_row_names_the_binary_and_its_version() {
        let claude = ProcessInfo { name: "claude".into(), version: Some("2.1.226".into()) };
        assert_eq!(
            switcher_process_label(Some(&claude), "Corriger child_processes"),
            Some("claude 2.1.226".to_string())
        );
    }

    #[test]
    fn switcher_row_stays_quiet_when_the_title_is_already_the_binary() {
        let vim = ProcessInfo { name: "vim".into(), version: None };
        assert_eq!(switcher_process_label(Some(&vim), "vim"), None);
        // A shell prompt has no foreground process at all.
        assert_eq!(switcher_process_label(None, "kova"), None);
    }

    #[test]
    fn tab_jumps_to_the_next_unread_pane_forward() {
        // Column 0: header, pane, unread pane. Column 1: header, unread pane.
        let g = switcher_grid(&["h.b", "hc"]);
        assert_eq!(next_unread_row(&g, 0, 1), Some((0, 2)));
        // From the last unread row of column 0, cross into the next column.
        assert_eq!(next_unread_row(&g, 0, 2), Some((1, 1)));
    }

    #[test]
    fn tab_wraps_around_to_the_first_unread_pane() {
        let g = switcher_grid(&["h.b", "h."]);
        // Past the only unread row, the search wraps back onto it.
        assert_eq!(next_unread_row(&g, 1, 1), Some((0, 2)));
        // Standing on the only unread row, Tab cycles back to itself rather
        // than reporting "nothing found".
        assert_eq!(next_unread_row(&g, 0, 2), Some((0, 2)));
    }

    #[test]
    fn tab_does_nothing_when_no_pane_is_unread() {
        let g = switcher_grid(&["h..", "h."]);
        assert_eq!(next_unread_row(&g, 0, 1), None);
        assert_eq!(next_unread_row(&[], 0, 0), None);
    }

    #[test]
    fn tab_never_lands_on_a_tab_header() {
        // Headers are not selectable; a header row must never be returned even
        // when it sits between the cursor and the unread pane.
        let g = switcher_grid(&["hb", "hc"]);
        let (c, r) = next_unread_row(&g, 0, 1).expect("an unread pane exists");
        assert!(matches!(g[c][r], SwitcherRow::Pane { .. }));
    }

    fn pane_titled(title: &str) -> SwitcherRow {
        match switcher_grid(&["."]).remove(0).remove(0) {
            SwitcherRow::Pane { pane_id, is_current, has_bell, has_completion, minimized, working, process, bookmarked, anchored, raf_todo, raf_idea, subject, last_in_subject, .. } => {
                SwitcherRow::Pane { pane_id, title: title.into(), is_current, has_bell, has_completion, minimized, working, process, bookmarked, anchored, raf_todo, raf_idea, subject, last_in_subject }
            }
            _ => unreachable!(),
        }
    }

    fn titles(groups: &[Vec<SwitcherRow>]) -> Vec<String> {
        groups
            .iter()
            .flatten()
            .map(|r| match r {
                SwitcherRow::Pane { title, .. } => title.clone(),
                _ => "?".into(),
            })
            .collect()
    }

    #[test]
    fn the_memory_list_puts_the_biggest_pane_first_and_keeps_tab_order_on_ties() {
        let groups = memory_groups(vec![
            (100, pane_titled("small")),
            (900, pane_titled("big")),
            (500, pane_titled("tie-first")),
            (500, pane_titled("tie-second")),
        ]);
        assert_eq!(titles(&groups), ["big", "tie-first", "tie-second", "small"]);
        // One group per row, so the column packing can spread them.
        assert!(groups.iter().all(|g| g.len() == 1));
    }

    #[test]
    fn memory_reads_in_mb_then_gb() {
        assert_eq!(format_memory(0), "0 MB");
        assert_eq!(format_memory(312 * 1024 * 1024), "312 MB");
        assert_eq!(format_memory(1023 * 1024 * 1024), "1023 MB");
        assert_eq!(format_memory(1024 * 1024 * 1024), "1.0 GB");
        assert_eq!(format_memory((13.8 * 1024.0 * 1024.0 * 1024.0) as u64), "13.8 GB");
    }

    #[test]
    fn b_and_m_flip_their_own_list_and_jump_from_the_other() {
        use SwitcherView::*;
        assert_eq!(toggled_view(Panes, Memory), Memory);
        assert_eq!(toggled_view(Memory, Memory), Panes);
        assert_eq!(toggled_view(Bookmarks, Memory), Memory);
        assert_eq!(toggled_view(Memory, Bookmarks), Bookmarks);
        assert_eq!(toggled_view(Bookmarks, Bookmarks), Panes);
    }

    #[test]
    fn b_and_m_are_read_from_the_typed_letter_whatever_the_layout() {
        use SwitcherView::*;
        assert_eq!(view_for_key(Some("m")), Some(Memory));
        assert_eq!(view_for_key(Some("M")), Some(Memory));
        assert_eq!(view_for_key(Some("b")), Some(Bookmarks));
        // QWERTY's `m` key on AZERTY types a comma: not the memory list.
        assert_eq!(view_for_key(Some(",")), None);
        assert_eq!(view_for_key(None), None);
    }

    fn subs(spec: &[Option<&str>]) -> Vec<Option<String>> {
        spec.iter().map(|s| s.map(str::to_string)).collect()
    }

    fn names(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    /// An outline in short: `[name` opens a box (`[name]` an empty one), a
    /// number is a pane by its index, `-3` a member and `-3]` the box's last.
    fn outline_spec(items: &[OutlineItem]) -> Vec<String> {
        items
            .iter()
            .map(|it| match it {
                OutlineItem::Subject { name, empty: true } => format!("[{}]", name),
                OutlineItem::Subject { name, empty: false } => format!("[{}", name),
                OutlineItem::Pane { index, subject: None, .. } => index.to_string(),
                OutlineItem::Pane { index, last: false, .. } => format!("-{}", index),
                OutlineItem::Pane { index, last: true, .. } => format!("-{}]", index),
            })
            .collect()
    }

    #[test]
    fn without_subjects_the_outline_is_the_pane_order() {
        let panes = subs(&[None, None, None]);
        assert_eq!(outline_spec(&subject_outline(&[], &panes)), ["0", "1", "2"]);
    }

    #[test]
    fn empty_subjects_open_the_tab_then_boxes_sit_at_their_first_member() {
        let panes = subs(&[None, Some("a"), Some("a"), None]);
        let outline = subject_outline(&names(&["a", "idle", "later"]), &panes);
        assert_eq!(outline_spec(&outline), ["[idle]", "[later]", "0", "[a", "-1", "-2]", "3"]);
    }

    #[test]
    fn members_torn_apart_are_gathered_under_the_first_one() {
        // A split at the end of the tab, or an IPC move, put another pane
        // between two members: one box, at the first member, in pane order.
        let panes = subs(&[Some("a"), None, Some("a")]);
        assert_eq!(outline_spec(&subject_outline(&names(&["a"]), &panes)), ["[a", "-0", "-2]", "1"]);
    }

    #[test]
    fn subject_rows_insert_the_headers_and_tag_each_member() {
        let rows = switcher_grid(&["...."]).remove(0);
        let panes = subs(&[None, Some("a"), Some("a"), None]);
        let out = subject_rows(7, &names(&["a", "idle"]), rows, &panes);
        assert_eq!(switcher_spec(&[out]), vec!["s.s...".to_string()]);
        let out = subject_rows(7, &names(&["a", "idle"]), switcher_grid(&["...."]).remove(0), &panes);
        let tags: Vec<(Option<&str>, bool)> = out
            .iter()
            .filter_map(|r| match r {
                SwitcherRow::Pane { subject, last_in_subject, .. } => Some((subject.as_deref(), *last_in_subject)),
                _ => None,
            })
            .collect();
        assert_eq!(tags, [(None, false), (Some("a"), false), (Some("a"), true), (None, false)]);
        match &out[0] {
            SwitcherRow::SubjectHeader { name, empty, tab } => assert!(name == "idle" && *empty && *tab == 7),
            _ => panic!("the empty subject comes first"),
        }
    }

    #[test]
    fn a_subject_header_is_selectable_and_leaves_the_attention_list() {
        let header = SwitcherRow::SubjectHeader { name: "a".into(), empty: true, tab: 0 };
        assert!(header.is_selectable(), "Enter renames it, Cmd+Backspace deletes it");
        assert!(!header.is_pane());
        let mut group = switcher_grid(&["hb"]).remove(0);
        group.insert(1, header);
        assert_eq!(switcher_spec(&retain_attention_rows(vec![group])), vec!["hb".to_string()]);
    }

    #[test]
    fn cmd_right_files_under_the_subject_drawn_right_above() {
        let subjects = names(&["a", "idle"]);
        let panes = subs(&[None, Some("a"), Some("a"), None, None]);
        // Right under the box's last member: joins it.
        assert_eq!(subject_above(&subjects, &panes, 3).as_deref(), Some("a"));
        // Right under a pane outside any box: a name is asked for.
        assert_eq!(subject_above(&subjects, &panes, 4), None);
        // The first pane sits under the empty subjects: it joins the last one.
        assert_eq!(subject_above(&subjects, &panes, 0).as_deref(), Some("idle"));
        // Right under the tab's header: a name is asked for.
        assert_eq!(subject_above(&[], &subs(&[None, None]), 0), None);
        // A box's first member sits under the header of its own box.
        assert_eq!(subject_above(&subjects, &panes, 1).as_deref(), Some("a"));
    }

    #[test]
    fn cmd_left_moves_a_member_past_the_rest_of_its_box() {
        let panes = subs(&[None, Some("a"), Some("a"), Some("a"), None]);
        assert_eq!(outdent_steps(&panes, 1), 2, "from the top of the box, under its last member");
        assert_eq!(outdent_steps(&panes, 2), 1);
        assert_eq!(outdent_steps(&panes, 3), 0, "the last member is already right above the outside");
        assert_eq!(outdent_steps(&panes, 0), 0, "not in a subject");
    }

    #[test]
    fn subject_listing_follows_the_boxes_cmd_p_draws() {
        let panes: Vec<(PaneId, Option<String>)> = vec![
            (10, Some("a".into())),
            (11, None),
            (12, Some("b".into())),
            (13, Some("a".into())),
        ];
        let listing = subject_listing(&names(&["b", "a", "idle"]), &panes);
        assert_eq!(
            listing,
            vec![
                ("idle".to_string(), vec![]),
                ("a".to_string(), vec![10, 13]),
                ("b".to_string(), vec![12]),
            ],
            "empty subjects first, then each box at its first member"
        );
        assert!(subject_listing(&[], &[(1, None)]).is_empty());
    }

    #[test]
    fn filing_and_unfiling_a_pane_as_cmd_right_and_left_do() {
        let (a, _fa) = Pane::capture(10, 2);
        let (b, _fb) = Pane::capture(10, 2);
        let (c, _fc) = Pane::capture(10, 2);
        let (ia, ib, ic) = (a.id, b.id, c.id);
        let mut tab = Tab::with_columns(vec![vec![a], vec![b], vec![c]]);

        assert_eq!(file_under_subject(&mut tab, ia, "auth"), Some(true), "a new name is created");
        assert_eq!(file_under_subject(&mut tab, ic, "auth"), Some(false), "an existing one is joined");
        assert_eq!(tab.subjects, ["auth"]);
        assert_eq!(file_under_subject(&mut tab, 9999, "auth"), None);

        assert_eq!(take_out_of_subject(&mut tab, ib), None, "not in a subject");
        // The first member leaves past the last one: it lands under the box.
        assert_eq!(take_out_of_subject(&mut tab, ia), Some(true));
        let order: Vec<_> = tab.pane_subjects();
        assert_eq!(order, vec![(ib, None), (ic, Some("auth".to_string())), (ia, None)]);
        assert_eq!(take_out_of_subject(&mut tab, ic), Some(false), "the last member stays put");
        assert_eq!(tab.subjects, ["auth"], "the subject stays, empty");
    }

    #[test]
    fn without_subjects_cmd_up_down_is_still_one_step() {
        let panes = subs(&[None, None, None]);
        assert_eq!(subject_move(&[], &panes, 1, true), SubjectMove::Steps(1));
        assert_eq!(subject_move(&[], &panes, 1, false), SubjectMove::Steps(1));
        assert_eq!(subject_move(&[], &panes, 0, false), SubjectMove::Stay);
        assert_eq!(subject_move(&[], &panes, 2, true), SubjectMove::Stay);
    }

    #[test]
    fn a_box_moves_whole_over_the_pane_or_box_next_to_it() {
        use SubjectBlockMove::*;
        // 0, [a: 1 2], 3, [b: 4]
        let panes = subs(&[None, Some("a"), Some("a"), None, Some("b")]);
        let list = names(&["a", "b"]);
        assert_eq!(subject_block_move(&list, &panes, "a", false), Panes(vec![1, 2, 0, 3, 4]));
        assert_eq!(subject_block_move(&list, &panes, "a", true), Panes(vec![0, 3, 1, 2, 4]));
        assert_eq!(subject_block_move(&list, &panes, "b", false), Panes(vec![0, 1, 2, 4, 3]));
        assert_eq!(subject_block_move(&list, &panes, "b", true), Stay, "the end of the tab");
        // Two boxes trade places whole.
        let panes = subs(&[Some("a"), Some("b"), Some("b")]);
        assert_eq!(subject_block_move(&list, &panes, "b", false), Panes(vec![1, 2, 0]));
        assert_eq!(subject_block_move(&list, &panes, "a", false), Stay, "the top of the tab");
    }

    #[test]
    fn a_box_torn_apart_comes_out_whole() {
        // [a: 0 2], 1 — moving a down gathers its members after 1.
        let panes = subs(&[Some("a"), None, Some("a")]);
        assert_eq!(
            subject_block_move(&names(&["a"]), &panes, "a", true),
            SubjectBlockMove::Panes(vec![1, 0, 2])
        );
    }

    #[test]
    fn empty_boxes_move_among_themselves_only() {
        use SubjectBlockMove::*;
        let panes = subs(&[Some("a"), None]);
        let list = names(&["x", "a", "y"]);
        assert_eq!(subject_block_move(&list, &panes, "y", false), Subjects(names(&["y", "a", "x"])));
        assert_eq!(subject_block_move(&list, &panes, "x", false), Stay);
        assert_eq!(subject_block_move(&list, &panes, "y", true), Stay, "never below the empty ones");
        assert_eq!(subject_block_move(&list, &panes, "a", false), Stay, "a box with panes stays under them");
    }

    #[test]
    fn inside_a_box_cmd_up_down_trades_places_with_a_member_and_stops_at_the_edge() {
        let subjects = names(&["a"]);
        let panes = subs(&[None, Some("a"), Some("a"), None]);
        assert_eq!(subject_move(&subjects, &panes, 1, true), SubjectMove::Swap(2));
        assert_eq!(subject_move(&subjects, &panes, 2, false), SubjectMove::Swap(1));
        assert_eq!(subject_move(&subjects, &panes, 1, false), SubjectMove::Stay, "leaving is Cmd+Left");
        assert_eq!(subject_move(&subjects, &panes, 2, true), SubjectMove::Stay);
        // Members torn apart: the next member, wherever it is.
        let torn = subs(&[Some("a"), None, Some("a")]);
        assert_eq!(subject_move(&subjects, &torn, 0, true), SubjectMove::Swap(2));
    }

    #[test]
    fn outside_a_box_cmd_up_down_jumps_over_the_whole_box() {
        let subjects = names(&["a"]);
        let panes = subs(&[None, Some("a"), Some("a"), None]);
        assert_eq!(subject_move(&subjects, &panes, 0, true), SubjectMove::Steps(2));
        assert_eq!(subject_move(&subjects, &panes, 3, false), SubjectMove::Steps(2));
        // Two panes outside, side by side: one line, as before.
        let plain = subs(&[None, None, Some("a")]);
        assert_eq!(subject_move(&subjects, &plain, 1, false), SubjectMove::Steps(1));
    }

    #[test]
    fn empty_subjects_at_the_top_are_not_a_place_to_move_to() {
        let subjects = names(&["idle"]);
        let panes = subs(&[None, None]);
        assert_eq!(subject_move(&subjects, &panes, 0, false), SubjectMove::Stay);
        assert_eq!(subject_move(&subjects, &panes, 1, false), SubjectMove::Steps(1));
    }
}
