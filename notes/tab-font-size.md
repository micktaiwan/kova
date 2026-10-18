# Tab Bar Font Size — Design Notes

## Problem

The tab bar uses the renderer's `cell_size()` (= terminal font size) for:
- the bar height (`cell_h * 2.0`)
- the size of the tab text

No separate config. We want to be able to configure the tab font size independently, per window.

## Current state

- `TabBarConfig` only contains colors (`bg_color`, `fg_color`, `active_bg`)
- `FontConfig` has a single global `size` for the terminal
- `tab_bar_height()` in `window.rs:913` depends on `renderer.cell_size()`
- The renderer draws the tabs via `build_tab_bar_vertices()` with the same font as the terminal

## Options

### A — `font_size` in `TabBarConfig` (global TOML config)

Add a `font_size: f64` field to `TabBarConfig`.

```toml
[tab_bar]
font_size = 13.0
```

- The renderer keeps separate metrics for the tab bar (ascent, descent, dedicated cell_h)
- `tab_bar_height()` uses these metrics instead of `cell_size()`
- Simple, a single value for all windows
- No need to store per-window state

### B — Per-window override (runtime)

Each `KovaWindow` stores a `tab_font_size: Option<f64>` that overrides the global config.

- Dedicated shortcuts (e.g. `Cmd+Shift+=` / `Cmd+Shift+-`) to adjust per window
- Not persisted across sessions (volatile)
- More complex: each window can have different tab bar metrics

### C — Ratio relative to the terminal font

A `tab_font_ratio: f64` (e.g. `0.8` = 80% of the terminal size) in the config.

```toml
[tab_bar]
font_ratio = 0.8
```

- Tabs scale automatically when the terminal is zoomed
- Natural behavior when changing the font size with Cmd+/-
- Less absolute control

## Technical points

### Glyph atlas

The atlas is indexed by (glyph_id, font_size). If the tab bar has a different size from the terminal, the tab glyphs will be separate entries in the atlas. No architectural problem, just a bit more GPU memory.

### Layout recomputation

Changing the tab font size changes `tab_bar_height()`, which affects the terminal content rect (`content_rect()`). We need to:
1. Recompute the layout
2. Resize the PTYs if the cell dimensions change
3. Redraw

It is the same path as the window resize, already handled.

### Suggested implementation (option A)

1. Add `font_size: Option<f64>` to `TabBarConfig` (None = same as terminal)
2. In the renderer, compute `tab_cell_h` separately if configured
3. `tab_bar_height()` uses `tab_cell_h` instead of `cell_h`
4. `build_tab_bar_vertices()` uses the tab metrics to position the text

## Decision

To be made. Option A is the natural starting point — the per-window override (B) can always be added on top later.
