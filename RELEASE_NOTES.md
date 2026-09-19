# 1.13.0

- Cmd+P groups your anchors by directory, and an anchored conversation is no longer listed a second time under its tab — even when its pane is still waiting on its `claude --resume` line
- An anchor whose pane is open shows the version running in it, at the right end of its row
- Press `b` in Cmd+P to swap the open panes for your bookmarks, and back: the list opens on the panes, and the hint line now colors its keys
- Bookmarks are no longer capped at 32
- The same conversation running in two panes is detected: Kova asks whether to focus the original (the duplicate pane is closed) or keep both
