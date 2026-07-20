# IPC

Kova exposes a Unix-socket JSON API for external scripting — listing panes, spawning splits, sending keystrokes, capturing pane content, waiting for command completion. This turns Kova from "a terminal with splits" into a programmable substrate (e.g. orchestrating Claude Code agents in dedicated panes).

## Connection

Each running Kova process listens on its own socket:

```
/tmp/kova-{pid}.sock
```

Permissions are `0o600` (owner-only). The socket is removed automatically when the app exits.

Inside any pane spawned by Kova, two env vars are set:

| Variable | Value |
|---|---|
| `KOVA_SOCKET` | absolute path to that Kova's socket |
| `KOVA_PANE_ID` | numeric ID of the pane the shell is running in |

So a script running inside a pane can self-identify and address its own Kova:

```bash
echo "{\"cmd\":\"list-panes\"}" | nc -U "$KOVA_SOCKET"
```

## Wire protocol

- One request per line: a single JSON object terminated by `\n`.
- One response per request: a single JSON object on its own line.
- Multiple requests can be pipelined on the same connection.
- Each line is capped at **64 KB** on the request side. Responses have **no cap** — `get-pane-content` can return arbitrary size.

### Response envelope

Every response uses the same wrapper:

```json
{ "ok": true,  "data": <command-specific payload> }
{ "ok": false, "error": "<message>" }
```

### Common errors

| `error` | When |
|---|---|
| `invalid JSON: ...` | malformed request |
| `missing "<field>" field` | required field absent |
| `unknown command: <x>` | typo in `cmd` |
| `unknown field "<key>" for command "<cmd>"` | a field not accepted by that command (see below) |
| `pane <N> not found` | unknown pane ID |
| `tab <N> not found` | unknown tab ID |
| `request too large` | line exceeded 64 KB |
| `timeout waiting for response` | main thread didn't reply within the connection deadline |

Field validation is **strict**: every command accepts only its documented fields
(plus `cmd`). Any other top-level key is rejected with `unknown field "<key>" for
command "<cmd>"` rather than silently ignored — so a typo or a field meant for a
different command fails loudly instead of making the command do something else.

## Commands

### `split` — split the focused pane

```json
{ "cmd": "split", "direction": "horizontal" | "vertical",
  "command": "<optional shell cmd>", "cwd": "<optional absolute path>" }
```

- `direction` defaults to `horizontal`.
- `cwd` falls back to the focused pane's CWD.
- If `command` is provided, the new shell runs it on launch.

Response: `{ "data": { "pane_id": <new-id> } }`

---

### `new-tab` — open a new tab

```json
{ "cmd": "new-tab", "cwd": "<optional>", "command": "<optional>" }
```

Response: `{ "data": { "tab_id": <int>, "pane_id": <int> } }`

---

### `list-tabs` — enumerate every tab across every window

```json
{ "cmd": "list-tabs" }
```

Response: `{ "data": [ { ... }, ... ] }` where each entry has:

```json
{
  "id": 7, "window": 0, "tab_index": 2,
  "title": "build watch",
  "pane_count": 3,
  "focused_pane_id": 42,
  "active": true,
  "has_bell": false,
  "has_completion": false,
  "has_running": false
}
```

`id` is the stable tab ID (use for `close-tab` / `merge-tab`; note `set-tab-title` is addressed by `pane_id`, not tab ID); `tab_index` is the positional index in its window's tab bar (changes when tabs are reordered/closed). `active: true` only on the tab of the key window.

---

### `list-panes` — enumerate every pane across every window

```json
{ "cmd": "list-panes" }
```

Response: `{ "data": [ { ... }, ... ] }` where each entry has:

```json
{
  "id": 42, "window": 0, "tab": 3,
  "cwd": "/path", "title": "...",
  "focused": true,
  "pid": 12345,
  "child_processes": [ { "pid": 67890, "name": "node" } ],
  "is_idle": false,
  "working": true
}
```

`is_idle` means the shell has no child process — useful to check whether a pane is "free to receive a new command".

`working` is `true` when the app in the pane is actively generating or running a tool, detected from its OSC 0/2 title: Claude Code prepends an **animated Braille spinner glyph** (U+2800–U+28FF, e.g. `⠂`/`⠐`) followed by a space *only while it works*. At the prompt it instead shows an asterisk-like idle marker (`✳ Claude Code`) or a plain title, so the asterisk is explicitly NOT treated as busy. Counting panes with `working: true` therefore gives the number of Claude Code sessions actually busy — as opposed to those merely open and waiting for input (which stay `is_idle: false` too, since the `claude` process is always a child). It reads the live OSC 0/2 title even when a sticky custom title (OSC 1 / manual rename) shadows the display. Kova also shows this count in the global status bar as `✳N` (hidden when zero).

---

### `focus-pane` — bring a pane into focus

```json
{ "cmd": "focus-pane", "pane_id": 42 }
```

Switches tab and window if needed. Response: `{ "ok": true }`.

---

### `close-pane` — close a pane by ID

```json
{ "cmd": "close-pane", "pane_id": 42 }
```

Response: `{ "ok": true }`. Closes the tab if it was the last pane.

---

### `close-tab` — close a tab by ID

```json
{ "cmd": "close-tab", "tab_id": 7 }
```

Response: `{ "ok": true }`. Returns an error if the target is the **last tab** of its window — closing it would terminate the app, which is too surprising for a remote caller. (To shut down Kova, kill the process or use Cmd-Q on the window.)

---

### `merge-tab` — merge one tab into another

```json
{ "cmd": "merge-tab", "source_tab_id": 7, "target_tab_id": 4 }
```

Appends `source`'s columns to `target`, then removes `source`. Both tabs must be in the same window. The merged result becomes the active tab. Equivalent of the `Cmd+Ctrl+M` keybinding but addressable by ID.

Response: `{ "ok": true }`.

---

### `merge-window` — merge a whole window into another

```json
{ "cmd": "merge-window", "source_window": 1, "target_window": 0 }
```

Moves **every tab** of `source_window` into `target_window` (preserving order), then closes the now-empty source window. Windows are addressed by the `window` index reported in `list-tabs` / `list-panes`. The two indices must differ; the target is validated before the source is drained, so an invalid target never loses tabs.

This is the deterministic, scriptable counterpart of the `Cmd+Ctrl+Shift+M` keyboard shortcut (which instead opens an interactive window-picker overlay).

Response: `{ "ok": true }`.

---

### `swap-pane` — swap two panes

```json
{ "cmd": "swap-pane", "pane_id_a": 42, "pane_id_b": 99 }
```

Both panes must be in the same tab.

- **Same column** → swap the two panes within their column (other panes unaffected).
- **Different columns** → swap the two **whole columns** (any other panes in those columns swap with them). Matches the `Cmd+Shift+Left/Right` keyboard semantic. To move a single pane across a multi-pane column, use [reparent semantics] on the keyboard side instead.

Response: `{ "ok": true }`.

---

### `resize-pane` — adjust the ratio of a split

```json
{ "cmd": "resize-pane", "pane_id": 42,
  "axis": "horizontal" | "vertical",
  "direction": "grow" | "shrink",
  "amount_pct": 5.0 }
```

| Field | Default | Range | Meaning |
|---|---|---|---|
| `axis` | `"horizontal"` | — | `horizontal` resizes the **column** containing `pane_id`; `vertical` resizes the **row** within its column |
| `direction` | required | `grow` \| `shrink` | what to do to the pane's column/row |
| `amount_pct` | `5.0` | `[0.1, 50.0]` | percentage of weight to transfer (one keyboard nudge ≈ 5%) |

Equivalent of `Cmd+Ctrl+Arrows` keyboard resize, addressable by pane ID. Returns an error if the pane has no neighbor along the chosen axis (e.g. `vertical` on a column with a single row).

Response: `{ "ok": true }`.

---

### `rename-pane` — set a pane's sticky title

```json
{ "cmd": "rename-pane", "pane_id": 42, "title": "agent: claude" }
```

Sets the pane's custom title — the same field that `Cmd+Option+R` and `OSC 1` write to. Sticky: survives OSC 0/2 (window title) sequences emitted by programs running in the pane. Pass `"title": null` to clear (pane falls back to its OSC 0/2 / auto-derived title).

Response: `{ "ok": true }`.

---

### `dispatch-action` — trigger any keyboard action by name

```json
{ "cmd": "dispatch-action", "action": "next-tab", "pane_id": 42 }
```

Runs the exact same handler as the corresponding keyboard shortcut — this is the generic bridge that makes **every** keybinding scriptable, so Kova can be fully driven from Claude Code / shell. The typed commands above (`split`, `resize-pane`, `swap-pane`, `merge-tab`, `merge-window`, `rename-pane`, `close-tab`, `close-pane`) remain the preferred path when you want to address a specific pane/tab/window by ID; `dispatch-action` covers everything else and acts on the *focused* pane / active tab.

| Field | Default | Meaning |
|---|---|---|
| `action` | required | one of the action names below |
| `pane_id` | omitted | if given, that pane's window is focused first and the action runs there; otherwise the action runs against the key window |

Response: `{ "ok": true }`, or `{ "ok": false, "error": "unknown action: ..." }`.

**Action names** (kebab-case, mirroring the config keys in `config.rs`):

```
new-tab  close-pane-or-tab  close-tab  close-window  kill-window  new-window
vsplit  hsplit  vsplit-root  hsplit-root  equalize  repaint-pane
prev-tab  next-tab  switch-tab-1 … switch-tab-9
navigate-up|down|left|right        (move focus between panes)
swap-up|down|left|right            (swap panes/columns)
reparent-up|down|left|right        (move a pane across the tree)
resize-left|right|up|down          (ratio resize, ±5%)
edge-grow-left|right               (grow the focused pane's edge)
minimize-pane  restore-minimized
detach-tab  break-pane  merge-tab  merge-window
rename-tab  rename-pane            (open the inline rename editor)
open-recent-project  open-search  open-pane-switcher   (open an overlay)
copy  copy-raw  paste  toggle-filter  clear-scrollback
toggle-help  mem-report
```

Note: a few actions open an **interactive overlay** that then expects keyboard input — `merge-tab`, `merge-window`, `detach-tab` (when several windows exist), `rename-tab`, `rename-pane`, `open-recent-project`, `open-search`, `open-pane-switcher`. For headless automation, prefer the deterministic typed commands where one exists (e.g. `merge-window` with explicit indices, `rename-pane` with a title).

---

### `send-keys` — write text to a pane's PTY

```json
{ "cmd": "send-keys", "pane_id": 42, "text": "ls -la\n" }
```

Sends raw bytes to the shell's stdin. Use `\n` to submit a command line. Control bytes are forwarded verbatim — e.g. send the byte `0x03` for Ctrl-C, `0x1b` for Esc.

Response: `{ "ok": true }`.

---

### `set-tab-title` — override the auto-derived tab title

```json
{ "cmd": "set-tab-title", "pane_id": 42, "title": "build watch" }
```

Pass `"title": null` to clear the override (tab falls back to the auto title, e.g. shell CWD).

Response: `{ "ok": true }`.

---

### `get-pane-content` — capture the rendered text of one or more panes

```json
{ "cmd": "get-pane-content",
  "panes": "all" | [42, 43, ...],
  "mode": "visible" | "scrollback" | "all",
  "trim_trailing_blank_lines": true }
```

| Field | Default | Meaning |
|---|---|---|
| `panes` | `"all"` | which panes to dump (string `"all"`, integer array, or omitted) |
| `mode` | `"visible"` | `visible` = current grid only; `scrollback` = scrollback only; `all` = scrollback + grid |
| `trim_trailing_blank_lines` | `true` | drop fully-blank lines at the very end of each pane's output |

Per-line trailing whitespace from grid padding is always stripped. Wrapped grid lines (long output rewrapped at column boundary) are reassembled into a single logical line.

Response:

```json
{ "data": { "panes": [
  { "id": 42, "text": "...", "cols": 80, "rows": 24, "cursor": { "row": 1, "col": 20 } },
  { "id": 99, "error": "not found" }
] } }
```

Per-pane errors don't fail the whole request — missing IDs come back as `{ "id": ..., "error": "not found" }` entries inside the array.

---

### `count-pane-content` — measure what `get-pane-content` would return

Same input fields as `get-pane-content`. Returns sizes only (no `text`):

```json
{ "data": {
  "total_chars": 12345, "total_bytes": 13800,
  "panes": [
    { "id": 42, "chars": 4000, "bytes": 4500 },
    { "id": 99, "error": "not found" }
  ]
} }
```

`chars` is Unicode code points (useful for LLM cost estimation), `bytes` is the UTF-8 byte length (useful for sizing network buffers). They differ when the pane contains multi-byte characters (emoji, box-drawing, accents).

Use this **before** `get-pane-content` to decide whether the payload is worth fetching — there is no server-side cap on response size.

---

### `wait-for-completion` — block until a shell command finishes

```json
{ "cmd": "wait-for-completion", "pane_id": 42, "timeout_ms": 30000 }
```

| Field | Default | Max | Meaning |
|---|---|---|---|
| `pane_id` | required | — | pane to watch |
| `timeout_ms` | `30000` | `300000` | give up after this many ms |

Returns when the shell emits **OSC 133;D** (command-completed marker) for that pane, or when the deadline passes.

Response:

```json
{ "data": { "completed": true,  "pane_id": 42, "timed_out": false } }
{ "data": { "completed": false, "pane_id": 42, "timed_out": true  } }
{ "ok": false, "error": "pane 42 closed during wait" }
```

**Requires shell integration.** The shell must emit OSC 133 sequences. Most modern prompt frameworks (Starship, Powerlevel10k, fig/atuin, vscode-shell-integration) do this automatically. Without it, this command always times out.

**Semantics — sticky flag.** Kova's `command_completed` flag is set on OSC 133;D and stays set until the shell starts the next command (OSC 133;A). Implications:

- If the wait arrives **after** the command already finished, it returns `completed: true` immediately.
- Calling `wait-for-completion` twice in a row without sending a new command in between returns `completed: true` both times. The flag isn't consumed by observation. The intended pattern is `send-keys` → `wait-for-completion`, never two waits without a send in between.

**Long timeouts.** The connection-thread timeout is automatically extended to `timeout_ms + 2s`, so you can ask for a long wait without the connection dying first.

## Common patterns

### Run a command and capture its output

```bash
SOCK=$KOVA_SOCKET
PID=$KOVA_PANE_ID

# 1. Send the command
printf '%s' "{\"cmd\":\"send-keys\",\"pane_id\":$PID,\"text\":\"make build\\n\"}" | nc -U $SOCK

# 2. Wait for it to finish (max 5 min)
printf '%s' "{\"cmd\":\"wait-for-completion\",\"pane_id\":$PID,\"timeout_ms\":300000}" | nc -U $SOCK

# 3. Fetch what was printed
printf '%s' "{\"cmd\":\"get-pane-content\",\"panes\":[$PID],\"mode\":\"all\"}" | nc -U $SOCK \
  | jq -r '.data.panes[0].text'
```

### Spawn an agent in a new tab

```bash
printf '%s' '{"cmd":"new-tab","cwd":"/Users/me/projects/foo","command":"claude --resume"}' \
  | nc -U $KOVA_SOCKET
```

### Decide whether to fetch a large dump

```bash
SIZE=$(printf '%s' '{"cmd":"count-pane-content","panes":"all","mode":"all"}' \
  | nc -U $KOVA_SOCKET | jq '.data.total_chars')

if [ "$SIZE" -lt 100000 ]; then
  printf '%s' '{"cmd":"get-pane-content","panes":"all","mode":"all"}' | nc -U $KOVA_SOCKET
else
  echo "skipping — $SIZE chars is too much"
fi
```

## Notes

- All operations run on Kova's main thread (AppKit requirement). The IPC listener thread forwards parsed commands via an mpsc channel; the main thread processes them on its render tick (~60 Hz). End-to-end latency for a request is typically a single-digit number of milliseconds.
- `wait-for-completion` is the only command that can defer its response across multiple ticks — it doesn't block the main thread or freeze the UI.
- The socket file is removed both on graceful shutdown and on panic (via a guard); a stale socket from a previous crash is cleaned up at startup.
