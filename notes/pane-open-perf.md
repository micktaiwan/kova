# Pane open performance

## The central finding: two distinct latencies

"Opening a pane" mixes **two latencies** that are not solved by the same fixes:

1. **Time-to-rectangle** — the pane is drawn on screen. Delayed by work
   **on the AppKit main thread**: `fork`/`exec` of the shell, synchronous reflow of
   neighboring panes (`resize_all_panes`), and up to one frame of waiting before the
   next `NSTimer` tick.
2. **Time-to-prompt** — the shell becomes usable (first prompt). Dominated by
   the **login shell sourcing `/etc/zprofile` + `~/.zshrc`** (oh-my-zsh, plugins)
   in the **child** process. Already runs **off Kova's thread**: no main-thread or
   rendering optimization touches it. The only lever on Kova's side: a pool
   of pre-warmed shells.

Verified nuance: on macOS `fork` costs O(number of vm_map entries), **not**
O(mapped bytes) — the glyph atlas / MTLBuffers do not inflate it. The
`fork` blocks the main thread for about one or two frames. The value
of moving the spawn off the main thread is the **decoupling** (the pane appears before the
`fork` finishes), not getting back the `fork` time.

## Instrumentation (in place)

Three milestones logged under the `PANE-OPEN` prefix, relative to a single reference
`entry` = start of `Pane::spawn` (the pre-spawn work in the split handlers
is sub-µs floating-point math, folded into it). Implemented by `PaneOpenTimer`
(`src/pane.rs`), shared immutably with the PTY reader thread.

| Milestone | Meaning | Set in |
|---|---|---|
| `tree-inserted` | spawn (fork/exec) + insertion into the tree | `do_split` / `do_split_root` / `ipc_split` after `drop(tabs)`, before `resize_all_panes` (`src/window.rs`) |
| `first-paint` | pane submitted to the renderer = visible → **time-to-rectangle** | `pane_data` collection loop in `tick()` (`src/window.rs`) |
| `shell-ready` | first byte from the shell = first prompt → **time-to-prompt** | reader thread, at the `shell_ready` switch (`src/terminal/pty.rs`) |

Reading:

```bash
grep PANE-OPEN ~/Library/Logs/Kova/kova.log
# PANE-OPEN id=7 tree-inserted +2.1ms
# PANE-OPEN id=7 first-paint  +5.4ms (time-to-rectangle)
# PANE-OPEN id=7 shell-ready  +212.0ms (time-to-prompt)
```

- An interactive split emits **3 lines** (`tree-inserted` + `first-paint` + `shell-ready`).
- A **restore** pane emits **2 lines** (no `tree-inserted`: the restore
  handlers do not call `mark_inserted`) → the absence of `tree-inserted` tells
  a restore from a split.
- Each milestone logs only once (atomic guard).
- `entry` ≈ keystroke to within a sub-µs; if we ever want the exact keystroke→spawn
  delay, capture an `Instant` at the top of the split handlers (main-thread path,
  no race).

## Prioritized plan (from the multi-agent analysis, verified findings)

Measure **before** optimizing: without the rectangle vs prompt split, we optimize
the wrong half. That is the purpose of the instrumentation above.

### For "the pane appears instantly"

1. **Insert a `shell_ready=false` placeholder BEFORE spawning, then spawn the
   PTY off the main thread and swap** (high impact / medium complexity). The
   best ratio. Today the 3 entry points call `Pane::spawn` (blocking fork/exec)
   **then** insert → the rectangle cannot be drawn before the spawn
   finishes. Detail: do not reuse `Pane::placeholder` as is, it sets
   `shell_ready=true` (would hide the loading overlay). `Pty` is `Send`.
3. **Force a synchronous render after a split + drain IPC before the tick**
   (quick win). There is **no `setNeedsDisplay` path**: rendering depends
   only on the `NSTimer`. A `self.tick()` at the end of `do_split`/`do_split_root` (after
   `drop(tabs)`) removes the ~16.67 ms floor. For IPC, the drain (`app.rs`)
   runs **after** `view.tick()` → an `ipc_split` renders one frame later; move
   it up before the tick loop.
4. **Decouple the neighbors' scrollback reflow** (high complexity).
   `resize_all_panes` (`src/window.rs`) is synchronous after each split and, on a
   **horizontal** split (the default), reflows the neighbors' whole scrollback `VecDeque`
   in O(scrollback × cols), up to 10k lines. The new pane itself never
   pays. Keep the visible grid resize synchronous, defer the scrollback
   reflow.

### For "the prompt is usable instantly"

2. **Pool of pre-warmed shells (1-2 pre-forked shells)** (high impact /
   high complexity). The **only lever on Kova's side**. Pre-spawn 1-2 real shells
   in `$HOME` that source their rc in the background and reach
   `shell_ready=true` while we work; on split, pop a warm shell instead of
   forking, `cd <cwd>` if needed. **Tension with the "minimal RAM" principle**:
   keep N small, never any PTY capture on the warm shells. `Pty::dummy` /
   `Pane::placeholder` do not fit (no real process).

### Independent quick wins

- **PTY capture OFF by default** (`src/terminal/pty.rs`, the `Err(_) => true` arm).
  Not a latency gain (`shell_ready` is set before the capture write), but it
  writes to disk in clear **everything** that goes through the PTY (including
  typed passwords) — it is a security exposure. Switch to opt-in `=1`, or
  an in-memory ring buffer flushed only when the alt-screen bug strikes.
- **Read buffer 4096 → 16-64 KiB** (`src/terminal/pty.rs`): coalesces the
  `read()` calls and lock acquisitions under heavy throughput. Throughput, not opening.
- **Poll 100 ms → ~400 ms** (`src/terminal/pty.rs`): 4× fewer idle wake-ups
  (stays under the join warning >500 ms on Drop).
- **Compare cols/rows before the `write()` lock** in `resize_all_panes`
  (`src/window.rs`, the lock is taken unconditionally).

### Out of scope / honest

- Dotfiles (`~/.zshrc`, oh-my-zsh) dominate time-to-prompt and **are not**
  Kova code. Possible mitigation: pool (rank 2), opt-in ZDOTDIR warm-park, or
  a doc hint — no direct speed-up of the sourcing.
- The **naive** non-login fix (removing the `-` from `arg0`) is **UNSAFE**: splits
  would inherit Kova's launch env, not the augmented PATH of the first shell
  (brew/node/python/rvm live in `~/.zprofile`/`~/.zlogin`) → PATH regression
  when Kova is launched from the Finder. If we pursue non-login, capture
  the resolved env of the first login shell and inject it into the splits.
