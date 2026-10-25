---
name: kova
description: "Drive Kova, a native macOS terminal (Rust + Metal), from Claude Code: open a split or a tab and run a command in it, list panes and the Claude Code or Codex sessions they hold, read a pane (its whole conversation, not just the screen), send keys, wait for a command to finish, trigger any keyboard shortcut, follow focus changes, post a notification, ask the user a question on a card. Use when: 'open a pane', 'new tab', 'run X in another pane', 'split', 'list the panes', 'read pane N', 'merge pane N', 'what is running in my terminal', 'ask me when you need an answer', or when a task needs a terminal the user can see."
---

# Kova — a scriptable terminal

Kova is a macOS terminal with tabs and splits. Every running Kova listens on a Unix socket and
accepts one JSON command per line. This skill lives in the kova repo (`skill/`).

**If a `LOCAL.md` sits next to this file, read it first.** It carries what is specific to the
machine and its user: names, habits, working rules. It is excluded from git.
## Where to look — three levels


1. **This file**: the commands you need most. Enough to open a pane, run something in it and read
   what it printed.
2. **`docs/ipc.md` — the source of truth, read it as soon as you leave the trivial case.** It
   documents every command, every field, its default and its exact semantics. The list below is an
   excerpt: never conclude from it that a capability does not exist.
3. **The Rust code, when the doc is not enough.** Entry points:
   - `src/ipc.rs`, `fn allowed_fields`: the strict table `command → accepted fields`, the exact
     inventory of what the socket accepts.
   - `src/keybindings.rs`, `fn action_from_ipc_name`: the `kebab-case name → Action` table that
     `dispatch-action` uses, i.e. the real list of actions you can trigger.
   - `src/window/mod.rs`, `dispatch_action()`: what each action really does.
   When the socket cannot do something the user needs, the answer is not "impossible": read the
   code, say what would have to be added, and offer to write it.

## Talking to Kova

Inside a pane Kova started, two variables are set: `$KOVA_SOCKET` (`/tmp/kova-<pid>.sock`, the
socket of that Kova) and `$KOVA_PANE_ID` (the pane the shell runs in). Hook processes inherit them.

```bash
printf '%s\n' '{"cmd":"list-panes"}' | nc -U "$KOVA_SOCKET"
```

- Outside a Kova pane, `$KOVA_SOCKET` is empty. `ls /tmp/kova-*.sock` lists candidates, not live
  instances: a Kova that died leaves its socket file behind, and an app embedding Kova (a browser
  side panel, a notes app) has its own socket there too. A dead one answers nothing, so a loop
  over them gets an empty reply and a JSON parser fails on it: skip empty replies, or keep only
  the sockets whose `<pid>` is a running process. The name changes when Kova restarts.
- Every answer is `{"ok":true}` (with a `data` payload for the commands that return something) or
  `{"ok":false,"error":"…"}`. Pipe it into `jq`.
- Fields are validated strictly: a field a command does not accept fails the request
  (`unknown field "x" for command "y"`) instead of being ignored.
- Build JSON with `jq -n --arg` when it carries text (quotes, newlines), never by string
  concatenation.
- **The `kova` on the PATH is the app binary, not a control CLI.** Its only subcommand is
  `kova ask`; everything else goes through the socket. Older builds started a second Kova window
  for any other word, so never type `kova list-panes`.

## The commands

| Goal | Command |
|---|---|
| Split the focused pane | `{"cmd":"split","direction":"horizontal"}` (side by side, the default) or `"vertical"` (stacked) → `pane_id` |
| New tab | `{"cmd":"new-tab","cwd":"/abs/path","command":"…"}` → `tab_id`, `pane_id` |
| List panes / tabs | `{"cmd":"list-panes"}`, `{"cmd":"list-tabs"}` |
| Read a pane's screen | `{"cmd":"get-pane-content","panes":[42],"mode":"all"}` (the field is `panes`, a list or `"all"`, not `pane_id`) |
| Size before reading | `{"cmd":"count-pane-content","panes":"all","mode":"all"}` (responses have no cap) |
| Type into a pane | `{"cmd":"send-keys","pane_id":42,"text":"make\n"}` (end with `\n` or it is typed, not run) |
| Wait for the command | `{"cmd":"wait-for-completion","pane_id":42,"timeout_ms":300000}` |
| Focus / close a pane | `{"cmd":"focus-pane","pane_id":42}`, `{"cmd":"close-pane","pane_id":42}` |
| Resume a pane asleep since a relaunch, focus untouched | `{"cmd":"wake-pane","pane_id":42}` |
| Layout | `resize-pane`, `swap-pane`, `merge-tab`, `merge-window`, `close-tab` |
| Names and colors | `{"cmd":"rename-pane","pane_id":42,"title":"x"}` (the field is `title`, not `name`), `set-tab-title`, `set-tab-color` |
| Any keyboard shortcut | `{"cmd":"dispatch-action","action":"equalize"}` |
| Notify, the click focuses the pane | `{"cmd":"notify","pane_id":42,"title":"Claude Code","message":"done"}` |
| Follow changes live | `{"cmd":"subscribe","events":["focus","pane-working"]}` |
| Anchors | `set-anchor`, `clear-anchors` |
| Subjects (the boxes of `Cmd+P`) | `{"cmd":"list-subjects"}`, `{"cmd":"set-pane-subject","pane_id":42,"subject":"auth"}` (`null` takes it out), `rename-subject`, `delete-subject`, `set-subject-color` (`"color"` 0..5) (all by `tab_id` + `name`) |
| Reload the config | `{"cmd":"reload-config"}` |
| Hook signals | `set-pane-status`, `mark-completed`, `mark-started`, `bell` |

- **Prefer the typed commands**, which address a pane by id. `dispatch-action` is the skeleton key
  for everything else (`navigate-left`, `minimize-pane`, `repaint-pane`, `break-pane`…) and acts on
  the focused pane; some actions open an overlay that then waits for keyboard input (`rename-tab`,
  `open-search`, `merge-tab`, `open-pane-switcher`…).
- The `command` field of `split` depends on the shell; `split` then `send-keys` is more reliable.
- **`subscribe`** never gives the connection back: its answer is a snapshot (focus and every pane,
  the same objects as `list-panes`), then one JSON line per change (`focus`, `pane-status`,
  `pane-working`, `pane-open`, `pane-close`, `subjects` with the whole `list-subjects` list, and a
  `ping` after 30 s of silence). Use it to follow
  the user instead of polling `list-panes`. `events` filters the topics; an unknown name is an
  error. `focus` also says when Kova itself goes to the background (`pane: null`,
  `reason: "app-inactive"`). Open a second connection for commands. A slow client is dropped and
  resynchronizes from the next snapshot. `focus` is compared every frame, the other events come
  from a ~4 Hz sweep, so they can lag by about 250 ms.
- **`set-anchor`** puts a conversation at the head of the anchors (the section that opens `Cmd+P`
  and the target of `Cmd+A`): either a pane of this Kova (`pane_id`), or a named conversation
  (`cwd`, plus `session_id` and `agent` to reopen it, `label` for the row). It moves up without
  duplicating; the list keeps 8 and the last one falls off, its label returned in `dropped`.
  `clear-anchors` empties the list; Kova never empties it on its own.
- **`reload-config`** re-reads `~/.config/kova/config.toml` and applies its appearance half live
  (`[colors]`, `[status_bar]`, `[global_status_bar]`, `[tab_bar]`, `[bookmarks]`, the fade and
  outline of `[splits]`). Font, `[terminal]`, `[window]` and `[keys]` keep their startup values and
  come back in `needs_restart`.
- **`set-pane-status`** (`waiting` / `none`) sets the `awaiting` flag, pushed by Claude Code hooks
  (`Stop` and the permission prompt set `waiting`, `UserPromptSubmit` and `SessionEnd` set `none`).
  Kova drops it on its own when the session dies or goes back to work. `Stop` fires at the end of
  every turn, so an `awaiting` pane has not necessarily asked anything: it gave the hand back.
- **`mark-completed`** lights the pane's "finished" dot like an OSC 133;D and releases a
  `wait-for-completion`; `mark-started` clears it like an OSC 133;C; `bell` raises the bell flag
  like a BEL. Hooks use them instead of printing escapes. **Never write an escape or a BEL to the
  tty of a pane where an app runs** (`printf … > /dev/ttysNNN`): the bytes land in the middle of
  the app's own sequences and leave garbage on screen.
- **`notify`**: only `message` is required; `title` defaults to `"Kova"`, `sound` to `false`;
  without `pane_id` the click only brings Kova forward. Kova posts it itself because only Kova can
  act on the click (`terminal-notifier --execute` still uses an API macOS removed, so its command
  never runs).
- **From a pane of a library host** (an app embedding Kova), `KOVA_SOCKET` names that host: it
  answers the pane commands, `subscribe` (same events; one tab per pane, `focus` follows the
  pane that has the keyboard in the host) and `notify` (handed to the host, which posts it; an
  error when the host registered no handler). Window, layout and `ask` commands are Kova.app's.

## Reading `list-panes`

The answer is `{"ok": true, "data": [ … ]}`: `data` is a flat list of pane objects, not a dict.
To find pane N:
`printf '%s' '{"cmd":"list-panes"}' | nc -U "$KOVA_SOCKET" | python3 -c 'import json,sys; print(next(p for p in json.load(sys.stdin)["data"] if p["id"]==N))'`.

Each pane carries `id`, `window`, `tab`, `cwd`, `title`, `focused`, `pid`, `child_processes`,
`is_idle` (no child process: free for a new command), `minimized`, and:

- `working`: the app in the pane is generating or running a tool, read from the Braille spinner
  Claude Code puts in its terminal title.
- `awaiting`, `awaiting_since`, `awaiting_seen`: the pane declared it waits for the user (see
  `set-pane-status`), since when, and whether it has already been looked at on this Mac since. A
  client walking waiting panes must honor `awaiting_seen`, or it offers panes already read.
  `awaiting` shows nowhere in Kova's UI: it is an IPC field. What `Cmd+J`, the switcher's `Tab`
  and the `●N` counter walk is unread output (a bell, a finished command).
- `agent` (`"claude"`, `"codex"` or `null` for a bare shell), `agent_session_id` and
  `agent_session_name`: the conversation in the pane, whatever the agent. For Codex the name comes
  from `~/.codex/session_index.jsonl`. A Codex session resumes with `codex resume <id>` and is not
  reachable through `ListAgents` / `SendMessage`.
- `claude_session_id` (what `claude --resume` takes) and `claude_session_name` (the `/rename`
  name, `null` until there is one): Claude only, `null` on a Codex pane so that a client passing it
  to `claude --resume` never gets an id Claude cannot open. **This id is the only identifier that
  outlives the pane**: the pane id dies with its tab, and a directory is shared by every
  conversation of the same repo. Key on it.
- `peer_name`: **the `SendMessage` address** of the pane's Claude session, the name `ListAgents`
  shows. To talk to a pane's session, take its `peer_name` and pass it as is to `SendMessage`,
  never `send-keys` (typed text is not submitted the way a message is). It carries the name Claude
  Code derived from the directory even before any `/rename`, which `claude_session_name` does not.
  `null` for Codex and for a pane without Claude.
- `dormant_session_id`: **after a Kova relaunch, a restored agent resumes only once its pane is
  on screen in the active tab** (or on `wake-pane`). Until then the pane is a bare shell with this
  field set, and `agent`, the session ids and `peer_name` are `null`: the session is not in
  `ListAgents`, so `SendMessage` cannot reach it. To talk to it, `wake-pane` first, then wait for
  its `peer_name` in `list-panes`. Never `send-keys` into a sleeping pane: it cancels the resume
  for good and leaves a bare shell.

**Kova can restart under a running session, and pane ids do not survive it.** The user relaunches
it (after a build, for instance) while sessions run in its panes; the tabs come back, but every
pane gets a new id, numbered afresh at launch, and its agent sleeps until shown (see
`dormant_session_id`). A pane id or a `#N` the user named before the restart can then point to
another pane or to nothing. Never reuse one across a restart: list the panes
again and find the pane by its conversation, `claude_session_id` or, while it sleeps,
`dormant_session_id`.

The conversation name outranks the pane's custom title in `Cmd+P`, open-pane search, the IPC
`title` and automatic tab titles; an explicitly renamed tab keeps its title. A `/rename` in the
focused pane also emits a `focus` event with `reason: "session"`.

## Subjects

A subject is a named group of panes inside one tab, drawn in `Cmd+P` as a box under its name
(`Cmd+→` files the selected pane, `Cmd+←` takes it out), and on screen as a colored band naming it
over its panes, in a strip under the tab bar (only in a tab where a pane is filed: it costs a row). Each pane of `list-panes` carries
`subject` (its name, or `null`). `list-subjects` answers one entry per subject: `tab_id`,
`window`, `tab_index`, `name`, `color` (tab palette index, drawn at random at creation, then kept),
`pane_ids`, in the order `Cmd+P` draws the boxes (the user changes a color by right-clicking the band
or pressing `c` on the header in `Cmd+P`); an empty subject
has `pane_ids: []`. A subject belongs to its tab: the same name in two tabs is two subjects, so
`rename-subject` and `delete-subject` take the `tab_id` from `list-subjects`. `set-pane-subject`
creates the subject in the pane's tab when the name is new (`created: true`). `delete-subject`
asks nothing, unlike `Cmd+⌫` in `Cmd+P`: the panes stay open, out of any box. To follow changes,
`subscribe` to `subjects`: each event carries the whole list again.

## Anchors and bookmarks, as the user sees them

- `Cmd+Shift+A` anchors the focused pane's conversation (`~/.config/kova/anchors.json`, 8 at
  most); `Cmd+A` goes to the first anchor, to the pane holding it or by reopening the conversation.
  `Cmd+P` opens on an "Anchors" section listing them whether or not a pane holds them. A
  conversation both anchored and bookmarked shows only under the anchors.
- `Cmd+B` bookmarks the focused pane's conversation (`~/.config/kova/bookmarks.json`); `Cmd+P`
  lists bookmarks at the end, grouped by directory. Enter jumps to the pane if it still runs,
  otherwise reopens the conversation in its project and runs its resume line. `Cmd+⌫` on a
  bookmark row removes it after a confirmation. Kova does not check that the transcript still
  exists: Claude Code deletes transcripts idle longer than `cleanupPeriodDays`, and such a bookmark
  reopens on an error.
- Inside `Cmd+P`, `Cmd+Shift+A` and `Cmd+B` toggle the selected row without an alert.

## "Read pane N" = read its whole conversation, not its screen

When the user works in Claude Code sessions, a pane is a conversation, and "read pane N",
"what is going on in that pane", "pick up that thread" mean **the whole conversation**, not the few
lines still on screen.

**`get-pane-content` cannot do it, and that is not a Kova bug.** Claude Code runs in the alternate
screen (`\x1b[?1049h`). There, a line scrolling off the screen is never pushed to the scrollback:
`src/terminal/mod.rs` only does it `if top == 0 && !self.in_alt_screen`, and `build_text` knows
`scrollback` and `grid`, never `alt_grid`. A `mode: "all"` that returns 3,000 characters for an
hour-long conversation is the screen, not a full dump.

The recipe:

```bash
# 1. pane → session
printf '%s\n' '{"cmd":"list-panes"}' | nc -U "$KOVA_SOCKET" |
  jq -r '.data[] | select(.id==42) | "\(.claude_session_id) \(.cwd) \(.claude_config_dir)"'
# 2. transcript: ~/.claude/projects/<dir>/<session_id>.jsonl (<claude_config_dir>/projects/… when
#    that field is not null: a session started with its own CLAUDE_CONFIG_DIR), where <dir> is the directory the
#    conversation started in with every non-alphanumeric character turned into "-"
#    (/Users/me/projects/app → -Users-me-projects-app). Usually the pane's cwd; if the file is
#    missing, look for <session_id>.jsonl under ~/.claude/projects/.
# 3. read the turns: the "text" blocks of user and assistant messages. The body of an invoked
#    skill arrives as a user message and weighs a lot: skip it.
```

**"Which open sessions talk about X"** = one call, not a hand-written loop over the transcripts:
`python3 ~/projects/perso/kova/scripts/grep-sessions.py '<regex>'`. It searches only what was
written (user prompts, assistant text), skips task notifications, cross-session messages,
compaction summaries and skill bodies (they repeat the same words in every session), skips the
calling session, and prints per pane the user hits plus the last assistant hits. `--json` for
the raw list, `--all` for every assistant hit.

`get-pane-content` stays the right answer for an ordinary shell pane (a running command, an output
to reread) and for the last screen of a Claude pane when what you want is precisely what is shown
right now (a question waiting on screen).

## "Merge pane N" = bring its conversation here, so it can be closed

1. Read the whole conversation (recipe above). Never the screen. A long session easily weighs
   200 KB of text once the turns are extracted, far above what one `Read` returns (about 25k
   tokens): extract the turns into a file, check its size (`wc -c`, `wc -l`), list the turn
   headers with their line numbers, then read it in slices of a few hundred lines. Skipping
   `tool_result` blocks and skill bodies keeps the file small; agent results arriving as
   `<task-notification>` user messages are part of the conversation and are read too.
2. Report here in short prose: what was said and decided, new facts (in the user's words when they
   matter), drafted texts, what is still open. Not a turn-by-turn summary.
   ⚠️ **The transcript only says what that session saw, not the state of the world now.** Anything
   outside the conversation (a message sent or not, a task created or not, a PR merged, a file
   committed) is re-read at the moment you report it, and the command you ran is cited in the
   same sentence; otherwise the sentence says "not verified". A pane's "proposed, not created" or
   "drafted, not sent" may have changed since, in another session or by the user's own hand.
3. Say in one line what is written nowhere else: once the pane is closed, this conversation is the
   only one that carries it. That line is a state claim too: check it (git status of the files the
   session wrote, the task list, the channel) before writing it.
4. Never close the pane yourself (`close-pane` only if asked), and never write into that pane's
   session.

## Asking the user — `kova ask`

```bash
kova ask "Deploy to staging?" --actions OK,KO      # Bash tool: timeout 600000
```

A card shows the question in the corner of the screen and the answer comes back on stdout: the
action, or `action: text` when the user typed something.

- Exit 0: answered. Exit 2: no answer within the timeout (540 s by default inside a Claude Code
  conversation); **end your turn**: Kova delivers the late answer to the conversation itself, typed
  into its pane, after a routine still running there, or by reopening it in a new tab with
  `claude --resume`. It arrives as a message starting with `[Kova] The user answered your question`.
- Exit 3: the user answered in the conversation instead, and the card was withdrawn (a
  `UserPromptSubmit` hook runs `kova ask --withdraw --session -`). Their message is the answer:
  end your turn.
- Exit 1: an error, or Kova quit while you waited (the question comes back at the next launch). If
  the message says the running Kova "predates `ask`", it learns the command at its next launch.
- **Offered choices end with an "Other" button** (`--actions "Retry,Call,Other"`), never two closed
  options alone: the card has a text field, but without a button announcing it the card reads as a
  forced choice. A yes/no validation (`OK,KO`) is the exception.
- **Break a long question into short lines**: one idea per line, a blank line between blocks, a
  list as one item per line. The card shows line breaks as they are, but only real ones: inside
  `"…"` a `\n` stays a backslash and an `n` on the card. Put actual line breaks inside the quotes,
  or use `$'Sort done.\n\nArchive 8: …\nKeep 15: …\n\nGo?'`.
- `--no-input` (buttons only), `--from <name>` (name on the card), `--json`, `--socket <path>`.
  Outside a Kova pane the client finds Kova.app's socket on its own; from a pane of a library host
  (an app embedding Kova), it falls back to Kova.app too.
- Pending questions can also be listed and answered over the socket, for a remote client:
  `{"cmd":"list-asks"}` returns `data.asks` (each question with its `buttons`), and
  `{"cmd":"answer-ask","id":"<uuid>","action":"OK","text":"…"}` answers exactly like a click. **A
  session never answers in the user's place**: these two are for the user's other devices, not for
  an agent.
- **The card is visible to anyone looking at the screen.** Never put private content on it
  (health, intimate matters, other people's affairs, secrets). For those, ask in the text of your
  reply, inside the pane, which the user can hide.
- Protocol: `docs/ipc.md` § `ask`, `list-asks`, `answer-ask`; code: `src/ask.rs` (logic, delivery,
  client) and `src/ask_card.rs` (the card).

## Traps and rules of conduct

- `wait-for-completion` needs something that marks the end: shell integration emitting OSC 133 for
  a shell (Starship, Powerlevel10k and most prompt frameworks do), the `Stop` hook's
  `mark-completed` for a Claude session; without it, it always times out. The flag stays set until
  the next start (OSC 133;C, `mark-started`, or a `send-keys` containing a newline), so two waits
  in a row without a `send-keys` both return `completed: true`. The right pattern is `send-keys` →
  `wait-for-completion`, and it works on a Claude pane too.
- Kova's socket uses the key `cmd`; do not mix it up with other apps' sockets that use `command`.
- **Do not steal focus without being asked.** Creating a pane, focusing one, or dispatching an
  action interrupts whatever the user is typing. Read state freely; act on the layout when asked.
- **Never type into a live Claude session to test something**: it injects text into a real
  conversation. Create a dedicated pane. More generally, do not close or type into a pane you did
  not create without the user's say.
- A command that starts a long-lived process (dev server, watcher) in a pane is still a process the
  user has to know about: say what you launched and where.
