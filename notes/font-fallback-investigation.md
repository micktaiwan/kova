# Font Fallback — Investigation (2026-02-24)

## What was implemented

File changed: `src/renderer/glyph_atlas.rs`

1. **UTF-16 surrogate pair support**: removed the early return `if count != 1` that blocked characters outside the BMP (emoji). We now pass `count` (1 or 2) to `glyphs_for_characters`.

2. **Font fallback via CoreText**: new `resolve_glyph()` method that:
   - First tries the primary font
   - If it fails (`!ok || glyph_id == 0`), calls `CTFont::for_string()` (= `CTFontCreateForString`) to get a fallback font
   - Caches the fallback font in `HashMap<char, CFRetained<CTFont>>`

3. **Imports added**: `CFRange`, `CFRetained` from `objc2_core_foundation`

## What works

- `printf '─│┌┐\n'` → box-drawing renders correctly
- `printf '\e[38;5;208m╭─╮\e[0m\n'` → colored box-drawing OK
- Alt screen with box-drawing (manual test) → OK
- Emoji `echo "🎉"` → the fallback to Apple Color Emoji works (glyph found), but the rendering is ugly because we rasterize in white monochrome (1-component alpha bitmap). Color emoji would need a separate RGBA pipeline.

## Remaining problem: Claude Code banner

The Claude Code banner (Ink/React) shows `─` (U+2500) borders. Investigation:

### Verified facts

1. The `─` reach `put_char` (rows 2, 6, 8) — **OK**
2. The glyph is rasterized (272 non-zero bytes on a 17×33 bitmap) — **OK**
3. Vertices are generated every frame (3948 render logs over ~10s) — **OK**
4. The glyph works in normal mode (`printf '─\n'`) — **OK**
5. The glyph works in a manual alt screen (`printf '\e[?1049h...'`) — **OK**
6. The glyph works with ANSI color (`printf '\e[38;5;208m─\e[0m\n'`) — **OK**
7. Menlo has the box-drawing glyphs natively (glyph_id=2236 for ─) — no fallback needed
8. `$TERM=xterm-256color`, `$TERM_PROGRAM=Kova` — standard config

### Observation not yet explained

A render log shows `fg=[0.1, 0.1, 0.12]` (almost black) for some `─`. **WARNING**: this log may have been read from a stale log file (the code filter was `row_idx==2 && col_idx<3` but the log showed `row=0, col=48`). **It must be re-checked with a clean log file** to confirm whether the fg color really is the problem.

### Next step for the banner

Run a clean test again with Claude Code in Kova, log to a **new file**, and check:
- The exact fg color of the banner's `─` (with a row/col filter matching the banner lines)
- Whether cells are overwritten between `put_char` and rendering

## Confirmed problem: rendering of block elements and box-drawing

### Finding

Comparison of the Claude Code banner in Kova vs another terminal:
- **Horizontal black line** crossing the logo (made of block elements ▐▛█▜▌▝▘)
- **Darker/desaturated colors** in Kova

The black line comes from the font glyphs not filling 100% of the cell (hinting, margins). Block elements like `█` are meant to cover the whole cell edge to edge, but CoreText rendering leaves gaps.

### Approach of modern terminals (verified)

The major terminals draw box-drawing and block elements themselves instead of going through the font:
- **Alacritty**: builtin font (commit f717710) — "font glyphs tend to overlap or not align"
- **Windows Terminal**: draws box-drawing and powerline glyphs manually
- **GNOME Terminal**: 5×5 bitmaps stretched to fill the cell
- **Kitty**: custom rendering for box-drawing

### Implementation plan

In `rasterize_char()`, for the following Unicode ranges, draw the pixels directly into the bitmap instead of going through CoreText:

1. **Block elements** (U+2580-U+259F) — high priority (Claude logo)
   - `█` (U+2588): fill the whole cell
   - `▌` (U+258C): fill the left half
   - `▐` (U+2590): fill the right half
   - `▀` (U+2580): fill the top half
   - `▄` (U+2584): fill the bottom half
   - `▛` (U+259B), `▜` (U+259C), `▝` (U+259D), `▘` (U+2598), etc.: quadrants

2. **Box-drawing** (U+2500-U+257F) — medium priority (borders)
   - `─` (U+2500): centered horizontal line
   - `│` (U+2502): centered vertical line
   - `┌┐└┘`: corners (line junctions)
   - `╭╮╰╯`: rounded corners
   - `├┤┬┴┼`: T junctions and crosses
   - Bold (━┃), double (═║╔╗╚╝) variants, etc.

Approach: in `rasterize_char()`, before calling `resolve_glyph()`, check whether the char is in these ranges. If so, fill `bmp_buf` directly with white pixels at the right positions, then continue the normal flow (atlas copy, GPU upload).

Ref: https://github.com/alacritty/alacritty/commit/f7177101eda589596ab08866892bd4629bd1ef44

## Debug logs added (to remove)

- `src/renderer/glyph_atlas.rs`: `resolve_glyph()` logs the char, utf16_len, primary/fallback results, nonzero bytes
- `src/renderer/mod.rs`: log of the rendering of `─` with fg/bg (filtered row_idx==2, col_idx<3)
- `src/terminal/mod.rs`: log in `put_char` for box-drawing U+2500-U+257F

## Code state

The code compiles and the font fallback works. The debug logs are still present.
