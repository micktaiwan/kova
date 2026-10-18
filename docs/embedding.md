# Kova as a library: one terminal for Kova.app and for a host app

This file covers the part of the work done inside Kova: the architecture, the tests to write
before touching the code, and the order of the work. The host side (the need, the Claude part,
syncing with the text, discarded options) lives with the host app. Line numbers may be stale;
reread them before relying on them.

The need, in one sentence: a host app (an editor) wants one Kova pane per note, running a
Claude Code conversation, with the same terminal code as Kova.app (no copy), zero regression
on Kova guaranteed by tests written before the refactoring, and the sessions visible to remote
clients that already talk to Kova's socket.

## The chosen solution: Kova becomes a library, Kova.app and the host app are two of its clients

One terminal codebase, used by two apps. Kova.app is the first client and loses nothing; the
host app is the second and shows one pane per note. This is Ghostty's architecture: a terminal
core as a library, and a macOS app that uses it with one view per split
(`ghostty_surface_new`, `include/ghostty.h` in the ghostty-org/ghostty repo). Ghostty presents
this API as internal, not designed for other apps; here both clients have the same owner, so
that is not a blocker.

### The pieces

- **Nothing exists until a pane is opened**: opening a note's pane creates its
  `Surface` (the shell, then `claude`); closing it destroys the `Surface`, which ends `claude`.
  The shared rendering context is created with the first pane and freed with the last.

- **One `Surface` per pane, in the library**: the PTY, the terminal state, the selection, the
  scrolling, the agent session, the "waiting for the user" flag. The base exists:
  `Pane` already carries its PTY, its terminal, `awaiting` and `agent_session`
  (`kova/src/pane.rs:1656-1687`). Refreshing the agent session, done today at the
  tab level (`Tab::check_running`, `pane.rs:779-800`), moves down into the pane.
- **Input through a typed API, like Ghostty**: `key`, `text`, `preedit` (the text being
  composed, accents), `mouse`, `scroll`. All the logic (which bytes to send for a key, the
  choice between composing an accent or sending Option, Ctrl+L, click selection, scrolling)
  lives in the library, once. Translating macOS events (keys, composed text, mouse) into this
  API is also written once, in the library: a shared input component. `KovaView` first handles
  what belongs to the window (overlays, tab shortcuts) then hands over to it for the active
  pane; in the host app, the pane view is provided by the library and is just this component
  plus a `Surface`. Each view class keeps only the methods macOS requires, each a one-line
  delegation: no duplicated input code, and the host app writes none.
  Ghostty separates the logic (in the core: `ghostty_surface_text`,
  `ghostty_surface_preedit`) from the macOS view in the same way.
  Status at step 2: `key`, `text`, `mouse`, `scroll` are on `Surface`
  (`src/input.rs`); composed text stays in the shared input component
  (`src/appkit_input.rs`), because nothing in the terminal reads it yet.
- **Drawing: a shared context and one drawer per pane.** A `RenderContext` (Metal device,
  pipeline, glyph atlas per screen scale) and a `PaneRenderer` that draws a pane in the
  rectangle the host gives it. Kova.app keeps a single layer per window and draws its panes in
  it, as today; the host app provides a layer that contains only one pane.
  Why not one layer per pane like Ghostty: Kova today draws the whole window as one image (a
  single `nextDrawable` and a single `presentDrawable`,
  `kova/src/renderer/mod.rs:1092`, `:1444`), overlays included. Keeping this model means
  changing nothing in what Kova.app displays, so risking nothing on already validated behavior;
  switching to one layer per pane would change how the window is composed, with no proven gain
  (no measurement has been made either way). The code is already almost split this way: a
  vertex cache and a clip area per pane (`mod.rs:927`, `:1317-1325`). Ghostty's gain, not
  redrawing a pane that doesn't change while scrolling the columns, can be had too: the cache
  today contains the pane's position (`mod.rs:827`); it is enough to draw in local coordinates
  and pass the position separately.
  **Corrected at step 2**: not that simple. The build rounds each position to the whole pixel
  in drawable space (`(ox + col_idx as f32 * cell_w).round()`,
  `src/renderer/pane_renderer.rs`), and a pane's position can be fractional (column widths as
  weights, trackpad scrolling). A build done at the origin then translated places glyphs up to
  half a pixel off; the test `a_shifted_build_is_not_a_build_in_place` freezes this
  counterexample. So the cache stays keyed on the absolute rectangle. Unevaluated variant:
  build with the fractional part of the position and translate by the integer part, which keeps
  the rounding; what remains are the unrounded sums (status bar text, italic slant) that would
  land on other f32 values, with an on-screen effect not measured.
- **The clock stays with the host**: the pane says whether it needs a redraw, the host decides
  when. Kova.app keeps its timer (`kova/src/app.rs:226-232`). Cursor blinking, counted in
  frames today (`mod.rs:1002`), moves to real time so as not to depend on the host.
- **The IPC socket for remote clients**: the transport (`kova/src/ipc.rs`) and the commands
  that act on a pane (`list-panes`, `send-keys`, `get-pane-content`, `count-pane-content`,
  `focus-pane`, `close-pane`, `notify`…) move into the library, written once against a host
  interface ("give me the panes, this pane, bring it to the front"). The transport is not
  entirely self-contained: it reads a rendering constant, `crate::renderer::TAB_COLORS`
  (`kova/src/ipc.rs:24`), to be moved along with it.
  Done at step 2: it lives in `src/tab_colors.rs`, and the host interface is the trait
  `IpcHost` (`src/ipc/host.rs`).
  Tab and window commands stay with Kova.app, except two that a remote client sends to the
  instance holding the pane: `list-tabs` and `close-tab` (closing a project from the client).
  The host app answers them with "one tab per note", and `close-tab` there closes the note's
  pane.
- **The host app has its own socket**, `/tmp/kova-<host pid>.sock`, with the same protocol. A
  client that already discovers every instance's socket and groups panes by socket sees it
  without changes. `KOVA_SOCKET` and `KOVA_PANE_ID`, given to the shell
  (`kova/src/terminal/pty.rs:234-235`), then point to the host app, so the Claude Code hooks
  that mark a pane work in the host app without changes. Registering the host's panes in the
  Kova.app process would require passing the terminal from one process to the other: more
  complicated, and the host app would depend on Kova being open.
- **What stays with Kova.app**: windows, tabs, columns, separators, overlays, bookmarks,
  anchors, Cmd+J.

### Two Kova rules to lift explicitly

`kova/CLAUDE.md`, Principles section, said "No feature creep: tabs, splits, config, that's
it" and "Code the user has validated […] is no longer modified", because touching validated
behavior creates regressions. This work adds a function to Kova (serving another host) and
moves validated code. The second rule targets behavior: the refactoring must change none of
it, and the tests below are there to prove it. Both had to be lifted by the user before
starting, and `kova/CLAUDE.md` updated.

### Tests first, then the refactoring

The code that is going to move has almost no tests: none in `kova/src/input.rs`, a single one
in `kova/src/window/mod.rs` (2,682 lines). Kova's policy tests pure logic and leaves Metal and
AppKit aside (`kova/CLAUDE.md`, Tests section). Hence the order, each step keeping Kova.app
usable and tested:

1. Tests for selection, words, selected text and URLs on `TerminalState`, without moving
   anything: the tests already build an in-memory terminal (`kova/src/terminal/mod.rs:3317`).
2. Pull the "key → bytes" computation out into a pure function (`input.rs:53-174`) and freeze
   the whole table: Ctrl+letter, arrows in application mode, kitty protocol, Shift+Tab, Option.
3. The choice between composing an accent or sending Option (`window/mod.rs:605-608`), Ctrl+L,
   Ctrl+F.
4. SGR mouse bytes and the trackpad scroll step.
5. Pixel → cell conversion without `NSEvent`, then click and drag selection.
6. A "write bytes" interface in place of the real PTY, then the IPC command tests.
7. A vertex builder with a fake glyph atlas, then vertex snapshots (more stable than an image
   comparison, which the font and the macOS version would make vary).
8. What can't be automated (real French dead keys, ^ then e, Option+$; position of the accent
   window; inertial scrolling; real triple-click): a short list of gestures, played before and
   after the refactoring, in a pane that shows the bytes received (`od -c`) and read back via
   `get-pane-content`.

### The manual gestures of step 8

Played twice: a "before" pass on the installed Kova, an "after" pass on each refactoring
build, and both traces compared with `diff`. The measurement pane runs
`python3 scripts/key-probe.py <label>` (one `od -c` line per PTY read, trace in
`~/Downloads/kova-embedding/key-probe-<label>.log`, Ctrl+D alone to stop); the mouse goes
through `scripts/mouse-probe.py <label>` (q to stop). What these gestures cover and tests
can't touch: the choice macOS makes between `insertText:` and `doCommandBySelector:`, the
real AZERTY keyboard, the real accent window, the real trackpad.

Keyboard pass without kitty (`key-probe.py avant`), then the same with `--kitty`
(`avant-kitty`):

1. `^` then `e`, `¨` then `i`, `` ` `` then `a` (dead keys); `^` then space.
2. Option+$, Option+(, Option+e then e.
3. Long press on `e`, pick `é` with the keyboard: note where the accent window opens.
4. Ctrl+A, Ctrl+C, Ctrl+[, Ctrl+Option+A.
5. Arrows, Home, End, Page Up/Down, Forward Delete, F1 to F4, Shift+Tab, Return, Tab, Escape, Backspace.
6. Ctrl+Up arrow, Option+Up arrow, Shift+Return, Option+←, Option+→, Cmd+←, Cmd+→, Cmd+Backspace.

Outside the probe, in an ordinary shell:

7. Ctrl+L: the screen clears and the scrollback with it (wheel up: nothing); in `vim`, Ctrl+L doesn't clear the scrollback.
8. Ctrl+F opens the filter, Escape closes it.
9. Ctrl+Option+→ and ←: the virtual width changes, nothing reaches the shell.

Mouse (known text on screen, then `pbpaste` after each gesture):

10. Single click: no selection, clipboard unchanged.
11. Drag over two lines; double-click on a path `~/src/main.rs:12`; real triple-click on a line; double-click then drag.
12. Drag past the bottom of the pane: auto-scroll speeds up with distance.
13. Cmd+hover over a URL (underlined), Cmd+click (opened in the browser).
14. Inertial trackpad scroll in a long history, then in `mouse-probe.py inertie` (SGR 64/65 reports).

### The order of the work

1. The tests above.
2. The refactoring in Kova, with Kova.app still the only client: `Surface`, input API,
   `RenderContext` and `PaneRenderer`, IPC against the host interface. At each step, green
   tests and the gesture list replayed.
3. The library target: `[lib]` and `staticlib` in `kova/Cargo.toml` (none today), plus a C API
   (create a pane in a view, input, size, content reading, close).
   Done: the library is called `kova` like the binary (`src/lib.rs`, `libkova.a`,
   `libkova.rlib`), so that log lines and panics keep their `kova::…` prefix; Cargo sees no
   collision there. The `kova` binary is built on top of it and keeps its name and path, so
   `build.sh` doesn't change. The C API is `include/kova.h` (`src/capi.rs`), above
   `src/embed.rs`: a pane is a `Surface` plus, if the host gives an `NSView`, a library subview
   (the shared input component) that draws the pane alone in its `CAMetalLayer`
   (`src/renderer/layer.rs`, the same `PaneRenderer` as Kova.app; one `RenderContext` per
   scale, shared, freed with the last pane). The clock stays with the host (`kova_pane_tick`,
   `kova_pane_draw`), and `kova_ipc_start` serves the socket over the host's panes, one tab per
   pane (bound before returning: a `bind` failure comes back as -1). On quit, the host calls
   `kova_shutdown`, which waits until shells and foreground jobs are dead; `kova_pane_free`
   alone only starts reaping them on a thread. Kova.app goes through none of this. Proven
   outside the host app: Rust tests on the C functions (including the whole frozen key table)
   and `tests/c_smoke/smoke.m`, an Objective-C program that links the library, launches two
   panes (one of them in an off-window `NSView`, drawn) and queries the socket.
4. The host app: the pane view, linking the library in its build, the socket, the note →
   session table (path and inode), the three hooks (SessionStart, UserPromptSubmit,
   PreToolUse), an edit command, the command that receives a new session id, watching the
   notes folder, and a three-way merge if the user validates it.

## Verified since

- Two calls to `MTLCreateSystemDefaultDevice` return the same object (same pointer, same
  `registryID`), measured with
  `cargo test the_system_default_device -- --nocapture` (`src/renderer/layer.rs`), which
  printed `MTLCreateSystemDefaultDevice: 0x1016b9800 / 0x1016b9800, registryID 4294968545 /
  4294968545`. On a machine without a Metal device, this test passes without checking
  anything. The library doesn't depend on it: a new context reuses the device of a live
  context.

## Not verified

- That a Rust view provided by the library fits without friction into the host app's AppKit
  window (first responder, focus between the editor and the terminal).
- That a remote client copes with a server that answers `list-tabs` with one tab per note.
- That keeping one image per window or switching to one layer per pane changes anything
  measurable: nothing has been measured.
