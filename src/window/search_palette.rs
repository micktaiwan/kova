//! The global search palette (`Cmd+Shift+F`): the snapshot it searches, the
//! worker that matches it, and what picking a row does — jump to a live pane,
//! or reopen a Claude session that no longer has one.

use super::*;

/// What a hit points at: something open right now, or a Claude conversation
/// that only exists as a transcript on disk.
#[derive(Clone)]
pub(super) enum SearchTarget {
    /// A pane, or a whole tab when `pane_id` is `None`. Stable across
    /// window/tab reordering because the lookup happens by id at jump time
    /// rather than by index.
    Open {
        /// Tab containing this hit (always set, even for pane/content hits).
        tab_id: TabId,
        /// Pane the hit lives in. `None` for tab-title hits — jump uses the
        /// tab's currently focused pane and skips the per-pane flash.
        pane_id: Option<PaneId>,
    },
    /// A closed Claude Code session. Opening it means a new pane in the
    /// session's own project directory, running its `--resume` line.
    Archived { session_id: String, cwd: String },
    /// A query searched earlier in this run of Kova. Opening it does not jump
    /// anywhere: it retypes the query into the input and searches again.
    Recall { query: String },
    /// A bookmarked conversation (Cmd+B). Opening it is the same gesture as its
    /// Cmd+P row: focus the pane that holds it, else reopen it in its project.
    Bookmark(crate::bookmarks::Bookmark),
}

/// One hit returned by the search worker.
#[derive(Clone)]
pub(super) struct SearchHit {
    pub(super) target: SearchTarget,
    /// Pre-rendered label shown in the result list.
    pub(super) label: String,
}

/// A row in the result list. Headers are non-selectable group titles (a tab name
/// for the panes section, or the "Tabs" section divider); hits are the selectable
/// entries. Navigation skips headers; `selected` always lands on a `Hit`.
#[derive(Clone)]
pub(super) enum SearchRow {
    Header(String),
    Hit(SearchHit),
}

impl SearchRow {
    pub(super) fn is_hit(&self) -> bool {
        matches!(self, SearchRow::Hit(_))
    }
}

pub(super) struct SearchPaletteState {
    /// Current input string.
    pub(super) query: String,
    /// Caret position in `query`, in chars.
    pub(super) cursor: usize,
    /// Generation counter — bumped on each new submit so stale worker results are dropped.
    pub(super) query_id: u64,
    /// Receiver from the worker thread, if a search is in flight.
    pub(super) rx: Option<std::sync::mpsc::Receiver<(u64, Vec<SearchRow>)>>,
    /// True while a worker is running.
    pub(super) searching: bool,
    /// Last submitted query string, kept so the user can see what produced the results.
    pub(super) submitted_query: String,
    /// Result rows (headers + hits) from the last completed search.
    pub(super) rows: Vec<SearchRow>,
    /// Selected index into `rows` — always points at a `Hit` when one exists.
    pub(super) selected: usize,
    /// Scroll offset (index of first visible row in the list).
    pub(super) scroll: usize,
    /// Trackpad scroll remainder, carried until it adds up to a whole row.
    pub(super) scroll_acc: f64,
    /// Set when the query changed and a live search is owed once debounced.
    pub(super) needs_search: bool,
    /// Timestamp of the last edit, for debouncing the live search.
    pub(super) last_edit: Option<std::time::Instant>,
}

/// Off-thread snapshot of a tab's identity for substring search.
struct SearchTabSnapshot {
    tab_id: TabId,
    title: String,
}

/// Off-thread snapshot of a pane's identity + terminal handle for substring search.
struct SearchPaneSnapshot {
    tab_id: TabId,
    tab_title: String,
    pane_id: PaneId,
    pane_title: String,
    terminal: Arc<parking_lot::RwLock<crate::terminal::TerminalState>>,
    /// Conversation the pane runs, if any: its indexed prompts are searched
    /// along with the screen.
    session_id: Option<String>,
    /// Conversation id of whatever agent the pane runs (Claude or Codex), the
    /// id a bookmark is keyed on.
    agent_session: Option<String>,
}

/// A key pressed with Cmd or Ctrl held. Option stays typing: it composes
/// characters on macOS layouts (French: Option+( = {).
pub(super) fn is_shortcut(event: &NSEvent) -> bool {
    let flags = event.modifierFlags();
    flags.contains(NSEventModifierFlags::Command) || flags.contains(NSEventModifierFlags::Control)
}

/// Insert `text` at the input caret and move the caret past it.
fn insert_at_cursor(state: &mut SearchPaletteState, text: &str) {
    let byte_idx = state.query.char_indices()
        .nth(state.cursor).map(|(i, _)| i)
        .unwrap_or(state.query.len());
    state.query.insert_str(byte_idx, text);
    state.cursor += text.chars().count();
}

/// Drop the recall list once the user starts typing a query of their own.
///
/// The recall rows are only on screen while nothing has been searched yet
/// (`submitted_query` empty). Leaving them there during the debounce would show
/// a list that has nothing to do with what is being typed — and ⏎ before the
/// first results land would recall an old query instead of searching the new
/// one, silently throwing away what was just typed.
fn drop_recall_rows(state: &mut SearchPaletteState) {
    if state.submitted_query.is_empty() && !state.query.is_empty() {
        state.rows.clear();
        state.selected = 0;
        state.scroll = 0;
    }
}

/// Scroll the result list by `lines` rows (positive goes down), within the
/// list, and pull the selection along when it leaves the view: Enter opens a
/// row the user can see, and the renderer does not snap the view back to it.
fn scroll_rows(state: &mut SearchPaletteState, lines: i64, max_visible: usize) {
    let max_visible = max_visible.max(1);
    let max_scroll = state.rows.len().saturating_sub(max_visible);
    let from = crate::renderer::search_palette_scroll(state.selected, state.scroll, max_visible);
    state.scroll = (from as i64 + lines).clamp(0, max_scroll as i64) as usize;
    let end = (state.scroll + max_visible).min(state.rows.len());
    let in_view = &state.rows[state.scroll..end];
    if state.selected < state.scroll {
        if let Some(off) = in_view.iter().position(SearchRow::is_hit) {
            state.selected = state.scroll + off;
        }
    } else if state.selected >= end {
        if let Some(off) = in_view.iter().rposition(SearchRow::is_hit) {
            state.selected = state.scroll + off;
        }
    }
}

/// The hit row drawn at height `py`, if any: headers and the space around the
/// list are not clickable.
fn hit_row_at(state: &SearchPaletteState, py: f32, geom: &crate::renderer::OverlayListGeometry) -> Option<usize> {
    if py < geom.content_top || geom.row_height <= 0.0 {
        return None;
    }
    let visible = ((py - geom.content_top) / geom.row_height) as usize;
    if visible >= geom.max_visible {
        return None;
    }
    let idx = crate::renderer::search_palette_scroll(state.selected, state.scroll, geom.max_visible) + visible;
    state.rows.get(idx).filter(|r| r.is_hit()).map(|_| idx)
}

/// Whether a key event's character is something to type into a text input.
///
/// AppKit hands the arrows, the function keys, Home/End/PageUp and friends as
/// characters in the Unicode private-use block (U+F700…U+F8FF). Those are
/// neither control chars nor below ' ', so the plain "printable" test lets them
/// through and they land in the query as an invisible char that matches nothing.
/// The text a key event types, composed by the keyboard layout (Option and
/// dead keys applied), keeping only characters an input field accepts.
pub(super) fn typed_text(event: &NSEvent) -> String {
    event
        .characters()
        .map(|s| s.to_string())
        .unwrap_or_default()
        .chars()
        .filter(|&c| is_typed_char(c))
        .collect()
}

pub(super) fn is_typed_char(c: char) -> bool {
    // Only AppKit's own range (NSUpArrowFunctionKey…NSModeSwitchFunctionKey).
    // The rest of the private-use block is real typing: U+F8FF is the Apple
    // logo, and Nerd Font glyphs live just below it.
    c >= ' ' && !c.is_control() && !('\u{F700}'..='\u{F747}').contains(&c)
}

/// The rows shown while the input is empty: the queries searched earlier in
/// this run of Kova, most recent first. Empty (no header either) on the first
/// search of the run, so nothing is announced before there is anything to show.
fn recent_search_rows() -> Vec<SearchRow> {
    let recent = crate::search_history::list(crate::search_history::Scope::Palette);
    if recent.is_empty() {
        return Vec::new();
    }
    let mut rows = vec![SearchRow::Header("Recent searches".to_string())];
    rows.extend(recent.into_iter().map(|query| {
        SearchRow::Hit(SearchHit {
            label: query.clone(),
            target: SearchTarget::Recall { query },
        })
    }));
    rows
}

/// The line of a pane's scrollback that explains why it matched, trimmed to fit
/// one row. Without it every content hit reads as a bare pane title and the list
/// gives no reason to prefer one row over another.
///
/// `text` is the pane dump in its original case and `lower` its ASCII-lowercased
/// twin — the one the worker already built to test the match. Both are passed so
/// the line is found on `lower` (no per-line allocation over a 10k-line dump) and
/// shown from `text`. ASCII lowercasing leaves byte lengths untouched, so an
/// offset into one indexes the other.
/// Unicode lowercase that keeps every byte offset: `split_terms` lowercases
/// the query with `to_lowercase`, so an ASCII-only fold here would never match
/// "État". A char whose lowercase changes its UTF-8 length (İ, ẞ, the Kelvin
/// sign) is left as is, since `content_snippet` maps offsets from the lowered
/// text back onto the original.
fn lowercase_same_len(s: &str) -> String {
    s.chars()
        .map(|c| {
            let mut lower = c.to_lowercase();
            match (lower.next(), lower.next()) {
                (Some(l), None) if l.len_utf8() == c.len_utf8() => l,
                _ => c,
            }
        })
        .collect()
}

fn content_snippet(text: &str, lower: &str, term: &str) -> Option<String> {
    const MAX_SNIPPET_CHARS: usize = 90;
    /// Chars of context kept before the match, so it does not sit on the edge.
    const LEAD_CHARS: usize = 20;
    debug_assert_eq!(text.len(), lower.len());

    let (pos, _) = crate::claude_history::find_term(lower, term)?;
    let line_start = text[..pos].rfind('\n').map(|i| i + 1).unwrap_or(0);
    let line_end = text[pos..].find('\n').map(|i| pos + i).unwrap_or(text.len());
    let raw = &text[line_start..line_end];
    let line = raw.trim();
    if line.is_empty() {
        return None;
    }

    // Where the match sits in the trimmed line, in chars.
    let lead_ws = raw.len() - raw.trim_start().len();
    let match_byte = (pos - line_start).saturating_sub(lead_ws).min(line.len());
    let match_char = line[..match_byte].chars().count();

    // Start a few words before the match rather than at the start of a line
    // that may be mostly indentation or a long prefix.
    let start = match_char.saturating_sub(LEAD_CHARS);
    let chars: Vec<char> = line.chars().collect();
    let end = (start + MAX_SNIPPET_CHARS).min(chars.len());
    let mut out = String::new();
    if start > 0 {
        out.push('…');
    }
    out.extend(chars[start..end].iter());
    if end < chars.len() {
        out.push('…');
    }
    Some(out)
}

/// How a pane answers a query: how well its weakest term fits, and the line
/// to show for it.
struct PaneMatch {
    fit: crate::claude_history::Fit,
    snippet: Option<String>,
}

/// Match one pane against `terms`. A term counts if the pane's title, its
/// screen or — for a Claude pane — its conversation's indexed prompts carry it;
/// the best of the three is kept per term, and the pane fits as well as its
/// weakest term.
///
/// The screen alone is not enough: Claude Code draws on the alternate screen,
/// which keeps no scrollback, so a word typed an hour ago is nowhere in the
/// terminal (measured: 0 chars of scrollback on 45 Claude panes of 48).
fn match_pane(p: &SearchPaneSnapshot, terms: &[String], session_text: Option<&String>) -> PaneMatch {
    use crate::claude_history::{fit, Fit};
    let title = lowercase_same_len(&p.pane_title);
    let title_fits: Vec<Fit> = terms.iter().map(|t| fit(&title, t)).collect();
    // Dumping a pane's whole buffer is the expensive part: skip it when the
    // title already carries every term as a word.
    if title_fits.iter().all(|f| *f == Fit::Word) {
        return PaneMatch { fit: Fit::Word, snippet: None };
    }
    let screen = {
        let term = p.terminal.read();
        term.dump_text(crate::terminal::DumpMode::All, true).text
    };
    let screen_lower = lowercase_same_len(&screen);
    let empty = String::new();
    let said = session_text.unwrap_or(&empty);

    let mut worst = Fit::Word;
    // The first term the title does not carry as a word is the one whose line
    // says something the row does not already show.
    let mut snippet_term: Option<(&String, Fit, Fit)> = None;
    for (t, title_fit) in terms.iter().zip(&title_fits) {
        let on_screen = fit(&screen_lower, t);
        let in_prompts = fit(said, t);
        let best = (*title_fit).max(on_screen).max(in_prompts);
        worst = worst.min(best);
        if *title_fit != Fit::Word && snippet_term.is_none() {
            snippet_term = Some((t, on_screen, in_prompts));
        }
    }
    if worst == Fit::Absent {
        return PaneMatch { fit: worst, snippet: None };
    }
    // Show the line from where the term sits best, the screen on a tie: it
    // keeps the original case, the index is lowercased.
    let snippet = snippet_term.and_then(|(t, on_screen, in_prompts)| {
        if on_screen >= in_prompts && on_screen != Fit::Absent {
            content_snippet(&screen, &screen_lower, t)
        } else {
            content_snippet(said, said, t)
        }
    });
    PaneMatch { fit: worst, snippet }
}

/// How a pane or tab name answers the query: its weakest term's fit, so `Word`
/// means the name alone carries every term as a whole word.
fn title_fit(title: &str, terms: &[String]) -> crate::claude_history::Fit {
    use crate::claude_history::{fit, Fit};
    let lower = lowercase_same_len(title);
    terms.iter().map(|t| fit(&lower, t)).min().unwrap_or(Fit::Absent)
}

/// Order name hits by how close the name is to the query: a name made of
/// exactly the typed words first, then the names with the fewest words. Stable,
/// so equal names keep their window → tab order.
fn rank_by_name(named: &mut [(String, SearchHit)], terms: &[String]) {
    let mut typed: Vec<&String> = terms.iter().collect();
    typed.sort();
    named.sort_by_cached_key(|(name, _)| {
        let mut words = crate::claude_history::split_terms(name);
        words.sort();
        let exact = words.iter().eq(typed.iter().copied());
        (!exact, words.len())
    });
}

/// Match one bookmark against `terms`: its saved label, its directory and —
/// for a Claude conversation — the indexed prompts. Same rule as a pane: the
/// best source per term, the weakest term for the bookmark.
fn match_bookmark(
    bm: &crate::bookmarks::Bookmark,
    terms: &[String],
    said: Option<&String>,
) -> PaneMatch {
    use crate::claude_history::{fit, Fit};
    let label = lowercase_same_len(&bm.label);
    let cwd = lowercase_same_len(&bm.cwd);
    let empty = String::new();
    let said = said.unwrap_or(&empty);
    let mut worst = Fit::Word;
    let mut snippet_term: Option<&String> = None;
    for t in terms {
        let named = fit(&label, t).max(fit(&cwd, t));
        let in_prompts = fit(said, t);
        worst = worst.min(named.max(in_prompts));
        // A term the row already shows needs no line to explain it.
        if named != Fit::Word && in_prompts != Fit::Absent && snippet_term.is_none() {
            snippet_term = Some(t);
        }
    }
    let snippet = match worst {
        Fit::Absent => None,
        _ => snippet_term.and_then(|t| content_snippet(said, said, t)),
    };
    PaneMatch { fit: worst, snippet }
}

/// The bookmark hits for `terms`: those carrying every term as a word, then
/// those that only match inside longer words. A bookmark whose conversation is
/// in `listed_sessions` — a pane already listed above — is left out: it is
/// that row, not a second one (same rule as Cmd+P).
fn bookmark_hits(
    bookmarks: &[crate::bookmarks::Bookmark],
    terms: &[String],
    said: &std::collections::HashMap<String, String>,
    listed_sessions: &std::collections::HashSet<String>,
) -> (Vec<SearchHit>, Vec<SearchHit>) {
    use crate::claude_history::Fit;
    let mut word = Vec::new();
    let mut inside = Vec::new();
    for bm in bookmarks {
        if bm.session_id.as_ref().is_some_and(|id| listed_sessions.contains(id)) {
            continue;
        }
        let m = match_bookmark(bm, terms, bm.session_id.as_ref().and_then(|id| said.get(id)));
        let dir = std::path::Path::new(&bm.cwd)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(&bm.cwd);
        let mut label = format!("{} · {}", dir, bm.label);
        if let Some(s) = m.snippet {
            label = format!("{}  ·  {}", label, s);
        }
        match m.fit {
            Fit::Absent => {}
            Fit::Inside => inside.push(SearchHit {
                label: format!("Bookmark · {}", label),
                target: SearchTarget::Bookmark(bm.clone()),
            }),
            Fit::Word => word.push(SearchHit { label, target: SearchTarget::Bookmark(bm.clone()) }),
        }
    }
    (word, inside)
}

/// Worker-thread search, case-insensitive.
///
/// The query is cut into terms on spaces and commas, and a pane or tab has to
/// carry every one of them (see `claude_history::split_terms`): one word is too
/// coarse once there are dozens of panes, and matching the whole query as a
/// single string would make "parser mcp" find nothing.
///
/// A term counts in full only as a whole word: "pitr" is the backup, not the
/// middle of "chapitre". What matches only inside longer words is not dropped
/// — the term may be a prefix still being typed — but moved to a last section
/// of its own, so it never sits among the real hits.
///
/// Produces up to five sections:
///   0. "Names": panes and tabs whose own name carries every term, the name
///      closest to the query first (see `rank_by_name`).
///   1. Other panes whose screen or conversation matches, grouped under a
///      per-tab header (one entry per pane).
///   3. A "Bookmarks" section: saved conversations (Cmd+B) whose label,
///      directory or prompts match, minus those whose pane is listed in 1.
///   4. A "Claude sessions (closed)" section: past conversations that match,
///      most recent first. This is what makes a session findable once its pane
///      is gone — the whole point of `claude_history`. A bookmarked one is
///      left to section 3.
///   5. "Inside longer words": the panes, tabs, bookmarks and closed sessions
///      of sections 1–4 that only matched that way.
/// `panes` arrives already ordered by tab (window → tab → pane), so consecutive
/// grouping by `tab_id` reconstructs the per-tab groups without sorting.
fn run_search_worker(
    query: &str,
    tabs: &[SearchTabSnapshot],
    panes: &[SearchPaneSnapshot],
    live_sessions: &[String],
    bookmarks: &[crate::bookmarks::Bookmark],
) -> Vec<SearchRow> {
    use crate::claude_history::Fit;
    let terms = crate::claude_history::split_terms(query);
    let mut rows: Vec<SearchRow> = Vec::new();
    if terms.is_empty() {
        return rows;
    }

    // First, as it refreshes the index the live panes are then matched on.
    // A bookmarked conversation is not offered again as a closed session.
    let mut not_closed: Vec<String> = live_sessions.to_vec();
    not_closed.extend(bookmarks.iter().filter_map(|b| b.session_id.clone()));
    let archived = crate::claude_history::search(query, &not_closed);
    let mut indexed: Vec<String> = panes.iter().filter_map(|p| p.session_id.clone()).collect();
    indexed.extend(bookmarks.iter().filter_map(|b| b.session_id.clone()));
    let said = crate::claude_history::texts_of(&indexed);
    let mut inside: Vec<SearchHit> = Vec::new();
    let mut listed_sessions: std::collections::HashSet<String> = std::collections::HashSet::new();

    // Section 0: panes and tabs whose own name carries every term. A pane
    // named "PR" is what "PR" means; listing it in window order among every
    // pane whose screen says "PR" buries it.
    let mut named: Vec<(String, SearchHit)> = Vec::new();
    for p in panes {
        if title_fit(&p.pane_title, &terms) == Fit::Word {
            listed_sessions.extend(p.agent_session.clone());
            named.push((p.pane_title.clone(), SearchHit {
                target: SearchTarget::Open { tab_id: p.tab_id, pane_id: Some(p.pane_id) },
                label: format!("{} · {}", p.tab_title, p.pane_title),
            }));
        }
    }
    for tab in tabs {
        if title_fit(&tab.title, &terms) == Fit::Word {
            named.push((tab.title.clone(), SearchHit {
                target: SearchTarget::Open { tab_id: tab.tab_id, pane_id: None },
                label: format!("Tab · {}", tab.title),
            }));
        }
    }
    if !named.is_empty() {
        rank_by_name(&mut named, &terms);
        rows.push(SearchRow::Header("Names".to_string()));
        rows.extend(named.into_iter().map(|(_, hit)| SearchRow::Hit(hit)));
    }

    // Section 1: panes matching by their screen or conversation, grouped by tab.
    let mut current_tab: Option<TabId> = None;
    for p in panes {
        if title_fit(&p.pane_title, &terms) == Fit::Word {
            continue;
        }
        let m = match_pane(p, &terms, p.session_id.as_ref().and_then(|id| said.get(id)));
        let label = match m.snippet {
            Some(s) => format!("{}  ·  {}", p.pane_title, s),
            None => p.pane_title.clone(),
        };
        let target = SearchTarget::Open { tab_id: p.tab_id, pane_id: Some(p.pane_id) };
        if m.fit != Fit::Absent {
            listed_sessions.extend(p.agent_session.clone());
        }
        match m.fit {
            Fit::Absent => continue,
            Fit::Inside => {
                inside.push(SearchHit { target, label: format!("{} · {}", p.tab_title, label) });
                continue;
            }
            Fit::Word => {}
        }
        if current_tab != Some(p.tab_id) {
            rows.push(SearchRow::Header(p.tab_title.clone()));
            current_tab = Some(p.tab_id);
        }
        rows.push(SearchRow::Hit(SearchHit { target, label }));
    }

    // Tabs whose title matches only inside a longer word (a whole-word match
    // is listed under "Names").
    for tab in tabs {
        if title_fit(&tab.title, &terms) == Fit::Inside {
            inside.push(SearchHit {
                target: SearchTarget::Open { tab_id: tab.tab_id, pane_id: None },
                label: format!("Tab · {}", tab.title),
            });
        }
    }

    // Section 3: bookmarks.
    let (bookmarked, bookmarked_inside) = bookmark_hits(bookmarks, &terms, &said, &listed_sessions);
    if !bookmarked.is_empty() {
        rows.push(SearchRow::Header("Bookmarks".to_string()));
        rows.extend(bookmarked.into_iter().map(SearchRow::Hit));
    }
    inside.extend(bookmarked_inside);

    // Section 4: Claude conversations that are not open anywhere any more.
    // Only the latest few are listed — a common word matches hundreds of
    // sessions, and the rest of them would bury the open panes above. What was
    // cut is named in the header rather than dropped silently.
    let orphans = crate::claude_session::orphan_session_ids();
    let archived_hit = |hit: crate::claude_history::Hit| {
        let mut label = crate::claude_history::hit_label(&hit);
        if orphans.contains(&hit.id) {
            // Its pane is gone but the process lives on: say so, since
            // reopening it ends that process.
            label.push_str(" · stranded");
        }
        SearchHit {
            label,
            target: SearchTarget::Archived { session_id: hit.id, cwd: hit.cwd },
        }
    };
    if !archived.hits.is_empty() {
        rows.push(SearchRow::Header(capped_header(
            "Claude sessions (closed)",
            archived.hits.len(),
            archived.total,
        )));
        rows.extend(archived.hits.into_iter().map(|h| SearchRow::Hit(archived_hit(h))));
    }

    // Section 5: what only matched inside longer words, open things first.
    let inside_shown = inside.len() + archived.inside.len();
    if inside_shown > 0 {
        rows.push(SearchRow::Header(capped_header(
            "Inside longer words",
            inside_shown,
            inside.len() + archived.inside_total,
        )));
        rows.extend(inside.into_iter().map(SearchRow::Hit));
        rows.extend(archived.inside.into_iter().map(|h| SearchRow::Hit(archived_hit(h))));
    }

    rows
}

/// A section title that says when the list below it was cut.
fn capped_header(title: &str, shown: usize, total: usize) -> String {
    if total > shown {
        format!("{} — latest {} of {}", title, shown, total)
    } else {
        title.to_string()
    }
}

/// Find a tab whose panes sit in `cwd`, bring it to the front and spawn a pane
/// in it running `command` once the shell is up. Returns false when no window
/// holds such a tab.
///
/// A pane's cwd is read live from its shell, so this follows the directory the
/// user is actually in rather than the one the tab was born with.
fn open_pane_in_tab_for_cwd(cwd: &str, config: &crate::config::Config, command: Option<&str>) -> bool {
    if cwd.is_empty() {
        return false;
    }
    let mtm = unsafe { MainThreadMarker::new_unchecked() };
    let app = NSApplication::sharedApplication(mtm);
    let ns_windows = app.windows();
    for i in 0..ns_windows.count() {
        let win = ns_windows.objectAtIndex(i);
        let view = match crate::app::kova_view(&win) {
            Some(v) => v,
            None => continue,
        };
        let tab_id = {
            let tabs = view.ivars().tabs.borrow();
            let mut found = None;
            for tab in tabs.iter() {
                let mut hit = false;
                tab.for_each_pane(&mut |pane| {
                    if pane.cwd().as_deref() == Some(cwd) {
                        hit = true;
                    }
                });
                if hit {
                    found = Some(tab.id);
                    break;
                }
            }
            found
        };
        let tab_id = match tab_id {
            Some(id) => id,
            None => continue,
        };
        win.makeKeyAndOrderFront(None);
        if !view.activate_tab(tab_id) {
            return false;
        }
        let spawned = view.ipc_split(
            config,
            SplitDirection::Horizontal,
            Some(cwd),
            command.map(str::to_string),
        );
        if let Some(pane_id) = spawned {
            view.run_pending_command_in(pane_id);
            // The tab may have been off-screen: pulse the new pane so the eye
            // finds where the session came back.
            view.set_pane_flash(pane_id, 30, None);
        }
        return spawned.is_some();
    }
    false
}

/// Focus the pane running Claude conversation `session_id`, in whichever window
/// holds it, and pulse its border. False when no pane in this process has it.
fn focus_pane_with_claude_session(session_id: &str) -> bool {
    let mtm = unsafe { MainThreadMarker::new_unchecked() };
    let app = NSApplication::sharedApplication(mtm);
    let ns_windows = app.windows();
    for i in 0..ns_windows.count() {
        let win = ns_windows.objectAtIndex(i);
        let Some(view) = crate::app::kova_view(&win) else { continue };
        let found = {
            let tabs = view.ivars().tabs.borrow();
            let mut found = None;
            for tab in tabs.iter() {
                tab.for_each_pane(&mut |pane| {
                    // Asleep since launch counts: focusing it wakes it.
                    let asleep = pane.dormant_session()
                        .is_some_and(|(agent, id)| agent == crate::agent_session::Agent::Claude && id == session_id);
                    if found.is_none() && (pane.claude_session_id().as_deref() == Some(session_id) || asleep) {
                        found = Some(pane.id);
                    }
                });
                if found.is_some() {
                    break;
                }
            }
            found
        };
        let Some(pane_id) = found else { continue };
        win.makeKeyAndOrderFront(None);
        view.ipc_focus_pane(pane_id);
        view.set_pane_flash(pane_id, 30, None);
        return true;
    }
    false
}

/// Bring the right window/tab/pane to focus and trigger the highlight flash.
/// Walks every Kova window in the process to find the hit's tab_id.
fn jump_to_search_hit(hit: &SearchHit) {
    let (tab_id, pane_id) = match &hit.target {
        SearchTarget::Open { tab_id, pane_id } => (*tab_id, *pane_id),
        SearchTarget::Archived { .. } | SearchTarget::Recall { .. } | SearchTarget::Bookmark(_) => return,
    };
    let mtm = unsafe { MainThreadMarker::new_unchecked() };
    let app = NSApplication::sharedApplication(mtm);
    let ns_windows = app.windows();
    for i in 0..ns_windows.count() {
        let win = ns_windows.objectAtIndex(i);
        let view = match crate::app::kova_view(&win) {
            Some(v) => v,
            None => continue,
        };
        let has_tab = {
            let tabs = view.ivars().tabs.borrow();
            tabs.iter().any(|t| t.id == tab_id)
        };
        if !has_tab {
            continue;
        }
        // Order the window front and make it key so it visibly takes focus.
        win.makeKeyAndOrderFront(None);
        match pane_id {
            // `ipc_focus_pane` restores a minimized pane first: focusing it in
            // place left the tab with no visible focus (status bar "26/25").
            Some(pane_id) => {
                if view.ipc_focus_pane(pane_id) {
                    // ~30 frames ≈ 0.5s @ 60fps; renderer pulses the pane border for that span.
                    view.set_pane_flash(pane_id, 30, None);
                }
            }
            None => {
                view.activate_tab(tab_id);
            }
        }
        return;
    }
    log::debug!("jump_to_search_hit: tab_id {} not found in any window", tab_id);
}

impl KovaView {
    /// Bring back a Claude conversation that is no longer open in any pane.
    ///
    /// The pane has to land in the session's own project directory: `claude
    /// --resume <id>` only finds a conversation from the directory it ran in.
    /// Where exactly it lands follows what is still around — the tab it lived in
    /// is not recoverable, since nothing outlives the pane that held it:
    ///   1. a tab already open on that directory → a new pane next to it;
    ///   2. else the project is in the recents → its saved tab comes back first;
    ///   3. else a new tab on that directory.
    /// The `--resume` line is typed and run: picking the hit already said which
    /// conversation to bring back.
    fn open_archived_claude_session(&self, session_id: &str, cwd: &str) {
        // The rows were built a moment ago, off the main thread: the pick can
        // land on a conversation that has been reopened since, in Kova or
        // elsewhere. Focusing the pane that holds it beats running a second
        // `--resume` on a live id, which Claude Code accepts without a word.
        if focus_pane_with_claude_session(session_id) {
            return;
        }
        // A conversation stranded by a closed pane still has a process writing
        // to its transcript: end it before a second one takes over.
        crate::claude_session::end_orphan(session_id);
        // Same guard as the restore path: an id that cannot make a safe command
        // line never reaches a PTY.
        // A conversation started by a launcher with its own config dir
        // (`deepseek-cc`) comes back through that launcher, or at least under
        // that dir, rather than under the default account.
        let config_dir = crate::claude_session::config_dir_of(session_id);
        let launcher = config_dir
            .as_deref()
            .and_then(crate::launchers::for_config_dir);
        let command = match crate::claude_session::resume_command(
            launcher.as_deref(),
            session_id,
            config_dir.as_deref(),
        ) {
            Some(c) => c,
            None => {
                log::warn!("Refusing to resume an unsafe session id: {:?}", session_id);
                return;
            }
        };
        self.open_conversation_in_project(Some(command), cwd);
    }

    /// Put a conversation back in front of the user, in the project it belongs
    /// to. `command` is the agent's resume line, typed and run once the shell is
    /// up — unlike a restored pane, where it waits for Enter; `None` just opens
    /// a shell in `cwd`.
    ///
    /// Shared by the search palette (a closed session found by its text) and the
    /// bookmark list (a session the user asked to keep).
    pub(super) fn open_conversation_in_project(&self, command: Option<String>, cwd: &str) {
        let config = match self.ivars().config.get() {
            Some(c) => c,
            None => return,
        };
        let cwd_opt = if cwd.is_empty() { None } else { Some(cwd) };
        let Some(command) = command else {
            // A bookmarked shell: nothing to resume, just the directory.
            if !open_pane_in_tab_for_cwd(cwd, config, None) {
                self.ipc_new_tab(config, cwd_opt, None);
            }
            return;
        };

        // 1. A tab is already open on this project.
        if open_pane_in_tab_for_cwd(cwd, config, Some(&command)) {
            return;
        }

        // 2. A closed tab worked in this project: put it back, then add the pane
        //    — unless the restored tab already brought this very session back.
        //    A tab focused on this directory first, else the latest tab with any
        //    pane in it: closed tabs are keyed by name, not by directory. A
        //    closed tab whose name is open again is skipped — restoring it would
        //    give two tabs of the same name.
        let open_keys = super::recent_projects_overlay::open_tab_keys();
        let projects: Vec<_> = crate::recent_projects::load()
            .projects
            .into_iter()
            .filter(|p| !open_keys.contains(&p.key()))
            .collect();
        let recent = projects
            .iter()
            .find(|p| p.path == cwd)
            .or_else(|| projects.iter().find(|p| p.has_cwd(cwd)))
            .cloned();
        if let Some(entry) = recent {
            self.restore_recent_project(&entry);
            let already_there = {
                let tabs = self.ivars().tabs.borrow();
                let idx = self.ivars().active_tab.get();
                let mut found = None;
                if let Some(tab) = tabs.get(idx) {
                    tab.for_each_pane(&mut |pane| {
                        if pane.last_command().as_deref() == Some(command.as_str()) {
                            found = Some(pane.id);
                        }
                    });
                }
                found
            };
            let pane_id = already_there
                .or_else(|| self.ipc_split(config, SplitDirection::Horizontal, cwd_opt, Some(command)));
            if let Some(id) = pane_id {
                self.run_pending_command_in(id);
            }
            return;
        }

        // 3. Nothing to attach to.
        if let Some((_, pane_id)) = self.ipc_new_tab(config, cwd_opt, Some(command)) {
            self.run_pending_command_in(pane_id);
        }
    }

    /// Have the pane `pane_id` of this window run its pending command once its
    /// shell is up, instead of leaving it typed at the prompt.
    fn run_pending_command_in(&self, pane_id: PaneId) {
        let tabs = self.ivars().tabs.borrow();
        if let Some(pane) = tabs.iter().find_map(|t| t.pane(pane_id)) {
            pane.run_pending_command();
        }
    }

    /// Open the search palette overlay (Cmd+Shift+F — global search across all panes).
    pub(super) fn do_open_search_palette(&self) {
        // Index the closed Claude sessions now, so the scan overlaps with the
        // user typing rather than delaying the first query.
        crate::claude_history::warm();
        // An empty input is not an empty screen: it offers what was searched
        // earlier in this run, so a repeat search is one keypress.
        let rows = recent_search_rows();
        let selected = rows.iter().position(SearchRow::is_hit).unwrap_or(0);
        *self.ivars().search_palette.borrow_mut() = Some(SearchPaletteState {
            query: String::new(),
            cursor: 0,
            query_id: 0,
            rx: None,
            searching: false,
            submitted_query: String::new(),
            rows,
            selected,
            scroll: 0,
            scroll_acc: 0.0,
            needs_search: false,
            last_edit: None,
        });
        self.mark_dirty();
    }

    /// Walk every Kova window in the process and collect a snapshot suitable for
    /// off-thread substring search. Cloning Arc<RwLock<TerminalState>> is cheap.
    fn collect_search_snapshot() -> (Vec<SearchTabSnapshot>, Vec<SearchPaneSnapshot>, Vec<String>) {
        let mut tabs_snap = Vec::new();
        let mut panes_snap = Vec::new();
        // Claude conversations that must not show up as closed. Every live
        // session file on the machine is the seed rather than only what the
        // panes report: a pane that Kova has not yet linked to its conversation
        // would otherwise offer it as closed, and resuming it would put the same
        // conversation in two panes (see `claude_session::live_session_ids`).
        let mut live_sessions = crate::claude_session::live_session_ids();

        let mtm = unsafe { MainThreadMarker::new_unchecked() };
        let app = NSApplication::sharedApplication(mtm);
        let ns_windows = app.windows();
        for i in 0..ns_windows.count() {
            let win = &ns_windows.objectAtIndex(i);
            let view = match crate::app::kova_view(win) {
                Some(v) => v,
                None => continue,
            };
            let tabs = view.ivars().tabs.borrow();
            for tab in tabs.iter() {
                let tab_title = tab.title();
                tabs_snap.push(SearchTabSnapshot { tab_id: tab.id, title: tab_title.clone() });
                tab.for_each_pane(&mut |pane| {
                    // Asleep since launch is open too: not a closed session.
                    if let Some(id) = pane.agent_session_id().or_else(|| pane.dormant_session_id()) {
                        live_sessions.push(id);
                    }
                    panes_snap.push(SearchPaneSnapshot {
                        tab_id: tab.id,
                        tab_title: tab_title.clone(),
                        pane_id: pane.id,
                        pane_title: pane.display_title("shell"),
                        terminal: pane.terminal.clone(),
                        session_id: pane.claude_session_id(),
                        agent_session: pane.agent_session_id(),
                    });
                });
            }
        }
        (tabs_snap, panes_snap, live_sessions)
    }

    /// Submit the current query: snapshot panes on the main thread, spawn a
    /// worker thread to scan, and stash a Receiver on the palette state.
    /// Keeps the previous rows visible until the new ones land (no flicker while
    /// live-typing); `poll_search_palette` replaces them and resets the selection.
    fn submit_search_palette(&self) {
        let (query, query_id) = {
            let mut guard = self.ivars().search_palette.borrow_mut();
            let state = match guard.as_mut() {
                Some(s) => s,
                None => return,
            };
            // A worker still running keeps the search owed: the poll tick fires
            // it once that worker's rows land, instead of showing them under
            // a query they do not answer.
            if state.searching {
                return;
            }
            state.needs_search = false;
            if state.query.is_empty() {
                return;
            }
            state.query_id = state.query_id.wrapping_add(1);
            state.searching = true;
            state.submitted_query = state.query.clone();
            (state.query.clone(), state.query_id)
        };

        let (tabs_snap, panes_snap, live_sessions) = Self::collect_search_snapshot();
        let (tx, rx) = std::sync::mpsc::channel();

        // Store rx into state before spawning, so the polling tick can pick it up
        // even if the worker finishes immediately.
        if let Some(state) = self.ivars().search_palette.borrow_mut().as_mut() {
            state.rx = Some(rx);
        }

        std::thread::spawn(move || {
            let bookmarks = crate::bookmarks::load().items;
            let hits = run_search_worker(&query, &tabs_snap, &panes_snap, &live_sessions, &bookmarks);
            let _ = tx.send((query_id, hits));
        });

        self.mark_dirty();
    }

    /// Drain any pending worker results into the palette state. Called by the
    /// app delegate's tick on each frame so results land without the user
    /// having to press a key.
    pub fn poll_search_palette(&self) {
        let mut updated = false;
        // Decide whether a debounced live search is owed; do it without holding a
        // mutable borrow across the call to submit_search_palette (which re-borrows).
        let mut trigger_search = false;
        let mut clear_for_empty = false;
        {
            let mut guard = self.ivars().search_palette.borrow_mut();
            let state = match guard.as_mut() {
                Some(s) => s,
                None => return,
            };

            // Phase 1: drain any pending worker results.
            if let Some(rx) = state.rx.as_ref() {
                while let Ok((id, rows)) = rx.try_recv() {
                    if id == state.query_id {
                        state.rows = rows;
                        state.searching = false;
                        // Land the selection on the first selectable hit.
                        state.selected = state
                            .rows
                            .iter()
                            .position(SearchRow::is_hit)
                            .unwrap_or(0);
                        state.scroll = 0;
                        updated = true;
                    }
                    // else: stale query, drop the rows silently
                }
                // Drop the receiver once a result for the current query has arrived.
                if !state.searching {
                    state.rx = None;
                }
            }

            // Phase 2: fire a debounced live search if the query changed.
            if state.needs_search && !state.searching {
                let ready = state
                    .last_edit
                    .map(|t| t.elapsed() >= SEARCH_DEBOUNCE)
                    .unwrap_or(true);
                if ready {
                    if state.query.is_empty() {
                        clear_for_empty = true;
                    } else {
                        trigger_search = true;
                    }
                }
            }
        }

        if clear_for_empty {
            // Erasing the query brings back the same list the palette opened
            // with, rather than a blank panel.
            let rows = recent_search_rows();
            if let Some(state) = self.ivars().search_palette.borrow_mut().as_mut() {
                state.needs_search = false;
                state.submitted_query.clear();
                state.selected = rows.iter().position(SearchRow::is_hit).unwrap_or(0);
                state.rows = rows;
                state.scroll = 0;
            }
            updated = true;
        } else if trigger_search {
            self.submit_search_palette();
            updated = true;
        }

        if updated {
            self.mark_dirty();
        }
    }

    /// Close the palette, remembering what was typed. The one exit point for
    /// the overlay, so no path can drop a query without recording it.
    pub(super) fn close_search_palette(&self) {
        if let Some(state) = self.ivars().search_palette.borrow_mut().take() {
            crate::search_history::record(
                crate::search_history::Scope::Palette,
                &state.query,
            );
        }
        self.mark_dirty();
    }

    /// Cmd+V while the palette is open: the clipboard's first line goes in at
    /// the caret, and a live search is queued as for typed text.
    pub(super) fn paste_into_search_palette(&self, text: &str) {
        let pasted = super::overlays::line_paste_text(text);
        if pasted.is_empty() {
            return;
        }
        if let Some(state) = self.ivars().search_palette.borrow_mut().as_mut() {
            insert_at_cursor(state, &pasted);
            state.needs_search = true;
            // A paste is a complete edit, like a recalled query: no debounce.
            state.last_edit = None;
            drop_recall_rows(state);
        }
        self.mark_dirty();
    }

    /// Open the selected hit (Enter, or a click on a row). Live search keeps
    /// the rows fresh, so the only fallback is to force a scan if one is owed
    /// but the debounce hasn't fired yet.
    fn open_selected_search_hit(&self) {
        let action = {
            let guard = self.ivars().search_palette.borrow();
            let state = match guard.as_ref() {
                Some(s) => s,
                None => return,
            };
            // Rows that answer an older query (a paste or a keystroke
            // still inside the debounce) must not be opened: search the
            // query on screen instead, and let the next Enter open.
            let stale = state.needs_search
                || state.searching
                || state.query != state.submitted_query;
            match state.rows.get(state.selected) {
                Some(SearchRow::Hit(hit))
                    if !stale || matches!(hit.target, SearchTarget::Recall { .. }) =>
                {
                    Some(hit.clone())
                }
                _ => None,
            }
        };
        match action {
            // A recalled query stays in the palette: it retypes the
            // query and searches again, it does not open anything.
            Some(SearchHit { target: SearchTarget::Recall { query }, .. }) => {
                if let Some(state) = self.ivars().search_palette.borrow_mut().as_mut() {
                    state.cursor = query.chars().count();
                    state.query = query;
                    state.needs_search = true;
                    // No debounce for a query the user did not type:
                    // it is complete, so run it on the next tick.
                    state.last_edit = None;
                }
                self.mark_dirty();
            }
            Some(hit) => {
                self.close_search_palette();
                match &hit.target {
                    SearchTarget::Open { .. } => jump_to_search_hit(&hit),
                    SearchTarget::Archived { session_id, cwd } => {
                        self.open_archived_claude_session(session_id, cwd)
                    }
                    SearchTarget::Bookmark(bm) => self.open_saved_conversation(bm),
                    SearchTarget::Recall { .. } => unreachable!("handled above"),
                }
            }
            None => self.submit_search_palette(),
        }
    }

    fn search_palette_geometry(&self) -> Option<crate::renderer::OverlayListGeometry> {
        let renderer = self.ivars().renderer.get()?;
        let vh = self.drawable_viewport().height;
        let geom = renderer.read().search_palette_list_geometry(vh);
        Some(geom)
    }

    /// A click on a result row opens it, as Enter would; anywhere else in the
    /// palette it does nothing.
    pub(super) fn handle_search_palette_click(&self, py: f32) {
        let Some(geom) = self.search_palette_geometry() else { return };
        let row = {
            let guard = self.ivars().search_palette.borrow();
            match guard.as_ref() {
                Some(state) => hit_row_at(state, py, &geom),
                None => return,
            }
        };
        let Some(row) = row else { return };
        if let Some(state) = self.ivars().search_palette.borrow_mut().as_mut() {
            state.selected = row;
        }
        self.mark_dirty();
        self.open_selected_search_hit();
    }

    /// The wheel or trackpad scrolls the result list, the selection following
    /// along so it stays on screen.
    pub(super) fn handle_search_palette_scroll(&self, event: &NSEvent, is_trackpad: bool) {
        let Some(geom) = self.search_palette_geometry() else { return };
        let mut guard = self.ivars().search_palette.borrow_mut();
        let Some(state) = guard.as_mut() else { return };
        // Same feel as the Cmd+P list: natural scrolling, a trackpad's fine
        // deltas added up until they make a whole row.
        let dy = event.scrollingDeltaY();
        let lines = if is_trackpad {
            let acc = state.scroll_acc - dy / 8.0;
            let discrete = acc.trunc();
            state.scroll_acc = acc - discrete;
            discrete as i64
        } else {
            state.scroll_acc = 0.0;
            -dy as i64
        };
        if lines == 0 {
            return;
        }
        scroll_rows(state, lines, geom.max_visible);
        drop(guard);
        self.mark_dirty();
    }

    /// Handle key events while the search palette overlay is active.
    pub(super) fn handle_search_palette_key(&self, event: &NSEvent) {
        let key_code = event.keyCode();
        let chars = event.charactersIgnoringModifiers();
        let ch_str = chars.map(|s| s.to_string()).unwrap_or_default();
        let ch = ch_str.chars().next().unwrap_or('\0');

        // Up/Down navigate the result list (only meaningful with results).
        match key_code {
            0x7E => {
                // Up: select the previous hit row, skipping headers.
                let mut guard = self.ivars().search_palette.borrow_mut();
                if let Some(state) = guard.as_mut() {
                    if let Some(prev) = state.rows[..state.selected]
                        .iter()
                        .rposition(SearchRow::is_hit)
                    {
                        state.selected = prev;
                        // Pull a preceding header into view if there is one.
                        let top = state.selected.saturating_sub(1);
                        if top < state.scroll {
                            state.scroll = top;
                        }
                    }
                }
                drop(guard);
                self.mark_dirty();
                return;
            }
            0x7D => {
                // Down: select the next hit row, skipping headers.
                let mut guard = self.ivars().search_palette.borrow_mut();
                if let Some(state) = guard.as_mut() {
                    let next = state.selected + 1;
                    if next < state.rows.len() {
                        if let Some(off) = state.rows[next..].iter().position(SearchRow::is_hit) {
                            state.selected = next + off;
                        }
                    }
                }
                drop(guard);
                self.mark_dirty();
                return;
            }
            // Left/Right arrows move the input caret.
            0x7B => {
                let mut guard = self.ivars().search_palette.borrow_mut();
                if let Some(state) = guard.as_mut() {
                    if state.cursor > 0 { state.cursor -= 1; }
                }
                drop(guard);
                self.mark_dirty();
                return;
            }
            0x7C => {
                let mut guard = self.ivars().search_palette.borrow_mut();
                if let Some(state) = guard.as_mut() {
                    let len = state.query.chars().count();
                    if state.cursor < len { state.cursor += 1; }
                }
                drop(guard);
                self.mark_dirty();
                return;
            }
            _ => {}
        }

        match ch {
            '\u{1B}' => {
                // Escape — close the palette, keeping the query in the history:
                // a search abandoned is still a search one may want back.
                self.close_search_palette();
            }
            '\r' => self.open_selected_search_hit(),
            '\u{7F}' | '\u{08}' => {
                // Backspace — remove char before cursor; queue a live search.
                let mut guard = self.ivars().search_palette.borrow_mut();
                if let Some(state) = guard.as_mut() {
                    if state.cursor > 0 {
                        if let Some((byte_idx, _)) = state.query.char_indices().nth(state.cursor - 1) {
                            state.query.remove(byte_idx);
                            state.cursor -= 1;
                            state.needs_search = true;
                            state.last_edit = Some(std::time::Instant::now());
                            drop_recall_rows(state);
                        }
                    }
                }
                drop(guard);
                self.mark_dirty();
            }
            // Cmd or Ctrl with a letter is a shortcut, not typing: without this
            // guard Cmd+A would insert an "a".
            c if is_typed_char(c) && !is_shortcut(event) => {
                // Insert what the layout produced, not the bare key: Option
                // composes characters (French: Option+( = {), and
                // charactersIgnoringModifiers would turn them back into the key.
                let text = typed_text(event);
                if text.is_empty() {
                    return; // a dead key waiting for its second stroke
                }
                let mut guard = self.ivars().search_palette.borrow_mut();
                if let Some(state) = guard.as_mut() {
                    insert_at_cursor(state, &text);
                    state.needs_search = true;
                    state.last_edit = Some(std::time::Instant::now());
                    drop_recall_rows(state);
                }
                drop(guard);
                self.mark_dirty();
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Call `content_snippet` the way the worker does: the dump and its
    /// lowercased twin.
    fn snippet_of(text: &str, term: &str) -> Option<String> {
        content_snippet(text, &lowercase_same_len(text), term)
    }

    #[test]
    fn lowercase_same_len_folds_accented_capitals() {
        assert_eq!(lowercase_same_len("État des lieux"), "état des lieux");
        assert_eq!(lowercase_same_len("ÇA À Ω"), "ça à ω");
    }

    #[test]
    fn lowercase_same_len_keeps_byte_offsets() {
        // İ lowercases to two chars, the Kelvin sign to a 1-byte 'k'.
        for s in ["İstanbul", "\u{212A}elvin", "ẞtraße", "État"] {
            assert_eq!(lowercase_same_len(s).len(), s.len(), "{s}");
        }
    }

    #[test]
    fn snippet_matches_an_accented_capital() {
        let text = "noise\n  État des lieux signé\nmore";
        assert_eq!(snippet_of(text, "état").as_deref(), Some("État des lieux signé"));
    }

    fn palette_state(query: &str, submitted: &str, rows: Vec<SearchRow>) -> SearchPaletteState {
        SearchPaletteState {
            cursor: query.chars().count(),
            query: query.to_string(),
            query_id: 0,
            rx: None,
            searching: false,
            submitted_query: submitted.to_string(),
            selected: rows.iter().position(SearchRow::is_hit).unwrap_or(0),
            rows,
            scroll: 0,
            scroll_acc: 0.0,
            needs_search: false,
            last_edit: None,
        }
    }

    #[test]
    fn typing_drops_the_recall_list_before_it_can_be_opened_by_mistake() {
        // Opened, nothing searched yet: the rows are the recall list.
        let recall = vec![
            SearchRow::Header("Recent searches".to_string()),
            SearchRow::Hit(SearchHit {
                label: "old query".to_string(),
                target: SearchTarget::Recall { query: "old query".to_string() },
            }),
        ];
        let mut state = palette_state("d", "", recall.clone());
        drop_recall_rows(&mut state);
        // Gone: ⏎ during the debounce searches what was typed instead of
        // recalling an unrelated query.
        assert!(state.rows.is_empty());
        assert_eq!(state.selected, 0);

        // Still empty input (the user only moved the caret): the list stays.
        let mut state = palette_state("", "", recall);
        drop_recall_rows(&mut state);
        assert_eq!(state.rows.len(), 2);

        // Real results are never dropped by an edit.
        let results = vec![SearchRow::Hit(SearchHit {
            label: "a pane".to_string(),
            target: SearchTarget::Archived { session_id: "x".into(), cwd: "/tmp".into() },
        })];
        let mut state = palette_state("de", "d", results);
        drop_recall_rows(&mut state);
        assert_eq!(state.rows.len(), 1);
    }

    fn bookmark(id: Option<&str>, cwd: &str, label: &str) -> crate::bookmarks::Bookmark {
        crate::bookmarks::Bookmark {
            config_dir: None,
            launcher: None,
            agent: id.map(|_| crate::agent_session::Agent::Claude),
            session_id: id.map(str::to_string),
            cwd: cwd.to_string(),
            label: label.to_string(),
        }
    }

    fn hit_labels(hits: &[SearchHit]) -> Vec<&str> {
        hits.iter().map(|h| h.label.as_str()).collect()
    }

    #[test]
    fn a_bookmark_matches_on_its_label_its_directory_or_its_prompts() {
        use std::collections::{HashMap, HashSet};
        let items = vec![
            bookmark(Some("a"), "/Users/x/kova", "Search palette"),
            bookmark(Some("b"), "/Users/x/api", "Weekly"),
            bookmark(None, "/Users/x/infra", "shell"),
        ];
        let said: HashMap<String, String> =
            [("b".to_string(), "we deploy to staging".to_string())].into();
        let none = HashSet::new();
        let terms = crate::claude_history::split_terms;

        let (word, _) = bookmark_hits(&items, &terms("palette"), &said, &none);
        assert_eq!(hit_labels(&word), ["kova · Search palette"]);
        let (word, _) = bookmark_hits(&items, &terms("infra"), &said, &none);
        assert_eq!(hit_labels(&word), ["infra · shell"], "the directory counts");
        // Found by what was said in it: the row shows the line that matched.
        let (word, _) = bookmark_hits(&items, &terms("api deploy"), &said, &none);
        assert_eq!(hit_labels(&word), ["api · Weekly  ·  we deploy to staging"]);
        // Every term has to be there.
        let (word, inside) = bookmark_hits(&items, &terms("kova deploy"), &said, &none);
        assert!(word.is_empty() && inside.is_empty());
    }

    #[test]
    fn a_bookmark_inside_a_longer_word_goes_to_the_last_section() {
        let items = vec![bookmark(Some("a"), "/k", "Chapitre trois")];
        let (word, inside) = bookmark_hits(
            &items,
            &crate::claude_history::split_terms("pitr"),
            &Default::default(),
            &Default::default(),
        );
        assert!(word.is_empty());
        assert_eq!(hit_labels(&inside), ["Bookmark · k · Chapitre trois"]);
    }

    #[test]
    fn a_bookmark_whose_pane_is_listed_is_not_listed_twice() {
        let items = vec![bookmark(Some("a"), "/k", "palette"), bookmark(None, "/k", "palette")];
        let listed: std::collections::HashSet<String> = ["a".to_string()].into();
        let (word, _) = bookmark_hits(
            &items,
            &crate::claude_history::split_terms("palette"),
            &Default::default(),
            &listed,
        );
        assert_eq!(word.len(), 1);
        assert!(word[0].target_is_shell_bookmark());
    }

    impl SearchHit {
        fn target_is_shell_bookmark(&self) -> bool {
            matches!(&self.target, SearchTarget::Bookmark(b) if b.session_id.is_none())
        }
    }

    /// A header, then `n` hits, then a header and `n` more hits.
    fn two_sections(n: usize) -> Vec<SearchRow> {
        let hit = |i: usize| {
            SearchRow::Hit(SearchHit {
                target: SearchTarget::Recall { query: String::new() },
                label: i.to_string(),
            })
        };
        let mut rows = vec![SearchRow::Header("A".into())];
        rows.extend((0..n).map(hit));
        rows.push(SearchRow::Header("B".into()));
        rows.extend((n..2 * n).map(hit));
        rows
    }

    #[test]
    fn the_wheel_scrolls_within_the_list_and_drags_the_selection_along() {
        let mut state = palette_state("q", "q", two_sections(10)); // 22 rows
        assert_eq!(state.selected, 1);
        scroll_rows(&mut state, 5, 4);
        assert_eq!(state.scroll, 5);
        assert_eq!(state.selected, 5, "the first hit in view");
        scroll_rows(&mut state, 100, 4);
        assert_eq!(state.scroll, 18, "stops on the last screenful");
        assert_eq!(state.selected, 18);
        // Back up past the selection: it moves to the last hit in view.
        scroll_rows(&mut state, -10, 4);
        assert_eq!(state.scroll, 8);
        assert_eq!(state.selected, 10);
        scroll_rows(&mut state, -100, 4);
        assert_eq!((state.scroll, state.selected), (0, 3));
    }

    #[test]
    fn the_wheel_keeps_a_selection_already_in_view() {
        let mut state = palette_state("q", "q", two_sections(10));
        state.selected = 3;
        scroll_rows(&mut state, 2, 4);
        assert_eq!((state.scroll, state.selected), (2, 3));
        // Starts from the view on screen, which the selection had pushed down.
        state.selected = 9;
        state.scroll = 0;
        scroll_rows(&mut state, 1, 4);
        assert_eq!((state.scroll, state.selected), (7, 9));
    }

    #[test]
    fn a_click_lands_on_the_hit_drawn_under_it() {
        let geom = crate::renderer::OverlayListGeometry { content_top: 100.0, row_height: 20.0, max_visible: 5 };
        let mut state = palette_state("q", "q", two_sections(3)); // A 0 1 2 B 3 4 5
        assert_eq!(hit_row_at(&state, 125.0, &geom), Some(1));
        assert_eq!(hit_row_at(&state, 105.0, &geom), None, "a header");
        assert_eq!(hit_row_at(&state, 90.0, &geom), None, "above the list");
        assert_eq!(hit_row_at(&state, 205.0, &geom), None, "below the last visible row");
        // Selection at row 6 pushes the view down by 2, as the renderer does.
        state.selected = 6;
        assert_eq!(hit_row_at(&state, 105.0, &geom), Some(2));
    }

    #[test]
    fn a_name_matching_the_query_ranks_first() {
        let terms = crate::claude_history::split_terms("PR");
        let hit = |name: &str| {
            (name.to_string(), SearchHit {
                target: SearchTarget::Recall { query: String::new() },
                label: name.to_string(),
            })
        };
        let mut named = vec![hit("review PR acme"), hit("PR merge"), hit("PR")];
        rank_by_name(&mut named, &terms);
        let order: Vec<&str> = named.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(order, ["PR", "PR merge", "review PR acme"]);
    }

    #[test]
    fn a_name_holds_the_query_only_as_whole_words() {
        use crate::claude_history::{split_terms, Fit};
        assert_eq!(title_fit("PR", &split_terms("pr")), Fit::Word);
        assert_eq!(title_fit("fix PR review", &split_terms("review pr")), Fit::Word);
        assert_eq!(title_fit("prompts", &split_terms("pr")), Fit::Inside);
        assert_eq!(title_fit("PR", &split_terms("pr kova")), Fit::Absent);
    }

    #[test]
    fn a_paste_lands_at_the_caret() {
        let mut state = palette_state("héllo", "", Vec::new());
        state.cursor = 2;
        insert_at_cursor(&mut state, "XY");
        assert_eq!(state.query, "héXYllo");
        assert_eq!(state.cursor, 4);
    }

    #[test]
    fn function_keys_are_not_typed_into_an_input() {
        // What a person types.
        assert!(is_typed_char('a'));
        assert!(is_typed_char(' '));
        assert!(is_typed_char('é'));
        assert!(is_typed_char('▲'));
        // AppKit's private-use block: arrows, F-keys, Home/End, Page Up/Down.
        assert!(!is_typed_char('\u{F700}')); // up arrow
        assert!(!is_typed_char('\u{F701}')); // down arrow
        assert!(!is_typed_char('\u{F729}')); // home
        assert!(!is_typed_char('\u{F747}')); // mode switch, the last of them
        // Above that range the private-use block carries characters people do
        // type: the Apple logo, and the Nerd Font glyphs in a shell prompt.
        assert!(is_typed_char('\u{F8FF}'));
        assert!(is_typed_char('\u{F748}'));
        // Control chars stay out, as before.
        assert!(!is_typed_char('\u{1B}'));
        assert!(!is_typed_char('\r'));
    }

    #[test]
    fn a_content_hit_shows_the_line_that_matched() {
        let text = "first line\n  the deploy failed on staging\nlast line";
        assert_eq!(
            snippet_of(text, "deploy").as_deref(),
            Some("the deploy failed on staging")
        );
        // Case-insensitive, and the row shows the original case.
        assert_eq!(snippet_of("Deploy Failed", "deploy").as_deref(), Some("Deploy Failed"));
        // A term nowhere in the text has no line to show.
        assert_eq!(snippet_of(text, "absent"), None);
    }

    #[test]
    fn a_long_line_is_cut_around_the_match() {
        let line = format!("{}needle{}", "a".repeat(200), "b".repeat(200));
        let snippet = snippet_of(&line, "needle").expect("the line matches");
        // Ellipsis on both sides, the match kept in view, and the row stays short.
        assert!(snippet.starts_with('…'), "{snippet}");
        assert!(snippet.ends_with('…'), "{snippet}");
        assert!(snippet.contains("needle"), "{snippet}");
        assert!(snippet.chars().count() <= 92, "{}", snippet.chars().count());
    }

    #[test]
    fn a_snippet_never_splits_a_multibyte_char() {
        // Box-drawing and emoji are everywhere in a terminal: cutting by bytes
        // would panic here.
        let line = format!("{}échec du déploiement", "é".repeat(50));
        let snippet = snippet_of(&line, "déploiement").expect("the line matches");
        assert!(snippet.contains("déploiement"), "{snippet}");
    }
}
