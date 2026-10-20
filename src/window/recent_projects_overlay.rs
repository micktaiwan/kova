//! The closed-tabs overlay (`Cmd+O`): the tabs it lists, by name, and what
//! reopening one restores.

use super::*;

pub(super) struct RecentProjectItem {
    pub(super) entry: crate::recent_projects::RecentProject,
    /// Pre-computed render data for the renderer.
    pub(super) render: crate::renderer::RecentProjectEntry,
}

pub(super) struct RecentProjectsState {
    pub(super) items: Vec<RecentProjectItem>,
    /// Filter typed in the input field, matched live against name and path.
    pub(super) query: String,
    /// Caret position in `query`, in chars.
    pub(super) cursor: usize,
    /// Indices into `items` of the rows the query keeps, in list order.
    pub(super) visible: Vec<usize>,
    /// Index into `visible`.
    pub(super) selected: usize,
    /// Scroll offset (index into `visible` of the first row on screen).
    pub(super) scroll: usize,
}

impl RecentProjectsState {
    fn new(items: Vec<RecentProjectItem>) -> Self {
        let visible = (0..items.len()).collect();
        Self { items, query: String::new(), cursor: 0, visible, selected: 0, scroll: 0 }
    }

    /// Recompute the rows the query keeps and select the first one: after an
    /// edit the best candidate is the top of the new list.
    fn refilter(&mut self) {
        self.visible = matching_indices(&self.items, &self.query);
        self.selected = 0;
        self.scroll = 0;
    }

    fn selected_item(&self) -> Option<&RecentProjectItem> {
        self.visible.get(self.selected).and_then(|&i| self.items.get(i))
    }

    fn insert_at_cursor(&mut self, text: &str) {
        let byte_idx = self.query.char_indices()
            .nth(self.cursor).map(|(i, _)| i)
            .unwrap_or(self.query.len());
        self.query.insert_str(byte_idx, text);
        self.cursor += text.chars().count();
        self.refilter();
    }
}

/// Rows whose name or path contains every word of `query`, case-insensitive.
/// An empty query keeps everything.
fn matching_indices(items: &[RecentProjectItem], query: &str) -> Vec<usize> {
    let terms: Vec<String> = query.split_whitespace().map(str::to_lowercase).collect();
    items.iter().enumerate()
        .filter(|(_, item)| {
            let haystack = format!(
                "{} {}",
                item.render.title.as_deref().unwrap_or(""),
                item.render.path,
            ).to_lowercase();
            terms.iter().all(|t| haystack.contains(t.as_str()))
        })
        .map(|(i, _)| i)
        .collect()
}

fn build_items(entries: Vec<crate::recent_projects::RecentProject>) -> Vec<RecentProjectItem> {
    entries.into_iter().map(|e| {
        let render = crate::renderer::RecentProjectEntry {
            title: e.title().map(String::from),
            color: e.tab.color,
            path: crate::recent_projects::tildify(&e.path),
            time_ago: crate::recent_projects::time_ago(e.last_opened),
            pane_count: crate::recent_projects::pane_count_tab(&e.tab),
            invalid: !std::path::Path::new(&e.path).is_dir(),
        };
        RecentProjectItem { entry: e, render }
    }).collect()
}

/// Keys of the tabs open in ALL windows: a closed tab whose key is here is open
/// again, and must not be restored a second time. Uses NSApplication::windows()
/// to avoid borrowing the app delegate's window list (which may be borrowed by
/// the timer tick).
pub(super) fn open_tab_keys() -> std::collections::HashSet<String> {
    let mtm = unsafe { MainThreadMarker::new_unchecked() };
    let app = NSApplication::sharedApplication(mtm);
    let ns_windows = app.windows();
    let mut keys = std::collections::HashSet::new();
    for i in 0..ns_windows.count() {
        let win = &ns_windows.objectAtIndex(i);
        if let Some(view) = crate::app::kova_view(win) {
            let tabs = view.ivars().tabs.borrow();
            for tab in tabs.iter() {
                keys.insert(crate::recent_projects::tab_key(tab));
            }
        }
    }
    keys
}

impl KovaView {
    /// Open the closed-tabs overlay, minus the tabs that are open again.
    pub(super) fn do_open_recent_projects(&self) {
        let open_keys = open_tab_keys();
        let all = crate::recent_projects::load();
        let entries: Vec<_> = crate::recent_projects::dedup(all.projects).into_iter()
            .filter(|p| !open_keys.contains(&p.key()))
            .collect();

        *self.ivars().recent_projects.borrow_mut() = Some(RecentProjectsState::new(build_items(entries)));
        self.mark_dirty();
    }

    /// Cmd+V while the overlay is open: the clipboard's first line goes into
    /// the filter at the caret.
    pub(super) fn paste_into_recent_projects(&self, text: &str) {
        let pasted = super::overlays::line_paste_text(text);
        if pasted.is_empty() {
            return;
        }
        if let Some(state) = self.ivars().recent_projects.borrow_mut().as_mut() {
            state.insert_at_cursor(&pasted);
        }
        self.mark_dirty();
    }

    /// Handle key events in the recent projects overlay.
    pub(super) fn handle_recent_projects_key(&self, event: &NSEvent) {
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
                    let item = s.selected_item()?;
                    if !item.render.invalid { Some(item.entry.clone()) } else { None }
                })
            };
            if let Some(entry) = entry {
                *self.ivars().recent_projects.borrow_mut() = None;
                self.restore_recent_project(&entry);
            }
            return;
        }

        // Backspace: Cmd+Backspace removes the selected entry from the list,
        // a plain Backspace edits the filter.
        if keycode == 0x33 {
            let has_cmd = event.modifierFlags().contains(NSEventModifierFlags::Command);
            if has_cmd {
                let key = {
                    let mut guard = self.ivars().recent_projects.borrow_mut();
                    let state = match guard.as_mut() {
                        Some(s) => s,
                        None => return,
                    };
                    let Some(&idx) = state.visible.get(state.selected) else {
                        return;
                    };
                    let key = state.items[idx].entry.key();
                    state.items.remove(idx);
                    if state.items.is_empty() {
                        *guard = None;
                    } else {
                        // Keep the selection where it was in the filtered list.
                        let selected = state.selected;
                        state.visible = matching_indices(&state.items, &state.query);
                        state.selected = selected.min(state.visible.len().saturating_sub(1));
                        state.scroll = state.scroll.min(state.selected);
                    }
                    key
                };
                crate::recent_projects::remove(&key);
            } else if let Some(state) = self.ivars().recent_projects.borrow_mut().as_mut() {
                if state.cursor > 0 {
                    if let Some((byte_idx, _)) = state.query.char_indices().nth(state.cursor - 1) {
                        state.query.remove(byte_idx);
                        state.cursor -= 1;
                        state.refilter();
                    }
                }
            }
            self.mark_dirty();
            return;
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
                    if state.selected + 1 < state.visible.len() {
                        state.selected += 1;
                    }
                }
                0x7B => { // Left: move the caret
                    if state.cursor > 0 { state.cursor -= 1; }
                }
                0x7C => { // Right
                    if state.cursor < state.query.chars().count() { state.cursor += 1; }
                }
                // Typing goes into the filter. Cmd or Ctrl with a letter is a
                // shortcut, not typing; insert what the layout composed
                // (Option+( = { on a French keyboard), not the bare key.
                _ if !super::search_palette::is_shortcut(event) => {
                    let text = super::search_palette::typed_text(event);
                    if text.is_empty() {
                        return; // a dead key waiting for its second stroke
                    }
                    state.insert_at_cursor(&text);
                }
                _ => {}
            }
        }
        self.mark_dirty();
    }

    /// Restore a recent project as a new tab in this window.
    pub(super) fn restore_recent_project(&self, entry: &crate::recent_projects::RecentProject) {
        let config = self.ivars().config.get().unwrap();
        let cols = config.terminal.columns;
        let rows = config.terminal.rows;

        match crate::session::restore_saved_tab(&entry.tab, cols, rows, config, false) {
            Some(mut tab) => {
                tab.adopt_geometry_scale(self.backing_scale());
                let mut tabs = self.ivars().tabs.borrow_mut();
                let new_idx = self.ivars().active_tab.get() + 1;
                tabs.insert(new_idx, tab);
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
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A `SavedTab` carrying `n` panes in one flat column, built through serde
    /// so the test names only the fields it cares about.
    fn saved_tab_with_panes(n: usize) -> crate::session::SavedTab {
        let panes: Vec<serde_json::Value> = (0..n).map(|_| serde_json::json!({ "cwd": null })).collect();
        serde_json::from_value(serde_json::json!({
            "flat_columns": [{ "panes": panes, "row_weights": vec![1.0; n] }],
            "focused_leaf_index": 0,
            "custom_title": null,
            "color": null,
        }))
        .expect("a SavedTab the recent-projects list can count")
    }

    #[test]
    fn build_items_flags_a_project_whose_directory_is_gone() {
        // The overlay greys out an entry it can no longer reopen, so `invalid`
        // is read off the filesystem, not off the saved session.
        let items = build_items(vec![
            crate::recent_projects::RecentProject {
                path: "/".into(),
                last_opened: 0,
                tab: saved_tab_with_panes(2),
            },
            crate::recent_projects::RecentProject {
                path: "/kova-no-such-directory".into(),
                last_opened: 0,
                tab: saved_tab_with_panes(1),
            },
        ]);
        assert_eq!(items.len(), 2);
        assert!(!items[0].render.invalid, "/ exists");
        assert!(items[1].render.invalid, "a missing directory is flagged");
        // The pane count shown on the row comes from the saved tab.
        assert_eq!(items[0].render.pane_count, 2);
        assert_eq!(items[1].render.pane_count, 1);
        // The entry itself is carried through untouched, since selecting the
        // row restores it.
        assert_eq!(items[1].entry.path, "/kova-no-such-directory");
    }

    #[test]
    fn the_filter_keeps_rows_matching_every_word_in_name_or_path() {
        let entry = |title: Option<&str>, path: &str| {
            let mut tab = saved_tab_with_panes(1);
            tab.custom_title = title.map(String::from);
            crate::recent_projects::RecentProject { path: path.into(), last_opened: 0, tab }
        };
        let items = build_items(vec![
            entry(Some("Kova"), "/Users/me/projects/perso/kova"),
            entry(None, "/Users/me/projects/acme/app"),
            entry(Some("Notes"), "/tmp"),
        ]);
        assert_eq!(matching_indices(&items, ""), vec![0, 1, 2], "an empty filter keeps everything");
        assert_eq!(matching_indices(&items, "KOVA"), vec![0], "case-insensitive");
        assert_eq!(matching_indices(&items, "acme"), vec![1], "matches the path of an unnamed tab");
        assert_eq!(matching_indices(&items, "notes tmp"), vec![2], "name and path words combine");
        assert_eq!(matching_indices(&items, "projects app"), vec![1], "every word must match");
        assert!(matching_indices(&items, "zzz").is_empty());
    }
}
