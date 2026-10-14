# 2.0.0

- Kova is now also a terminal library: another app (Kana first) can host Kova panes through a C API, with the same terminal code as Kova.app
- Cmd+digit switches tabs on AZERTY keyboards too
- The attention banner shows the Claude session name, and `list-panes` gives each pane's SendMessage address
- Cmd+Shift+F finds words typed in open Claude panes, whole words first, closed sessions by date
- Wide fallback glyphs such as circled digits fit in their cell, and the cursor blinks on real time even when frames drop
