# Cmd+P: sorting panes into categories?

Decided and coded on 2026-10-21: see "The decision: subjects" at the end. The sections
before it are the reasoning that led there.

## The starting point

With many panes open (several dozen), Cmd+P becomes hard to navigate and the user gets lost.

## The measured state

`list-panes` and `list-tabs` on a live Kova with about fifty panes in about ten tabs of a single
window: two tabs hold most of them (one with about 25 panes, another with about 13), and the
other tabs hold 1 to 3 panes each.

Cmd+P already groups by tab (one header per tab followed by its panes, `src/window/switcher.rs`,
`open_pane_switcher`). So the problem is not the lack of groups but two groups of 26 and
13 rows with no landmark inside them.

## What "lost" means

Two readings were considered:

- A: the user is looking for a specific pane and cannot find it (what "typing filters the list"
  would solve, item #1 of `notes/ux-review-2026-10-10.md`, still not done).
- B: the user no longer knows which of the panes still matter.

**The answer was B.**

## The constraints

- **Tabs are projects**, not states or tags. Categories must therefore not be built by
  repurposing tabs.
- The imagined gesture: in Cmd+P, **Cmd+↑ / Cmd+↓ to file a pane into a category**.
- Neither the solution nor its usefulness is certain.

## What already exists in the code

- **Cmd+↑ / Cmd+↓ in Cmd+P** already moves the selected pane one step in its tab's order
  (`handle_pane_switcher_key` → `pane_switcher_move_selected`, `src/window/switcher.rs`;
  `move_pane_in_order`, `src/pane.rs:1516`). It stops at the tab's edges. The imagined
  gesture is therefore already taken, and sorting into categories would have to either share it
  (crossing a category boundary when moving down) or use another shortcut.
- **Anchors** (Cmd+Shift+A) and **bookmarks** (Cmd+B) are already two cross-tab
  classifications, set by hand; anchors open Cmd+P at the top.
- **Each terminal knows its last activity** (`last_activity_secs`, shown in the pane's status
  bar, `src/renderer/mod.rs:1920`). Not checked in detail: I did not see it in the
  rendering of Cmd+P rows.

## The objection raised during the discussion

Sorting by theme does not answer B: a pane dead for three days stays dead in the right
category. And sorting 50 panes by hand is work the system could do (making the end user do
work the system can do is a design failure). Opposite direction, with no sorting:
Kova greys out or pushes to the bottom the panes inactive for more than a day, from the
`last_activity_secs` it already has.

That direction got no answer: the reply was about tabs (projects, not states).
The imagined categories might therefore be **states** (active / paused / to close),
not themes. Not confirmed.

## The question that was open

In the largest tab (about 25 panes), how many are really finished? If half, the need is
end-of-life triage (closing); if almost none, sub-grouping inside a project. The answer was
sub-grouping: see below.

## The decision: subjects (2026-10-21)

The need, in the user's words: list the work in progress better, and see which panes belong to
it. A tab is a broad project tied to a directory; inside it, the user works on several
**subjects** (ongoing pieces of work). A subject is a second level of grouping, under the tab.

- **Display in Cmd+P**: the panes of a subject sit in a box, under the subject's name in bold,
  one per line, indented with a dash. Panes with no subject are shown as today.
- **Filing is manual**, like indenting a line:
  - **Cmd+→** files the selected pane into the subject right above it. If there is none, Kova
    prompts for a name and creates the subject with this pane.
  - **Cmd+←** takes the pane out of its subject.
  - **Cmd+↑ / Cmd+↓** keep moving the pane, as today.
- **Cmd+D** is unchanged (new column on the right); the new pane inherits the subject of the
  pane it was split from.
- **`n` opens a pane in a box** (2026-10-28): on a subject's header or on any pane filed under
  it, a new pane in that subject — a column right after the box's last member, starting in the
  box's first member's directory, focused, and `Cmd+P` closes. An empty box is filled from the
  tab's focused pane, having no column of its own to open next to. A pane outside any box, and
  the bookmark and anchor sections, name no box: `n` does nothing there rather than guess one.
  The cwd comes from the box rather than from the selected row, so the gesture gives the same
  pane from the header as from any member.
- **A subject lives in one tab.**
- **An empty subject stays**, moved to the top of its tab's list.
- **Subjects survive a restart** (saved with the session).
- **Rename and delete** (asked the same day): a subject's header is selectable. Enter (or a
  click) renames it, the current name filled in; a name the tab already has merges the two.
  Cmd+Backspace deletes it; its panes stay open where they are, out of the box. An empty
  subject goes at once, one holding panes asks first.
- **The list's text is the terminal's glyphs at their own size**: it used to be stretched by
  1.3, which blurred it on a 1x display.
