# Decision cards — spec

Status: implemented 2026-11-19 (Kova: `src/decision_card.rs`, `src/ask.rs`, `src/ask_card.rs`; the outbound gate and its sender in the dotfiles repo). Session-id checks of § 8.4 done. Pending: the visual check on a test instance. Written 2026-11-19.

## 1. Why

Agents already reach the user through `kova ask` (`docs/ipc.md` § `ask`, `src/ask.rs`,
`src/ask_card.rs`): one line of plain text, a few buttons, an optional text field. That is enough
for "OK / KO", not for the decisions the user's scheduled agents bring every day. Example: the
"Chief of Staff" agent presents a Slack draft. To decide, the user needs who asked, the thread
text, the draft itself, and buttons that *do* something ("Post", "Clear draft"). Today the agent
squeezes all of that into one line, and when the user needs more, he answers "Other: open a pane"
and the conversation moves to a throwaway session.

Decision cards make the card a small structured document, described by the agent in JSON and
drawn natively by Kova, with named actions. A click goes back to the asking session through the
existing `ask` delivery path (including late delivery to a closed or sleeping conversation).

## 2. Scope

**v1 (this spec):**

- a card document: a list of typed blocks (§ 4) and a list of actions (§ 5);
- rendered in the existing card panel (`src/ask_card.rs`, an AppKit `NSPanel` in the top-right
  corner), which grows to fit a rich card and scrolls inside;
- a queue the user can browse without answering (§ 6);
- the asker can replace or withdraw its own card while it waits (§ 7);
- **grant actions**: a button that authorizes one precise outbound action, such as posting
  exactly the draft shown on the card (§ 8).

**Not in v1 (planned v2, same document format):** interactive views living *inside a pane*
(dashboards, forms, charts that an agent keeps updating). A pane is a Metal-drawn terminal grid
today; hosting a native view there is a separate piece of work. The card panel is already AppKit,
so v1 needs no new rendering surface.

**Non-goals:** HTML, web views, scripts in cards, images loaded from the network.

## 3. Compatibility

- A question without a `card` field behaves exactly as today. Nothing changes for existing
  callers of `kova ask "<message>" --actions …`.
- `card` is a new optional field of `Question`. `message` stays mandatory and holds a plain-text
  fallback. When the caller gives none (CLI or socket, `ask` or `update-ask`), Kova derives it: the `title`, followed by ` — ` and the
first 200 chars of the first `text` block if there is one, newlines turned into spaces. On
`--update`, a missing `message` is derived again from the new card. The 1,000-char limit below
applies only to a `message` given by the caller; a derived one is at most ~325 chars anyway.
  It is what an older Kova shows, and what `list-asks` and the late-delivery line quote.
- Answers keep their shape (`id`, `action`, `text`) plus `card_version` (§ 7), present in every
  answer, plain questions included. A question saved on disk by an older Kova (no `card_version`)
  loads with `card_version: 1` (serde default).
- `list-asks` returns each question whole, `card` and `card_version` included.
- For an older Kova, the legacy fields are filled from the card: `actions` = the ids of the card's
  actions **except** the grant action, `input` = whether the card has `input`. An older Kova thus
  shows usable buttons that answer with the same ids, and can never answer a grant action. A card
  must therefore have at least one non-grant action (`a card needs a non-grant action`). Those
  buttons show the ids as their labels (the card's labels are lost there; acceptable for a
  fallback).
- When a `card` is present, a `message` given by the caller is limited to 1,000 chars
  (`message too long`). Plain questions keep today's behaviour.
- SHA-256 (§ 8) comes from the `sha2` crate, added for this: small, pure Rust, no runtime cost
  outside a grant check.
- `kova/CLAUDE.md` § Principles allows `kova ask` as an extension of the app. Decision cards stay
  inside that extension. The v2 pane surface will need that principle rewritten first.

## 4. The card document

```json
{
  "version": 1,
  "title": "Slack draft — reply to Alex in #infra",
  "subtitle": "Prepared by the drafts agent, 19/11 15:34",
  "blocks": [ … ],
  "actions": [ … ],
  "input": { "placeholder": "Change the draft or say what to do" }
}
```

| Field | Type | Required | Meaning |
|---|---|---|---|
| `version` | integer | yes | Format version. v1 = `1`. Kova refuses a version it does not know (error, § 9). |
| `title` | string, ≤ 120 chars | yes | Bold first line. |
| `subtitle` | string, ≤ 200 chars | no | Grey line under the title (origin, date). |
| `blocks` | array, 1 to 20 items | yes | Content, in order (§ 4.1). |
| `actions` | array, 1 to 6 items | yes | Buttons, in order (§ 5). |
| `input` | object or absent | no | Shows a multi-line text field. `placeholder`: string. Absent = no field. |

Whole document ≤ 64 KiB, measured on the bytes of the `card` value as received on the socket (the
CLI parses the file and sends it re-serialized compact, `serde_json::to_string`).
Every "chars" limit in this spec counts Unicode scalar values (Rust `str::chars().count()`), not
bytes. Strings are UTF-8; control characters other than `\n` and `\t` are rejected (`\r` included,
so a payload with CRLF line endings cannot be granted: the agent writes LF only).

Inline markup (§ 4.1) applies to `text` blocks and `fields` values only. `title`, `subtitle`,
`quote`, `code`, `context`, `fields` keys and button labels are plain text, shown as is.

### 4.1 Blocks

Every block is an object with a `type`. Unknown types are rejected (not skipped): an agent must
know its card shows what it sent.

| `type` | Fields | Rendering |
|---|---|---|
| `text` | `text` (≤ 4,000 chars) | Body text. Inline markup: `**bold**`, `_italic_`, `` `code` ``, `[label](https://…)`. Links open in the default browser; only `https:` and `slack:` schemes are clickable, others render as plain text. |
| `quote` | `text` (≤ 4,000), `author` (optional, ≤ 80), `time` (optional, ≤ 40, string as given) | Indented, with a vertical bar on the left; `author · time` above in grey. For a message being answered. |
| `code` | `text` (≤ 8,000), `label` (optional, ≤ 40) | Monospace, framed. `label` sits above in grey (e.g. "Draft"), followed by `· editable` on the `kova ask` card. For exactly what will be sent. No syntax colouring. On the `kova ask` card the user can **edit** it (plain text: no smart quotes, dashes or replacements); see § 4.2. In a pane view (`kova ui`) it stays read-only. |
| `fields` | `items`: array of `{ "key": string ≤ 40, "value": string ≤ 300 }`, 1 to 12 | Two columns, key in grey, value with the same inline markup as `text`. |
| `divider` | none | Thin horizontal line. |
| `context` | `text` (≤ 300) | Small grey line (source, "relaxed rules", ids). |

### 4.2 Edited `code` blocks travel with the answer

A `code` block is what the agent will send, so the user corrects it in place rather than asking for
a rewrite. Whatever button answers the card (a click, ⌘Return, `answer-ask`), Kova writes the final
text of **every** `code` block, edited or not, byte for byte, to
`<ask dir>/answers/<question id>/block-<index>.txt` (directory 0700, file 0600, written before any
grant and before delivery; a write failure keeps the card with a red line, like a grant failure),
and the answer carries one entry per block:

```json
"blocks": [{ "block": 2, "label": "Draft", "file": "/Users/…/answers/<id>/block-2.txt",
             "sha256": "…", "edited": true }]
```

The one-line answer (stdout, late delivery) appends
`[block <i> "<label>" edited by the user|unchanged: <file> sha256 <hex>]` per block. The agent acts
on that file, never on its own copy, so the user's version is always the one sent. Edits survive a
redraw and browsing, and are dropped when the asker sends a new card version. `answer-ask` carries
no edits: its blocks are the card's text. Answer folders are deleted after 7 days.

Inline markup is parsed by a small hand-written, non-recursive parser (no markdown dependency):

- `` `code` `` first: code spans are found before anything else, everywhere, and render in
  monospace even inside `**…**` or `_…_` (the only exception to "no nesting" below); their content
  is literal, nothing inside is parsed;
- `[label](url)`: the label is plain text (no markup inside it). The URL ends at the first `)`;
  parentheses inside a URL must be written `%28` / `%29`. A link whose scheme is neither `https:`
  nor `slack:` is shown as plain text `label (url)`, not clickable;
- `**bold**` and `_italic_`, not nested in each other. `_` opens italic only at the start of the
  text or after a space or punctuation, and closes only before the end, a space or punctuation, so
  `snake_case_name` stays plain. "Punctuation" = ASCII punctuation except `_` and `*`; non-ASCII characters are not punctuation. No nesting:
  the first marker opened wins, and markers inside it print as is (a link inside `**…**` shows its
  brackets literally);
- `\*`, `\_`, `` \` ``, `\[` and `\\` print the character itself;
- an opening marker without its closing marker prints as is;
- hovering a link shows its full URL in a tooltip, so a label cannot hide where it leads.

Layout: card width 560 pt (the plain card keeps its current width). Height grows with content up to
70 % of the visible height of the screen the panel is on (the panel keeps today's placement rule); beyond that the block area scrolls and the title and buttons
stay visible. The text field, when there is one, is pinned at the bottom with the buttons and does
not scroll with the blocks. The panel can already become key (`src/ask_card.rs`,
`can_become_key_window`), which the text field and ⌘[ / ⌘] need. Light and dark appearance follow
the system, like the current card.

## 5. Actions

```json
{ "id": "post", "label": "Post", "style": "primary", "grant": { … } }
```

| Field | Type | Required | Meaning |
|---|---|---|---|
| `id` | string `[a-z0-9_-]{1,32}`, unique in the card | yes | What the answer carries. |
| `label` | string ≤ 24 chars | yes | Button text. |
| `style` | `primary` \| `default` \| `destructive` | no, `default` | `primary`: the default button (Enter), at most one per card. `destructive`: red text. |
| `needs_input` | boolean | no, `false` | The button stays disabled while the text field is empty. Requires `input`. |
| `grant` | object | no | Makes this a grant action (§ 8). |

Keyboard: in the text field, Return inserts a new line. ⌘Return triggers the `primary` button
from anywhere in the panel; Return does too when the field does not have focus. A disabled button
(a `needs_input` one with an empty field) ignores both. Without a `primary` action, no key answers.

The text field's content is kept per question: browsing away and back, or an `update-ask` redraw,
keeps what the user typed. It is dropped only when the question leaves the board.

Kova enforces the same rules for an answer that comes through `answer-ask`: an action id that is not
in the card is refused (`no action <id> in this card`), and a `needs_input` action with empty text
is refused (`<id> needs text`).

The text field is shared by all buttons. Every answer carries the field's content, trimmed, whatever
the button (empty if the user typed nothing): `needs_input` only disables a button while the field is
empty.

Kova adds no "Other" button to a card (unlike plain questions): the agent lists the actions it
wants, and adds a `needs_input` action if a free answer makes sense. The "Dismiss" button of a
superseded question (`docs/ipc.md` § `withdraw-asks`) still applies, unchanged: it is added after
the card's actions (not counted in the 6), and closes the card without delivering anything.

The answer sent back is `{ "id": <question id>, "action": <action id>, "text": <field content,
trimmed>, "card_version": <n> }`. The late-delivery line typed into the asker's conversation
becomes:

```
[Kova] The user answered your card "<title>" (v<card_version>): <action id>[: <text>]
```

## 6. Queue

The panel still shows one card at a time, oldest first. A rich or plain card shows `n / N` in its
header when other questions wait (N > 1; nothing when it is alone), with `‹` and `›` buttons (and
⌘[ / ⌘] while the panel is key) to browse. ⌘[ and ⌘] are free in Kova (the tab shortcuts are
⌘⇧[ / ⌘⇧], `src/config.rs`), and terminal shortcuts do not apply while the panel is key. Browsing answers nothing. The panel follows the question **id** it shows, not its rank: if
an older question is answered or withdrawn while the user looks at another one, the displayed card
stays and only `n / N` changes. When the shown card is answered or withdrawn, the panel shows the
next one in order (or the previous one if it was the last). `N` and the Dock badge count the same
set: questions on the board, not yet answered (an answer waiting for late delivery no longer
counts).

A question arriving while the user browses behaves as today (the panel's current appearance rule
is unchanged) except that the panel keeps showing the card the user is on; only `N` grows. The Dock
badge already exists (`docs/ipc.md` § `ask`).

The counter and the arrows apply to plain questions too. That is a visible change to the existing
card, intended and unconditional (no flag), to be written in `docs/ipc.md` § `ask` along with the
rest of this spec.

## 7. Updating and withdrawing a card

New socket command:

```json
{ "cmd": "update-ask", "id": "<question id>", "session_id": "<asker session>", "card": { … }, "message": "…" }
```

- Allowed only if `session_id` equals the question's `from.session_id`. Otherwise: error
  `not the asker of this question`.
- `card` is mandatory (`missing card`): an update cannot turn a card back into a plain question.
- Checks run in this order, first failure returned: the caller has a session id (`updating requires
  an asker session id`), the question is on the board (`no such question`), the caller's session id
  equals the question's `from.session_id` (`not the asker of this question`; a question whose asker
  had no session id never matches), then the card's validation (§ 9).
- The board lives on the main thread (`App.asks`, a `RefCell<Board>` in `src/app.rs`);
  `update-ask` reaches it the same way `answer-ask` does.
- Requires an asker with a `session_id` (it is the only proof of who asked): a question whose
  asker has none cannot be updated (`updating requires an asker session id`).
- Clicks and `update-ask` are both handled on the main thread, one after the other, so there is no
  race to lock: a click records the `card_version` displayed when Kova handled it. If the update
  was handled first, the user clicked the new version; if the click was first, the question is
  answered and the update gets `no such question`.
- Works only on a question still on the board. A question already answered (even if its answer
  still waits for a late delivery) or withdrawn answers `no such question`.
- Every question has a `card_version`, 1 at creation, plain ones included. `update-ask` may turn a
  plain question into a card. The new card is validated like a new one (§ 9), so it may add or
  remove a grant action.
- Replaces `card` (and `message`, given or derived) and increments `card_version`. The question keeps its place in the queue and its id; the waiting `kova ask` client keeps
  waiting.
- If the panel is showing that card, it redraws in place. A click made on a version older than the
  current one is not lost: the answer carries the `card_version` the user saw, and the agent
  decides what to do with it.
- A grant created by a click is tied to the version clicked (§ 8), so updating a card can never
  turn an old click into a grant for new content.

CLI: `kova ask --update <question id> --card <file|->`. Withdrawing uses the existing
`withdraw-asks` (all of a session's questions) or the new `kova ask --withdraw-id <question id>`
(one question, same asker check; socket form `{"cmd":"withdraw-ask","id":…,"session_id":…}`).
`--update` and `--withdraw-id` return at once: no output and exit 0 on success, the error on
stderr and exit 1 otherwise; `--timeout` or `--from` with either is an error. Both take the session id from
the environment exactly as `kova ask` does (`Asker::from_env`), and
fail with `kova ask: no session id in the environment` when there is none.

**What the agent does with an answer to an older version.** The answer carries the `card_version`
current when Kova handled the click. Inside Kova, the shown card is always the current version
(clicks and updates are serialized on the main thread, § 7 above), so Kova never keeps old versions
and a grant always hashes the displayed card. An "older version" exists only from the agent's point
of view: its `update-ask` arrived after the click and got `no such question`. In that case the
answer's `card_version` is lower than the last version the agent tried to send, and the agent must
not act on it: it asks again with the current card (a new `ask`) and ignores the answer. A grant
created by that click covers only the old content, stays unused and expires (§ 8.4). An answer
given through `answer-ask` carries the current `card_version`.

## 8. Grant actions

### 8.1 What they are for

Some actions reach a person: posting a Slack message, sending an email. The user's machine refuses
those unless the user typed a passphrase in his own prompt; an agent cannot submit a prompt, so it
cannot authorize itself. A grant action lets one click authorize **one** such action, for **exactly
the content shown on the card**, without typing the passphrase.

### 8.2 Declaration

```json
"grant": {
  "kind": "outbound",
  "target": "slack:C0123ABCDEF/1790000000.000100",
  "payload_sha256": "9f2c…",
  "payload_block": 2
}
```

| Field | Meaning |
|---|---|
| `kind` | Only `outbound` in v1. |
| `target` | Free string, ≤ 200 chars, copied into the grant. For Slack: `slack:<channel>/<thread_ts or ->`. The card shows it in a small grey line `Authorizes: <target>` just above the buttons. One click is enough, there is no second confirmation. |
| `payload_sha256` | Lowercase hex SHA-256 of the exact bytes that will be sent. |
| `payload_block` | 0-based index in `blocks` of the `code` block that shows that payload. Kova checks that the SHA-256 of `blocks[payload_block].text`, taken as the UTF-8 bytes of the decoded JSON string with **no normalization at all** (no trimming, no line-ending or Unicode normalization), equals `payload_sha256`, and refuses the card otherwise (§ 9). The user therefore always sees the bytes he authorizes. If he edits that block (§ 4.2), the grant written on click carries the SHA-256 of **his** text, which is the block's answer file: the agent sends that file. |

A card has at most one grant action. The payload is bounded by the `code` block limit (8,000 chars):
a longer message cannot be granted and goes through the passphrase as today.

**Agent side.** The agent writes the payload to a file first, then builds the `code` block from that
file's bytes as is (a final newline included) and hashes that same file. It then sends that file,
unchanged. Any byte of difference (CRLF, a trailing newline added by an editor) makes the gate
refuse the send.

A grant action requires an asker with a `session_id`: a card with a grant action from an asker
without one is refused (`grant requires an asker session id`), because a grant is bound to a
session. It also requires `from.agent` to be Claude Code: the outbound gate is a Claude Code hook,
and nothing reads a grant in omp, Pi or Codex. A grant card from another agent is refused
(`grant requires a Claude Code asker`). A scheduled job that wants to offer "Post" must therefore
run through `claude -p` (a `claude/…` model in its job), not omp. The Chief of Staff and Slack
drafts jobs already do (`claude/opus`, read from MacTools `agents-state` on 2026-11-19), so the first
client needs no migration.

`from.agent` is not declared by the caller (`--from` only sets the display name): `Asker::from_env`
derives it from the environment, `CLAUDE_CODE_SESSION_ID` first, then omp/Pi's variables
(`src/ask.rs`, `agent_and_session`). A local process could fake that environment; that is inside
the threat model of § 8.4 (mistakes, not hostility).

### 8.3 What a click does

Only a real click, or ⌘Return when the grant action is the primary button, creates a grant. A plain
Return never triggers a grant action, even when it is the primary button and the field does not
have focus: a stray key after browsing must not send anything. Return then does nothing (no beep),
and the grant button shows a small `⌘↩` hint after its label so the user knows the shortcut. If the
grant is written but the answer's delivery fails later, the grant stays valid until it expires:
that is intended, the click happened. If the grant file cannot be written (disk, permissions), the answer is **not**
delivered: the card stays, with a red line `Could not record the authorization: <error>`, and the
user can try again; browsing (‹ ›) and Dismiss, when present, stay active. The grant is written
whatever happens to the answer next (delivered at once, delivered late, or waiting on disk): the
gate's validity window (§ 8.4) is long enough to cover late delivery. Withdrawing a card that was
never clicked leaves nothing to clean up, since no grant exists before the click. Kova writes
`<ask dir>/grants/<question id>.json` **before** delivering the answer. `<ask dir>` is
`$KOVA_ASK_DIR` when set, else `~/.config/kova/ask` (the directory that already holds questions), so
with `KOVA_ASK_DIR=X` the grants live in `X/grants/`. Directory 0700, file 0600. The gate computes
the same path the same way. Content:

```json
{
  "question_id": "…",
  "session_id": "<asker session>",
  "kind": "outbound",
  "target": "slack:C0123ABCDEF/1790000000.000100",
  "payload_sha256": "9f2c…",
  "card_version": 3,
  "clicked_at": 1795000000
}
```

`answer-ask` (`docs/ipc.md`) answers "as if on the card", so any local process can call it. For a
grant action it is refused with `a grant action can only be clicked` and no grant is written,
whatever client sent the command. The
answer itself (without a grant) is not delivered either: the card stays.

### 8.4 Who consumes a grant (outside this repo)

Kova only writes the file. The outbound gate (`~/.claude/hooks/no-outbound.py`, source in the
dotfiles repo under `claude/hooks/`, tests in `no-outbound.test.py`) must be extended to accept it.
**That extension is part of this work**, in its own commit in the dotfiles repo, with its tests.
The owner of the machine asked for it (2026-11-19: a click on "Post" must count as his passphrase
for that one draft), so changing the gate along this contract is approved; anything beyond the
contract is not.
The Kova side can be delivered first; the feature is done when both are merged. "Own commit" means:
when the owner says to commit, the dotfiles change is a separate commit; until then both repos stay
uncommitted (§ 12). Only files under `~/projects/perso/dotfiles/` are edited (`~/.claude/settings.json`
and `~/.claude/hooks/` are symlinks into it; the Edit tool refuses a symlink, so edit the target).
Write `post.sh` with the Write tool: the gate refuses a heredoc whose text names a Slack send method.
The session-id checks below are done in this work, after asking the owner's consent with one card
covering both; no answer or a refusal leaves them marked unverified, and the rest is delivered.
Kova also deletes expired grants (older than 24 hours) at launch, so a grant whose session died
does not stay forever.
Deployment order does not matter: Kova's grants are inert files until the gate reads them, and
the gate's new path never fires until grants exist. Contract:

- **one command form only.** The gate accepts a grant for a single sender, new in this work:
  `~/.claude/skills/slack-later/post.sh --sha256 <hex> <channel> <thread_ts|-> <file>`, beside
  `draft.sh` in the same skill (dotfiles repo), alone on its command line: no `&&`, `;`, `|`, `cd`,
  redirection, variable or substitution anywhere in the Bash call, otherwise the grant is ignored.
  `post.sh` reads the file **once** into memory, checks that its SHA-256 equals `--sha256`, and posts
  that buffer as the message text, byte for byte, and nothing else; a mismatch exits 1 and posts
  nothing. So a file changed between the gate's check and the post cannot go out. The gate checks
  that `--sha256` equals the grant's `payload_sha256` and that the file's current hash does too. `post.sh --dry-run …` prints the request body it would send and calls
  nothing, which is what its tests compare: one line of compact JSON with keys in this order,
  `{"channel":…,"text":…}` plus `"thread_ts":…` last when one is given, never the token. Those are
  the only `chat.postMessage` parameters it sends (`-` = top-level message, no `thread_ts`). The gate hashes the raw bytes of `<file>` (the path must appear once in the
  command and be readable; a path built by a substitution or `-` stays gated). Every other way of
  sending (`curl` to the Slack API, mail, WhatsApp) ignores grants and still needs the passphrase;
- the gate also checks the destination: `slack:<channel>/<thread_ts or ->` built from `post.sh`'s
  arguments must equal the grant's `target`. Same text, other channel = refused;
- `post.sh` takes its token and cookie from the same place as `draft.sh` (the web client's session,
  through Mira) and calls the Slack Web API method `chat.postMessage` with them, so the message is
  posted as the user, like a draft he sends himself. On success it deletes the matching draft
  (`draft.sh --delete <channel> <thread_ts|->`) so the composer does not keep it. If that deletion
  fails after a successful post, `post.sh` still exits 0 and prints a warning on stderr: a non-zero
  exit would invite a second send;
- `post.sh` is a send in the gate's eyes: refused unless a valid grant matches, or the passphrase
  armed the gate as today;
- the `<file>` path must be absolute or start with `~/`; a relative path stays gated; `-` as `thread_ts` means a top-level message (no thread);
- the session compared is the hook payload's `session_id`. It is the same value Kova stores as
  `from.session_id`, since `kova ask` reads it from `CLAUDE_CODE_SESSION_ID`, which Claude Code sets
  to its session id (checked on 2026-11-19: both equal in the same session). Kova's late delivery
  resumes the conversation with `claude --resume <session_id>`. Checked on 2026-11-19, with the
  owner's consent, in a clean environment (`env -i`): a `claude -p` run had
  `CLAUDE_CODE_SESSION_ID` equal to the `session_id` of its JSON result
  (`claude -p … --output-format json`), and a `claude -p --resume <that id>` run reported the same
  id in both places again: resuming keeps the session id. Not checked: the interactive
  `claude --resume` that Kova types (same flag, not run interactively here), and the hook payload
  itself (the JSON result's `session_id` stood in for it). If an id ever changes on resume, a grant
  clicked before is unusable and the agent falls back to the passphrase, which is safe, only less
  convenient;
- a send command is allowed once if a grant exists for the calling session, `clicked_at` is less
  than 24 hours old (a late delivery can wake the asking conversation hours after the click), and
  the SHA-256 of the body the command sends (read from the file the command passes, the gate
  already reads such files) equals `payload_sha256`. The window is long because the grant is
  already narrow: one session, one exact content, one use;
- several grants may exist for one session: the gate uses the one whose `session_id`, `target` and
  `payload_sha256` all match the command, and deletes every expired grant on each call;
- the gate only checks; `post.sh` consumes the grant (deletes its file) right after a successful
  post. If the post fails (no Slack session in Mira, expired cookie, Slack error), `post.sh` exits 2
  with the error on stderr and the grant stays, so the agent may retry within the validity window.
  `post.sh` finds the grant by the same match (session from `CLAUDE_CODE_SESSION_ID`, target, hash);
  its own deletion is not a tool call, so the gate's write protection does not apply to it;
- `post.sh` reuses `draft.sh`'s mechanism as is (same token and cookie retrieval through Mira);
- the grants directory gets the same protection as the passphrase token today: any tool call that
  writes, copies or removes inside it is refused (reading is allowed). For Bash, this reuses the
  gate's existing detection for the token path (`ARM_PATH` in `no-outbound.py`) with the grants
  directory added to it, nothing new. The gate is a `PreToolUse`
  hook registered for `Bash` today; its registration in `~/.claude/settings.json` gains `Write`,
  `Edit` and `NotebookEdit`, for which it only checks the target path against the grants directory
and is a no-op everywhere else. Paths are resolved with `realpath` (`~`, `..`, symlinks) before the
comparison, as for the passphrase token. For these three tools the path is `tool_input.file_path`
(`notebook_path` for NotebookEdit); if the hook cannot read it, it lets the call through (fail-open),
since these tools never send anything. Bash keeps today's behaviour.

Threat model: same as the current gate. It protects against an agent sending by mistake or
misunderstanding, not against a hostile process running as the user.

## 9. Errors (`ask` and `update-ask`)

Validation runs in this order: `missing card` (`update-ask` only), size (`card too large`), control
characters in any string, `message too long`, then the order of this spec (`missing version` first) (§ 4 document fields, § 4.1 blocks in order, § 5 actions in order, § 8 grant) and returns the first error only; tests may rely on that order. Internally
`message` is always set; on the wire it is optional (§ 3).

Responses use the existing shape `{"ok":false,"error":"…"}`; `kova ask` prints the error and
exits 1. Errors: `unknown card version <n>`, `unknown block type <t>`, `<field> too long`,
`too many blocks`, `too many actions`, `duplicate action id <id>`, `more than one primary action`,
`needs_input without input`, `more than one grant action`, `payload_block is not a code block`,
`payload_sha256 does not match block <i>`, `grant requires an asker session id`,
`grant requires a Claude Code asker`, `card too large`,
`not the asker of this question`, `no such question`, `updating requires an asker session id`.
A field is named by its path in the card: `title`, `blocks[2].text`, `blocks[0].items[3].key`,
`actions[1].id`, `actions[0].grant.target`. So `<field> too long` reads `blocks[2].text too long`.
Any other validation failure gets a message in the same style (short, lowercase, naming the field),
for example `grant action cannot need input` (a grant action may not have `needs_input`: the text
would not be sent), `missing version`, `bad action id <id>`, `control character in <field>`,
`payload_block out of range`, `unknown grant kind <k>`, `payload_sha256 is not 64 hex chars`.

## 10. CLI

```
kova ask --card card.json [--from NAME] [--timeout SECONDS]
kova ask --card - < card.json
kova ask --update <question id> --card card.json
kova ask --withdraw-id <question id>
```

`--card` excludes a positional message, `--actions` and `--no-input` (error if combined). The
answer is printed as today (`<action id>` or `<action id>: <text>`), and the exit codes are
unchanged; `--json` prints the full answer object, including `card_version`.

## 11. First client: Slack drafts through the Chief of Staff

The agent writes the draft to a file, hashes it, and asks:

```json
{
  "version": 1,
  "title": "Reply to Alex in #infra",
  "subtitle": "Slack drafts · 19/11 15:34 · draft already in your Slack composer",
  "blocks": [
    { "type": "fields", "items": [
      { "key": "Asked by", "value": "Alex, infra lead" },
      { "key": "Since", "value": "18/11 16:02" } ] },
    { "type": "quote", "author": "Alex", "time": "18/11 16:02",
      "text": "Can you review the backup policy before Friday?" },
    { "type": "code", "label": "Draft", "text": "Yes, I read it. Two remarks on section 4…" },
    { "type": "context", "text": "Source: the thread, read by a Slack reader (C0… 1794…)" }
  ],
  "actions": [
    { "id": "post", "label": "Post", "style": "primary",
      "grant": { "kind": "outbound", "target": "slack:C0…/1794…",
                 "payload_sha256": "<sha256 of the draft file>", "payload_block": 2 } },
    { "id": "edit", "label": "Rewrite", "needs_input": true },
    { "id": "clear", "label": "Clear draft", "style": "destructive" },
    { "id": "later", "label": "Later" }
  ],
  "input": { "placeholder": "What to change in the draft" }
}
```

On `post`, the agent runs `post.sh --sha256 <hash> <channel> <thread_ts> <block file>` (the exact
form of § 8.4) with the payload block's file and hash from the answer (§ 4.2: the user may have
edited the draft on the card), and the gate lets that one send through. On `edit`, it rewrites the draft and calls `update-ask` with the new card (new hash). On
`clear`, it deletes the Slack draft (not an outbound action). On `later`, nothing.

## 12. Tests (pure logic, `#[cfg(test)]`)

- card validation: each error of § 9, and a valid card round-trips through serde unchanged;
- a question without `card` serializes exactly as before (existing on-disk questions still load);
- `payload_sha256` check against the `code` block;
- `answer-ask` on a grant action is refused and writes nothing; on a non-grant action of a card it
  behaves as today;
- a grant is written only for the clicked version, with the fields of § 8.3;
- `update-ask` from another session is refused; from the asker it bumps `card_version` and keeps
  queue order;
- late-delivery line for a card;
- queue browsing (`n / N`, ‹ ›) never answers, and follows the shown id when another question
  leaves;
- inline markup parser: each rule of § 4.1, including `snake_case_name` and escapes;
- the click path goes through a pure function (for example
  `apply_click(board, id, action, text, shown_version) -> (Answer, Option<Grant>)`) that the AppKit
  handler calls, so "a grant only for the clicked version" and "Return never triggers a grant" are
  tested without AppKit. If `shown_version` differs from the current version (a redraw not done
  yet), the function refuses the click, writes no grant and asks for a redraw;
- grant file permissions (0700 directory, 0600 file) and `KOVA_ASK_DIR`, in a temporary directory;
- in the dotfiles repo: the gate allows one send with a matching grant, refuses a wrong hash, a
  wrong session, an expired grant, a second use, and any write into the grants directory. The
  gate's tests point it at a temporary grants directory through `KOVA_ASK_DIR`, so they never
  touch the real one; `post.sh` is tested with `--dry-run`.

The AppKit rendering is not unit-tested. It is checked visually by the user, on a test instance
launched only with his consent, following `CLAUDE.md` § test instance (announce it with a card,
clean environment, separate `KOVA_ASK_DIR` and `HOME`). The work can be handed over before that
visual check, saying it is pending. Nothing is committed in either repo until the owner says so.

## 13. Implementation notes

Choices the spec left open, as implemented:

- `card too large` is measured on the card re-serialized compact (`serde_json::to_string`), so a
  pretty-printed card sent straight on the socket is measured without its whitespace. Identical to
  "as received" for the CLI, which sends it compact.
- Fields the spec does not list are refused (`unknown field <path>`), so a card never carries
  something Kova silently drops. An empty `title` or label is refused (`empty title`,
  `empty actions[0].label`); `input.placeholder` is limited to 200 chars; a `fields` block needs 1
  to 12 items (`blocks[0].items needs an item`, `too many blocks[0].items`).
- A card's `card_version` is left out of the question's JSON on disk while it is 1 (a plain
  question serializes exactly as before); `list-asks` always adds it.
- A withdrawn question's waiting `kova ask` gets the error `withdrawn by the asker` (exit 1).
- The sound and Dock attention request play once per question, not again when browsing back to it.

