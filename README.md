# Kova

A blazing-fast macOS terminal built from scratch with Rust and Metal. No Electron, no cross-platform compromises — just native GPU rendering on Mac.

## Features

### GPU-rendered with Metal

Every frame is drawn on the GPU via Apple's Metal API. Glyph atlas with on-demand rasterization via CoreText. Dirty-flag rendering — the GPU only redraws when terminal state actually changes. Synchronized output (mode 2026) eliminates tearing during fast updates.

### Splits and tabs

- Binary tree splits — horizontal and vertical, nested arbitrarily
- Drag-to-resize separators or use keyboard shortcuts (Cmd+Ctrl+Arrows)
- Auto-equalize: splits rebalance to equal sizes when adding/removing panes
- Horizontal scroll — when splits exceed the screen width, trackpad horizontal scroll navigates the virtual viewport. Configurable minimum split width.
- Tabs with colored tab bar, drag-to-reorder, and rename (Cmd+Shift+R)
- Cross-tab split navigation (Cmd+Option+Arrows)
- Swap panes between splits (Cmd+Shift+Arrows)
- New splits and tabs inherit the CWD of the focused pane

### Session persistence

Layout (tabs, splits, CWD) is saved on quit and restored on launch. Window position is remembered automatically.

### Clickable URLs

Cmd+hover highlights URLs with an underline and pointer cursor. Cmd+click opens them in your browser. The hovered URL is shown in the status bar.

### Scrollback search

Cmd+F opens an inline search overlay with match highlighting. Click a match to jump to it.

Cmd+Shift+F searches every pane of every tab at once, and the same palette also reaches the Claude Code sessions that are no longer open: closed transcripts are indexed by the prompts you typed in them, and picking one reopens it with `claude --resume` in its own project directory. An empty input offers what you searched earlier in the run.

### Attention routing

Kova tracks which panes have something you have not seen — a bell, or a command that finished while you were looking elsewhere — and routes you to them instead of making you hunt.

- Cmd+J jumps to the next unread pane, across tabs and windows, and falls back to an idle Claude Code session when nothing is unread. A banner names the tier it landed in.
- Cmd+P opens the tab/pane switcher: every tab with its panes, arrows or click to pick, Enter to focus. `u` flips it to the panes asking for something; Cmd+Shift+J opens that filtered list directly.
- Inside the switcher, Cmd+Up/Down moves the selected pane up or down its tab's order.
- The status bar counts working Claude sessions (`✳N`) and unread panes (`●N`).
- Cmd+Shift+Option+Left/Right walks the panes you visited, back then forward.
- Desktop notifications are posted by Kova itself, so clicking one focuses the pane it came from.

### Status bar

Displays CWD, git branch (auto-polling every ~2s), scroll position indicator, and time. Each element's color is independently configurable.

### Wide characters

Full support for emoji and CJK characters with proper 2-column rendering.

### macOS-native input

| Shortcut | Action |
|---|---|
| Option+Left/Right | Word jump |
| Cmd+Left/Right | Beginning/end of line |
| Cmd+Backspace | Kill line |
| Shift+Enter | Newline without executing |

### IPC / scripting

Each running Kova listens on a Unix socket (`/tmp/kova-{pid}.sock`) and accepts JSON commands: list panes, spawn splits, send keystrokes, capture pane content, wait for command completion. Inside any pane, `$KOVA_SOCKET` and `$KOVA_PANE_ID` let scripts self-identify. See [`docs/ipc.md`](docs/ipc.md) for the full protocol.

### Configuration

TOML config at `~/.config/kova/config.toml`. All settings have sensible defaults — the file is entirely optional.

```toml
[font]
family = "Hack"
size = 13.0

[colors]
foreground = [1.0, 1.0, 1.0]
background = [0.1, 0.1, 0.12]
cursor = [0.8, 0.8, 0.8]

[terminal]
scrollback = 10000
fps = 60

[status_bar]
branch_color = [0.4, 0.7, 0.5]

[tab_bar]
active_bg = [0.22, 0.22, 0.26]

[splits]
min_width = 300.0                     # minimum pane width in points before horizontal scroll activates
dim_opacity = 0.3                     # how much an unfocused pane is faded (0.0 .. 1.0)
dim_mode = "full"                     # "full" veils the whole pane, "text" fades only its glyphs
focus_border_width = 2.0              # outline around the focused pane, 0.0 to disable
focus_border_color = [0.4, 0.6, 1.0]
```

### Keyboard shortcuts

| Shortcut | Action |
|---|---|
| Cmd+T | New tab |
| Cmd+W | Close pane/tab |
| Cmd+D | Vertical split (side by side) |
| Cmd+Shift+D | Horizontal split (stacked) |
| Cmd+E | Vertical split at root (full-height column) |
| Cmd+Shift+E | Horizontal split at root (full-width row) |
| Cmd+Shift+[ / ] | Previous/next tab |
| Cmd+1..9 | Jump to tab |
| Cmd+Option+Arrows | Navigate between splits (cross-tab) |
| Cmd+Shift+Arrows | Swap pane with neighbor |
| Cmd+Ctrl+Shift+Arrows | Reparent pane (move to adjacent column) |
| Cmd+Ctrl+Arrows | Resize split |
| Cmd+Ctrl+Option+Left/Right | Edge grow (resize focused pane, adjust virtual width) |
| Cmd+Shift+R | Rename tab |
| Cmd+Option+R | Rename pane |
| Cmd+Shift+T | Detach tab to new window |
| Cmd+Ctrl+T | Break pane out to new tab |
| Cmd+Ctrl+M | Merge tab into another tab (as split) |
| Cmd+N | New window |
| Cmd+M | Minimize pane |
| Cmd+Option+M | Restore last minimized pane |
| Cmd+Shift+= | Equalize all splits |
| Cmd+R | Repaint focused pane (force redraw via SIGWINCH) |
| Cmd+F | Search scrollback |
| Ctrl+L | Passed to the app as usual, and clears Kova's scrollback with it (not in alt-screen) |
| Cmd+Shift+F | Global search (all tabs and panes, plus closed Claude sessions) |
| Cmd+P | Tab/pane switcher |
| Cmd+Shift+J | Pane switcher, unread panes only |
| Cmd+J | Jump to the next unread pane |
| Cmd+Shift+Option+Left/Right | Walk the panes you visited, back and forward |
| Cmd+O | Open recent project |
| Cmd+Shift+W | Close tab |
| Cmd+Shift+C | Copy selection (raw) |
| Cmd+C | Copy selection |
| Cmd+V | Paste |
| Cmd+Q | Close window |
| Cmd+Option+Q | Kill window (no session save) |
| Cmd+Shift+/ | Toggle help overlay |
| Cmd+Shift+I | Memory/perf report |

## Build

Requires macOS with Metal support and Rust (edition 2024).

```bash
# First time — create the .app bundle:
mkdir -p /Applications/Kova.app/Contents/MacOS /Applications/Kova.app/Contents/Resources
cp Info.plist /Applications/Kova.app/Contents/
cp assets/kova.icns /Applications/Kova.app/Contents/Resources/

# Build, copy binary into bundle, and codesign:
./build.sh
```

Always use `./build.sh` — it builds the binary, copies it into the `.app` bundle, and codesigns the bundle. Don't use `cargo build --release` alone as the app won't be updated.

## Non-goals

- Cross-platform support
- Plugin system
- Network multiplexing (ssh tunneling, etc.)
- Built-in AI (Claude runs *in* the terminal, not *as* the terminal)

## License

MIT
