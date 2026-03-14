# Split System Specs

## Data Model

### SplitTree (binary tree)

```rust
enum SplitTree {
    Leaf(Pane),
    HSplit { left, right, ratio: f32, root: bool, custom_ratio: bool },
    VSplit { top, bottom, ratio: f32, root: bool, custom_ratio: bool },
}
```

- **`ratio`** — fraction of space allocated to the first child (left or top), range `0.0–1.0`. The second child gets `1.0 - ratio`.
- **`root`** — whether the split was created at root level (`Cmd+E` / `Cmd+Shift+E`). Root splits affect `virtual_width` for horizontal scrolling. Local splits (`Cmd+D` / `Cmd+Shift+D`) subdivide within existing space.
- **`custom_ratio`** — `true` when the user has manually adjusted the ratio (keyboard or mouse drag). Prevents `equalize()` from overwriting it.

Each tab owns one `SplitTree`. Each leaf is a `Pane` with its own PTY.

### Viewport computation

`for_each_pane_with_viewport()` walks the tree recursively. At each split node, `split_sizes(total, ratio)` computes child dimensions:

```
first_size  = total * ratio
second_size = total * (1.0 - ratio)
```

Exception: minimized panes collapse to a fixed `MINIMIZED_BAR_PX` (24px).

## Creating Splits

### Local split (Cmd+D / Cmd+Shift+D)

- Subdivides the focused pane in place.
- New split starts at `ratio: 0.5`, `root: false`, `custom_ratio: false`.
- `equalize()` is called after insertion to distribute space evenly.
- New pane inherits the CWD of the focused pane (via `proc_pidinfo`).

### Root split (Cmd+E / Cmd+Shift+E)

- Adds a column/row at the root level of the tree.
- `root: true` — counts toward `virtual_width` calculation.
- `equalize()` is called after insertion.
- When `root_columns * min_split_width > screen_width`, horizontal scrolling activates.

## Resizing Splits

Three distinct resize modes, each with its own shortcut and behavior. They never mix.

### Mode 1 — Ratio resize (Cmd+Ctrl+Arrows)

Moves the nearest separator in the arrow direction. Does not directly change virtual width, but the max pane width invariant may reduce it (see below).

| Shortcut | Action |
|---|---|
| `Cmd+Ctrl+Right` | Move separator right (focused pane or its neighbor grows/shrinks) |
| `Cmd+Ctrl+Left` | Move separator left |
| `Cmd+Ctrl+Down` | Move separator down |
| `Cmd+Ctrl+Up` | Move separator up |

**Algorithm**: find the separator adjacent to the focused pane in the direction of the arrow. Move it in that direction by adjusting the ratio (±0.05). If no separator exists in that direction, fall back to the nearest separator (pane shrinks).

- Clamp ratio to `[0.1, 0.9]`.
- Set `custom_ratio = true`.
- Blocked if either child is fully minimized.

### Mode 2 — Virtual width global (Ctrl+Option+Arrows)

Changes `virtual_width_override` for the entire tab. All panes grow or shrink proportionally (ratios unchanged).

| Shortcut | Action |
|---|---|
| `Ctrl+Option+Right` | Increase virtual width by 200pt × scale |
| `Ctrl+Option+Left` | Decrease virtual width (min = screen width, then override resets to 0) |

Horizontal scrolling activates when `virtual_width > screen_width`. Auto-scrolls to keep focused pane visible.

### Mode 3 — Edge grow (Cmd+Ctrl+Option+Arrows)

Grows or shrinks only the focused pane by expanding/shrinking virtual width. All other panes keep their absolute pixel size.

| Shortcut | Action |
|---|---|
| `Cmd+Ctrl+Option+Right` | Focused pane grows: virtual width increases, right border moves right. Ratios adjusted so all other panes keep their pixel size. |
| `Cmd+Ctrl+Option+Left` | Reverse: virtual width decreases, right border moves back left. No-op if `virtual_width_override` is already 0. |
| `Cmd+Ctrl+Option+Down` | Same on vertical axis (future — no-op for now) |
| `Cmd+Ctrl+Option+Up` | Same on vertical axis (future — no-op for now) |

Left is the undo of Right — it always affects the right border. To move the left border, use Mode 1 (ratio resize).

The pane does not need to be at an edge. For a middle pane (e.g. C in `A|C|B`), Right grows C while A and B both keep their pixel size. This requires adjusting ratios at every ancestor HSplit in the tree, not just the parent.

Only horizontal axis for now (no `virtual_height_override`). Vertical is a no-op.

### Status bar feedback

On any resize action (any of the 3 modes), the global status bar displays on the left:

- The active resize mode name (`Ratio`, `Virtual`, `Edge`)
- Screen width in pixels
- Virtual width in pixels

This info disappears 2 seconds after the last resize action.

### Mouse drag on separators

**Hit-testing** (`hit_test_separator`):

1. Collect all separator positions from the tree (`collect_separator_info`).
2. For each separator, check if the click is within `4px * backing_scale` of the separator line, and within its cross-axis extent.
3. On match, create a `SeparatorDrag` capturing:
   - `origin_pixel` — mouse position at drag start
   - `origin_ratio` — ratio at drag start
   - `parent_dim` — total dimension of the parent along the split axis
   - `node_ptr` — pointer address of the split node (stable identifier)
   - `is_hsplit` — axis

**Drag tracking** (`mouseDragged`):

```
new_ratio = origin_ratio + (current_pixel - origin_pixel) / parent_dim
```

The ratio is set via `set_ratio_by_ptr()`, clamped to `[0.1, 0.9]`, and `custom_ratio` is set to `true`.

**Drag end** (`mouseUp`): clears the `drag_separator` state.

### Ratio clamping

All ratio modifications clamp to `[0.1, 0.9]`. This guarantees each child gets at least 10% of the parent's dimension — no pane can be resized to zero.

### Max pane width

**Invariant**: no pane's pixel width may ever exceed the real screen width. Enforced after every resize operation via post-validation (action happens first, then correction).

Two enforcement strategies depending on the mode:

- **Mode 1** (ratio resize): the user's ratios are preserved. If a pane exceeds screen width, `virtual_width_override` is reduced instead (`cap_virtual_width`).
- **Modes 2 & 3** (virtual width changes): ratios are adjusted first to cap oversized panes at screen width — siblings absorb the freed space (`clamp_pane_widths`). If ratios alone can't fix it, `virtual_width_override` is reduced as last resort (`enforce_max_pane_width`).

## Equalization

`equalize()` redistributes ratios so that all panes along a same-direction chain get equal space. Only affects splits where `custom_ratio == false`.

**Example**: `HSplit(A, HSplit(B, C))` with no custom ratios:
- Outer HSplit: left has 1 pane, right has 2 → ratio = 1/3
- Inner HSplit: top has 1, bottom has 1 → ratio = 1/2
- Result: A=1/3, B=1/3, C=1/3

**Triggered after**: split creation, pane removal.

**Skipped for**: splits with `custom_ratio = true` (user-adjusted ratios are preserved).

## Separators

### Visual rendering

- 1px semi-transparent line between splits.
- HSplit → vertical line; VSplit → horizontal line.

### Hit zone

- Tolerance: `4px * backing_scale` on each side of the separator line.
- Cross-axis: full extent of the separator (from parent viewport start to end).

## Pane Removal

When a pane is closed (`exit` / `Cmd+W`):

1. The pane is removed from the tree; its sibling replaces the parent split node.
2. `equalize()` is called to redistribute non-custom ratios.
3. Focus moves to the next pane.
4. If no panes remain in the tab, the tab is closed. If no tabs remain, the window closes.

## Session Persistence

Saved in `SavedTree` (JSON via serde):

```rust
HSplit { left, right, ratio: f32, root: bool, custom_ratio: bool }
VSplit { top, bottom, ratio: f32, root: bool, custom_ratio: bool }
```

- `root` and `custom_ratio` use `#[serde(default)]` for backward compatibility.
- `virtual_width_override` and `scroll_offset_x` are saved per-tab.

## Configuration

```toml
[splits]
min_width = 300.0   # Minimum split width in points (for horizontal scroll threshold)

[keys]
# Mode 1 — Ratio resize
resize_left = "cmd+ctrl+left"
resize_right = "cmd+ctrl+right"
resize_up = "cmd+ctrl+up"
resize_down = "cmd+ctrl+down"
# Mode 2 — Virtual width global: Ctrl+Option+Arrows (hardcoded in keyDown)
# Mode 3 — Edge grow
edge_grow_left = "cmd+ctrl+option+left"
edge_grow_right = "cmd+ctrl+option+right"
edge_grow_up = "cmd+ctrl+option+up"
edge_grow_down = "cmd+ctrl+option+down"
# Splits
vsplit = "cmd+d"
hsplit = "cmd+shift+d"
vsplit_root = "cmd+e"
hsplit_root = "cmd+shift+e"
```

## Related

- [Horizontal scroll splits](../notes/horizontal-scroll-splits.md) — virtual width, root vs local splits, scroll mechanics.
