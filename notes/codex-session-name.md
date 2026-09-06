# Codex conversation names

Verified against the installed Codex CLI version 0.153.4 (2026-09-06).

Kova already maps a Codex process's open rollout file to a session id, then to
the pane's shell through its ancestors. The missing part was the name:
`agent_session::for_shell` always returned `name: None` for Codex.

Codex maintains `~/.codex/session_index.jsonl` with `id`, `thread_name`, and
`updated_at`. Updates append a row; file order decides the latest name, not a
timestamp comparison. Clearing a name appends an empty string. Name metadata
updates maintain this index for both legacy and paginated history.

Sources, pinned to the installed version:

- [Index writer and readers](https://github.com/openai/codex/blob/rust-v0.153.4/codex-rs/rollout/src/session_index.rs)
- [Name metadata persistence](https://github.com/openai/codex/blob/rust-v0.153.4/codex-rs/thread-store/src/local/update_thread_metadata.rs)
- [Rename command dispatch](https://github.com/openai/codex/blob/rust-v0.153.4/codex-rs/tui/src/chatwidget/slash_dispatch.rs)

Kova streams the index once per existing one-second detection cache refresh,
only if live Codex sessions were detected. It retains names only for those ids,
ignores malformed rows, trims whitespace, and removes a previous name on a blank
entry. A missing index does not discard the detected conversation id. No writes
to Codex data, SQLite dependency, transcript-content scan, or OSC title change
is required.

Unlike Claude's `nameSource`, this index has no source discriminator: automatic
names and explicit renames both become conversation names. Claude's filtering
and existing title precedence are preserved. Detection remains scoped to the
existing `~/.codex/sessions/` rollout layout; a custom Codex home is not added by
this change.

The generic IPC field is `agent_session_name`; the compatibility fields
`claude_session_id` and `claude_session_name` remain Claude-only. Focus identity
uses the generic session id and name so a focused Codex rename is observable.

Display consumers are the pane switcher, open-pane search results, IPC title,
and automatic tab title. The per-pane status bar continues to use its separate
URL/custom/OSC rule. A manually named tab keeps its title.
