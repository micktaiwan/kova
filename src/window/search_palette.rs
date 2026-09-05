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
    /// session's own project directory, with its `--resume` line pre-typed.
    Archived { session_id: String, cwd: String },
    /// A query searched earlier in this run of Kova. Opening it does not jump
    /// anywhere: it retypes the query into the input and searches again.
    Recall { query: String },
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

/// Whether a key event's character is something to type into a text input.
///
/// AppKit hands the arrows, the function keys, Home/End/PageUp and friends as
/// characters in the Unicode private-use block (U+F700…U+F8FF). Those are
/// neither control chars nor below ' ', so the plain "printable" test lets them
/// through and they land in the query as an invisible char that matches nothing.
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
fn content_snippet(text: &str, lower: &str, term: &str) -> Option<String> {
    const MAX_SNIPPET_CHARS: usize = 90;
    /// Chars of context kept before the match, so it does not sit on the edge.
    const LEAD_CHARS: usize = 20;
    debug_assert_eq!(text.len(), lower.len());

    let pos = lower.find(term)?;
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

/// Worker-thread search. Uses ASCII case-insensitive `contains` — good enough
/// for terminal text, which is overwhelmingly ASCII.
///
/// The query is cut into terms on spaces and commas, and a pane or tab has to
/// carry every one of them (see `claude_history::split_terms`): one word is too
/// coarse once there are dozens of panes, and matching the whole query as a
/// single string would make "dust mcp" find nothing.
///
/// Produces a three-section row list:
///   1. Panes whose title OR content matches, grouped under a per-tab header
///      (one entry per pane — title and content matches are deduped).
///   2. A "Tabs" section listing tabs whose title matches.
///   3. A "Claude sessions (closed)" section: past conversations that match,
///      most recent first. This is what makes a session findable once its pane
///      is gone — the whole point of `claude_history`.
/// `panes` arrives already ordered by tab (window → tab → pane), so consecutive
/// grouping by `tab_id` reconstructs the per-tab groups without sorting.
fn run_search_worker(
    query: &str,
    tabs: &[SearchTabSnapshot],
    panes: &[SearchPaneSnapshot],
    live_sessions: &[String],
) -> Vec<SearchRow> {
    let terms = crate::claude_history::split_terms(query);
    let mut rows: Vec<SearchRow> = Vec::new();
    if terms.is_empty() {
        return rows;
    }

    // Section 1: matching panes, grouped by tab.
    let mut current_tab: Option<TabId> = None;
    for p in panes {
        let title = p.pane_title.to_ascii_lowercase();
        // Terms the title already covers need no scrollback: dumping a pane's
        // whole buffer is the expensive part, so it only happens for what is
        // left, and only once.
        let unmatched: Vec<&String> = terms.iter().filter(|t| !title.contains(t.as_str())).collect();
        // Snippet of the scrollback line that matched, for the rows the title
        // alone does not explain.
        let mut snippet = None;
        let matches = if unmatched.is_empty() {
            true
        } else {
            let text = {
                let term = p.terminal.read();
                term.dump_text(crate::terminal::DumpMode::All, true).text
            };
            let lower = text.to_ascii_lowercase();
            let all = unmatched.iter().all(|t| lower.contains(t.as_str()));
            if all {
                // The first term the title did not carry is the one whose line
                // says something the row does not already show.
                snippet = content_snippet(&text, &lower, unmatched[0]);
            }
            all
        };
        if !matches {
            continue;
        }
        if current_tab != Some(p.tab_id) {
            rows.push(SearchRow::Header(p.tab_title.clone()));
            current_tab = Some(p.tab_id);
        }
        let label = match snippet {
            Some(s) => format!("{}  ·  {}", p.pane_title, s),
            None => p.pane_title.clone(),
        };
        rows.push(SearchRow::Hit(SearchHit {
            target: SearchTarget::Open {
                tab_id: p.tab_id,
                pane_id: Some(p.pane_id),
            },
            label,
        }));
    }

    // Section 2: tabs whose title matches.
    let mut tab_section_open = false;
    for tab in tabs {
        let title = tab.title.to_ascii_lowercase();
        if terms.iter().all(|t| title.contains(t.as_str())) {
            if !tab_section_open {
                rows.push(SearchRow::Header("Tabs".to_string()));
                tab_section_open = true;
            }
            rows.push(SearchRow::Hit(SearchHit {
                target: SearchTarget::Open {
                    tab_id: tab.tab_id,
                    pane_id: None,
                },
                label: tab.title.clone(),
            }));
        }
    }

    // Section 3: Claude conversations that are not open anywhere any more.
    // Only the most recent few are listed — a common word matches hundreds of
    // sessions, and the rest of them would bury the open panes above. What was
    // cut is named in the header rather than dropped silently.
    let archived = crate::claude_history::search(query, live_sessions);
    if !archived.hits.is_empty() {
        let header = if archived.total > archived.hits.len() {
            format!(
                "Claude sessions (closed) — {} most recent of {}",
                archived.hits.len(),
                archived.total
            )
        } else {
            "Claude sessions (closed)".to_string()
        };
        rows.push(SearchRow::Header(header));
        for hit in archived.hits {
            rows.push(SearchRow::Hit(SearchHit {
                label: crate::claude_history::hit_label(&hit),
                target: SearchTarget::Archived {
                    session_id: hit.id,
                    cwd: hit.cwd,
                },
            }));
        }
    }

    rows
}

/// Find a tab whose panes sit in `cwd`, bring it to the front and spawn a pane
/// in it carrying `command`. Returns false when no window holds such a tab.
///
/// A pane's cwd is read live from its shell, so this follows the directory the
/// user is actually in rather than the one the tab was born with.
fn open_pane_in_tab_for_cwd(cwd: &str, config: &crate::config::Config, command: &str) -> bool {
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
            Some(command.to_string()),
        );
        if let Some(pane_id) = spawned {
            // The tab may have been off-screen: pulse the new pane so the eye
            // finds where the session came back.
            view.set_pane_flash(pane_id, 30, None);
        }
        return spawned.is_some();
    }
    false
}

/// Bring the right window/tab/pane to focus and trigger the highlight flash.
/// Walks every Kova window in the process to find the hit's tab_id.
fn jump_to_search_hit(hit: &SearchHit) {
    let (tab_id, pane_id) = match &hit.target {
        SearchTarget::Open { tab_id, pane_id } => (*tab_id, *pane_id),
        SearchTarget::Archived { .. } | SearchTarget::Recall { .. } => return,
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
        if !view.activate_tab(tab_id) {
            return;
        }
        if let Some(pane_id) = pane_id {
            view.focus_pane_in_active_tab(pane_id);
            // ~30 frames ≈ 0.5s @ 60fps; renderer pulses the pane border for that span.
            view.set_pane_flash(pane_id, 30, None);
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
    /// The `--resume` line is pre-typed, not run, exactly like a restored pane.
    fn open_archived_claude_session(&self, session_id: &str, cwd: &str) {
        let config = match self.ivars().config.get() {
            Some(c) => c,
            None => return,
        };
        // Same guard as the restore path: an id that cannot make a safe command
        // line never reaches a PTY.
        let command = match crate::claude_session::resume_command(None, session_id) {
            Some(c) => c,
            None => {
                log::warn!("Refusing to resume an unsafe session id: {:?}", session_id);
                return;
            }
        };
        let cwd_opt = if cwd.is_empty() { None } else { Some(cwd) };

        // 1. A tab is already open on this project.
        if open_pane_in_tab_for_cwd(cwd, config, &command) {
            return;
        }

        // 2. The project is in the recents: put its tab back, then add the pane
        //    — unless the restored tab already brought this very session back.
        let recent = crate::recent_projects::load()
            .projects
            .into_iter()
            .find(|p| p.path == cwd);
        if let Some(entry) = recent {
            self.restore_recent_project(&entry);
            let already_there = {
                let tabs = self.ivars().tabs.borrow();
                let idx = self.ivars().active_tab.get();
                let mut found = false;
                if let Some(tab) = tabs.get(idx) {
                    tab.for_each_pane(&mut |pane| {
                        if pane.last_command().as_deref() == Some(command.as_str()) {
                            found = true;
                        }
                    });
                }
                found
            };
            if !already_there {
                self.ipc_split(config, SplitDirection::Horizontal, cwd_opt, Some(command));
            }
            return;
        }

        // 3. Nothing to attach to.
        self.ipc_new_tab(config, cwd_opt, Some(command));
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
        // Claude sessions currently running in a pane. They are already listed
        // in the panes section, so the archived section leaves them out.
        let mut live_sessions = Vec::new();

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
                    if let Some(id) = pane.claude_session_id() {
                        live_sessions.push(id);
                    }
                    panes_snap.push(SearchPaneSnapshot {
                        tab_id: tab.id,
                        tab_title: tab_title.clone(),
                        pane_id: pane.id,
                        pane_title: pane.display_title("shell"),
                        terminal: pane.terminal.clone(),
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
            state.needs_search = false;
            if state.query.is_empty() || state.searching {
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
            let hits = run_search_worker(&query, &tabs_snap, &panes_snap, &live_sessions);
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
            '\r' => {
                // Enter: open the selected hit. Live search keeps the rows fresh,
                // so the only fallback is to force a scan if one is owed but the
                // debounce hasn't fired yet.
                let action = {
                    let guard = self.ivars().search_palette.borrow();
                    let state = match guard.as_ref() {
                        Some(s) => s,
                        None => return,
                    };
                    match state.rows.get(state.selected) {
                        Some(SearchRow::Hit(hit)) => Some(hit.clone()),
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
                            SearchTarget::Recall { .. } => unreachable!("handled above"),
                        }
                    }
                    None => self.submit_search_palette(),
                }
            }
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
            c if is_typed_char(c) => {
                // Insert printable character; queue a live search.
                let mut guard = self.ivars().search_palette.borrow_mut();
                if let Some(state) = guard.as_mut() {
                    let byte_idx = state.query.char_indices()
                        .nth(state.cursor).map(|(i, _)| i)
                        .unwrap_or(state.query.len());
                    state.query.insert(byte_idx, c);
                    state.cursor += 1;
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
        content_snippet(text, &text.to_ascii_lowercase(), term)
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
