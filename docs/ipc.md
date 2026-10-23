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
- One command escapes that shape: [`subscribe`](#subscribe--stream-state-changes-as-they-happen) turns the connection into a one-way event stream. After its response, Kova pushes one line per state change and never reads that connection again.

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
  "child_processes": [ { "pid": 67890, "name": "node", "version": null } ],
  "is_idle": false,
  "working": true,
  "awaiting": false,
  "awaiting_since": null,
  "awaiting_seen": false,
  "minimized": false,
  "agent": "claude",
  "agent_session_id": "0b6f…",
  "agent_session_name": "subscribe on the Kova side",
  "claude_session_id": "0b6f…",
  "claude_session_name": "subscribe on the Kova side",
  "peer_name": "subscribe on the Kova side",
  "dormant_session_id": null,
  "subject": "auth"
}
```

`is_idle` means the shell has no child process — useful to check whether a pane is "free to receive a new command".

`child_processes[].name` is the program's name as it was invoked (argv[0], what `ps` shows), not the kernel's `p_comm`. The difference matters for anything installed under a versioned filename: Claude Code's binary is `~/.local/share/claude/versions/2.1.226`, so `p_comm` there is `2.1.226` and the name would say nothing about which program is running. `version` carries that number when the executable's own filename is one, and is `null` otherwise — so a Claude Code pane reads `{"name": "claude", "version": "2.1.226"}`.

`claude_session_id` is the id of the Claude Code conversation running in the pane — the argument `claude --resume` takes — and `claude_session_name` is the name its `/rename` set (`null` until the user sets one). Both are `null` in a pane with no Claude session. They are read from `~/.claude/sessions/<pid>.json`, on the same throttled probe as the rest, and they matter because **the id is the only identifier here that outlives the pane**: a client tying a pane to a subject must key on it rather than on `id` or `cwd`. A pane id dies with its tab and does not survive a Kova relaunch either (every restored pane is numbered afresh, so an id noted before the relaunch may name another pane), and a directory is shared by every unrelated conversation started in the same repository. The name also appears inside `title`, but only as one candidate among five (see `display_title`), where it cannot be told apart from an OSC title or a directory name.

`peer_name` is the name other Claude Code sessions use to reach this one with `SendMessage` / `ListAgents` (e.g. `"myproject-f1"`). It is the session file's `name` field as Claude Code wrote it, **including a name it derived on its own** (`nameSource: "derived"`), which `claude_session_name` deliberately drops so that it never shadows the pane title. After a `/rename` the two fields carry the same value. It is `null` in a pane with no Claude session, for Codex, and while Claude Code has not yet written a name. It comes from the same cached scan of `~/.claude/sessions/` as `claude_session_id`, with no extra filesystem pass.

`agent` is `"claude"`, `"codex"`, or `null`. `agent_session_id` and `agent_session_name` expose the detected conversation's id and name for either agent; the name is `null` when absent. The `claude_*` fields remain `null` for Codex, including named Codex sessions. Codex resumes with `codex resume <id>`.

Codex names come from `~/.codex/session_index.jsonl`, joined by session id to the live process detection. The last valid entry for an id wins; a blank name clears it. Reads share the detection's one-second cache, and missing or malformed entries leave the normal title fallback available. Unlike Claude's session files, this index does not distinguish automatic names from explicit `/rename` names: Kova displays the latest persisted name in both cases.

The pane switcher, open-pane search results, IPC `title`, and automatic tab titles use the conversation name before the pane's custom title, OSC title, foreground process, and directory. An explicitly named tab keeps its own title. The per-pane status bar has its own existing rule (hovered URL, custom pane title, OSC title), and does not read the conversation name directly.

`dormant_session_id` is the conversation a relaunch left asleep in the pane. After a relaunch, Kova resumes a restored Claude Code or Codex conversation only the first time its pane is drawn in the active tab of a window (focused or not; a pane minimized or scrolled out of view does not count, but a window hidden behind others, on another Space or in the Dock still wakes its active tab), or on `wake-pane`, or when a late `ask` answer is delivered to it. Until then the pane is a bare shell: its resume line is neither typed nor run, `agent`, the `*_session_id` fields and `peer_name` are `null`, and the session does not appear in `ListAgents`, so `SendMessage` cannot reach it. A tab reopened with Cmd+O is not concerned: its line is typed and waits for Enter. A `send-keys` to a sleeping pane cancels its sleep instead: the client is using the pane, so the resume line is dropped and will never be typed over what it started; `wake-pane` is the way to get the conversation back. `null` once the pane is woken or for any other pane. A sleeping pane, or one woken whose agent has not reported its session yet, is still saved with its conversation on quit, and the global status bar counts the sleeping ones as `◌N`.

`minimized` is `true` for a pane collapsed with `minimize-pane` (`Cmd+M` by default). Such a pane still runs and is still listed here; it simply takes no layout space and is not drawn. Kova marks it with a `⊟` glyph in the pane switcher and counts it in the status bar. A remote client should keep showing it and mark it the same way rather than filter it out.

`working` is `true` when the app in the pane is actively generating or running a tool, detected from its OSC 0/2 title: Claude Code prepends an **animated Braille spinner glyph** (U+2800–U+28FF, e.g. `⠂`/`⠐`) followed by a space *only while it works*. At the prompt it instead shows an asterisk-like idle marker (`✳ Claude Code`) or a plain title, so the asterisk is explicitly NOT treated as busy. Counting panes with `working: true` therefore gives the number of Claude Code sessions actually busy — as opposed to those merely open and waiting for input (which stay `is_idle: false` too, since the `claude` process is always a child). It reads the live OSC 0/2 title even when a sticky custom title (OSC 1 / manual rename) shadows the display. Kova also shows this count in the global status bar as `✳N` (hidden when zero).

`awaiting` is `true` when the app in the pane has declared, over `set-pane-status`, that it is waiting for the user — and nothing Kova observed since contradicts it. `awaiting_since` is the epoch second at which the wait started (`null` when not waiting), so a client can show how long a session has been unanswered. Unlike `working`, this is *pushed*, not guessed: `working` is inferred from the terminal title, while `awaiting` is a claim the running app makes about itself. See `set-pane-status` for how it is set and, more importantly, for the ways Kova retracts it on its own.

`subject` is the name of the subject the pane is filed under in `Cmd+P` (the box it is drawn in), or `null`. Kova.app only: a library host has no subjects and its panes carry no such field. See `list-subjects`.

`awaiting_seen` is `true` when a waiting pane has been looked at on this Mac since it started waiting: Kova stamps it on the focused pane of the key window, on every frame. It is the difference between the two things `awaiting` alone cannot tell apart, and the reason `Cmd+J` does not hand you the same pane twice. The marker stays up either way, because reading a question is not answering it; only the jump set shrinks. A remote client that walks waiting panes should obey the same halt, otherwise it offers panes already read here. It is `false` on a pane that is not waiting, where the bit means nothing.

---

### `set-pane-status` — declare that a pane is waiting for the user

```json
{ "cmd": "set-pane-status", "pane_id": 42, "status": "waiting" | "none" }
```

Marks the pane as waiting for an answer, or clears the mark. `status` accepts exactly those two strings; anything else is an error rather than a silent no-op, so a mistyped hook fails loudly instead of quietly clearing the flag.

Meant to be driven by the app running inside the pane. For Claude Code that means its hooks: `Stop` and the `permission_prompt` notification set `waiting`; `UserPromptSubmit` and `SessionEnd` set `none`. A pane knows its own id and socket from `$KOVA_PANE_ID` / `$KOVA_SOCKET`, which hook processes inherit.

Setting `waiting` twice does not restart the clock: `awaiting_since` keeps the timestamp of the first mark, so a second `Stop` on the same unanswered turn does not make an old wait look fresh.

Response: `{ "ok": true }`.

**The flag is a claim, not a fact.** A session killed with `-9` never gets to retract it, so Kova never reports the flag without re-checking it. It is dropped when:

- the pane's foreground process is gone (its Claude Code died, or the pane fell back to a bare shell prompt) — checked on the same throttled probe that maintains the tab "running" indicator;
- the pane's shell exited;
- Claude Code went back to work (the Braille spinner reappears in the OSC title) — reported instantly, without waiting for the throttled probe;
- the user typed into the pane, or `send-keys` wrote to it: someone answered;
- the pane was closed, or Kova restarted. The state is deliberately not persisted across restarts; a resumed session re-declares itself on its next turn.

There is intentionally **no time-based expiry**. A question left unanswered for three hours is exactly what this flag is for.

**Nothing in the UI draws this flag.** It used to show as a `?` on the switcher row and a `?N` status-bar counter; both were dropped, because `Stop` fires at the end of every turn — so the marker said "waiting" on sessions that asked for nothing, and it stayed up until the next prompt was submitted even after the pane had been read. What the UI shows instead is unread output (bell / finished command), which is what `Cmd+P`'s `Tab`, the `●N` counter and `Cmd+J`'s first tier all walk. The flag lives on for IPC clients only — `list-panes` and the `pane-status` event, which `/pane-sweep` consumes.

---

### `mark-completed` — light a pane's "finished" indicator

```json
{ "cmd": "mark-completed", "pane_id": 42 }
```

Does what OSC 133;D does: the pane gets the unread "finished command" dot that `Cmd+P`'s `Tab`, the `●N` counter and `Cmd+J` walk, and a pending `wait-for-completion` on the pane returns. Unlike the shell's own 133;D, it is never swallowed as a startup marker, so it works on a pane with no shell integration.

It exists for hooks of an app that owns the tty. Claude Code's `Stop` hook used to print `ESC ] 133;D BEL` straight to the pane's tty; those bytes interleave with Claude Code's output at any point, and landing inside one of its CSI sequences (`ESC [ 10 G`) aborts it, so the parameters are printed as text — `Les 1410Gmide` for `Les 14 min de`. Claude Code's differential renderer never repaints the line, so the garbage stays until something makes it redraw (a mouse selection does). Captures from one day held 30 such splits. Over the socket, nothing reaches the tty.

Response: `{ "ok": true }`.

---

### `mark-started` — clear a pane's "finished" flag

```json
{ "cmd": "mark-started", "pane_id": 42 }
```

Does to the flag what OSC 133;C does: the last completion no longer answers `wait-for-completion`. Claude Code's `UserPromptSubmit` hook sends it, so a Claude pane marked done by one turn's `Stop` does not look done for the whole next turn. It leaves the running indicator alone: a turn interrupted with Esc fires no `Stop`, and the foreground-process probe already tracks it.

Response: `{ "ok": true }`.

---

### `bell` — raise a pane's bell flag

```json
{ "cmd": "bell", "pane_id": 42 }
```

Same effect as a BEL in the pane's output (the unread bell marker), for the same reason as `mark-completed`: a BEL printed on the tty by a hook can land inside one of the app's OSC sequences (a title, a hyperlink) and terminate it early, spilling the rest as text. Claude Code's `Notification` and `Stop` hooks use it.

Response: `{ "ok": true }`.

---

### `focus-pane` — bring a pane into focus

```json
{ "cmd": "focus-pane", "pane_id": 42 }
```

Switches tab and window if needed. A minimized pane is given its space back on
the way — this is the only path that restores one by ID, and the reason the
keyboard jumps (Cmd+J, back/forward) leave minimized panes alone while this
command does not. Response: `{ "ok": true }`.

---

### `wake-pane` — resume the conversation asleep in a pane

```json
{ "cmd": "wake-pane", "pane_id": 42 }
```

Runs the resume line of a pane whose `dormant_session_id` is set (see
`list-panes`), where it is, without focusing it or switching tab. Response:
`{ "ok": true, "data": { "woken": true } }`; `woken` is `false` when the pane
was not asleep, and nothing was done. The agent reports its session in
`list-panes` once it has started, a few seconds later.

---

### `notify` — post a desktop notification that focuses a pane when clicked

```json
{ "cmd": "notify", "pane_id": 42, "title": "Claude Code", "message": "done", "sound": false }
```

Only `message` is required. `title` defaults to `"Kova"`, `sound` to `false`, and
`pane_id` may be omitted for a notification that carries no destination — clicking
it then only brings Kova to the front.

Kova posts the notification itself instead of leaving it to a helper because it
is the only process that can act on the click: the click focuses `pane_id` exactly
as `focus-pane` would, restoring it if it was minimized. `terminal-notifier
--execute` cannot do this on macOS 26 — it still speaks the removed
`NSUserNotification` API, so its notification is delivered but its command never
runs.

`pane_id` is not checked when the notification is posted: the pane may
legitimately be gone by the time the user clicks, and Kova logs the miss then
rather than refusing the notification now. Response: `{ "ok": true }`, or an error
when Kova runs outside its app bundle (notifications need a bundle identifier).

The first notification of a fresh install triggers the system permission prompt;
if the user declines, `notify` still answers `{ "ok": true }` and macOS silently
drops the banner.

A library host (an app that embeds Kova, `include/kova.h`) posts the notification
itself: it registers a handler with `kova_set_notify_handler`, which receives the
pane, title, message and sound, and handles the click (it knows which of its
documents holds the pane). The answer is `{ "ok": true }` once the request is
queued for that handler; a library host that registered none answers
`notify is not supported by this host`.

---

### `ask` — put an agent's question on the user's screen and wait for the answer

```json
{ "cmd": "ask", "question": {
    "id": "6f1c…-uuid", "message": "Deploy to staging?",
    "actions": ["OK", "KO"], "input": true, "asked_at": 1792000000,
    "from": { "name": "routine", "session_id": "…", "pane_id": 12,
              "kova_socket": "/tmp/kova-123.sock", "cwd": "/Users/…/project" } } }
```

Not meant to be spoken by hand: `kova ask "<message>" [--actions OK,KO]` builds it
from its environment (`CLAUDE_CODE_SESSION_ID`, `KOVA_PANE_ID`, `KOVA_SOCKET`) and
`kova ask --help` documents the client. `id` must be a UUID (it names files on
disk), `session_id` must be a plain `[A-Za-z0-9_-]` id (it reaches a command line).

Kova shows the question on a card in the top-right corner of the screen (one
card, oldest question first, a Dock badge counting them) and answers only when
the user does — there is no server-side timeout. The response is
`{"ok":true,"data":{"id":…,"action":"OK","text":"…"}}`, after which the client must
write the line `ack` within 2 s: without it, the answer is considered lost on
the way and Kova delivers it to the conversation itself:

1. if the conversation sleeps in a pane since a relaunch (`dormant_session_id`,
   looked up in the asker's Kova and in the one delivering), Kova wakes that
   pane (`wake-pane`), waits up to 60 s for its agent to report the session,
   and types the answer there; if the wake fails or the agent does not come
   up, the answer stays on disk, retried at the next launch. Steps 2
   to 4 are skipped then: a resume in a new tab would run the conversation twice
   once the sleeping pane is shown;
2. typed into the asker's pane, if that pane still runs the asker's Claude
   conversation (same `claude_session_id`, or the conversation's process
   descends from the pane's shell);
3. if the conversation is open in another process, Kova waits for it to end;
4. then reopens it in a new tab: `claude --dangerously-skip-permissions --resume
   <session_id> '<the answer>'`, in the directory the conversation ran in (read
   from its transcript, which `claude --resume` needs; not the asker's `cwd`).
   No transcript, or `claude` no longer running in the new pane 10 s after
   Enter (it exits at once on "No conversation found"), and the answer stays
   on disk, retried at the next launch.

A pane of a library host (an app that embeds Kova) names that host's socket, which answers `ask`
with `command not supported by this host` (or `unknown command: ask` for a
build older than `ask`): `kova ask` then asks Kova.app.

Questions and undelivered answers survive a restart (`~/.config/kova/ask/`,
`KOVA_ASK_DIR` overrides it). Only the Kova holding that directory's lock
(`owner.lock`, taken at launch) writes there and replays it: another Kova
running at the same time keeps its own questions in memory only, since
anything it wrote would be shown and delivered a second time by the holder. Kova.app only: a library host answers `command not
supported by this host`, which sends `kova ask` on to Kova.app.

---

### `list-asks` — the questions waiting on the card

```json
{ "cmd": "list-asks" }
```

For a client that answers somewhere else than the card, such as a companion app
that shows them on a phone. Response:
`{"ok":true,"data":{"asks":[…]}}`, oldest first (the one on the card comes
first), each entry the `question` object of `ask` exactly as it was asked, plus
`"buttons"`: the buttons the card shows, in order (`["Reply"]` when `actions` is
empty), so a client never re-derives that default. `"asks": []` when nothing
waits. Questions restored after a restart are listed like the others.

Kova.app only: a library host answers `command not supported by this host`.

---

### `answer-ask` — answer a pending question as if on the card

```json
{ "cmd": "answer-ask", "id": "6f1c…-uuid", "action": "OK", "text": "go ahead" }
```

`text` is optional (default `""`) and trimmed, as the card trims its field. The answer takes the card's own path: the
waiting `kova ask` client gets it (or it is delivered late, exactly as described
under `ask`), and the card moves on to the next question or disappears. An
answer given here is indistinguishable from a click. Response:
`{"ok":true,"data":{"id":…,"action":"OK","text":"go ahead"}}`.

Errors:

- `"id" must be a UUID`;
- `no pending question <id>`: already answered (on the Mac, or by another
  client), or never asked here;
- `"action" must be one of: OK, KO`: `action` is not one of that question's
  `buttons` (case matters);
- `"text" must be empty: this question has no text field`: the question was
  asked with `input: false`.

Kova.app only: a library host answers `command not supported by this host`.

---

### `withdraw-asks` — take back a conversation's questions, answered elsewhere

```json
{ "cmd": "withdraw-asks", "session_id": "6e67ffdd-…" }
```

The user answered in the conversation itself (typed a prompt in its pane, or
from a remote client) rather than on the card: every question that
conversation (`from.session_id`) still has on the card is taken off it and off
disk, and **nothing is delivered late** — a click twenty minutes later would
only retype a stale answer. A `kova ask` still waiting gets the error
`withdrawn: the user answered in the conversation instead of on the card` and
exits with code 3. Answers already given and waiting for delivery are not
touched. Response: `{"ok":true,"data":{"withdrawn":1}}` (0 when nothing was
pending).

Sent by a Claude Code `UserPromptSubmit` hook running
`kova ask --withdraw --session -` (it reads the hook's JSON input for the
`session_id`, prints nothing and always exits 0). Kova.app only: a library host
answers `command not supported by this host`, and the client then asks Kova.app.

---

### `set-anchor` — put a conversation at the head of the anchors

```json
{ "cmd": "set-anchor", "pane_id": 42 }
{ "cmd": "set-anchor", "cwd": "/Users/x/projects/prez", "session_id": "abc123", "agent": "claude", "label": "Release notes" }
```

The anchors are the few conversations today is for: they open `Cmd+P` in their
own section and `Cmd+A` goes to the first of them. This command puts one at the
head, which is how an outside tool that knows the day's work (a task manager, a
calendar) says "today is this one". Kova never calls out: it holds the list and
answers this.

Either name a pane of this Kova with `pane_id` — its conversation, directory and
title are read from the pane — or name the conversation outright with `cwd`,
plus `session_id` and `agent` (`"claude"` or `"codex"`) for Kova to be able to
reopen it, and `label` for the row (the directory's name otherwise). A
`session_id` without an `agent` is dropped: nothing could resume it. A
conversation already anchored moves to the head rather than gaining a second
row, so pushing the same anchor twice is harmless. The list is capped at 8; past
that the last anchor of the list falls off and its label comes back in `dropped` — a push
from outside always lands, where `Cmd+Shift+A` on a full list refuses instead
(you are there to drop one). An anchored conversation stops appearing among
the bookmarks below, even when it is bookmarked too.

Response: `{ "ok": true, "data": { "label": "Release notes", "anchors": 3, "dropped": null } }`.

Errors rather than a surprise: a request naming neither `pane_id` nor `cwd`, a
`pane_id` that is not open (or holds neither a conversation nor a directory), and
a `session_id` the agent could not resume.

---

### `clear-anchors` — empty the anchor list

```json
{ "cmd": "clear-anchors" }
```

Kova has no notion of a day and never clears the anchors on its own: this is how
the outside says the day is over. Bookmarks are untouched — a conversation that
was in both lists is simply a bookmark again.

Response: `{ "ok": true, "data": { "removed": 3 } }`.

---

### `reload-config` — re-read the config file without restarting

```json
{ "cmd": "reload-config" }
```

Reads `~/.config/kova/config.toml` again and pushes its appearance half into
every open window: `[colors]`, `[status_bar]`, `[global_status_bar]`, `[tab_bar]`,
`[bookmarks]`, and the fade / focus outline of `[splits]`. The next frame draws
with the new values — around 16 ms later, no restart, no session reload.

What it does not touch, because they are read once at startup and would mean
rebuilding the glyph atlas or resizing every PTY: `[font]`, `[terminal]`,
`[window]`, `[keys]`. An invalid TOML file leaves every window as it was (the
loader falls back to defaults and logs the parse error).

Response: `{ "ok": true, "data": { "windows": 2, "needs_restart": ["font", "keys", "terminal", "window"] } }`.

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

One thing outranks it in the pane's display title: the current Claude or Codex conversation name (`agent_session_name` in `list-panes`). A sticky title outlives whatever the pane is used for next, so a pane running a named session shows that name, and the custom title reappears once the session ends or its name is cleared. The per-pane status bar uses the separate rule described under `list-panes`.

Response: `{ "ok": true }`.

---

### `list-subjects` — the subjects of every tab

```json
{ "cmd": "list-subjects" }
```

A subject is a named group of panes inside one tab, drawn in `Cmd+P` as a box under its name. It belongs to its tab: the same name in two tabs is two subjects. Response: one entry per subject, tab by tab, each tab's in the order `Cmd+P` draws the boxes (empty subjects first, then each where its first member sits):

```json
{ "ok": true, "data": [
  { "tab_id": 3, "window": 0, "tab_index": 0, "name": "idle", "color": 2, "pane_ids": [] },
  { "tab_id": 3, "window": 0, "tab_index": 0, "name": "auth", "color": 4, "pane_ids": [42, 57] }
] }
```

`color` is the subject's color, an index into the tab palette (the same indices as `set-tab-color`): drawn at random when the subject is created, among the colors its tab uses least (ties broken at random), then kept across relaunches. The user can change it (right-click on the band, `c` on the header in `Cmd+P`), and so can `set-subject-color`. It frames the subject's box in `Cmd+P` and paints its band in the strip Kova draws under the tab bar. `pane_ids` follows the tab's order. To follow changes instead of polling, `subscribe` to `subjects`: the event carries this same list whenever anything in it changes. A pane id dies with its pane (see `list-panes`); a client that must remember which conversation sits in which subject keys on the pane's `agent_session_id`. Kova.app only: a library host answers `command not supported by this host`.

---

### `set-pane-subject` — file a pane under a subject, or take it out

```json
{ "cmd": "set-pane-subject", "pane_id": 42, "subject": "auth" }
{ "cmd": "set-pane-subject", "pane_id": 42, "subject": null }
```

With a name, files the pane under that subject of its own tab, as `Cmd+→` does; a name the tab does not have yet creates the subject. A pane already in another subject moves to this one. Nothing moves on screen. With `null`, takes the pane out of its subject, as `Cmd+←` does: a pane from the middle of the box moves past its last member, so it lands right under the box, and the subject stays even left empty. Taking out a pane that is in no subject does nothing and is not an error.

`subject` is required (a forgotten field never reads as "take it out"). The name is trimmed; it must not be empty, hold a control character, or exceed 200 characters. An open `Cmd+P` is redrawn with the new boxes.

Response: `{ "ok": true, "data": { "tab_id": 3, "subject": "auth", "created": false } }`. Error: `pane <id> not found`.

---

### `rename-subject` — rename a subject of a tab

```json
{ "cmd": "rename-subject", "tab_id": 3, "name": "auth", "new_name": "login" }
```

Renames the subject in place, its panes following, as Enter on its header in `Cmd+P` does. A `new_name` the tab already has merges the two: the renamed subject's panes join the other one, which keeps its place. Names are checked as in `set-pane-subject`.

Response: `{ "ok": true, "data": { "merged": false } }`. Errors: `tab <id> not found`, `tab <id> has no subject "<name>"`.

---

### `delete-subject` — delete a subject of a tab

```json
{ "cmd": "delete-subject", "tab_id": 3, "name": "auth" }
```

Deletes the subject. Its panes stay open where they are, out of any box. Unlike `Cmd+⌫` on the header in `Cmd+P`, nothing is confirmed: the client asked.

Response: `{ "ok": true, "data": { "released": 2 } }` — how many panes the subject held. Errors as in `rename-subject`.

---

### `set-subject-color` — choose a subject's color

```json
{ "cmd": "set-subject-color", "tab_id": 3, "name": "auth", "color": 4 }
```

Gives the subject a color of the tab palette, `0` to `5` (the same indices as `set-tab-color`). It is saved with the session and repaints the subject's box in `Cmd+P` and its band in the strip together. Required and never `null`: a subject always has a color.

Response: `{ "ok": true, "data": { "color": 4 } }`. Errors as in `rename-subject`, plus `"color" must be 0..5`.

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

**Action names** (kebab-case; the table is `action_from_ipc_name` in `src/keybindings.rs`):

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
next-attention                     (focus the next unread pane, else an idle Claude session; skips minimized panes)
history-back|history-forward       (walk the panes you visited, back then forward; skips minimized panes)
detach-tab  break-pane  merge-tab  merge-window
rename-tab  rename-pane            (open the inline rename editor)
open-recent-project  open-search  open-pane-switcher   (open an overlay)
open-unread-switcher               (the pane switcher, listing only panes with a bell, an unread completion or a question)
toggle-bookmark                    (bookmark the focused pane's conversation)
toggle-anchor  focus-anchor         (anchor the focused pane; jump to the first anchor)
copy  copy-raw  paste  toggle-filter
toggle-help  mem-report
```

Note: a few actions open an **interactive overlay** that then expects keyboard input — `merge-tab`, `merge-window`, `detach-tab` (when several windows exist), `rename-tab`, `rename-pane`, `open-recent-project`, `open-search`, `open-pane-switcher`, `open-unread-switcher`. For headless automation, prefer the deterministic typed commands where one exists (e.g. `merge-window` with explicit indices, `rename-pane` with a title).

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

### `set-tab-color` — color the tab holding a pane

```json
{ "cmd": "set-tab-color", "pane_id": 42, "color": 4 }
```

`color` is an index into the same six-color palette the tab bar's right-click menu offers:
`0` red, `1` orange, `2` yellow, `3` green, `4` blue, `5` violet. Pass `"color": null` to
clear it (tab falls back to the default background). Anything outside `0..5` is refused
rather than wrapped.

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

`cols` and `rows` are the pane's terminal size. A pane scrolled entirely off-screen (a tab wider than the window) keeps the size it had until it is on screen again: a window resize does not reach it, so it does not redraw for nothing.

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

Returns when the pane is marked completed — by the shell's **OSC 133;D** or by the IPC `mark-completed` command (Claude Code's `Stop` hook) — or when the deadline passes.

Response:

```json
{ "data": { "completed": true,  "pane_id": 42, "timed_out": false } }
{ "data": { "completed": false, "pane_id": 42, "timed_out": true  } }
{ "ok": false, "error": "pane 42 closed during wait" }
```

**Requires something to mark completion.** For a shell, that is shell integration emitting OSC 133 sequences; most modern prompt frameworks (Starship, Powerlevel10k, fig/atuin, vscode-shell-integration) do this automatically. For a Claude Code session, the `Stop` hook's `mark-completed`. With neither, this command always times out.

**Semantics — sticky flag.** Kova's `command_completed` flag is set on OSC 133;D or `mark-completed`, and stays set until the next command or turn starts: OSC 133;C, `mark-started`, or a `send-keys` whose text contains a newline or carriage return. Implications:

- If the wait arrives **after** the command already finished, it returns `completed: true` immediately.
- Calling `wait-for-completion` twice in a row without sending a new command in between returns `completed: true` both times. The flag isn't consumed by observation. The intended pattern is `send-keys` → `wait-for-completion`, never two waits without a send in between.
- Focusing the pane doesn't consume it either. The completion *dot* is acknowledged when you look at the pane, but that ack is a separate flag — a user switching panes can't make a pending `wait-for-completion` miss its event.

**Long timeouts.** The connection-thread timeout is automatically extended to `timeout_ms + 2s`, so you can ask for a long wait without the connection dying first.

---

### `subscribe` — stream state changes as they happen

```json
{ "cmd": "subscribe", "events": ["focus", "pane-status", "pane-working", "pane-open", "pane-close", "subjects"] }
```

Turns this connection into an event stream. `events` is optional — omit it to get
every topic. An unknown name is an error, not a silently ignored entry: a typo
would otherwise leave the client waiting forever for events that never come.

The **response is a snapshot** of the current state, so a subscriber never needs a
separate `list-panes` to bootstrap, and has no gap between "what is true now" and
"what changed since":

```json
{ "ok": true, "data": {
  "events": ["focus", "pane-status", "pane-working", "pane-open", "pane-close", "subjects"],
  "app_active": true,
  "focus": { ...pane object, or null... },
  "panes": [ ...same objects as `list-panes`... ],
  "subjects": [ ...same entries as `list-subjects`... ]
} }
```

Every line after that is one event, told apart from a response by its `event` key:

| `event` | Payload | Fires when |
|---|---|---|
| `focus` | `app_active`, `reason`, `pane` (a pane object, or `null`) | the pane holding the user's attention changed |
| `pane-status` | `pane_id`, `awaiting`, `awaiting_since` | a pane raised or retracted its `awaiting` flag |
| `pane-working` | `pane_id`, `working` | a pane started or stopped working |
| `pane-open` | `pane` | a pane appeared |
| `pane-close` | `pane_id`, `window`, `tab` | a pane went away |
| `subjects` | `subjects` (the whole `list-subjects` answer) | a subject was created, renamed, recolored, deleted or moved, or a pane was filed into or out of one — from Cmd+P, a split inheriting its subject, a closed pane or tab, or IPC |
| `ping` | — | 30 s of silence (see below) |

**`focus` folds "is Kova even frontmost" into the same stream.** Leaving Kova for
another app emits `focus` with `pane: null` and `reason: "app-inactive"`; coming
back emits the pane with `reason: "app-active"`. A client tracking where the
user's attention goes therefore doesn't have to join this stream with the system's
active-app notifications — "which pane is focused" and "you are actually looking
at it" are one fact here. `reason` is one of `pane`, `tab`, `window`, `session`,
`app-active`, `app-inactive`, `no-key-window`, and always names the *coarsest*
true hop: a pane change that is also a tab change reads `tab`.

`session` is the one that does not involve moving: the focused pane is the same,
but the Claude or Codex conversation in it changed — you launched an agent, it ended,
or its name changed through `/rename`, right where you already were. The conversation is deliberately part of the focused
pane's identity rather than just its payload, because a client keyed on
conversations would otherwise hear about a session started in place only when the
user next happened to leave the pane and come back.

Notes that matter when writing a client:

- **Redundant edges, never gaps.** The subscription is registered before the
  snapshot is built, so an event firing in between is delivered right after the
  snapshot instead of being lost. Every event carries absolute state, so applying
  one you already knew about changes nothing.
- **A `ping` every 30 s of silence.** It is how Kova notices a client that died
  without closing, and how a client notices Kova is gone — treat a long silence
  (say 90 s) as a dead link, reconnect, and re-subscribe.
- **Falling behind gets you disconnected.** Each subscriber has a bounded queue
  (256 events). A client that stops reading is dropped rather than served a hole:
  it has to reconnect, and the fresh snapshot resyncs it. This is what keeps a
  slow client from ever stalling Kova's render loop — the main thread only pushes
  to those queues, and the connection's own thread does the socket write.
- **The connection is one-way once subscribed.** Kova stops reading it. Open a
  second connection for commands.
- **`focus` is compared every frame; the pane sweep runs at ~4 Hz.** So
  `pane-status` / `pane-working` / `pane-open` / `pane-close` / `subjects` can lag a change by
  up to ~250 ms, while `focus` is reported on the next frame.
- **The socket name carries Kova's pid**, so a Kova restart moves it. A long-lived
  subscriber needs to re-glob `/tmp/kova-*.sock` and re-subscribe.
- **A library host serves it too**, over its panes in the socket's model (window 0,
  one tab per pane). Its `focus` is the pane whose view has the keyboard in the
  host's key window, and `app_active` is the host app being frontmost: `app-inactive`
  means the host went to the background, `no-key-window` that the host is in front
  but the keyboard is elsewhere in it (an editor next to the terminal), and moving
  between two panes reads `tab`. Its sweep runs every fps/4 calls of the host's
  poll, so the ~250 ms holds when the host polls at Kova's frame rate.

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

### Follow the user's attention

```bash
# One line per change; the first line is the snapshot.
printf '%s' '{"cmd":"subscribe","events":["focus"]}' | nc -U $KOVA_SOCKET \
  | jq -r 'if .ok then "snapshot: \(.data.focus.cwd // "-")" else "\(.reason): \(.pane.cwd // "-")" end'
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
- `subscribe` is the only command that keeps writing after its response. Events are produced on the main thread by diffing the previous state on the render tick — there is no single mutation site to hook, since focus moves from a dozen places and `working` / `awaiting` are derived rather than set. They are then handed to per-subscriber bounded queues; the socket write happens on the subscriber's own connection thread, so no client can slow the terminal down. When nobody is subscribed, the whole path costs one atomic load per tick.
- The socket file is removed both on graceful shutdown and on panic (via a guard); a stale socket from a previous crash is cleaned up at startup.
