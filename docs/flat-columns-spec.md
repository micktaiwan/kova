# Flat Columns Spec

Refactor of the horizontal split system: replace the nested binary tree with a flat list of columns. Vertical splits remain binary trees inside each column.

**Motivation**: nesting `[[A | B] | C]` creates a confusing mental model (root vs local, nested ratios, partial equalize). The user thinks `[A | B | C]` — the code should reflect that.

## New data model

### Tab

```rust
pub struct Tab {
    pub columns: Vec<ColumnTree>,   // flat list, always >= 1
    pub column_weights: Vec<f32>,   // one per column, proportional (not normalized)
    // ... existing fields preserved:
    pub focused_pane: PaneId,
    pub virtual_width_override: f32,
    pub scroll_offset_x: f32,
    pub custom_name: Option<String>,
    pub minimized_stack: Vec<PaneId>,
    // ...
}
```

Effective width of column `i`:

```
column_width[i] = total_width * (column_weights[i] / sum(column_weights))
```

### ColumnTree

```rust
pub enum ColumnTree {
    Leaf(Pane),
    VSplit {
        top: Box<ColumnTree>,
        bottom: Box<ColumnTree>,
        ratio: f32,           // fraction of height for top child (0.0–1.0)
        custom_ratio: bool,   // user-adjusted ratio
    },
}
```

No more `HSplit`. No more `root` flag. No more horizontal `custom_ratio` (column weights are always adjustable).

### What goes away

- `SplitTree::HSplit` — replaced by `Tab.columns` + `column_weights`
- `SplitTree::root` flag — no longer needed, all columns are at the same level
- `SplitTree::custom_ratio` on HSplits — replaced by the Vec's weights
- `chain_count(horizontal, root_only)` — replaced by `columns.len()`
- `equalize()` in its current form — replaced by a weight reset
- `with_split()` for horizontal splits — replaced by insertion into the Vec

## Operations

### Creating a horizontal split

**Cmd+D** — inserts a new column **right after** the focused pane's column.

```
[A | B], focus A → Cmd+D → [A | NEW | B]
[A | B], focus B → Cmd+D → [A | B | NEW]
```

**Cmd+E** — adds a new column **at the end**.

```
[A | B], focus A → Cmd+E → [A | B | NEW]
```

After insertion: the new column gets a weight = `sum(existing_weights) / n_existing` (= the average). Existing columns keep their weights, so their relative proportions are preserved. The new column gets exactly `1/(n+1)` of the total space.

### Creating a vertical split

**Cmd+Shift+D** — vertical split of the focused pane: inserts a new pane **right below** the focused pane. The focused pane is replaced by a `VSplit { old, new, ratio: 0.5 }`.

```
[A | B], focus A → Cmd+Shift+D →  [ A  | B ]
                                   [NEW |   ]
```

If the column already has a VSplit and focus is on an inner pane, the split is inserted after that pane:

```
[ A  | B ]                         [ A  | B ]
[ C  |   ], focus A → Cmd+Shift+D → [NEW |   ]
                                   [ C  |   ]
```

**Cmd+Shift+E** — vertical split **at the bottom of the column**: wraps the whole column in a VSplit with the new pane at the bottom.

```
[ A  | B ]                         [ A  | B ]
[ C  |   ], focus A → Cmd+Shift+E → [ C  |   ]
                                   [NEW |   ]
```

Same logic as Cmd+D/E for horizontal: D = after the current pane, E = at the end.

### Removing a pane

When a pane is closed:

1. **Pane alone in its column** (ColumnTree::Leaf) → remove the column from the Vec, remove the corresponding weight. Redistribute the removed weight proportionally to the remaining columns.
2. **Pane in a VSplit** → the sibling replaces the VSplit (promotion). The column stays.
3. **Last column of the tab** → close the tab.

### Equalize (new shortcut)

Reset all `column_weights` to `1.0`. Optional: also reset the `ratio` of inner VSplits and set `custom_ratio = false`.

Proposed shortcut: `Cmd+Shift+=` (consistent with tmux `select-layout even-horizontal`).

### Virtual width

```
auto_virtual_width = max(columns.len() * min_split_width, screen_width)
```

If `virtual_width_override > 0`:
```
virtual_width = max(virtual_width_override, screen_width)
```

No more need for `root_only` in the computation — `columns.len()` is enough.

## Resize

### Mode 1 — Column resize (Cmd+Ctrl+Left/Right)

Moves the separator between two adjacent columns.

- **Cmd+Ctrl+Right** with focus in column `i`: looks for the separator on the right (between `i` and `i+1`). Transfers weight from `i+1` to `i` (column `i` grows).
- **Cmd+Ctrl+Left**: the reverse.
- If there's no separator in that direction: look on the other side (column `i` shrinks).
- Minimum weight per column: matches `min_split_width` in pixels.

### Mode 1 — Vertical resize (Cmd+Ctrl+Up/Down)

Unchanged: adjusts the `ratio` of the ancestor VSplit in the column. Same logic as the current `adjust_ratio_directional` but limited to VSplits.

### Mode 2 — Virtual width global (Ctrl+Option+Left/Right)

Unchanged: adjusts `virtual_width_override`. All weights stay identical, columns grow/shrink proportionally.

### Mode 3 — Edge grow (Cmd+Ctrl+Option+Left/Right)

Grow/shrink a single column. The others keep their pixel size.

- Increases `virtual_width_override`.
- Adjusts the target column's weight so it absorbs the added space.
- The other weights are adjusted so their columns keep the same pixel width.

### Mouse drag

- Hit-test on vertical separators (between columns) and horizontal ones (in VSplits).
- Drag vertical separator between columns `i` and `i+1`: adjusts `column_weights[i]` and `column_weights[i+1]`.
- Drag horizontal separator: adjusts the `ratio` of the VSplit concerned (unchanged).

### Max pane width invariant

Unchanged: no pane may exceed `screen_width` in width. Post-validation after every resize.

## Navigation

### Left/Right

Find the column `i` of the focused pane. Go to column `i-1` (Left) or `i+1` (Right). In the target column, pick the vertically closest pane (by viewport center, as today).

### Up/Down

Navigate within the current column's VSplit. If already at the top/bottom, stay (no wrap).

### Swap (Cmd+Shift+Arrows)

- **Left/Right**: swap the whole column `i` with column `i±1` (swap in the Vec + swap the weights).
- **Up/Down**: swap the panes in the column's VSplit (unchanged).

### Reparent (Cmd+Ctrl+Shift+Arrows)

- **Left/Right**: move the focused pane into the adjacent column (as a VSplit). If the source column ends up empty, remove it.
- **Up/Down**: move the pane into the neighboring VSplit (unchanged).

## Visual separators

- Vertical separators: between each column. Position = sum of the widths of columns 0..i.
- Horizontal separators: in each column's VSplits (unchanged).

`collect_separators()` and `collect_separator_info()` iterate over the columns then descend into each ColumnTree for the horizontal separators.

## Session persistence

### SavedTree → SavedTab

```rust
enum SavedColumn {
    Leaf { cwd, env, ... },
    VSplit { top: Box<SavedColumn>, bottom: Box<SavedColumn>, ratio: f32, custom_ratio: bool },
}

struct SavedTab {
    columns: Vec<SavedColumn>,
    column_weights: Vec<f32>,
    focused_leaf_index: usize,
    // ...
}
```

### Migration

On restore, if the old `SavedTree` format is detected (presence of `HSplit`), convert it by flattening all HSplits into columns. Binary ratios are converted into proportional weights.

## Files affected

| File | Impact |
|---|---|
| `src/pane.rs` | **Major** — `SplitTree` → `ColumnTree`, rework of the ~30 methods |
| `src/window.rs` | **Major** — do_split, resize, navigation, mouse drag |
| `src/session.rs` | **Medium** — SavedTree → SavedTab, old format migration |
| `src/renderer/mod.rs` | **Medium** — viewport computation via `for_each_pane_with_viewport` |
| `src/config.rs` | **Minor** — shortcuts (add equalize, merge vsplit/vsplit_root) |
| `src/keybindings.rs` | **Minor** — add equalize, remove vsplit_root if merged |
| `src/recent_projects.rs` | **Minor** — uses SavedTree |
| `docs/split-specs.md` | Full rewrite |

## Implementation plan

### Step 1 — ColumnTree + Tab

New `ColumnTree` enum (VSplit + Leaf). Add `columns` and `column_weights` to `Tab`. Migrate the methods one by one: first the simple traversals (`pane()`, `pane_mut()`, `for_each_pane()`, `contains()`, `first_pane()`, `last_pane()`), then the operations (split, remove, viewport, navigate).

### Step 2 — Split & Remove

Rewrite `do_split` (Cmd+D → insert column), `do_split_root` (Cmd+E → push column), `do_close_pane_or_tab` with the flat logic. Remove `with_split()` for horizontal splits.

### Step 3 — Viewport & Render

Adapt `for_each_pane_with_viewport`: iterate over the columns with the weights, then descend into each ColumnTree for the VSplits. Adapt the renderer.

### Step 4 — Resize

Rewrite the 3 modes for the flat model. Horizontal mode 1 becomes a weight adjustment between adjacent columns. Modes 2 and 3 adapt using weights instead of recursive ratios.

### Step 5 — Navigation, Swap, Reparent

Simplify with the column index in the Vec.

### Step 6 — Session persistence

New `SavedTab` format + migration of the old `SavedTree`.

### Step 7 — Equalize shortcut

Add the shortcut and the action (trivial at this stage).

### Step 8 — Cleanup

Remove dead code (`SplitTree::HSplit`, `root`, `chain_count`, old `equalize`). Update `docs/split-specs.md`.
