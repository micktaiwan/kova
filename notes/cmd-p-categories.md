# Cmd+P: sorting panes into categories?

Nothing is decided or coded: this is an open design question to pick up later.

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

## Where to pick up

The question left open: in the largest tab (about 25 panes), how many are really finished? If
it is half, the need is end-of-life triage (closing), not sorting. If almost none, the need
is sub-grouping inside a project.
