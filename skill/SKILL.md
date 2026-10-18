---
name: kova
description: "Drive Kova, a native macOS terminal (Rust + Metal), from Claude Code: open a split or a tab and run a command in it, list panes and the Claude Code sessions they hold, read what a pane shows, send keys, wait for a command to finish, trigger any keyboard shortcut, post a notification, ask the user a question on a card. Use when: 'open a pane', 'new tab', 'run X in another pane', 'split', 'list the panes', 'read pane N', 'what is running in my terminal', 'ask me when you need an answer', or when a task needs a terminal the user can see."
---

# Kova — a scriptable terminal

Kova is a macOS terminal with tabs and splits. Every running Kova listens on a Unix socket and
accepts one JSON command per line. This skill lives in the kova repo (`skill/`); the full protocol
is `docs/ipc.md` in the same repo, and it is the source of truth when this file and it disagree.

**If a `LOCAL.md` sits next to this file, read it first.** It carries what is specific to the
machine and its user: names, habits, working rules. It is excluded from git.

## Talking to Kova

Inside a pane Kova started, two variables are set: `$KOVA_SOCKET` (the socket of that Kova) and
`$KOVA_PANE_ID` (the pane the shell runs in). Hook processes inherit them.

```bash
printf '%s\n' '{"cmd":"list-panes"}' | nc -U "$KOVA_SOCKET"
```

- Outside a Kova pane, `$KOVA_SOCKET` is empty. The socket is `/tmp/kova-<pid>.sock`; with one
  Kova running, `ls /tmp/kova-*.sock` finds it. The name changes when Kova restarts.
- Every answer is `{"ok":true}` (with a `data` payload for the commands that return something) or
  `{"ok":false,"error":"…"}`. Pipe it into `jq`.
- Fields are validated strictly: a field a command does not accept is an error, not ignored.
- Build JSON with `jq -n --arg` when it carries text (quotes, newlines), never by string
  concatenation.

## What you will use most

| Goal | Command |
|---|---|
| Split the focused pane and run something | `{"cmd":"split","direction":"vertical","command":"npm test","cwd":"/abs/path"}` → `pane_id` |
| New tab | `{"cmd":"new-tab","cwd":"/abs/path","command":"…"}` → `tab_id`, `pane_id` |
| List panes | `{"cmd":"list-panes"}` |
| Read a pane | `{"cmd":"get-pane-content","panes":[42],"mode":"all"}` |
| Size before reading | `{"cmd":"count-pane-content","panes":"all","mode":"all"}` |
| Type into a pane | `{"cmd":"send-keys","pane_id":42,"text":"make\n"}` |
| Wait for the command | `{"cmd":"wait-for-completion","pane_id":42,"timeout_ms":300000}` |
| Focus a pane | `{"cmd":"focus-pane","pane_id":42}` |
| Close a pane | `{"cmd":"close-pane","pane_id":42}` |
| Name a pane or a tab | `rename-pane`, `set-tab-title`, `set-tab-color` |
| Any keyboard shortcut | `{"cmd":"dispatch-action","action":"next-tab"}` (names in `docs/ipc.md`) |
| Notify, click focuses the pane | `{"cmd":"notify","pane_id":42,"message":"done"}` |

Prefer the typed commands, which address a pane by id. `dispatch-action` acts on the focused pane,
and some actions open an overlay that waits for keyboard input.

### Reading `list-panes`

Each pane carries `id`, `window`, `tab`, `cwd`, `title`, `focused`, `is_idle` (no child process:
free for a new command), `working` (a Claude Code session is generating), `minimized`, and for an
agent pane `agent` (`claude` or `codex`) with `agent_session_id` and `agent_session_name`. A Claude
pane also carries `claude_session_id`, `claude_session_name` (`null` until `/rename`) and
`peer_name`; those stay `null` for Codex.

- **To find a conversation, key on `claude_session_id`**, not on the pane id or the directory: a
  pane id dies with its tab, and several conversations share a directory.
- `peer_name` is the name `ListAgents` / `SendMessage` use. To talk to the Claude Code session in
  a pane, use `SendMessage` with that name, never `send-keys`: typed text is not submitted the way
  a message is.
- "Read pane N" in a user's mouth usually means the whole conversation, not the screen. With
  `claude_session_id` and the pane's `cwd`, the transcript is
  `~/.claude/projects/<dir>/<id>.jsonl`, where `<dir>` is the directory the conversation started in
  with every non-alphanumeric character turned into `-` (usually the pane's `cwd`, not always: if
  the file is missing, look for `<id>.jsonl` under `~/.claude/projects/`).
  `get-pane-content` only gives what the terminal rendered.

### Running a command and reading its output

```bash
S=$KOVA_SOCKET; P=42
jq -nc --argjson p $P --arg t $'make build\n' '{cmd:"send-keys",pane_id:$p,text:$t}' | nc -U $S
jq -nc --argjson p $P '{cmd:"wait-for-completion",pane_id:$p,timeout_ms:300000}' | nc -U $S
jq -nc --argjson p $P '{cmd:"get-pane-content",panes:[$p],mode:"all"}' | nc -U $S | jq -r '.data.panes[0].text'
```

`wait-for-completion` needs something that marks completion: shell integration emitting OSC 133
(Starship, Powerlevel10k and most prompt frameworks do), or a hook sending `mark-completed`.
Without it the wait always times out. The completed flag stays set until the next command, so
always send, then wait; never wait twice in a row.

## Asking the user — `kova ask`

```bash
kova ask "Deploy to staging?" --actions OK,KO      # Bash tool: timeout 600000
```

A card shows the question in the corner of the screen and the answer comes back on stdout: the
action, or `action: text` when they typed something. Exit 0: answered. Exit 2: no answer within
the timeout (540 s by default inside a Claude Code conversation); **end your turn**,
because Kova delivers the late answer to the conversation itself (typed into its pane, or by
reopening it with `claude --resume`). Exit 3: the user answered in the conversation instead, and
the card was withdrawn; their message is the answer, end your turn. Exit 1: error, or Kova quit. `kova ask --help` has the
options.

**The card is visible to anyone looking at the screen.** Never put private content on it
(health, intimate matters, other people's affairs, secrets). For those, ask in the text of your
reply, inside the pane, which the user can hide.

## Rules of conduct

- **Do not steal focus without being asked.** Creating a pane, focusing one, or dispatching an
  action interrupts whatever the user is typing. Read state freely; act on the layout when the
  user asked for it.
- **Do not close or type into a pane you did not create** without the user's say: another agent
  or a running job may live there.
- A command that starts a long-lived process (dev server, watcher) in a pane is still a process
  the user has to know about: say what you launched and where.
