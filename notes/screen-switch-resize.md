# Grid and pane widths change when switching display (2026-09-02)

Audited, then fixed the same day. The four causes below were read from the tree
at e2093ab, or measured with CoreText on the actual default font; what was done
about each is at the bottom.

**The rule this codebase now follows**: a pane keeps the same *apparent* width on
every display. The number of pixels that width takes changes with the display's
scale factor — that is expected and must be converted for; the physical size on
screen must not change, and neither must the grid inside the pane.

## What the user sees

Moving the window between the retina screen and the external one changes the
terminal grid (programs reflow their tables) and can change pane widths
permanently.

## Chain of events

`viewDidChangeBackingProperties` (`src/window.rs:1074`) → `handle_resize`
(`src/window.rs:6107`) → rebuild atlas at the new scale
(`renderer/mod.rs:2268`) → `enforce_max_pane_width` on every tab
(`src/window.rs:2096`) → `resize_all_panes` (`src/window.rs:5610`) →
`term.resize` + `pty.resize` → SIGWINCH → the child reflows.

## Cause 1 — the cell is rounded in physical pixels

The atlas is built at `font.size * scale` (`renderer/mod.rs:2270`) and the cell
metrics are `ceil()`-ed there in physical pixels: `cell_height`
(`renderer/glyph_atlas.rs:88`), `cell_width` (`:109`). Everything downstream is
physical: `drawable_viewport` (`window.rs:2238`), `cell_size`
(`renderer/mod.rs:2994`), and the two grid computations
(`viewport_to_grid` `window.rs:2349`, and the inline copy in `resize_all_panes`
`window.rs:5636-5638`).

Measured on the default font actually in use — there is no
`~/.config/kova/config.toml`, so defaults apply: Hack 13pt
(`src/config.rs:173-174`).

| | 'M' advance | cell_w phys | cell_w logical | asc+desc+lead | cell_h phys | cell_h logical |
|---|---|---|---|---|---|---|
| 1x | 7.827 | 8 | 8.00 | 15.133 | 16 | 16.00 |
| 2x | 15.653 | 16 | 8.00 | 30.266 | 31 | 15.50 |

So with this font the **width rounds identically** on both scales — the July 3
note assumed columns were the victim, at font 14. What actually differs here is
the **height**: 15.5 logical px per row on retina vs 16.0 on the external
screen, i.e. ~3% more rows on retina for the same physical window height.
Change the font or its size and the roles can swap; the defect is the rounding
position, not the particular font.

## Cause 2 — the horizontal padding is not scaled

`PANE_H_PADDING = 10.0` (`renderer/mod.rs:5`) is added in physical pixels and
never multiplied by the scale. Columns are
`(vp.width - 2*PANE_H_PADDING) / cell_w`, so for a logical width W:

- 2x: `(2W - 20) / 16 = W/8 - 1.25`
- 1x: `(W - 20) / 8  = W/8 - 2.50`

A constant **1.25-column difference** between the two screens, independent of
the font. This one is enough on its own to reflow a table, and it is the part
that bites with the current default font.

`min_split_width_px` (`window.rs:2053`) does scale correctly — it is the
exception, not the rule.

## Cause 3 — pane geometry is stored in physical pixels and never rescaled

`Tab::virtual_width_override` and `Tab::scroll_offset_x` (`src/pane.rs:461-463`)
hold physical pixels. Nothing rescales them when the backing scale changes:
`handle_resize` rebuilds the atlas and reflows, but no code path multiplies or
divides those two by `new_scale / old_scale`. An override of 4000 phys means
2000 logical px on retina and 4000 logical px on the external screen — the same
tab is twice as wide in real terms after the switch.

`virtual_width_override` is also persisted as-is in the session file
(`src/session.rs:191`, restored `:672`), so the mismatch survives a restart.

## Cause 4 — the correction is lossy and one-way

`enforce_max_pane_width` (`window.rs:2096`) runs on every `handle_resize`. On
the smaller screen it calls `clamp_pane_widths`, which **mutates**
`column_weights` in place (`src/pane.rs:1338`), then can set
`virtual_width_override` to `max_vw` or to `0.0` (`window.rs:2107`).

Nothing remembers the pre-switch values. Moving back to the large screen does
not restore them: the ratios stay clamped and the override stays dropped. This
is the "my panes are not the size I set them any more" half of the symptom, and
it is the only irreversible one — causes 1 to 3 are symmetric, this one loses
state.

## Secondary — the reflow round-trip has its own machinery

`resize_all_panes` already carries round-trip detection, a fake off-by-one PTY
resize and a debounced repaint (`window.rs:5643-5690`) because children coalesce
SIGWINCHs. A screen switch fires the exact pattern this code was written for, so
part of the visible flicker on switch comes from there, not from the geometry.

## What was done

1. **Cause 1** — `round_metric_in_points` (`renderer/glyph_atlas.rs`) rounds the
   cell up to whole logical points and only then converts back to pixels. The
   atlas is still rasterized at the physical size; only the cell metric moved.
   `GlyphAtlas::new` takes the scale for that. Covered by three unit tests.
2. **Cause 2** — `PANE_H_PADDING` is now documented as logical points and only
   reached through `Renderer::h_padding()` and `KovaView::h_padding()`, both of
   which multiply by the scale. All nine call sites went through them.
3. **Cause 3** — `Tab::geometry_scale` records which display's pixels
   `virtual_width_override` and `scroll_offset_x` are expressed in, and
   `Tab::adopt_geometry_scale` converts them by the ratio. It is called on scale
   change (`handle_resize`), at window setup, and on both runtime restore paths
   (recent project, deferred tab). The scale is persisted in the session file
   (`SavedTab::geometry_scale`), so a session restored on another display is
   converted too; sessions written before the field simply adopt the current
   scale. Two unit tests on `geometry_ratio`.
4. **Cause 4** — `handle_resize` no longer calls `enforce_max_pane_width`. On a
   narrower display the panes keep their width and the tab scrolls horizontally;
   only `clamp_scroll` still runs. `enforce_max_pane_width` remains on the
   user-driven resize paths (virtual width, edge grow), where it was asked for.

The reflow round-trip machinery in `resize_all_panes` was left untouched.

## Cause 5 — a tab with no manual width follows the screen (2026-09-04)

The four fixes above all act on `virtual_width_override`. A tab that never got
one is untouched by every one of them: `Tab::virtual_width` (`src/pane.rs`)
falls back to `max(n * min_split_width, screen_width)`, so its total width *is*
the screen's. Unplug the 2560pt monitor and the same six panes are redistributed
over the 1728pt builtin — each loses a third of its columns, permanently, and
`adopt_geometry_scale` converts nothing because there is nothing stored.

Fixed by pinning: `handle_resize` remembers the screen's logical width
(`last_screen_w`), and on a change of screen writes the width the tab was laid
out at into `virtual_width_override`, so the panes keep their size and the tab
scrolls. `pinned_virtual_width` (`src/pane.rs`, three unit tests) does the
arithmetic and stays silent when the new screen is at least as wide — there the
fallback already gives the panes their size back.

Keyed on the **screen**, not the scale, for two reasons: two displays can share a
backing scale, and AppKit can move the window on one event and change the backing
scale on the next, so the scale check can fire a resize too late.
