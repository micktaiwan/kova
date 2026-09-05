//! The recent-projects overlay (`Cmd+O`): the saved sessions it lists, and what
//! reopening one restores.

use super::*;

pub(super) struct RecentProjectItem {
    pub(super) entry: crate::recent_projects::RecentProject,
    /// Pre-computed render data for the renderer.
    pub(super) render: crate::renderer::RecentProjectEntry,
}

pub(super) struct RecentProjectsState {
    pub(super) items: Vec<RecentProjectItem>,
    pub(super) selected: usize,
    /// Scroll offset (index of first visible entry).
    pub(super) scroll: usize,
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

impl KovaView {
    /// Open the recent projects overlay.
    pub(super) fn do_open_recent_projects(&self) {
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
    pub(super) fn restore_recent_project(&self, entry: &crate::recent_projects::RecentProject) {
        let config = self.ivars().config.get().unwrap();
        let cols = config.terminal.columns;
        let rows = config.terminal.rows;

        match crate::session::restore_saved_tab(&entry.tab, cols, rows, config) {
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
}
