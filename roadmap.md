# Kova — Roadmap

## Vision

The fastest and lightest Mac terminal possible.
Rust + Metal, zero cross-platform compromise.

## V0 — Proof of concept

**Goal**: a window that launches a shell, displays the output, accepts input.

- [x] Minimal AppKit window via `objc2`
- [x] Metal text rendering (monospace, a single font, no ligatures)
- [x] Basic glyph atlas via CoreText
- [x] Dynamic atlas (on-demand rasterization of non-ASCII characters)
- [x] PTY: spawn a shell (zsh) via `Command` + `pre_exec` (multi-thread safe, controlling terminal via `setsid`+`TIOCSCTTY`)
- [x] Keyboard input → PTY
- [x] PTY output → screen (VT parsing via `vte`)
- [x] Basic scrollback
- [x] Ctrl+C (signal to the process)
- [x] Clean quit (Cmd+Q, window close, PTY cleanup)
- [x] Pre-allocated Metal buffers (double-buffering, no per-frame alloc)
- [x] Trackpad scroll (fractional accumulator)
- [x] Alternate screen buffer (CSI ?1049 h/l)
- [x] Window resize (recompute cols/rows + SIGWINCH)
  - The PTY slave is the controlling terminal (`setsid`+`TIOCSCTTY` in `pre_exec`), so SIGWINCH reaches subprocesses automatically
- [x] Cmd+V (paste from the clipboard)
- [x] On-demand rendering (dirty flag) — only redraw when state changes

**Success criterion**: run `ls`, `htop`, `claude` and have them work.

## V1 — Usable day to day

Recommended order: config first (independent), then text selection (single pane),
then multi-pane refactor, then splits, then tabs on top.

### Config & foundations

- [x] Config file (TOML): font, size, colors, FPS, cursor blink, scrollback
  - `~/.config/kova/config.toml`, sensible defaults, silent fallback
- [x] Detect shell death (EOF on PTY) → close the window
- [x] Status bar (CWD via OSC 7, git branch, scroll indicator, OSC 0/2 title, HH:MM time, configurable color per element)
- [x] Git branch polling — re-read `.git/HEAD` every ~2s to detect branch changes without waiting for a CWD change
- [x] Shift+Tab (backtab) — sends `CSI Z` instead of raw `0x19`
- [x] Text selection + copy/paste (mouseDown/Dragged/Up, Cmd+C, selection highlight, auto-copy to clipboard, soft-wrap respected)
- [x] Window resize: text reflow (`Row` struct with `wrapped` flag, rebuild logical lines, re-wrap to the new width)
- [x] Restore window position at launch — `NSWindow.setFrameAutosaveName` (automatic persistence via `NSUserDefaults`)

### macOS input
- [x] Option+Left/Right — word-by-word movement (sends `\x1bb`/`\x1bf`)
- [x] Cmd+Backspace — erase the whole line (sends `\x15` Ctrl+U)
- [x] Cmd+Left/Right — start/end of line (sends Home `\x1b[H` / End `\x1b[F`)
- [x] Option key — sends the macOS composed character when different from the base character

### Multi-pane refactor (prerequisite for splits)

- [x] Per-pane PTY lifecycle — shutdown per PTY (Arc<AtomicBool> per instance)
- [x] Split tree (`enum SplitTree { Leaf(Pane), Hsplit(...), Vsplit(...) }`) — binary tree in `pane.rs`
- [x] Focus model — track the active pane to route keyboard input
- [x] Multi-pane renderer — `render()` takes a viewport per pane, clipping and offset

### Splits & tabs

- [x] Horizontal and vertical splits (binary tree)
- [x] Navigation between splits (keyboard shortcuts)
- [x] Visual separators between splits (semi-transparent 1px line)
- [x] Horizontal pane padding (10px)
- [x] New split inherits the focused pane's CWD (via `proc_pidinfo`)
- [x] Split resize (Cmd+Ctrl+arrows + mouse drag on separators, clamp 0.1–0.9)
- [x] Automatic split equalization — after adding/removing a pane, all panes on the same axis are redistributed at equal size (1/N each)
- [x] Tabs (minimal bar at the top, Cmd+T new tab, Cmd+W closes pane/tab, Metal rendering, clickable tab bar)
- [x] Navigation between tabs (Cmd+Shift+[/], Cmd+1..9)
- [x] Tab drag & reorder (mouse drag with 3px threshold, real-time swap)
- [x] Tab renaming (Cmd+Shift+R, custom name takes priority, clear it to go back to the auto name)
- [x] Split close — `exit`/Cmd+W removes the pane from the tree, moves focus, `app.terminate` only when no pane is left

## V2 — Polished

- [x] Focus events (DEC mode 1004) — notify the shell/app when the window gains/loses focus
- [x] Kitty keyboard protocol (CSI u) — answer to the `CSI > 0 u` query (flags=0, clean fallback)
- [x] Save/restore session layout — save the tabs/splits tree and CWD on quit, restore at launch
- [x] File logging — write logs to a file for debugging
- [x] Tab bar redesign — tab colors, visual overhaul
- [x] Cross-tab navigation (Cmd+Option+Arrows between splits of different tabs)
- [x] Lazy write lock in the VTE parser — take the write lock only when needed, reduces contention
- [x] Synchronized output (mode 2026) — buffer rendering between h/l to avoid tearing
- [x] CPR (Cursor Position Report, CSI 6 n) — cursor position answer
- [x] DA1 (Device Attributes, CSI c) — VT220 + ANSI color identification
- [x] DECRPM (Report Private Mode, CSI ? Ps $ p) — reports state of modes 1, 7, 25, 1004, 1049, 2004, 2026
- [x] Bracketed paste mode (DEC 2004) — `\x1b[200~`/`\x1b[201~` wrapping on Cmd+V
- [x] DECCKM (mode 1) — cursor keys application mode (`\x1bO` vs `\x1b[`)
- [x] DECAWM (mode 7) — auto-wrap on/off, honored in put_char
- [x] Insert mode (SM/RM 4) — shifts characters instead of overwriting
- [x] ICH (CSI @) — insert blank characters at the cursor position
- [x] DECSCUSR (CSI Ps SP q) — cursor shape block/underline/bar
- [x] Scrollback search (Cmd+F — overlay filter, query highlight, click to scroll)
- [x] App icon in Info.plist (`CFBundleIconFile`) — fixes the oversized icon in Alt-Tab
- [x] Clickable URLs (Cmd+hover underlines in blue + hand cursor + URL in status bar, Cmd+click opens in the browser)
- [x] Wide characters (emojis, CJK) — detection via `unicode-width`, `'\0'` placeholder in col+1, 2× cell_width rasterization in the atlas
- [x] Move a split by shortcut (Cmd+Shift+Arrows — swaps the focused pane with its neighbor)
- [x] Bell indicator on inactive tabs (orange dot on unfocused tabs when a bell is received)
- [x] Horizontal scroll splits — when splits exceed the screen width, horizontal trackpad scroll + auto-reveal of the focused pane. Configurable `min_split_width`.
- [x] Color emoji rendering via CoreText fallback fonts
- [x] Grapheme cluster emoji (flags, ZWJ sequences, skin tones)
- [x] Cell RAM optimization — compact cell storage for the scrollback (48→32 bytes/cell, -33% RAM). fg/bg stored as `[u8; 3]` instead of `[f32; 3]`, converted for the GPU on the fly.
- [x] Multi-window — Cmd+N new window, Cmd+Q closes the active window, Cmd+Option+Q kill without save, Cmd+Shift+T detach tab to a new window, Cmd+Ctrl+T break pane to a new tab. Multi-window session restore. Deferred dealloc to avoid an AppKit segfault.
- [x] Keybindings config (configurable shortcuts via `[keys]` in config.toml)
- [x] Advanced visual notifications (activity in an inactive split)
- [x] PTY cleanup on a dedicated thread — `Drop for Pty` delegates the SIGHUP → SIGTERM → SIGKILL escalation to a detached thread (`pty-reaper-{pid}`), zero sleep on the main thread. `shutdown_all()` does the same escalation synchronously with shorter timeouts (25ms/step)
- [x] **Trim trailing blanks**: truncate empty cells at the end of lines in the scrollback (`shrink_to_fit`), re-expand on resize. ~50-70% scrollback RAM reduction.
- [x] Exposed perf metrics (memory, allocations) — Cmd+Shift+I overlay with RSS, terminal/renderer/pane breakdown. Frame time not included.
- [x] Double-click on a word → selects the whole word
- [x] Pane minimization — shrink a pane to a 24px bar showing title/CWD. The pane stays in the split tree, the sibling takes back the space. The PTY keeps running in the background, bell/activity visible on the bar. Cmd+M minimizes the focused pane, Cmd+Opt+M restores the last minimized one (FILO), clicking the bar restores that specific pane.
- [x] Arrows in tab/pane renaming — left/right arrows move within the text, positionable cursor, backspace/insertion at the cursor

### Next V2 priorities

- [x] **VT parser batching** _(priority 1 — perf)_ — the pty-reader holds the write lock on `TerminalState` for the whole `parser.advance()` of a 4 KB chunk. When a background pane receives a lot of data (build, logs…), the write lock blocks the renderer's read locks (`parking_lot` gives priority to writers → visible lag on tab switch). Solution: a local `Vec<TermOp>` parsed without the lock, flushed in a single write lock. Estimated 5-10× reduction of time under lock. See `notes/vt-parser-batching.md`.
- [x] **Font fallback (block elements/box-drawing)** _(priority 2 — visual)_ — characters U+2500-U+257F (box-drawing) and U+2580-U+259F (block elements) must be drawn directly into the bitmap instead of going through CoreText. Font glyphs don't fill the cell edge to edge → black line in the Claude Code banner, broken borders in every TUI (lazygit, btop). All modern terminals (Alacritty, Kitty, WezTerm, Ghostty) do this custom rendering. See `notes/font-fallback-investigation.md`.
- [ ] **Pinned columns (`custom_weight`)** — per-column flag marking a manual resize. Pinned columns keep their weight during redistributions. See `docs/split-specs.md` section "Pinned columns/rows".
- [ ] **Multi-column redistribution on resize** — when a separator is moved (mouse or keyboard), redistribute the space among all non-pinned columns on the opposite side, instead of touching only the two adjacent columns. See `docs/split-specs.md` section "Redistribution".
- [ ] **Mouse cursor on separators** — change the cursor when hovering a separator (↔ for columns, ↕ for VSplits, ±3px tolerance). See `docs/split-specs.md` section "Mouse drag".
- [ ] **Emoji presentation fallback** — characters with default `Emoji_Presentation` (U+2B1C, U+2B1B, U+25AA…) use the mono font's glyph instead of the Apple Color Emoji color version. Fix: force the emoji fallback for Unicode codepoints with `Emoji_Presentation=Yes`.
- [ ] **Cmd+V in the search field** (Cmd+F) — paste currently doesn't work in the search overlay.
- [ ] **Tab bar font size**: tab font size configurable independently (`tab_bar.font_size`), with a possible per-window override. See `notes/tab-font-size.md`.
- [ ] **Move a split by drag** (visual anchor during the drag — swap by shortcut Cmd+Shift+Arrows already exists).
- [ ] **Run-length encoding**: compress runs of the same color. (Marginal gain after trim, high complexity — deprioritized.)

## V3 — Advanced

- [x] **IPC / external control** _(priority 3 — strategic)_ — Unix socket (`/tmp/kova-{pid}.sock`) accepting JSON commands: `split --cmd "..."`, `list-panes`, `close-pane`, `send-keys`. Lets Claude Code Teams spawn agents in separate panes (today only tmux can). Turns Kova from a "terminal with splits" into a "scriptable development platform". See `docs/ipc.md`.
- [x] **Restoring Claude Code sessions** — a pane that was running a Claude Code conversation comes back with `claude --resume <id>` pre-typed instead of its last shell command. Makes restarting Kova painless when many conversations are open.
- [x] **Claude Code sessions waiting for an answer** — a first-class state: each session tells Kova it is waiting, through its hooks (`Stop` and `permission_prompt` set it, `UserPromptSubmit` and `SessionEnd` clear it), and Kova retracts it on its own when the pane contradicts it (dead process, spinner running again, keystroke). Shown by a `?` on the pane's line in Cmd+P (where `Tab` jumps to the next one) and a `?N` counter in the status bar. Answers "which of my many sessions need me" without reading a single screen. See `docs/ipc.md` section `set-pane-status`.
- [x] **Finding a closed Claude session** — the search palette (Cmd+Shift+F) also searches past Claude Code conversations, not only open panes. An index (`~/.config/kova/claude_history.json`) keeps from the transcripts only the typed prompts, the project directory and the date; it updates when the palette opens, rereading only the appended bytes (the `.jsonl` files are append-only). The query is split on spaces and commas and every word must be present (AND) — in all three sections of the palette, open panes included —, so a two-word query narrows the list much more than either word alone. The section shows only the 8 most recent sessions and states in its title how many matched in total, so as not to bury the open panes above. Enter on an archived line reopens a pane in the session's project, with `claude --resume <id>` typed and launched. See `src/claude_history.rs`.
- [x] **Undo a closed pane** — Cmd+W kills the pane's PTY, so a mis-press used to cost the whole conversation. The last ten closes now go on an in-memory stack (`src/closed_pane.rs`, taken while the pane is alive, since its conversation id is read off the running process), and Cmd+Shift+Z puts one back in the slot it left — same tab, same column and row; as a tab of its own, name and color included, when that tab went with it — with a fresh shell in its directory and the agent conversation it was holding resumed (`claude --resume`, `omp --resume`, … runs itself; a bare shell's last command only waits at the prompt). The dead terminal's scrollback is not replayed. Exposed as `dispatch-action reopen-closed-pane`.
- [x] **Anchors — what the day is for** — a second list next to the bookmarks, with a different promise: a bookmark says "this conversation matters", an anchor says "this is what I'm supposed to be doing now". Cmd+Shift+A sets or removes the anchor of the focused pane (8 max), Cmd+A always brings you back to the first one — the pane holding it, or the conversation reopened with its resume line when no pane holds it. Cmd+P opens on an "⚓ Anchors" section, at the top and before the tabs, which lists the anchors whether a pane holds them or not (unlike bookmarks, which an open conversation leaves); a conversation both anchored and bookmarked appears only under the anchors. The list belongs to Kova (`~/.config/kova/anchors.json`) and depends on no outside tool: a tool that knows what the day is for pushes it through the IPC commands `set-anchor` and `clear-anchors`, and Kova never calls out. See `src/anchors.rs` and `docs/ipc.md`.
- [x] **Search finds today's conversations** — a word typed in two still-open conversations showed up nowhere. Three fixes. (1) An open Claude pane is also searched in its conversation's indexed prompts, not only in its screen (`match_pane`, `src/window/search_palette.rs`). (2) The index takes slash commands with their arguments (ignored until then, though frequent) and prompts sent with an image; it keeps the 256 KB most recent per session instead of the first 64 (`INDEX_VERSION` 3, a full pass on first launch). (3) A term only counts fully as a whole word: a term found only inside a longer word drops into a last section "Inside longer words". Closed sessions are sorted by date alone; the relevance score (recency, investment, reopenings, project, title) was removed.
- [ ] **Widening what search looks at** — Cmd+Shift+F doesn't find a session from a word that was written in it, and it is a corpus problem, not a matching one. Two independent holes. (1) For an **open** pane, `run_search_worker` (`src/window.rs:3585`) searches `dump_text(DumpMode::All)`, but a Claude Code session runs on the alternate screen: `enter_alt_screen` (`src/terminal/mod.rs:1537`) sets the normal screen aside and `src/terminal/mod.rs:1041` only feeds the scrollback outside the alternate screen, so search only sees the current screen. (2) For a **closed** session, `claude_history.rs` indexes the typed prompts only — neither the model's answers nor tool outputs. **Done for the model's answers (`INDEX_VERSION` 5)**: `assistant_text` (Claude) and `omp_message` (both roles) now keep the agent's `text` blocks in a `reply` string of its own, capped at 256 KB per session beside the prompts' 256 KB, so a long answer cannot evict the questions; `search`, `texts_of` and the palette's snippets read both. Measured on this machine's 1.4 GB corpus: replies add 20 MB for 5966 Claude transcripts and 0.1 MB for 26 omp ones, no transcript reaching the cap, so `claude_history.json` goes from 10.5 MB to ~30 MB. Typical case still unserved: searching for a phrase that only ever appeared in tool outputs (such as a PR title discussed at length) returns nothing. Volumes measured on 40 random transcripts (1.5 GB corpus): typed prompts 0.4% of raw bytes, model text 1.1%, commands sent to tools 4.8%, tool outputs 18.4%, the rest being JSON plumbing. So indexing the model and the commands takes `claude_history.json` from 3.7 MB to ~50 MB (the monolithic JSON format still holds); adding tool outputs gives ~375 MB of text and requires SQLite FTS5. Also make **open** Claude panes search in their `.jsonl` rather than in `dump_text`, which reuses the archived section's code. **Semantic search with embeddings was ruled out for now**: it doesn't answer the problem (the searched word is in neither corpus, so no vector would catch it), it would require a model permanently loaded in a binary that today starts with no RAM, it degrades literal search (a PR number, a file name) and it costs ~22,000 chunks on prompts and answers alone, ~375,000 with tool outputs. If it ever comes back, it will be through a **separate** indexer that writes a vector file Kova only reads. Meanwhile, the workaround for open panes is a script that greps the output of the `get-pane-content` IPC command.
- [ ] Inline image support (Kitty Graphics Protocol) — display images in the terminal (`icat`, `yazi`, etc.). APC parser, image store, Metal texture manager, separate draw calls. See [`docs/image-support.md`](docs/image-support.md)
- [ ] Shell integration (marks, prompt-to-prompt navigation)
- [ ] Inline completion / suggestions

## Nice to have

Interesting but non-priority items — the gain doesn't justify the effort in the short term.

- [ ] ProMotion support (120Hz) — the dirty flag already skips rendering when nothing changes, so the extra cost is limited to scrolling/large outputs. But the 60→120 Hz difference is marginal for a terminal (static text 99% of the time).
- [ ] Color themes — colors are already individually configurable in `config.toml`. Themes would add a level of abstraction (`theme = "catppuccin-mocha"`) to switch the whole palette at once (16 ANSI + fg/bg/cursor/selection). Handy but not blocking: users can already copy-paste a block of colors into their config.
- [ ] Ligatures — complex (CoreText shaping by glyph groups vs the current 1 cell = 1 glyph)

## Non-goals

- Cross-platform (macOS only)
- Plugin system
- Proprietary custom protocols
- Network multiplexer (ssh tunneling etc.)
- Built-in AI (Claude runs in the terminal, no need)
