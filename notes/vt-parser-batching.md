# VT Parser Batching — Reducing write lock contention

## Problem

The PTY reader thread calls `parser.advance()` on a 4 KB chunk. For the whole duration of the parsing, the write lock on `TerminalState` is held (lazy acquisition on the first `self.term()`, release after `advance()`).

`parking_lot::RwLock` gives priority to writers. When a background pane receives a lot of data (build, logs…), the renderer on the main thread is blocked waiting for the read lock — even for panes that are not writing.

Result: visible lag on tab switch or during rendering while a build runs in the background.

## Solution: Op-buffer + batch flush

### Principle

Instead of the VTE callbacks (`print`, `execute`, `csi_dispatch`…) mutating `TerminalState` directly via `self.term()`, they accumulate operations in a local `Vec<TermOp>`. After `parser.advance()`, we take the write lock **only once**, replay the ops, then release.

The parsing (decoding the VT sequences) happens **without a lock**. Only the replay (mutations on the grid) needs the lock. The replay is much faster because there is no parsing logic left — just direct mutations.

### `TermOp` enum

```rust
enum TermOp {
    /// Text to display (grapheme clusters already split)
    Print(String),
    /// Control characters
    Backspace,
    Tab,
    Newline,
    CarriageReturn,
    Bell,
    /// Cursor movement
    CursorUp(u16),
    CursorDown(u16),
    CursorForward(u16),
    CursorBackward(u16),
    SetCursorPos(u16, u16),
    /// Cursor Horizontal Absolute — needs the current cursor_y
    SetCursorCol(u16),
    /// Vertical Position Absolute — needs the current cursor_x
    SetCursorRow(u16),
    SaveCursor,
    RestoreCursor,
    /// Erasing
    EraseInDisplay(u16),
    EraseInLine(u16),
    EraseChars(u16),
    /// Lines
    InsertLines(u16),
    DeleteLines(u16),
    DeleteChars(u16),
    InsertChars(u16),
    /// Scroll
    ScrollUp(u16),
    ScrollDown(u16),
    SetScrollRegion(u16, Option<u16>), // top, bottom (None = use rows)
    /// Modes
    SetDecMode(u16, bool),  // mode number, on/off
    SetMode(u16, bool),
    /// SGR
    SetSgr(Vec<u16>),
    /// Cursor shape
    SetCursorShape(u16),
    /// Screen
    EnterAltScreen,
    LeaveAltScreen,
    ReverseIndex,
    FullReset,
    /// Metadata (no state read needed)
    SetTitle(String),
    SetCwd(String, Option<String>), // path, git_branch
    SetLastCommand(String),
    /// Responses — need a state read then a PTY write
    /// Handled specially during the replay.
    CursorPositionReport,
    DeviceAttributes,
    ReportPrivateMode(u16),
    KittyKeyboardQuery,
}
```

### Shadow state

Some ops need to **read** the current terminal state to build the op. There are two approaches:

**Option A — High-level ops (recommended)**: encode the intent rather than the result. For example `SetCursorCol(col)` instead of `SetCursorPos(cursor_y, col)`. The replay resolves the dependency at application time. That is what the enum above does.

**Option B — Local shadow state**: track `cursor_x`, `cursor_y`, `rows`, `cols` locally in the handler to build complete ops. More complex and fragile (the shadow state can diverge).

### Callbacks that read the state — analysis

| Callback | What is read | Option A solution |
|----------|--------------|-------------------|
| CSI `G` (CHA) | `cursor_y` | `SetCursorCol(col)` — the replay does `set_cursor_pos(cursor_y, col)` |
| CSI `d` (VPA) | `cursor_x` | `SetCursorRow(row)` — the replay does `set_cursor_pos(row, cursor_x)` |
| CSI `r` (DECSTBM) | `rows` | `SetScrollRegion(top, None)` — the replay uses `rows` as default |
| CSI `n` (CPR) | `cursor_y/x` | `CursorPositionReport` — the replay reads the position and writes to the PTY |
| CSI `p` (DECRPM) | mode flags | `ReportPrivateMode(mode)` — the replay reads the flag and writes to the PTY |
| ESC `c` (RIS) | dims, colors | `FullReset` — the replay reads the values and rebuilds |

**Conclusion**: option A covers 100% of the cases. No shadow state needed.

### OSC 7 — special case

OSC 7 (set CWD) does filesystem I/O (`resolve_git_branch`), which is already handled by releasing the lock. With batching, we do the I/O during the parsing (no lock held) and push `SetCwd(path, git_branch)` into the buffer.

### PTY responses (CPR, DA1, DECRPM, Kitty query)

These callbacks read the state *and* write to the PTY. With batching:
1. We push a marker op (`CursorPositionReport`, etc.)
2. During the replay, when processing that marker, we read the current state (already updated by the previous ops) and write to the PTY

This adds a tiny delay to the response (the replay time), but it is negligible because the replay is fast and these responses are not time-critical.

### Implementation

1. Add `ops: Vec<TermOp>` to `VteHandler`
2. All callbacks push into `self.ops` instead of calling `self.term()`
3. `flush_print_buf()` becomes a push of `Print(buf)` into ops
4. New method `fn apply_ops(&mut self)`:
   - Takes the write lock once
   - Iterates over `self.ops.drain(..)`
   - Applies each op to `TerminalState`
   - Releases the lock
5. In `pty.rs`, after `parser.advance()`: call `handler.apply_ops()` instead of `handler.release_guard()`

### Changes in `pty.rs`

```rust
// Before
parser.advance(&mut handler, &buf[..n]);
handler.release_guard();

// After
parser.advance(&mut handler, &buf[..n]);
handler.apply_ops();
```

### Expected performance

- **Write lock duration**: goes from "VT parsing time of a 4 KB chunk" to "replay time of a Vec of already-parsed ops"
- The parsing (the slow part: decoding sequences, splitting graphemes, etc.) happens without a lock
- The replay is a simple match loop + direct mutations
- Estimate: 5-10× reduction of the time under write lock for a typical chunk

### Risks

- **Size of the `TermOp` enum**: ~40 variants. It is the bulk of the implementation work but it is mechanical.
- **Vec allocation**: a `Vec::with_capacity(256)` reused between reads is enough (drain, no realloc).
- **Correctness**: the replay must reproduce exactly the same behavior as before. Manual tests needed with `htop`, `vim`, `git log --oneline`, cargo builds, etc.
