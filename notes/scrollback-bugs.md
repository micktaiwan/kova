# Scrollback bugs

Tracking of issues related to scrolling and the scrollback buffer.

## Architecture

- `scrollback`: `VecDeque<CompactRow>` — historical lines (FIFO, oldest at the front)
- `grid`: `Vec<Row>` — current screen (fixed size = `rows`)
- `scroll_offset`: `i32` — how many lines to go back up into the scrollback (0 = no scroll)
- `visible_lines()` blends scrollback + grid according to `scroll_offset`
- `push_to_scrollback()` — single helper for push + limit + scroll_offset adjustment

## Fixed bugs

### Empty areas when scrolling up

**Symptom:** Large black areas with no text visible when scrolling far up.

**Cause:** `erase_in_display` mode 2/3 (`ESC[2J`) blanked the grid in place without saving its content into the scrollback. The empty lines were then pushed into the scrollback by `scroll_up` as new content arrived.

**Fix:** Before blanking the grid, we copy the lines up to the last non-empty one into the scrollback. Trailing blanks (empty bottom of the screen) are ignored, but intermediate empty lines are preserved to keep the visual spacing.

## Known bugs / to investigate

_(to be completed)_
