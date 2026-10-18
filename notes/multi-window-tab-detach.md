# Multi-window & tab detach

## Goal

Detach a whole tab (with all its splits) into a new window, and be able to reattach it to an existing window by drag.

## Current state

### Implemented (steps 1-3)

- **Multi-window**: `AppDelegate` manages a `Vec<Retained<NSWindow>>` instead of a `OnceCell`.
- **Single global timer**: a single `NSTimer` in `AppDelegate` iterates over all windows and calls `view.tick()`. Each `tick()` returns `bool` — `false` = dead window (no tabs left), the timer closes it and removes it from the Vec.
- **`create_window(mtm, config, tabs, active_tab)`**: single function to create a window with given tabs. No more implicit session restore in `setup_metal`.
- **Multi-window session restore**: `AppDelegate::did_finish_launching` loads the session and creates N windows with their tabs and positions (`setFrame_display`).
- **Multi-window session save**: v2 format with `Vec<WindowSession>` (v1 backward compat). Each window saves tabs + active_tab + frame (x, y, w, h).
- **Cmd+N**: new empty window (tab + fresh shell). Reaches the AppDelegate via `msg_send![self]` to register the window.
- **Unique autosave name**: each window has a `KovaWindow-{id}` via `AtomicU32`, avoiding NSUserDefaults collisions.
- **`kova_view()`**: pub in `app.rs`, reusable.
- **Cmd+Shift+T**: detaches the active tab into a new window (no-op if there is only one tab). New window offset by +20x/-20y (cascade).
- **Cmd+Q = close active window**: `closing` flag checked by `tick()`, confirmation if processes are running. The app terminates when the last window is closed.
- **Deferred dealloc**: closed windows are moved into `pending_close` (Vec) with `orderOut`, then deallocated on the next tick to avoid AppKit segfaults.

### What remains to be done

- Drag detach with ghost preview
- Drop reattach onto another window's tab bar

## Architecture

```
AppDelegate (src/app.rs)
├── windows: RefCell<Vec<Retained<NSWindow>>>
├── pending_close: RefCell<Vec<Retained<NSWindow>>>  ← dealloc deferred by one tick
├── closed_sessions: RefCell<Vec<WindowSession>>     ← sessions of closed windows
├── config: OnceCell<Config>
├── start_global_timer(fps)     ← a single NSTimer for all windows
├── app_delegate(mtm)           ← helper to reach the AppDelegate
├── create_new_window(mtm)      ← Cmd+N
├── detach_tab_to_new_window()  ← Cmd+Shift+T
└── did_finish_launching()      ← session restore → N windows

KovaView (src/window.rs)
├── setup_metal(mtm, config, tabs, active_tab)  ← no more session::load here
├── tick() -> bool              ← called by the global timer, false = to close
├── do_close_window()           ← Cmd+Q, set closing flag
├── do_detach_tab()             ← Cmd+Shift+T, removes tab and creates window
├── append_session_data(&mut Vec<WindowSession>) ← collection for save
├── confirm_running_processes() ← shared alert (should_terminate + close)
└── ivars: closing, git_poll_interval, git_poll_counter, last_title, ...

Session (src/session.rs)
├── Session { version: 2, windows: Vec<WindowSession> }
├── WindowSession { tabs, active_tab, frame }
├── WindowSession::from_tabs()  ← helper for save
├── save(&[WindowSession])      ← a single file for all windows
├── load() + restore_session()  ← backward compat v1 → v2
└── RestoredWindow { tabs, active_tab, frame }
```

## UX decisions

- **Cmd+N = new empty window** — opens a new window with a tab + fresh shell.
- **Cmd+Shift+T = detach active tab** — keyboard shortcut to detach the active tab into a new window. Serves as a first step before drag.
- **Detach by mouse drag (phase 2)** — drag a tab out of the tab bar to detach it. Ghost preview of the tab during the drag.
- **No detach with a single tab** — it makes no sense, the tab is already alone in its window.
- **Position of the new window** — for drag: where the mouse drops the tab. For Cmd+Shift+T/Cmd+N: offset from the active window.
- **Drop on an existing tab bar = reattach** — if the tab is dropped on another window's tab bar, it is reattached to that window instead of creating a 3rd window.
- **Drop elsewhere = new window** — if the tab is dropped outside a tab bar, a new window is created at the cursor position.
- **Cmd+Q closes the active window** — closes all tabs of the current window, not the whole app. The app terminates when no window is left open. Session saved.
- **Cmd+Option+Q = kill window** — closes the window without saving its session (the window will not be restored on next launch).
- **Multi-screen session restore** — the positions of all windows are remembered. A quit + reopen restores each window with its tabs/splits at the right position, including on the right screen.
- **Per-window config (future)** — global config for now, but the architecture must eventually allow different colors/themes per window.

## Implementation steps

### ~~1. Multi-window refactor~~ ✅

- `AppDelegate`: `OnceCell<NSWindow>` → `RefCell<Vec<Retained<NSWindow>>>`
- Single global timer in `AppDelegate` that calls `tick()` on each `KovaView`
- `tick()` returns `bool`; the timer closes dead windows and removes them from the Vec
- `create_window(mtm, config, tabs, active_tab)` — unified signature
- `setup_metal` no longer does session restore — it takes explicit tabs
- Session restore in `did_finish_launching` with window positions
- `applicationShouldTerminateAfterLastWindowClosed` → `true`
- `should_terminate` collects the processes of all windows
- `will_terminate` saves all windows into a single session file

### ~~2. Cmd+N~~ ✅

- `Cmd+N` in `performKeyEquivalent` → `crate::app::create_new_window(mtm)`
- `create_new_window` reaches the AppDelegate via `msg_send![&*delegate, self]`
- Creates a fresh tab, calls `create_window`, registers it in the Vec
- Unique autosave name per window (`KovaWindow-{AtomicU32}`)

### ~~Session v2~~ ✅

- `Session { version: 2, windows: Vec<WindowSession> }`
- `WindowSession { tabs, active_tab, frame: Option<(f64,f64,f64,f64)> }`
- v1 backward compat: `SessionV1` deserialize → automatic migration
- `WindowSession::from_tabs()` helper to build from live data
- `save(&[WindowSession])` — a single file
- `restore_session()` → `Vec<RestoredWindow>`

### ~~3. Cmd+Shift+T — detach active tab~~ ✅

Remove the active tab from the source Vec, create a new window with that tab. Block if there is only one tab. Also includes Cmd+Q = close the active window (not the whole app), with dealloc deferred by one tick via `pending_close` to avoid AppKit segfaults.

### 4. Drag detach with ghost preview

Tab dragged out of the tab bar → new window at the drop position. Needs a distance threshold to tell reorder from detach.

### 5. Drop reattach

Drop on another window's tab bar → reattach. Via AppKit's `NSDraggingSource`/`NSDraggingDestination` or a custom hit-test on the tab bars of all windows.

## Trade-offs

| Aspect | Choice | Rationale |
|--------|-------|---------------|
| GPU atlas | Duplicated per window | ~2-4 MB per window, simple and clean. Sharing between Metal devices is complex for a negligible gain. |
| Config | Global for now | A single config cloned per window. The architecture must eventually allow a config/theme per window. |
| PTY ownership | Moves with the tab | No Arc refactor. The reader thread keeps writing into the same `Arc<RwLock<TerminalState>>`. |
| Timer | Single global | A single `NSTimer` in `AppDelegate` iterates over all windows. Avoids N idle timers. |
| Session file | A single multi-window file | `~/.config/kova/session.json` with `Vec<WindowSession>`. v1 backward compat. |
| Autosave names | `KovaWindow-{id}` | Global `AtomicU32` counter, unique per session. |
| Window dealloc | Deferred by one tick | `pending_close` + immediate `orderOut`. Drop on the next tick to avoid an AppKit segfault (callbacks on a deallocated view). |
| Session save | Eager + terminate | `closed_sessions` collects at close time. `will_terminate` merges closed + live + pending. |

## Risks

- **Thread safety**: the global timer reaches the `KovaView`s via a cast of `contentView` — same pattern as `kova_view()`, no new risk.
- **PaneId/TabId uniqueness**: global counters, OK across windows.
- **Cross-window focus**: `NSWindow.keyWindow` handles it natively. The focused pane is per tab, so no ambiguity.
- **App shutdown**: `applicationShouldTerminateAfterLastWindowClosed` = `true` + the timer calls `app.terminate` when the Vec is empty.
- **Double borrow**: `tick()` never closes a window directly — it returns `false` and the timer handles the close after releasing the borrow.
- **AppKit dealloc segfault**: dropping a `Retained<NSWindow>` in the same tick as `close()`/`orderOut()` causes a segfault (AppKit keeps internal refs to the view in the run loop). Solved by `pending_close`: dealloc deferred to the next tick.
- **Session lost on close**: windows closed before quitting lost their data. Solved by `closed_sessions`, which collects at close time.

## Priority order

1. ~~Multi-window — `AppDelegate` refactor + window creation factoring + single global timer~~ ✅
2. ~~Cmd+N — new empty window~~ ✅
3. ~~Cmd+Shift+T — detach active tab into a new window (validates the tab move without drag)~~ ✅
4. Drag detach with ghost preview — tab dragged out of the tab bar → new window at the drop position
5. Drop reattach — drop on another window's tab bar → reattach
