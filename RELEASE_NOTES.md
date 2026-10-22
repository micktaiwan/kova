# 2.0.2

- Subjects: group a tab's panes under a named subject in Cmd+P (Cmd+Right files a pane, Cmd+Left takes it out); a subject survives a relaunch
- Each subject gets its own color, and a strip under the tab bar names the subject over its panes
- After a relaunch, agent conversations resume only when their pane is first shown (or on wake-pane), and the status bar counts the sleeping ones (◌N)
- Resizing the window or dragging a separator no longer resizes every pane several times a frame, and panes scrolled off-screen catch up when shown
- IPC: list-subjects, set-pane-subject, rename-subject, delete-subject, a subject field on each pane and a subjects event in subscribe
