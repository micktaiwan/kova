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
| `pane <N> not found` | unknown pane ID |
| `request too large` | line exceeded 64 KB |
| `timeout waiting for response` | main thread didn't reply within the connection deadline |

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
  "is_idle": false
}
```

`is_idle` means the shell has no child process — useful to check whether a pane is "free to receive a new command".

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
