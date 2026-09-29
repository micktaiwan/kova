# 1.14.0

- Closing a pane now ends what ran in it: a Claude session no longer lives on in the background with no pane to show it
- A Claude session stranded that way shows up in Cmd+Shift+F as closed, marked "stranded", and reopening it ends the old process first
- New hooks commands `mark-completed` and `bell`, and a pane holding one of today's anchors wears an anchor in the switcher, whose dots now update while it is open
- The search palette takes pastes and composed characters (Option+( types {), matches accented text like État, and keeps the caret visible on long queries
- Enter in the search palette never opens a result from an older query
