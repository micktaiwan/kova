# Pane UI — spec

Status: implemented 2026-11-19 (`src/pane_ui.rs`, `src/ui_board.rs`, `src/ui_cli.rs`,
`src/card_view.rs`, `src/window/dock_view.rs`; § 14 lists what is left out). The § 12 prototype
passed its three points on a test instance. Written 2026-11-19. Supersedes the "v2" line of
`docs/decision-cards-spec.md` § 2: this is the surface the user actually asked for.

## 1. What and why

An agent running in a Kova pane (Claude Code, omp, Pi, Codex) can draw a **native UI inside its
own pane**, or in a pane it opens next to it: text, quotes, the draft it prepared, tables, small
charts, and buttons that do something. A click goes back to the agent's conversation. It is the
idea of MCP Apps (a tool shows an interactive UI where the conversation happens) done natively by
Kova, without HTML.

The first user is the "Chief of Staff" agent: it shows each Slack draft it prepared as a card
(context, the thread, the draft text, `Post` / `Rewrite` / `Clear draft`), in the pane where the
user talks to it, and the user decides with one click, while the conversation goes on next to it.

The corner card of `kova ask` stays as it is: it remains the way to reach the user when he is not
looking at the agent's pane.

## 2. What already exists and is reused

`docs/decision-cards-spec.md` was implemented for the corner card on 2026-11-19. Everything except
the surface is reused as is:

- the card document: blocks and actions (§ 4, § 4.1, § 5 of that spec), validated by
  `src/decision_card.rs` (`validate`, `parse_markup`);
- the native rendering of blocks and buttons in `src/ask_card.rs` (attributed text, quote bar,
  framed code, fields, buttons), to be moved into a reusable view that both the corner card and
  the pane UI draw with;
- grant actions (§ 8 of that spec): the click on `Post` authorizes exactly the payload shown, the
  grant file, `post.sh`, the outbound gate. Unchanged;
- delivery of an answer to a conversation that is not waiting (late delivery, `src/ask.rs`
  `deliver_late`).

## 3. Where the UI lives: two placements

### 3.1 Docked in the agent's own pane

The pane gives part of its rectangle to the UI. **Nothing is overlaid**: the terminal grid shrinks
by the UI's size, the PTY is resized (fewer rows, or fewer columns), exactly as when the pane itself
is resized by a split. Claude Code redraws to the new size as it already does.

- `side`: `bottom` (default) or `right`.
- `size`: a fraction of the pane, 0.2 to 0.7 (default 0.4). For `pane-right` / `pane-down`, `size` is
  the share of the split given to the UI pane (default 0.5). The user can drag the separator between
  terminal and UI; the dragged size sticks for that view.
- When the view closes, the terminal gets its full rectangle back (PTY resized again).
- Minimum terminal size kept: 20 columns × 6 rows. If the pane is too small for both, the UI
  collapses to a one-line bar (`<title> · N cards · click to expand`) and the terminal keeps the
  rest.

The UI cannot be inserted *inside* Claude Code's conversation flow: Claude Code draws its whole
screen itself in the terminal's alternate screen, so nothing from outside can sit between two of
its messages. Docking next to the terminal is the closest equivalent.

### 3.2 In a pane of its own

The agent opens a new pane next to its own (same tab, split right or down, like the `split` IPC
command) that holds only the UI, no terminal. It closes with the view, or when the user closes the
pane (the agent receives a `closed` event, § 6).

Which placement to use is the agent's choice per view (`placement`, § 5): docked for a few
decisions alongside the conversation, a pane of its own for something bigger (a dashboard of 15
drafts, a table).

## 4. The view document

A view is a list of cards, stacked and scrollable, plus an optional header.

```json
{
  "version": 1,
  "title": "Slack drafts — 3 to decide",
  "cards": [
    { "id": "draft-alex", "card": { …a card document of decision-cards-spec § 4… } },
    { "id": "draft-sam",  "card": { … } }
  ]
}
```

| Field | Type | Required | Meaning |
|---|---|---|---|
| `version` | integer | yes | `1`. |
| `title` | string ≤ 120 chars | yes | Header of the view (and text of the collapsed bar). |
| `cards` | array, 0 to 50 | yes | Each item: `id` (`[a-z0-9_-]{1,32}`, unique in the view) and `card` (a card document, validated exactly as in decision-cards-spec § 9). The card document
holds only `version`, `title`, `subtitle`, `blocks`, `actions`, `input`; question fields of
`kova ask` (id, timeout, asker) are not part of it, so nothing needs ignoring. An empty list shows `title` and "Nothing to decide". |
| `close_when_empty` | boolean | no (`false`) | When a `ui-update-card` removal or a `ui-update` leaves the view with no card, Kova closes it as `ui-close` would (grants dropped, UI pane closed, a pending `ui-wait` returns) and the reply carries `"closed": true`; no `closed` event is sent. For one-shot lists (drafts to post); a board that lives across passes leaves it off and keeps "Nothing to decide". A view shown with no card stays open. |

Whole document ≤ 512 KiB. Each card keeps its own actions, text field and grant.

### 4.1 New blocks (added to the card format, usable in the corner card too)

| `type` | Fields | Rendering |
|---|---|---|
| `table` | `columns`: array of strings (1 to 8, ≤ 40 chars each); `rows`: array of arrays of strings (≤ 100 rows, each ≤ 200 chars, same length as `columns`) | A plain table, header in grey, rows separated by thin lines, horizontal scroll if too wide. Cells take the inline markup of `text`. |
| `bars` | `items`: array of `{ "label": string ≤ 40, "value": number ≥ 0 }`, 1 to 20; `unit` (optional, ≤ 10) | Horizontal bar chart, one bar per item, value and unit at the end of the bar, scaled to the largest value. Single accent colour. |
| `sparkline` | `values`: array of numbers, 2 to 200; `label` (optional, ≤ 40) | A small line chart, one text line high, last value printed after it. |
| `progress` | `value`: number 0 to 1, refused outside (`blocks[i].value out of range`); `label` (optional, ≤ 80) | A thin progress bar with its label. |

Edge cases: `bars` whose values are all 0 shows empty bars with their values; a constant `sparkline`
is a flat line at mid-height.

Unknown block types stay rejected, as in decision-cards-spec § 4.1.

## 5. Commands

Socket commands (spoken by the `kova ui` CLI, which finds the session and pane from its
environment like `kova ask`):

```json
{ "cmd": "ui-show", "session_id": "…", "pane_id": 12,
  "placement": "dock-bottom" | "dock-right" | "pane-right" | "pane-down",
  "size": 0.4, "view": { …view document… } }
```

Answers `{"ok":true,"data":{"view_id":"<uuid>","pane_id":<pane holding the UI>}}`.

- `pane_id` is the asker's own pane (from `KOVA_PANE_ID`). An agent can only show a view in the
  pane it runs in, or in a pane it opens with `pane-*`. Error otherwise: `not your pane`.
- One docked view per pane. `ui-show` on a pane that already docks a view replaces it (same
  `view_id`): ownership is by pane (below), so the session running in the pane always owns its
  docked view, even after a resume with a new session id.

```json
{ "cmd": "ui-update", "view_id": "…", "session_id": "…", "view": { … } }
{ "cmd": "ui-update-card", "view_id": "…", "session_id": "…", "card_id": "…", "card": { … } | null }
{ "cmd": "ui-close", "view_id": "…", "session_id": "…" }
{ "cmd": "ui-wait", "view_id": "…", "session_id": "…", "timeout_ms": 600000 }
```

- `ui-update` replaces the whole view; `ui-update-card` replaces one card (`null` removes it), which
  is what an agent does after acting on a click ("Posted ✓", then removed). Each card keeps a
  `card_version`, bumped on each replacement, as in decision-cards-spec § 7.
- **Ownership is by pane, not by session id**, so a resumed conversation with a new session id keeps
  its views: a docked view belongs to whatever agent session runs in the pane that docks it; a UI
  pane belongs to whatever agent session runs in the pane that opened it (its "opener pane", saved
  with the UI pane). Only that session may update, wait on or close the view (`not the owner of
  this view`). If the opener pane is closed, the UI pane has no owner: the user can still click and
  close it, clicks go through late delivery to the last owner session id.
- **How Kova checks "runs in the pane".** The caller is accepted when its process descends from that
  pane's shell, the check `kova ask` already does (`pane_holds_session` in `src/ask.rs`). This works
  at once after a resume, before Kova's agent detection has noticed the new session id.
- **Pane identity across relaunches.** Pane numbers change when Kova restarts, so links between
  panes (a UI pane's opener) use a persistent `uid` (a UUID) added to each pane record in the
  session file, created the first time a pane is saved without one.
- `ui-wait` blocks until the next event of that view, or the timeout (then `{"ok":true,"data":null}`).

CLI:

```
kova ui show [--placement dock-bottom|dock-right|pane-right|pane-down] [--size 0.4] --view <file|->
kova ui update <view_id> --view <file|->
kova ui card <view_id> <card_id> --card <file|->     # --remove to delete the card
kova ui wait <view_id> [--timeout SECONDS]           # default 600; prints one event as JSON, exit 0; timeout exit 2
kova ui close <view_id>
```

## 6. Events: how a click reaches the agent

An event is `{ "view_id", "card_id", "action", "text", "card_version", "blocks" }`, or
`{ "view_id", "event": "closed" }` when the user closes the view or its pane. The typed line for
`closed` is `[Kova] UI "<view title>" was closed by the user`.

**Edited `code` blocks.** A `code` block of a card in a view is editable, exactly as on the `kova
ask` card (decision-cards-spec § 4.2): same plain-text editor, same `· editable` caption, not while
the card is locked after a click. Every click on a card with `code` blocks (window or `answer-ui`)
writes the final text of each block, edited or not, to
`<ask dir>/answers/<view_id>_<card_id>/block-<index>.txt`, before any grant, and the event carries
`"blocks": [{ "block", "label", "file", "sha256", "edited" }]` (absent for a card without `code`
blocks). The typed line appends `[block <i> "<label>" edited by the user|unchanged: <file> sha256
<hex>]` per block, as an ask answer does. **The agent sends that file, never its own copy.** Edits
survive redraws, tab switches and `ui-update` calls that leave the card unchanged; a new version of
the card (`kova ui card`, or a `ui-update` that changes it) shows its own text and drops them. A
remote `answer-ui` carries no edits: its blocks are the card's text. A file that cannot be written
drops the click with a red line on the card.

Two ways in, the same as for `kova ask`:

- **The agent is waiting** (`kova ui wait`): the event is printed and the command returns.
- **The agent is not waiting** (it is talking with the user, or idle): Kova types the event into
  the agent's conversation, through the late-delivery path of `kova ask` (`deliver_late`):
  `[Kova] UI "<view title>", card "<card title>" (v<n>): <action>[: <text>]`, followed by the block
  reports above, where `<card title>` is the card's `title` (mandatory in the card format). Kova does not wait for the agent to finish
  its turn: it types the line as an answer to a card is typed today, as soon as the user has nothing
  half-typed in that pane (§ 12).
- **"Waiting"** means a `ui-wait` connection is open on that view. Between two `kova ui wait` calls of
  a loop, Kova holds a new event for 5 seconds after the previous `ui-wait` returned, so the next
  `ui-wait` gets it instead of the conversation. A view that never had a `ui-wait` gets no such
  delay. The typed line reaches Claude Code, Codex, Pi and omp the way card answers do today; how
  each of them handles input typed during a turn has only been relied on for Claude Code, so the
  implementer checks the three others once, in the test instance and under the same consent card as
  the prototype (§ 12), and writes the result here; without consent they stay marked unverified.
  A line typed while Claude Code shows a dialog (a permission prompt, a selector) would answer that
  dialog. Kova already tells an idle agent from a working one (`src/auto_pause.rs`); the implementer
  checks whether that detection also recognizes such a dialog. If it does, holding the line until
  the dialog closes is mandatory; if not, the risk is written here as known.

  *Results (2026-11-19).* Claude Code: a line typed by Kova during a turn arrived as a user message
  in the asker's conversation (a late `kova ask` answer, observed in this work's own session).
  Codex, Pi, omp: **unverified**. The test instance runs with an empty `HOME` (`CLAUDE.md` § test
  instance), where those agents have no login, so they cannot be started there. Dialogs: **known
  risk**. Kova's working/idle detection reads the spinner in the terminal title
  (`Surface::is_working`, `src/surface.rs`) and does not recognize a Claude Code dialog, so a line
  typed while a permission prompt or a selector is open would answer it.

Events are kept on disk until delivered (`<ask dir>/ui/`), like undelivered answers.

After a click, the card shows a spinner and its buttons are disabled until the agent updates or
removes that card, so a second click cannot repeat an action. If the agent does nothing within 10
minutes, the buttons come back. `ui-update-card` always releases the lock and bumps `card_version`,
even when the new card is identical; `ui-update` releases the lock on every card.

`closed` is sent only when the user closes the view (✕ or closing its pane), never for the agent's
own `ui-close`. When the owner is gone, it goes through late delivery like a click.

## 7. Grants

Unchanged from decision-cards-spec § 8, with `question_id` replaced by `<view_id>:<card_id>` in the
grant file and the gate's matching. The grant's `session_id` is the session running in the owner
pane at click time (Kova's agent detection of that pane); if Kova does not know it, the last owner
session id. Adapting `post.sh` and the gate in the dotfiles repo (they match
grants by session, target and hash, so the id form matters only for the file name and its tests) is
part of this work, already approved by the owner for the corner card on 2026-11-19
(decision-cards-spec § 8.4); the same contract applies, nothing beyond it. As on the ask card, the
grant authorizes the payload block **as the user left it**: its `payload_sha256` is the hash of the
edited text written to the block file, not the one the agent declared. A click on a grant
action in the pane UI is a real click: the
same rules apply (no plain Return, ⌘Return only on the primary button, refused through any socket
command).

## 8. Focus and keyboard

- Clicking in the UI gives it the keyboard; Escape gives it back to the terminal. Typing in the
  terminal never reaches the UI.
- Inside the UI, Tab moves between cards' text fields and buttons; ⌘Return triggers the focused
  card's primary action, the one with `"style": "primary"` (decision-cards-spec § 5; at most one per
  card, none means ⌘Return does nothing). Plain Return never triggers a grant (§ 8.3 there).
- The pane's existing shortcuts (split, close, switch pane) still work while the UI has focus.

## 9. Persistence

A view is saved inside the record of the pane that shows it, in Kova's session file
(`src/session.rs`): document, placement, size, owner session id, last click time. That record is the
only copy. `<ask dir>/ui/` holds only undelivered events and the `<view_id>.last.json` files (§ 12). When Kova restarts and restores the pane, the view comes back as it was, buttons active.
When the owner session is gone, clicks are still recorded and delivered later through late delivery,
like answers today.

## 10. Rendering

- The block rendering of `src/ask_card.rs` is extracted into a reusable view (`card_view`), used by
  the corner card and by each card of a pane view, so both look the same.
- A docked view is an AppKit view placed by Kova inside the pane's rectangle, in the part the
  terminal grid no longer uses; it moves and resizes with the pane (splits, window resize); when the
  pane is minimized (existing feature, `Tab::minimize_pane` in `src/pane.rs`) the view is hidden with
  it. A UI pane minimizes like any pane.
- `bars`, `sparkline` and `progress` use the accent colour of the corner card (`src/ask_card.rs`).
  Numbers show at most 2 decimals, trailing zeros removed, no thousands separator.
- Light and dark follow the system, like the corner card.

## 11. Also in this work

- `CLAUDE.md` § Principles: replace the "No feature creep … that's it" line with: "Kova is a
  terminal that agents can also draw native UI in: `kova ask` cards (`docs/decision-cards-spec.md`)
  and views inside panes (`docs/pane-ui-spec.md`)."
- `docs/ipc.md`: the `ui-*` commands. `skill/SKILL.md`: how an agent shows a view, waits for clicks,
  updates a card. These go with the Kova code. Kova and the dotfiles repo (`post.sh`, the gate) get
  one commit each, only when the owner says to commit.

## 12. Details

**UI panes (§ 3.2).** A UI pane appears in `list-panes` like any pane, with an id and
`"kind": "ui"`, and is restored at relaunch. Commands that need a terminal (`send-keys`,
`get-pane-content`, read-pane) answer `pane has no terminal`. Its content is changed with
`ui-update` / `ui-update-card`; `ui-show` aimed at a UI pane is refused (`use ui-update`).

**How many views (§ 5).** At most one docked view per pane, and at most 4 UI panes per opener pane
(`too many ui panes`; docked views do not count). `ui-show` with a dock placement on a pane that
already docks a view replaces it (same `view_id`). `ui-show` with a `pane-*` placement always creates a new view in a
new pane. A view never changes placement: close it and show another.

**Updating cards (§ 5).** `ui-update-card` with an unknown `card_id` appends the card at the end.
`ui-update` bumps `card_version` only for cards whose JSON changed. Replacing or removing a card
deletes its unused grants: the agent must run the send before it replaces the card (for example
with "Posted ✓"). The user closing the view does not delete the grant of a card they clicked:
clicking Post then closing the view is one gesture, and the agent's send may start after the close
(the grant stays single-use and expires with its TTL). `ui-close` from the agent still deletes them.

**Versions (§ 6).** Kova never rejects a click. Clicks and updates run one after the other on the
main thread, so a click always carries the version on screen; comparing it with the last version
the agent sent is the agent's job, as in decision-cards-spec § 7.

**Delivering events exactly once (§ 6).** Each view has one ordered event queue, owned by the main
thread. An event leaves the queue when it is handed to a `ui-wait` or typed into the conversation,
never both. A second `ui-wait` on a view that already has one waiting gets `already waiting`.
Events are delivered one at a time, in click order.

**Typing into the conversation (§ 6).** The line goes through `deliver_late` unchanged; it already
knows how to reach Claude Code, Codex, Pi and omp (`src/ask.rs`, `resume_line`). Kova does not
detect whether the agent is "at its prompt"; what it does detect is the user's own typing, since it
forwards every key: if the user typed characters in that pane since his last Return, the event
waits until he presses Return or empties the line, so it never lands in the middle of his
sentence. Newlines and control characters in the text and titles become spaces (as
`late_answer_message` does), and the user's text is cut at 500 chars with `…`. The typed line
consumes the event, so the full event is also written to `<ask dir>/ui/<view_id>.last.json`, and
the line ends with that path when it was cut, for the agent to read.

**Sizes (§ 3.1).** A requested or dragged `size` that would leave the terminal under 20 × 6 is clamped
to the largest size that keeps 20 × 6. `size` is a fraction of the pane's height for `bottom` and of
its width for `right`. The one-line bar appears only when, after keeping 20 × 6 for the terminal,
less than 80 pt would remain for the view. The size is stored per view.

**Grants and updates (§ 7).** A card whose JSON is unchanged by a `ui-update` keeps its grants; any
change to a card's JSON deletes its unused grants, even if the payload text is the same.

**The 10-minute button lock (§ 6).** The click time is saved with the view, so the lock survives a
relaunch. It applies to pane views only; the corner card is unchanged.

**Closing (§ 3.1).** The view header has a ✕ button: it closes the view and sends `closed`. The
collapsed bar expands back to the remembered size on click. The dragged size is saved in the pane
record with the rest of the view (§ 9).

**Focus (§ 8).** In a docked view, a first Escape leaves the text field (focus stays on the card), a
second gives the keyboard back to the terminal. In a UI pane, Escape only leaves the text field.
"The focused card" is the card holding the focused control; if none, ⌘Return does nothing.

**Restoration (§ 9).** A view is saved inside the record of the pane that shows it, in Kova's
session file (`src/session.rs`), so it comes back with that pane, whatever happens to the agent.
The owner session id is kept only for ownership (§ 5) and delivery (§ 6). A UI pane is restored as
its own pane record in the tab layout. The view is removed from the pane record on `ui-close`, on ✕, or when the pane is closed. A view whose
agent is gone stays until the user closes it; its clicks go through late delivery. Undelivered
events older than 7 days are deleted at launch.

**New blocks in the corner card (§ 4.1).** Allowed, same limits; a wide table scrolls horizontally
there too. `sparkline` accepts negative values; only `bars` requires ≥ 0.

**Main technical risk (§ 10).** Kova draws all panes of a window into one Metal view (`KovaView`,
see `src/app.rs`). Native AppKit subviews inside a pane's rectangle are new in Kova: z-order above
the Metal layer, mouse and scroll events routed to the subview instead of the terminal, keeping
them in place during splits, scrolling and minimize. Start the work with a prototype of one docked
view with one button. It is a checkpoint. Seeing it needs a test instance, which needs the owner's
consent: ask it with a `kova ask` card, launch the instance per `CLAUDE.md` § test instance, then
report how the three points went on a second card; if all three work, go on without waiting. The
three points: (1) the view is drawn above the Metal layer at the right place; (2) mouse clicks and
scroll over it reach the view, not the terminal; (3) it stays in place through a split, a window
resize and a minimize. If one fails, stop there and report it on the card, with the alternative:
drawing the same blocks in Metal inside the pane's grid area.

**`list-panes` consumers (§ 3.2).** Every pane gets `"kind": "terminal"` or `"ui"`. Check the
consumers that assume a terminal: `skill/SKILL.md`, `scripts/` in this repo, and `grep -rn
list-panes ~/.claude/skills ~/projects/perso/dotfiles/bin`.

**More details.**

- *Pane versus `session_id`.* For permissions only the pane counts (process descent). The
  `session_id` a command carries is used only for grants and late delivery; if it differs from what
  Kova detected in the pane, the detected one wins.
- *Grants.* A grant is created at the click, never at display (decision-cards-spec § 8.3). "Unused
  grants" of a card = grants clicked but not yet consumed by `post.sh`.
- *`size`.* 0.2 to 0.7 for every placement; a value outside is refused (`size out of range`).
- *Collapsed bar on `dock-right`.* A one-line strip at the top of the right column; the 80 pt
  threshold applies to the column width.
- *Hidden pane.* When the pane is not visible (another tab, a zoomed sibling, hidden window), its
  docked view is not visible either; clicks happen only on what is visible. Nothing else changes.
- *Focus.* `ui-show` never takes the keyboard or brings the window forward: the focus stays where the
  user is.
- *The 5-second hold* applies only to an event arriving within 5 s after a `ui-wait` returned; it
  adds to the half-typed-line hold when both apply.
- *Half-typed line.* Kova counts printable keys and pastes sent to the pane since the last Return;
  Ctrl-C, Ctrl-U and Escape reset the count; arrow keys do not count. A false "half-typed" only
  delays the line until the next Return, which is the safe side.
- *Edits to this spec* during the work (results of the checks of § 6, known risks) are made by the
  implementer. One consent card covers the prototype, the test instance and the agent checks.
- *Metal fallback.* If the prototype fails and the Metal alternative is chosen, it needs a new design
  (text fields, Tab, table scroll, selection): stop and report, do not continue on this spec.
- *Validation messages* follow `src/decision_card.rs` (path of the field, lowercase), for example
  `cards[3].card…`, `too many cards`, `duplicate card id <id>`, `view too large`,
  `blocks[i].rows[j] has the wrong number of cells`. `sparkline` values must be finite numbers; its
  height is one line of the card's body text.
- *`ui-update-card`* with `card: null` on an unknown id is a no-op (ok); adding a 51st card is
  refused (`too many cards`).
- *Pane `uid`.* Duplicates found when loading a session file are given a new `uid`.
- *An orphan `closed` event* follows the 7-day rule of undelivered events.
- *Never type into a bare shell.* A line is typed into a pane only if that pane holds an agent
  session (Kova's agent detection). If the pane runs a plain shell, the event stays queued (or goes
  to the session's resume path, exactly as `deliver_late` does for an answer whose conversation is
  closed); it is never typed at a shell prompt, where it would run as a command.
- *Acknowledged hand-off to `ui-wait`.* As with `kova ask` (the client writes `ack` within 2 s), the
  `kova ui wait` client acknowledges the event it printed; without the ack the event goes back to
  the head of the queue.
- *Persisted with the view:* `view_id`, each card's `card_version` and click lock, besides § 9's
  list, so `ui-update` keeps working after a relaunch. Unused grants live in the grants directory
  and expire on their own (24 h).
- *`closed` of a docked view whose pane was closed* goes to the owner session's resume path through
  `deliver_late`, then follows the 7-day rule.
- *Defaults.* `kova ui show` without `--placement` = `dock-bottom`; size 0.4 docked, 0.5 for
  `pane-*`. A `ui-show` replacing a docked view keeps the size the user dragged, if any.
- *Collapsed `dock-right`.* The one-line strip spans the view's column only; the terminal keeps its
  width.
- *Socket size.* Check that the socket reader accepts a 512 KiB line; raise its limit if not.
- *Grant file name.* `<view_id>:<card_id>` is the id inside the file; the file name replaces `:` with
  `_`. The dotfiles repo is `~/projects/perso/dotfiles`.
- *Tables.* A newline in a cell is shown as a space. A table scrolls horizontally inside its card;
  the card list scrolls vertically; a horizontal scroll gesture over a table goes to the table.
- *Commits.* Changes are written in both working trees (Kova, dotfiles) and committed only when the
  owner says so, one commit per repo.

## 13. Tests (pure logic)

- view document validation (each error, 0 and 50 cards, size limit) and the new blocks;
- dock layout: terminal rows/columns after docking at each `side` and `size`, minimum terminal size,
  collapse to the one-line bar, restore on close;
- ownership: `ui-update`/`ui-close`/`ui-wait` from another session refused; `ui-show` on a pane of
  another session refused;
- events: delivered to `ui-wait` when waiting, queued for late delivery otherwise, kept on disk;
- a card's buttons disabled after a click until the agent updates it, back after 10 minutes;
- grant from a pane-view click: same tests as decision-cards-spec § 12 with the new id form.

The AppKit rendering is checked visually by the user on a test instance, launched only with his
consent (`CLAUDE.md` § test instance). Nothing is committed until he says so.

## 14. Implementation notes

As implemented on 2026-11-19, and what is left out.

- **Prototype (§ 12)**, measured on a test instance: (1) the view is drawn above the Metal layer,
  in the bottom 40 % of the pane, and the terminal is resized (`stty size` answered 37 rows);
  (2) 18 clicks reached the button and a scroll over the view moved its text, not the terminal;
  (3) it kept its place through a split, a minimize and restore, and a window resize to
  1100 × 700.
- **Ownership** uses the pid at the other end of the socket (`LOCAL_PEERPID`, `peer_pid` in
  `src/ipc.rs`) and checks that it descends from the owner pane's shell. The `session_id` a
  command carries serves only grants and delivery, the session Kova detects in the pane winning.
- **Done after the first delivery (2026-11-19):**
  - dragging the edge between terminal and docked view (`hit_test_dock_edge`, `drag_dock_edge`,
    `src/window/geometry.rs`; `dragged_dock_size`, `src/pane.rs`): the size sticks for the view
    (`dragged`), is saved with it, and keeps both minimums. A UI pane's separator is an ordinary
    split separator;
  - `size` of a UI pane: it gets that share of the room its opener had (`share_with_ui_pane`);
  - two-step Escape: the first leaves the field for the view's root (`DockRoot`), which keeps the
    keyboard and lets no key reach the terminal; the second, docked views only, gives the keyboard
    back to the terminal. A click on the view (not a control) gives it the keyboard;
  - Tab walks fields and buttons in reading order (buttons only with Full Keyboard Access);
  - ⌘Return is a key equivalent: AppKit offers it to `KovaView` first and the field editor has no
    binding for it, so it is handled in `pane_view_key_equivalent` (`src/window/dock_view.rs`);
  - ⌘V, ⌘C, ⌘X, ⌘A in a card's field go to the field: Kova has no Edit menu, and its own Paste
    pasted into the terminal pane.
- **Measured on a test instance** (keys posted to its pid, focus set through accessibility, the
  drag with the real mouse): Tab moved from the first card's field to the second; after one Escape
  typed keys reached neither the field nor the terminal, after a second they reached the terminal.
  ⌘Return first did nothing (log: `performKeyEquivalent: UNMATCHED … Enter`); with the
  key-equivalent fix it answered from a field (`go: alpha`) and from the card after Escape
  (`go: beta`). ⌘V put the clipboard in the field and nothing in the terminal. Dragging the edge
  up took the terminal from 37 to 19 rows, and the next session save held `size 0.677,
  dragged: true`. A `pane-right` view with `--size 0.3` got 420 of 1400 points. A docked view
  came back after a relaunch. The edge is grabbed in a 4-point band on the terminal's side only:
  the view keeps its own clicks.
- **Still not done:** a click on the collapsed bar does not expand anything, since collapsing is
  decided by the room left, not by the user.
- The card format's `Eq` derive is gone (`bars`, `sparkline` and `progress` carry floats), so
  `Question` and `Undelivered` compare with `PartialEq` only.
- **Visual check (2026-11-19, test instance)**: a view of two cards shown by `kova ui show` from
  the pane's shell drew every block (quote, framed code, field, buttons, table, bars, sparkline,
  progress). A click on `Rewrite` with text reached a waiting `kova ui wait`
  (`{"action":"edit","card_id":"draft-alex","card_version":1,"text":"test",…}`, exit 0, then
  acknowledged), and the card locked with a spinner. Clicks made while nothing waited, in a pane
  running a bare shell, were not typed into it. One defect seen and fixed: the list opened
  scrolled down instead of at its top. Scrolling to the top once after the first build was not
  enough (measured in a real Kova: still scrolled down), so the view now scrolls back to the top
  during its first ten frames (`top_frames`, `src/window/dock_view.rs`), and a rebuild keeps the
  scroll position. Re-measured in a real Kova: the list opens at its top, first card title
  visible.

