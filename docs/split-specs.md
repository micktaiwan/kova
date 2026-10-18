# Split System Specs

## Data Model

### Tab (flat grid)

Each tab owns a flat two-level grid: columns and rows. No more nesting.

```rust
pub struct Tab {
    pub columns: Vec<Column>,         // >= 1, flat list of columns
    pub column_weights: Vec<f32>,     // one weight per column (proportional, not normalized)
    pub custom_weights: Vec<bool>,    // true = column resized manually ("pinned")
    pub focused_pane: PaneId,
    pub virtual_width_override: f32,
    pub scroll_offset_x: f32,
    // ...
}
```

### Column (flat rows)

```rust
pub struct Column {
    pub panes: Vec<Pane>,             // >= 1, flat list of panes (top → bottom)
    pub row_weights: Vec<f32>,        // one weight per row (proportional, not normalized)
    pub custom_row_weights: Vec<bool>,// true = row resized manually ("pinned")
}
```

Effective width of column `i`:

```
column_width[i] = total_width * (column_weights[i] / sum(column_weights))
```

Effective height of pane `j` in column `i`:

```
row_height[j] = column_height * (row_weights[j] / sum(row_weights))
```

Exception: minimized panes → collapse to `MINIMIZED_BAR_PX` (24px). Non-minimized panes share the remaining height proportionally.

- No more `HSplit` or `VSplit` — both axes are flat lists.
- `custom_weights` (columns) and `custom_row_weights` (rows) mark a manual resize. See section "Pinned columns/rows".

### Viewport computation

The viewport is computed on two levels:

1. **Columns**: iterate over `columns` + `column_weights`, allocate each column's width.
2. **Within each column**: iterate over `panes` + `row_weights`, allocate each pane's height.

## Creating splits

### Horizontal split — adding a column

**Cmd+D** — inserts a new column **right after** the focused pane's column.

```
[A | B], focus A → Cmd+D → [A | NEW | B]
[A | B], focus B → Cmd+D → [A | B | NEW]
```

**Cmd+E** — adds a new column **at the end**.

```
[A | B], focus A → Cmd+E → [A | B | NEW]
```

Weight of the new column = `sum(existing_weights) / n_existing` (the average). Existing columns keep their weights — their relative proportions are preserved. The new column gets `1/(n+1)` of the total space.

The new pane inherits the focused pane's CWD (via `proc_pidinfo`).

### Vertical split — adding a row in a column

**Cmd+Shift+D** — inserts a new pane **right below** the focused pane in its column.

```
[A | B], focus A → Cmd+Shift+D →  [ A  | B ]
                                   [NEW |   ]
```

**Cmd+Shift+E** — adds a new pane **at the bottom of the column**.

```
[ A  | B ]                         [ A  | B ]
[ C  |   ], focus A → Cmd+Shift+E → [ C  |   ]
                                   [NEW |   ]
```

Same logic as Cmd+D/E for horizontal: **D = after the current pane, E = at the end**.

Weight of the new row = `sum(existing_row_weights) / n_existing` (the average). Existing rows keep their weights.

## Removing a pane

When a pane is closed (`exit` / `Cmd+W`):

1. **Pane alone in its column** (1 pane, 1 column) → remove the column and its weight from the Vec. The removed weight is redistributed proportionally to the remaining columns.
2. **Pane in a multi-row column** → remove the pane and its `row_weight` from the Vec. The removed weight is redistributed proportionally to the remaining rows of the column.
3. **Last pane of the last column** → close the tab. Last tab → close the window.

Focus after removal: next pane (same logic as today).

## Resizing

Three distinct modes, never mixed.

### Mode 1 — Column/ratio resize (Cmd+Ctrl+Arrows)

**Horizontal (Cmd+Ctrl+Left/Right)** — moves the controlled edge of the focused column.

The **controlled edge** is the column's right edge, except for the last column (left edge).

| Position | `Cmd+Ctrl+Right` | `Cmd+Ctrl+Left` |
|---|---|---|
| First or middle | Right edge → : column grows | Right edge ← : column shrinks |
| Last | Left edge → : column shrinks | Left edge ← : column grows |

- Minimum weight per column: matches `min_split_width` in pixels.
- No fallback: every case is covered by the controlled edge.

**Vertical (Cmd+Ctrl+Up/Down)** — moves the controlled edge of the focused pane within its column.

The **controlled edge** is the pane's bottom edge, except for the last pane of the column (top edge). Same logic as horizontal.

| Position | `Cmd+Ctrl+Down` | `Cmd+Ctrl+Up` |
|---|---|---|
| First or middle | Bottom edge ↓ : pane grows | Bottom edge ↑ : pane shrinks |
| Last | Top edge ↓ : pane shrinks | Top edge ↑ : pane grows |

- Minimum weight per row: 5% of the total.
- Blocked if a neighboring pane is fully minimized.

### Mode 2 — Virtual width global (Ctrl+Option+Left/Right)

Changes `virtual_width_override` for the whole tab. All weights stay identical — columns grow/shrink proportionally.

| Shortcut | Action |
|---|---|
| `Ctrl+Option+Right` | Increases virtual width by 200pt × scale |
| `Ctrl+Option+Left` | Decreases (min = screen width, then override reset to 0) |

Horizontal scroll kicks in when `virtual_width > screen_width`. Auto-scroll keeps the focused pane visible.

### Mode 3 — Edge grow (Cmd+Ctrl+Option+Left/Right)

Grow/shrink a single column. The others keep their pixel size.

| Shortcut | Action |
|---|---|
| `Cmd+Ctrl+Option+Right` | Focused column grows: virtual width increases, weights adjusted so the other columns keep their pixel width |
| `Cmd+Ctrl+Option+Left` | The reverse. No-op if `virtual_width_override` is already 0 |

Horizontal axis only for now.

### Equalize (Cmd+Shift+=)

Resets all `column_weights` to `1.0` and `custom_weights` to `false` (all columns the same width, none pinned). Also resets all `row_weights` to `1.0` and `custom_row_weights` to `false` in every column.

### Status bar feedback

On any resize (3 modes): the status bar shows the active mode, screen width and virtual width. Disappears after 2 seconds.

### Pinned columns/rows (`custom_weight` / `custom_row_weight`)

A column (or row) with `custom_weight = true` was resized manually. It keeps its weight during automatic redistributions. The rules are identical for both axes.

**When a column/row becomes pinned:**

- Mouse drag of a separator → only the column/row **on the direct side** (the one explicitly pushed/pulled) becomes pinned.
- Keyboard resize mode 1 → the **focused column/row** becomes pinned (it is the one the user intentionally resizes). The others that adapt don't change status.

**When a column/row is unpinned:**

- Equalize (Cmd+Shift+=) → everything reset to `false`.
- New column/row created → the new one has `custom_weight = false`. Existing ones keep their state.

**Persistence:** `custom_weight` and `custom_row_weight` are saved/restored with the session.

### Redistribution (columns and rows)

The redistribution algorithm is **identical** for both axes (columns horizontally, rows vertically). In this section, "element" means a column or a row depending on the axis.

#### Algorithm (mouse drag)

Drag of the separator between elements `i` and `i+1`, delta in pixels:

1. **Pushed element**: the element on the side the separator moves toward. Its weight decreases by the delta. It becomes pinned.

2. **Free side**: the space is redistributed among the non-pinned elements on the other side.
   - Non-pinned ones receive an equal share.
   - Pinned ones keep their weight unchanged.
   - None changes pinned status.

3. **Fallback**: if **all** elements on the free side are pinned, only the adjacent one absorbs.

4. **Minimum**: no element can go below 5% of the total weight.

#### Algorithm (keyboard mode 1)

The **controlled edge** is the outer edge of the focused element (right for columns, bottom for rows), except for the **last** element (left / top edge).

**Grow** (push the edge outward):
- The focused element grows and becomes pinned.
- The loss is redistributed among the non-pinned elements on the outer side.

**Shrink** (pull the edge inward):
- The focused element shrinks (stays pinned if it was).
- The gain is redistributed among the non-pinned elements on the outer side.

#### Examples (horizontal)

- `A | B | C`, focus B, `Right`: B grows, C (+ beyond) gives way
- `A | B | C`, focus B, `Left`: B shrinks, C (+ beyond) absorbs
- `A | B | C`, focus C (last), `Left`: C grows (left edge ←), A and B give way

#### Examples (vertical)

```
A
B
C
```
- Focus B, `Down`: B grows, C (+ beyond) gives way
- Focus B, `Up`: B shrinks, C (+ beyond) absorbs
- Focus C (last), `Up`: C grows (top edge ↑), A and B give way

### Mouse drag

**Mouse cursor**: when hovering a separator (±3px around the 1px line), the cursor changes:
- Vertical separator (between columns) → `resizeLeftRight` (↔)
- Horizontal separator (between rows of a column) → `resizeUpDown` (↕)

The cursor goes back to the default arrow when the mouse leaves the tolerance zone.

**Hit-testing**: vertical separators (between columns) and horizontal ones (between rows of a column).

**Vertical separator drag**: see "Redistribution" above (columns axis).

**Horizontal separator drag**: see "Redistribution" above (rows axis).

### Max pane width invariant

**Invariant**: no pane may exceed `screen_width` in pixel width. Checked after every resize via post-validation.

- **Mode 1**: user weights are preserved. If a pane exceeds, `virtual_width_override` is reduced.
- **Modes 2 & 3**: weights are adjusted first to cap columns that are too wide. If that is not enough, `virtual_width_override` is reduced as a last resort.

## Navigation

### Left/Right (Cmd+Option+Left/Right)

Find the column `i` of the focused pane. Go to column `i-1` (Left) or `i+1` (Right). In the target column, pick the vertically closest pane (by viewport center).

### Up/Down (Cmd+Option+Up/Down)

Navigate the pane list of the current column. If already at the first/last, stay.

### Swap (Cmd+Shift+Arrows)

- **Left/Right**: swap the whole column `i` with column `i±1` (swap elements in the Vec + swap the weights).
- **Up/Down**: swap the focused pane with its neighbor in the column (swap in the Vec + swap the row_weights).

### Reparent (Cmd+Ctrl+Shift+Arrows)

- **Left/Right**: move the focused pane into the adjacent column (added at the bottom of the target column). If the source column ends up empty, remove it.
- **Up/Down**: move the pane to the nearest column above/below (if a cross-column vertical layout allows it).

## Visual separators

- **Vertical separators** (1px): between each column. Position = sum of the widths of columns 0..i.
- **Horizontal separators** (1px): between each pane of a multi-row column.

## Virtual width

```
auto_virtual_width = max(columns.len() * min_split_width, screen_width)
```

If `virtual_width_override > 0`:
```
virtual_width = max(virtual_width_override, screen_width)
```

## Session persistence

### Save format

```rust
struct SavedPane {
    cwd: String,
    env: HashMap<String, String>,
    // ...
}

struct SavedColumn {
    panes: Vec<SavedPane>,
    row_weights: Vec<f32>,
    custom_row_weights: Vec<bool>,
}

struct SavedTab {
    columns: Vec<SavedColumn>,
    column_weights: Vec<f32>,
    custom_weights: Vec<bool>,
    focused_leaf_index: usize,
    virtual_width_override: f32,
    scroll_offset_x: f32,
    // ...
}
```

### Old format migration

Two old formats to support:

1. **v2 — `SavedTree` (HSplit/VSplit)**: flatten HSplits into columns, flatten VSplits into rows. Binary ratios → proportional weights.

2. **v3 — `SavedColumn` (enum Leaf/VSplit)**: flatten VSplits into rows. Columns are already flat. Binary ratios → proportional weights. `custom_ratio` → `custom_row_weight`.

Both migrations produce the v4 format (flat lists of columns + rows).

## Configuration

```toml
[splits]
min_width = 300.0   # Minimum column width in points

[keys]
# Columns
vsplit = "cmd+d"             # New column after the focused pane
vsplit_root = "cmd+e"        # New column at the end
# Vertical
hsplit = "cmd+shift+d"       # Vertical split after the focused pane
hsplit_root = "cmd+shift+e"  # Vertical split at the bottom of the column
# Resize mode 1
resize_left = "cmd+ctrl+left"
resize_right = "cmd+ctrl+right"
resize_up = "cmd+ctrl+up"
resize_down = "cmd+ctrl+down"
# Resize mode 3 (edge grow)
edge_grow_left = "cmd+ctrl+option+left"
edge_grow_right = "cmd+ctrl+option+right"
# Resize mode 2 (virtual width) : Ctrl+Option+Arrows (hardcoded in keyDown)
# Equalize
equalize = "cmd+shift+="
```
