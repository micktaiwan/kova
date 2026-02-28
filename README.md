# Kova

A blazing-fast macOS terminal built from scratch with Rust and Metal. No Electron, no cross-platform compromises — just native GPU rendering on Mac.

## Features

### GPU-rendered with Metal

Every frame is drawn on the GPU via Apple's Metal API. Glyph atlas with on-demand rasterization via CoreText. Dirty-flag rendering — the GPU only redraws when terminal state actually changes. Synchronized output (mode 2026) eliminates tearing during fast updates.

### Splits and tabs

- Binary tree splits — horizontal and vertical, nested arbitrarily
- Drag-to-resize separators or use keyboard shortcuts (Cmd+Ctrl+Arrows)
- Auto-equalize: splits rebalance to equal sizes when adding/removing panes
- Tabs with colored tab bar, drag-to-reorder, and rename (Cmd+Shift+R)
- Cross-tab split navigation (Cmd+Shift+Arrows)
- New splits and tabs inherit the CWD of the focused pane

### Session persistence

Layout (tabs, splits, CWD) is saved on quit and restored on launch. Window position is remembered automatically.

### Clickable URLs

Cmd+hover highlights URLs with an underline and pointer cursor. Cmd+click opens them in your browser. The hovered URL is shown in the status bar.

### Scrollback search

Cmd+F opens an inline search overlay with match highlighting. Click a match to jump to it.

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
```

### Keyboard shortcuts

| Shortcut | Action |
|---|---|
| Cmd+T | New tab |
| Cmd+W | Close pane/tab |
| Cmd+D | Split horizontally |
| Cmd+Shift+D | Split vertically |
| Cmd+Shift+[ / ] | Previous/next tab |
| Cmd+1..9 | Jump to tab |
| Cmd+Shift+Arrows | Navigate between splits |
| Cmd+Ctrl+Arrows | Resize split |
| Cmd+Shift+R | Rename tab |
| Cmd+F | Search scrollback |
| Cmd+C | Copy selection |
| Cmd+V | Paste |

## Build

Requires macOS with Metal support and Rust (edition 2024).

```bash
cargo build --release
```

The binary lands in `~/.cargo/target/release/kova` (global target dir).

### Install as .app

```bash
mkdir -p /Applications/Kova.app/Contents/MacOS
cp Info.plist /Applications/Kova.app/Contents/
ln -sf ~/.cargo/target/release/kova /Applications/Kova.app/Contents/MacOS/kova
```

After the initial setup, `cargo build --release` is all you need — the symlink picks up the new binary.

## Non-goals

- Cross-platform support
- Plugin system
- Network multiplexing (ssh tunneling, etc.)
- Built-in AI (Claude runs *in* the terminal, not *as* the terminal)

## License

MIT
