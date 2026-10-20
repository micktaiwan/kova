# Kova

Ultra-fast Mac terminal in Rust + Metal.

This repo is public: nothing personal goes into any tracked file, and an example taken from a conversation is rewritten generically before it lands in skill/SKILL.md or anywhere else.

## Stack

- **Rust** — main language
- **Metal** — native macOS GPU rendering
- **AppKit** — window and events (via `objc2`)
- **CoreText** — glyph shaping
- **`vte`** — VT sequence parsing

## Architecture

- One binary tree of splits per tab
- One PTY per terminal pane
- Glyph atlas on the GPU

## Status

See `roadmap.md` for the versions and progress.

## Build

- The Cargo target directory is **global**: `~/.cargo/target` (not `./target`)
- So the release binary lives in `~/.cargo/target/release/kova`
- **Build**: **always** use `./build.sh`, even just to check that the code compiles. Never run `cargo build` directly — the binary would not be copied into the `/Applications/Kova.app` bundle and the app would not be updated.
- `build.sh` does: cargo build → copy the binary + Info.plist into the bundle → codesign the whole bundle (needed for macOS to keep TCC permissions across builds).
- **Does the running Kova have the last build?** Compare its start time with the binary's: `ps -axo pid=,lstart=,comm= | /usr/bin/grep Kova.app/Contents/MacOS/kova` and `ls -lT /Applications/Kova.app/Contents/MacOS/kova`. Don't go through `pgrep`: from a Claude Code Bash call, `pgrep -l kova` printed nothing while Kova was running, and `ps -p $(pgrep …)` then fails on an empty argument. The same goes for a host app linking the library: it gets a fix only once its addon is rebuilt against `~/.cargo/target/release/libkova.a` and the app restarted.

## Installation

```bash
mkdir -p /Applications/Kova.app/Contents/MacOS /Applications/Kova.app/Contents/Resources
cp Info.plist /Applications/Kova.app/Contents/
cp assets/kova.icns /Applications/Kova.app/Contents/Resources/
./build.sh
```

## Release

`/release <major|minor|patch>` — Claude Code skill that bumps the version in Cargo.toml + Info.plist, commits with a message based on the changelog, tags `vX.Y.Z`, pushes, and creates a GitHub release.

## Logs

`~/Library/Logs/Kova/kova.log` (INFO level by default, configurable via `RUST_LOG`, e.g. `RUST_LOG=debug`).

## Technical notes

- `notes/pty-spawn.md` — why `Command + pre_exec` rather than `posix_spawn` or raw `fork` for the controlling terminal

## Recurring pitfalls

- **Bytes vs chars** — Terminal cells are indexed by column (1 Cell = 1 char), but Rust `String`s are indexed by byte. NEVER do `&text[i..i+n]` on text that comes from cells (it contains emoji, box-drawing, etc.). Always work with `Vec<char>` or char iterators when handling column positions.

- **Clipping text in f32** — Right-aligned text starts at `max_x - n × cell_w`, so its last cell ends exactly on `max_x` in exact arithmetic, never in f32: an overshoot of one ulp made the last character disappear (two columns of the switcher showed `2.1.22` and `2.1.228` for the same string). Every margin-overflow test goes through `glyph_fits` (`src/renderer/mod.rs`), which tolerates a quarter of a pixel — never compare `x + cell_w > max_x` by hand again.

- **A desktop notification that shows up does not prove its click works** — `terminal-notifier` 2.0.0 only speaks `NSUserNotification`, the API deprecated since 10.14 (`nm` on its binary shows only that). On macOS 26 the banner is delivered, but the mechanism that relaunches the app on click is dead: its `-execute` never runs, silently. Symptom: "clicking the notification does nothing", and the diagnosis wrongly heads toward the socket or the command. So Kova posts its notifications itself (`src/notification.rs`, IPC command `notify`): the pane id travels in the `userInfo` and the click is handled in-process, with no external helper. Every click action goes through there — never wire it back to a third-party notifier without checking that it uses `UNUserNotificationCenter`.

- **Everything Kova retypes into a PTY comes from elsewhere and is validated first** — the line pre-typed on restore (`src/session.rs`, `restore_command`) is built from two sources Kova does not write: the `sessionId` from `~/.claude/sessions/*.json`, and `last_command`, which any program can set by printing an OSC 7777 on its own tty. A `\n` in the first is enough to fire a command without anyone pressing Enter. Hence two guards not to remove: `is_safe_session_id` (`src/claude_session.rs`) rejects anything outside `[A-Za-z0-9_-]`, and `last_command_slot_open` (`src/terminal/`) accepts only one OSC 7777 per OSC 133;C, the one the shell's `preexec` hook sends — the output of a running command no longer gets to name the next command. Every new source of injected text goes through the same question: who can write it? A third source since, same guard: resuming a closed session from the search palette (`src/claude_history.rs`) takes the id from the transcript's **file name**, so a file name anyone can drop into `~/.claude/projects/` — it goes through `resume_command`, which rejects any id outside `[A-Za-z0-9_-]`, both at indexing and at opening. A fourth source, on the other side of the library: the `command` option of `kova_pane_new` (`include/kova.h`) comes from the host app and is sent with Enter without any guard from Kova — the host answers for it, and a `\n` in it launches a second command. A fifth source: the late-delivery message of `kova ask` (`src/ask.rs`), retyped into the pane of the agent that asked the question or passed as an argument to `claude --resume` in a new tab. It carries two texts Kova does not write: the question, sent by any process that reaches the socket, and the answer the user types on the card. Every control character in it becomes a space (`late_answer_message`, not only `\n`: a `^U` would erase the line), the line is quoted for zsh (`resume_line`), the session id goes through `is_safe_session_id`, and the pane is targeted only if it runs the agent's conversation (`pane_holds_session`).

- **`NSApplication::windows()` lists windows that are not ours** — AppKit panels, tooltip holders, etc. Casting their `contentView` to `KovaView` without checking reads the ivars of another class: the resulting `Vec` of tabs held a null pointer, and `Cmd+J` segfaulted in `Tab::for_each_pane` with `self = 0`. `kova_view` (`src/app.rs`) now asks `isKindOfClass` before casting — go through it, never through a direct cast.

- **A second Kova starts with no tabs, and it is neither the config nor the session file** — the first instance locks `~/.config/kova/session.json` (`owns_session`, `src/session.rs`), the following ones open a blank window and save nothing. The log says so plainly: "Another Kova owns … : this instance starts fresh and will not save its session". Misleading symptom: you relaunch `kova &` while a Kova is still running, you don't get your tabs back, and the diagnosis heads toward config loading. Read `~/Library/Logs/Kova/kova.log` before looking elsewhere.

- **"A pane focuses by itself": the line `IPC: focused pane N` does not prove IPC** — it is written by `ipc_focus_pane` (`src/window/ipc_handlers.rs`), which Cmd+P, Cmd+J, a notification click, the search palette and the duplicate dialog also take. To find the author: look at the `performKeyEquivalent` line just before (a Cmd+P = the user), then search the transcripts `~/.claude/projects/**/*.jsonl` for `new-tab`, `close-tab`, `split` or `focus-pane` sent at the same moment. One real case: another Claude session testing a Kova client was opening and closing tabs in the user's Kova. Log lines carry only the time, not the date: filter by line number from a known marker, not by time.

## Tests

- **Run the automated tests after every code change.** As soon as a change touches Rust code, run `cargo test` (the target is global, no need for `build.sh` for that) and check that everything is green before considering the change done. A red test is part of the diff: fix it, don't leave it aside.
- **Write a unit test when adding/changing pure, testable logic** (formatting, layout/geometry computation, hit-testing, parsing, state machines). Add an inline `#[test]` that covers it, in a `#[cfg(test)] mod tests` in the same file. Don't try to test Metal rendering or the AppKit UI (GPU/window side effects): isolate the pure logic and test that.
- Never launch the application (open, Kova.app, etc.) — let the user test manually.
- **A test instance (the build binary launched next to the user's Kova, when they allowed it) announces itself before starting, with a card**: `kova ask "…" --actions "OK,Later"`, and wait for their OK. Its window comes to the front at launch; seen arriving in the middle of work, it gets closed. Launch with a clean environment (`env -i HOME USER SHELL PATH…`): inherited from the Claude session that launches it, the `CLAUDE_CODE_*` variables make the Claudes in the panes believe they are sub-sessions, with no registry and no transcript. And a separate `KOVA_ASK_DIR`, so as not to touch the real Kova's questions.

## Principles

- Mac-only, no cross-platform
- Performance and minimal RAM above all
- No feature creep on the app side: tabs, splits, config, that's it. Two extensions allowed: Kova also serves as a terminal library for a host app (work described in `docs/embedding.md`), and Kova shows agents' questions (`kova ask`; `src/ask.rs`, `src/ask_card.rs`, `docs/ipc.md` § `ask`).
- **Behavior the user has validated ("that works for me", "it's good") no longer changes; the code itself can move, provided tests prove the behavior is identical.** Required order: the tests that freeze the behavior first, green, then the move. If a review finds a gap between the code and the specs, the specs are aligned with the code, never the reverse: changing validated behavior introduces regressions on something already accepted.

## The `kova` skill is this tool's public surface

`skill/SKILL.md` in this repo describes how a Claude Code session, from anywhere on the machine, uses kova: the commands, the paths, the ports, what it is not allowed to do. The user's specifics (names, habits, working rules) go in `skill/LOCAL.md`, which is excluded from git. Nothing syncs the skill automatically.

**A change here that touches what the skill promises is carried over into the skill right away**: a command or a flag, a socket or data path, a port, a default value, a config file name, a rule about what a session may touch. An outdated skill is worse than no skill at all, because a session acts on what it says.
