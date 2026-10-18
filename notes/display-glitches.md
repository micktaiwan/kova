# Display glitches — investigation and fixes (v1.8.0)

Original symptom: with Claude Code in Kova, black lines and large gaps
between two lines, persisting until Cmd+R. ~50 bugs fixed in 4 review
iterations (multi-agent + adversarial verification, 41 regression tests).

## Key reasoning

The renderer rebuilds all vertices from the grid on every rendered
frame, and a full rebuild is forced at least every 2 s (RSS tick).
So a **persistent** glitch = corrupted cell grid, not a lost
frame. Cmd+R fixes it because it forces the app to rewrite all cells
(soft-reset + double SIGWINCH). TUIs with differential rendering (Claude Code) only
rewrite the lines they believe changed: any drift of a line
on the terminal side persists indefinitely.

## Root causes of the glitch

1. **Pending wrap (xterm "Last Column Flag")** — after a character in the
   last column, `cursor_x` was `cols` (outside the grid). A vertical
   movement (CUU/CUD) followed by a print triggered a spurious wrap: text
   one line lower than expected, possible spurious scroll of the whole screen
   at the bottom of the pane → the whole differential rendering permanently shifted.
   Fix: explicit `pending_wrap` flag, xterm/DEC STD 070 semantics
   (cleared by CR/LF/BS/CUP/CUU…, NOT by HT/CBT; saved by DECSC).

2. **Synchronized output (DEC 2026) not honored at draw time** — the defer only
   lasted 16 ms and the pane was drawn anyway from the grid being
   updated (erased lines not yet rewritten = black
   lines). Fix: per-pane vertex cache (the previous consistent frame
   stays displayed during the burst, 150 ms window), with `mid_sync`
   / `ready` flags so a torn frame is never frozen, invalidation on
   viewport change and atlas generation change.

3. **`y_offset_rows` gravity** (content pushed to the bottom when the screen is not
   full) fired for TUIs in the middle of a full redraw → large gaps.
   Fix: gravity limited to the shell flow (cursor on/after the last content
   line), cells with a colored background counted as content.

## Other families fixed

- **Unified reflow**: scrollback + grid reflowed as a single logical flow
  (wrapped lines straddling the boundary stayed cut forever).
  Reflow traps: never split a wide pair (`base` + `\0`), the
  trims must keep the `\0` and the colored backgrounds, the cursor
  relocation must replay the pair-aware chunking (the pads inserted at the
  boundaries shift the raw offsets), pop the empty bottom lines BEFORE
  the split (otherwise visible content goes to the scrollback).
- **Glyph atlas**: the shelf packer advanced `next_y` by the height of the
  new glyph instead of the actual shelf height (bottom of tall glyphs
  overwritten in the texture, permanently). `generation` counter + build
  retry when the atlas changes mid-frame (normalized UVs invalidated
  by the growth), including when an overlay makes it grow.
- **ED 3 (CSI 3J)** was aliased to ED 2: the scrollback was never
  cleared (Claude Code's `/clear` emits 2J+3J).
- **SGR `:` sub-parameters** (ITU): vte flattening corrupted the
  attributes — an isolated `4:0` became an SGR 0 (full reset, seen with neovim).
- **BCE** (back-color erase) everywhere, consistent with reverse/bold/dim.
- **DEC graphics charset** (`ESC ( 0` + SO/SI + ACS table): ncurses borders
  rendered `qqqxxx` instead of lines. State saved by DECSC.
- **Combining marks / ZWJ cut by 4 KB PTY chunks**: merged into
  the previous cell, holdback bounded to one grace chunk, width recomputed
  on VS16 promotion.
- **Resize**: DECSC/DECRC clamped and complete (SGR + flags + charset),
  resize in alt-screen (the saved main grid lost the prompt),
  A→B→A round-trip detection (coalesced SIGWINCHs → repaint nudge),
  real tab stops (HTS/TBC/CBT), DECOM, CUU/CUD margins, REP (CSI b),
  DA2/XTVERSION, modes 47/1047/1048.

## Safeguards

- 41 tests in `src/terminal/mod.rs` and `src/terminal/parser.rs` (the
  latter drives the real vte parser with raw bytes, separate chunks).
- The repros of the reflow bugs are encoded as tests — any regression of the
  chunking or of the cursor relocation breaks the suite.

---

# Round 2 — "the hole is back" (June 2026): it is the alt-screen, NOT the scrollback

Reported symptom (2026-06-17, on Kova **v1.8.0**): white band
("hole") in the middle of the text in a Claude Code pane. Cmd+R fixes it. When
scrolling again (up then down), the **same** hole reappears at the same place.

## Method

Multi-agent code review (7 reviewers + adversarial verification) THEN
ground truth via IPC dump (`get-pane-content`) of live Claude Code
panes. **The ground truth invalidated part of the review's conclusions.**

## Established facts (VERIFIED)

1. **Claude Code runs in the alternate screen.** `count-pane-content mode=scrollback`
   returns **0 bytes** on the 6 Claude panes tested (98, 100, 113, 135, 182,
   194). So: no Kova scrollback, `scroll_offset` stays 0, `visible_lines()`
   == the grid, and `y_offset_rows()` returns 0 (alt-screen early return,
   `mod.rs:2330`). → The whole "scrollback" side of the previous analyses
   (push_to_scrollback, visible_lines+offset stitching, gravity) is **off-topic**
   for this bug.

2. **The hole is a band of N empty rows in the alt grid.** Dump of
   pane 194 (rows=65, cols=85, cursor {row:60,col:2}) at the time of the hole:
   lines **33–41 empty (9 consecutive rows)**, right after a
   `⏺ Update(<a SKILL.md file>)` block containing **soft-wrapped
   lines** (continuations at lines 22-23, 25-26, 27-28), and right before
   the next `⏺ …` block. Snapshot: `/tmp/kova_hole_194.json`.

3. **Scroll mechanism.** Claude enables SGR mouse reporting (mode ≥1000 +
   1006). On scroll, Kova forwards wheel events (`button 64/65`) to the PTY
   (`window.rs:919-930`); Claude redraws its alt grid **differentially**.
   Kova parses this redraw. The hole = cells that Kova does not update during this
   redraw.

4. **Cmd+R = RepaintPane** (`window.rs:1582`): `soft_reset` (reinit of scroll
   region/SGR/cursor, `mod.rs:1470`) + winsize nudge rows±1 → double SIGWINCH →
   Claude repaints EVERYTHING → fixes the grid. The hole comes back on the next scroll because
   the differential repaint triggers the bug again.

## Cleared (review + reading of the current HEAD)

- **Renderer / vertex cache**: rebuilt from the grid on every dirty
  frame; `reuse_cached` bounded to the DEC 2026 sync window (≤150 ms) + viewport
  (`renderer/mod.rs:443-453`, `605-623`). (Correction to the Round 1 note: there is
  NO "forced rebuild every 2 s".)
- **`y_offset_rows` gravity**: returns 0 in alt-screen (`mod.rs:2330`).
- **Cross-tab bleed**: `pane_id`s globally unique, cache scoped to the active tab.
- **`wrapped` flag / reflow**: the renderer never looks at `wrapped` when
  drawing; reflow only runs on a column change (never via Cmd+R).
- **`reverse_index` (1930), `scroll_down` (961), `scroll_up` (939), IL (1281) /
  DL (1307), `set_scroll_region` (1376)**: VT-compliant, no off-by-one on
  reading.

## Not yet elucidated (the CAUSE)

The exact VT sequence that Kova misses during the differential scroll repaint.
Strong presumption: a **height desync** between what Claude believes it wrote
and what Kova places, around a soft-wrapped block → **wrap / wide-char /
width** lead, or a region+repaint sequence. Not provable by reading alone
(another session ended up adding **uncommitted** `probe_*` probes in
`mod.rs`, on a "scrollback" hypothesis that the ground truth rules out).

## Already fixed? NO (VERIFIED)

- Running Kova process: started **2026-06-11 07:18** → binary = **v1.8.0**
  (`afd6e9a`), no commit between v1.8.0 and 07:18.
- Only terminal commit post-v1.8.0: `48625d1` (search/URL wide-char, SGR clamp,
  selection invalidation, OSC8 cap) — **does not touch** scroll/RI/region/wrap.
- → The bug is present in the current HEAD (`0e30761`) too: a rebuild does not
  fix it.

## Resumption (plan)

1. Capture the PTY stream of a repro: `script -q /tmp/kova_cap.raw claude` in a
   fresh pane, generate tall blocks, scroll to make the hole appear.
2. Replay the file through the `drive(cols, rows, chunks)` harness (`parser.rs:913`)
   at 85×65 (or the real dims), reproduce the band outside Kova.
3. Bisect the stream down to the faulty sequence; chunk at the PTY read
   size if the bug depends on chunk boundaries.
4. Regression test (`parser.rs`) + fix.
- Alternative if not reproducible in a fresh session: a hook capturing the raw
  bytes in `pty.rs` (gated by an env var), built via `build.sh`, launched as a 2nd
  instance so as not to kill the running sessions.

### Capture ON by default (June 2026)

Since the bug is **not reproducible on demand**, the capture must already be running
when the hole occurs. So capture is **enabled by default** (`pty.rs`):

- Can be disabled with `KOVA_PTY_CAPTURE=0` (or `off`/`false`/`no`).
- File `~/Library/Logs/Kova/pty-capture-{pane_id}.raw`, **truncated on
  open** (`pane_id`s restart at 1 on each launch → appending would
  splice unrelated sessions).
- Cap of **256 MiB per pane** (stop + warn beyond it) to bound disk usage.
- **Purge at startup** of captures older than 24 h (`prune_old_captures`
  in `main.rs`).

Workflow when the hole appears: note the `pane_id` of the faulty pane (without
Cmd+R first — or it does not matter, the `.raw` already contains the earlier bytes),
fetch the matching `.raw`, replay it in `drive()` to reproduce the
band outside Kova, then bisect.

Snapshots from this session: `/tmp/kova_hole_194.json` (grid with hole),
`/tmp/kova_dump_194_visible.json`, `/tmp/kova_dump_194_all.json`.

---

# Round 3 (July 2026): the hole is MULTI-CAUSAL — ignored VT sequences + pending-wrap

Method: IPC scan of all live panes (several dozen) → **4 panes with the hole in progress** (13, 45,
97, 100; scrollback 0 = alt-screen confirmed). Signature reconfirmed: empty band
right after a **soft-wrapped numbered listing** (often with special glyphs
`⏺ ⎿ ⚠️`), right before a `⏺` block. Then a multi-agent study (7 mappers →
9 hypotheses → 11 repro probes in worktrees with real `cargo test` →
adversarial verification, each divergence replayed + validated against the xterm
sources). **The hole was reproduced outside the app through several independent paths** —
hence the failure of the previous rounds to "fix" it in one go.

## Root causes fixed (each with a regression test in `parser.rs`)

1. **ESC D (IND) and ESC E (NEL) silently ignored** (`esc_dispatch`,
   `parser.rs`). No `(b'D',[])`/`(b'E',[])` arm → one scroll lost per use.
   Emitted by real terminfo entries (tmux-256color `nel=\EE`). A single one shifts the whole
   differential rendering below it → band never repainted. Reproduced at 85×65 alt.
   Fix: IND → `Newline`; NEL → `CarriageReturn` + `Newline`.
2. **Flattening of `:` sub-parameters corrupts non-SGR CSIs**
   (`csi_dispatch`, `parser.rs:~609`). The `flat_map` merged the ITU subparams
   into the positional list → `CSI 3:99;5H` set the column to 99, and
   `CSI ?7:25l` toggled a phantom mode 25 (hidden cursor). Fix: one value per
   group (`first`), `raw_groups` unchanged for the `'m'` arm. (Cousin of the SGR
   `4:0` bug from round 1, but on the position side.)
3. **VPR (CSI e), HPR (CSI a), HPA (CSI \`) ignored** → positioned repaints that
   pile up on a single line, leaving rows never painted. Fix:
   merged into the equivalent arms (VPR→CursorDown, HPR→CUF, HPA→CHA).
4. **ICH (CSI @) / DCH (CSI P) did not cancel `pending_wrap`** (`insert_chars`/
   `delete_chars`, `mod.rs`), whereas `erase_chars` does. An end-of-line edit
   on an exactly full line → spurious wrap; **at the bottom of the screen it scrolls the whole
   alt grid by one row**, not modeled by the app → persistent band. Fix:
   `self.pending_wrap = false;` at the top of both functions (consistent with xterm
   `ResetWrap`). It is the most direct "hole-capable" cause.

Suite: 80 green tests (71 + 9 regressions). The 9 new tests were
verified red on the pre-fix code (targeted temporary revert).

## Hardened tooling (same session)

- **PTY capture that no longer dies**: the 256 MiB cap stopped the capture
  for good, yet panes live for days → when the hole happened the capture
  was off. Replaced by a **2×128 MiB rotation** (`pty.rs`): continuous
  capture, bounded disk. Replay = `.raw.1` then `.raw`.
- **Paths scoped by instance PID**: `pty-capture-{pid}-{pane}.raw`. Before,
  the name depended only on the `pane_id` (restarts at 1 per launch) → a 2nd
  Kova instance truncated the captures of the live instance. `prune_old_captures`
  and `is_capture_file` (`main.rs`) recognize the new names + the legacy ones.
- **Replay tool**: `parser.rs`, `#[ignore]` test `replay_capture_file`.
  `KOVA_REPLAY_FILE=<.raw> KOVA_REPLAY_COLS=85 KOVA_REPLAY_ROWS=65 cargo test
  replay_capture_file -- --ignored --nocapture` → final grid + cursor +
  automatic detection of interior empty bands. It is the bisector of the plan above.
- **`kova --version`/`--help`** (`main.rs`) print and exit instead of launching
  the full app (which restored/deleted `session.json` and truncated the
  captures of a concurrent instance — incident hit during the session).

## Still open (not proven to cause the observed hole, but hole-capable suspects)

- **DECSTBM param 0 / invalid region**: `CSI 5;0r` treated as full screen instead
  of "default = last line"; region `top>=bottom` resets + homes the cursor.
  Reproduced in a test but fix not applied (more invasive — to be done with its dedicated repro
  geometry).
- **PTY chunk boundary in the middle of a grapheme** (ZWJ emoji / skin tone):
  inflated width → overflow at the bottom of the screen. Reproduced; partly overlaps the
  combining-mark holdback from round 1 but not the ZWJ/skin-tone clusters.
- **Width table divergence** app vs Kova on `⏺`/VS16 (`⚠️`): Node
  string-width counts 2 columns where Kova counts 1 → a line pre-wrapped by
  the app overflows. Priority lead for the next occurrence: replay a
  real capture with `replay_capture_file` and bisect around the glyph.

Round 3 snapshots: `holes_report.json` + `hole_pane_{13,45,97,100}.json`
(session scratchpad, not kept in the repo), along with the workflow journal (repro test code, verdicts).

---

# Round 4 (July 2026): the hole survives a capture replayed outside the app → RESIZE lead, debounced fix (IN TEST)

Starting point: hole captured **live** in a Claude Code pane (pane 229, 118×65)
with its fresh PTY capture still alive
(`pty-capture-53097-229.raw`). For the first time on this bug, we had the
exact bytes + the immediate repro.

## Established facts (VERIFIED)

1. **The hole is deterministic from the byte stream.** Replayed cold via
   `replay_capture_file` at 118×65: same grid, same band **rows 27..46 (20
   empty lines)**, cursor row 61. It is not a live rendering race.
2. **The band = a literal `CSI 20 B` (CUD)** in the stream. Sequence: end of the
   table block → `\r` → `CSI 20 B` → `⏺ C'est fait`. Claude jumps 20 rows
   over cells that nothing fills. The hole is **inherited** from earlier
   frames (the last frame only redraws line 1) — signature of the
   differential repaint.
3. **tmux 3.6a reproduces the hole identically** (same bytes, 118×65, `cat` of the
   `.raw` in a 118×65 pane): empty band at lines 28–47 before `⏺ C'est fait`.
   Two independent emulators agree from the same stream.
4. **It is NOT the width table** (contrary to the priority suspect of
   round 3). For all the glyphs on this screen (⏺ ⎿ 🔧 ✅ ★ ❯ →), Kova
   (`unicode-width` at cluster level) and ink (`string-width` npm) give the SAME
   width. And ink positions horizontally at an absolute column (`CSI n G`), so
   any horizontal drift resynchronizes — only vertical drift matters.
5. **Resizes did happen** (Kova log of the live instance): bursts of
   `Resize(*)`, including a **vertical shrink→grow round-trip** (Up key then Down)
   at 22:44 — exactly what changes the number of lines. 38 resize events in
   total. (Caveat: the log does not say which pane had focus, so it is not pinned to
   229 specifically — but the behavior is established.)
6. Absolute positioning: **max line referenced = 65, never beyond** → if the
   pane was resized, it only went ≤ 65 then back to 65 (round-trips
   bounded at 65), consistent with resizing panes by keyboard (CTRL+OPTION+arrows).

## Methodological trap (a premature conclusion, withdrawn)

"tmux reproduces the hole → so not a Kova bug" is **wrong as an exoneration**:
the replay AND the tmux test ran at a **FIXED size 118×65**, whereas the real
session was **resized**. Replaying the output of a resized session in a
fixed-size terminal desynchronizes by construction — it does not distinguish a
Claude renderer bug from a bug in Kova's resize path. The PTY capture only
contains the output, **not** the resizes (out-of-band ioctl), so we cannot
replay the resizes at the right offsets from this `.raw`.

So **root cause not isolated**: either Kova's alt-screen resize path
leaves a non-standard state that Claude does not catch up on differentially, or
ink's reconciler does not recover from a resize (and Kova renders it faithfully, like
tmux). Both remain open.

## Fix applied (IN TEST, not confirmed)

Rather than betting on the unproven cause, we automate the **known reliable**
remedy: Cmd+R (`do_repaint_pane`) always fixes the hole (soft_reset + double-SIGWINCH
nudge → Claude repaints everything). It is now triggered **on its own
after a resize**, debounced.

- `resize_settle: HashMap<PaneId, u32>` (`window.rs`): per-pane frame counter,
  (re)armed on each resize (~150 ms). Observed: each
  CTRL+OPTION+arrow keystroke called `resize_all_panes` immediately, without debounce →
  burst of SIGWINCHs that Claude coalesces → partial repaint → band.
- When the burst calms down (counter at 0), `fire_resize_settle_repaints` →
  `repaint_pane_settle(pane_id)` does the same work as Cmd+R (soft_reset +
  `pty.resize(rows-1)` + deferred restore), **without** the border flash.
- Debounce logic extracted into a pure function `step_resize_settle` + **3 regression
  tests** (`window.rs` mod tests). Full suite: 93 green tests.
- Nature of the fix: **mitigation by forced repaint**, not a parsing fix. Safe
  (soft_reset = DECSTR, does not touch the content; applies to all apps).

## To validate (next action)

Restart Kova (the instance running during the investigation = old
binary) then resize as usual during a Claude session and check that
the hole no longer survives / fixes itself ~150 ms after the burst. **If the hole
comes back despite the fix**, the cause is upstream of the repaint (non-standard post-resize state of the
Kova grid, OR ink's renderer) and the isolating test will be needed:
replay a stream WITH the resizes injected at the right offsets in Kova vs tmux and
diff the grids — which requires instrumenting the PTY capture (`pty.rs`) to
log winsize changes + offset.

## Tooling (test-only)

- `KOVA_REPLAY_BYTES=<n>` (truncates the stream) and `KOVA_REPLAY_QUIET` added to
  `replay_capture_file` (`parser.rs`) for bisection by prefixes.
- Snapshot of the hole: session scratchpad `kova_hole_229.json`; capture
  copied as `cap_229.raw`.

---

# Round 5 (2026-07-24): CAUSE FOUND — Claude Code emits `2J` + a PARTIAL repaint when the winsize flaps, and it is our nudge that makes it flap

Repro: pane 19, 47-line hole (rows 7..53) on an
89×65 grid, captured live with its fresh PTY capture
(`pty-capture-84103-19.raw`, copied to the scratchpad as `cap_19.raw` +
`kova_hole_19_visible.json`).

## Established facts (VERIFIED, offsets in cap_19.raw)

1. **The hole is deterministic from the bytes**: `replay_capture_file` at
   89×65 reproduces the live grid exactly (band 7..53, cursor row 61).
   Kova faithfully renders what the app emitted — consistent with the tmux test of
   round 4.
2. **The faulty frame is in the stream**: the app emits `CSI 2J` (full clear)
   THEN a **partial** repaint (~2 KB instead of ~4.3 KB for a full
   frame) that skips the middle with a `CSI 34/44 B`. After a 2J everything is empty:
   the skipped rows stay empty. Offsets of the `2J+partial` frames:
   956947 (64 lines, CUD 34), 964702 (65 l., CUD 44), **972442** (65 l.,
   CUD 44 — the last one, the one that leaves the final hole). The following
   frames (differential, `[H` + ~6 lines at the top + `CR`+`[46/47B` +
   bottom block) never repaint the middle: the app believes those rows
   still contain the transcript text.
3. **The triggering context is a 65↔64 flapping of the winsize**: real resize
   93×84 → 89×65 at offset ~939214 (last `[84;1H`) / 943484 (first
   `[65;1H`). Right after, the app does TWO healthy full repaints (full 2Js,
   4.2 KB, offsets 939288 and 943558) — the real resize was already handled well.
   Then the rows±1 nudges of OUR round 4 mitigation (`repaint_pane_settle`
   + `PtyRestore`) make it flap 65→64→65 several times (3 `[64;1H` frames,
   947743..972368): it is on these round-trips that the app outputs its
   `2J+partial` frames. The last nudge ended on a broken frame, nothing
   forces a repaint afterwards → permanent hole.
4. Responsibility assessment: **renderer bug on the Claude Code/ink side**
   (differential computed against its cache while it has just cleared the
   physical screen with 2J, when the size quickly comes back to an already seen value);
   **Kova is an accomplice** because its nudge mitigation creates exactly this
   A→B→A pattern, including when it was useless (the app had already
   full-repainted the real resize).

## Why rounds 1-4 were not enough

Round 4 had the right lead (resize) but the wrong weapon: the repaint
forced by nudge fixes the case "the app coalesced the SIGWINCHs and redrew
nothing", and AT THE SAME TIME causes the "2J+partial" case in ink. The
mitigation heals and reinfects.

## Fix applied (2026-07-24, IN TEST — build done, Kova restart required)

Two defensive parts, implemented:

1. **Nudge only if necessary** (`should_skip_settle_nudge`,
   `window.rs`): `repaint_pane_settle` skips the nudge when the app has already
   addressed ≥ 80% of the rows since the last winsize change
   (`TerminalState::rows_touched`, reset on each `term.resize` /
   nudge / restore; fed by print, EL, ICH/DCH/ECH, IL/DL — NOT by
   ED, a bulk clear is not painting) AND the grid has no interior
   empty band ≥ 8 rows (`interior_blank_band`, a BCE-colored or
   attributed row counts as content). In the captured case, it was a useless
   nudge (the app had already full-repainted the real resize) that triggered
   the hole.
2. **Bounded self-healing** (`fire_post_restore_band_checks`, `window.rs`):
   ~0.5 s (30 frames) after each winsize restore, grid scan;
   if there is an interior empty band ≥ 8 rows in alt-screen → forced re-repaint,
   max 2 attempts per pane between two real resizes (INFO log on each
   attempt, WARN on giving up). In cap_19.raw, 4 nudge cycles out of 6
   end in a healthy full repaint → one retry almost always
   converges.

Tests: 111 green, including `cleared_screen_plus_partial_repaint_leaves_interior_band`
(parser.rs — the distilled signature of cap_19.raw: full paint → `2J` +
partial repaint → band detected at the exact rows), the tests of
`interior_blank_band` / `row_coverage` (mod.rs) and of
`should_skip_settle_nudge` (window.rs).

Validation: restart Kova (the running instance = binary from 07/22 without the
fix), then use it normally (resizes, minimize/restore, scroll in
Claude Code panes). If a hole appears and stays > 1 s, look in the log for
`post-restore check:` — if it says "giving up", the app answered with 3 broken
frames in a row; fetch the pane's PTY capture and bisect as here.

Upstream: the exact pattern (fast 65→64→65 flap → `2J` + partial
differential frame) can be reported to Anthropic with `cap_19.raw` as a
byte-for-byte repro (replayable via `replay_capture_file` at 89×65).

# Round 6 (2026-07-29): the hole ALSO happens without a resize — trigger = scrolling, and Kova was doubling the mouse reports

Live repro: pane 11, 98×65 grid, empty band at rows 9..49
(41 rows), cursor row=61 col=2, alt-screen. Capture
`pty-capture-63758-11.raw`, 1,652,462 bytes.

A DIFFERENT mechanism from round 5: here the app never clears the screen (no `2J`)
and no resize is involved. Round 5 remains valid for its case (winsize
flap caused by our nudges); it does not cover this one.

## Established facts (VERIFIED)

1. **The hole is in the grid, not in the rendering**: `get-pane-content` mode
   `visible` reads `self.grid` (`terminal/mod.rs:465`) and shows the 41 empty
   rows. So it is not a Metal framebuffer problem.
2. **It follows the app's scroll position**: scrolled up → hole;
   back down to the bottom → clean grid. Verified on three successive dumps of the
   same pane. Claude Code is in alt-screen and has mouse tracking active, so
   the wheel is forwarded to it and it is IT that repaints
   (`window.rs:820-832`) — Kova's scrollback is not involved.
3. **Deterministic outside the app**: `replay_capture_file` at 98×65 reproduces
   the live grid exactly (band 9..49, cursor row=61 col=2).
4. **tmux 3.6a gives the same thing**: same bytes, same size, band 9..49
   and same line lengths. Kova faithfully renders what the app emitted.
5. **No resize**: last event for pane 11 in `kova.log` =
   `settle repaint skipped for pane 11 … no hole` at 13:42:59; the hole
   appeared around 15:00 with no resize in between.
6. **No erase in the scroll stream** (208,956 bytes): 0 `CSI J`,
   0 `IL`/`DL`, 0 `SU`/`SD`, 0 `LF`. Only 9,164 `CHA`, 2,207 `CUD`,
   2,207 `CR`, 998 `EL`, 189 `CUP`, framed by `?2026h`/`?2026l`.
7. **Birth dated to the frame** (by replaying at each `?2026l`):
   last clean frame at offset 1,564,755; the next one (1,569,555) already has
   6 empty rows; the band grows by 2 to 4 rows per frame — 6, 10, 11,
   15, 16, 20, 21, 25, 27, 31, 33, 37, 39 — then saturates at 41.
8. **Mechanism**: at each notch, the app redraws its block 4 rows lower than
   the previous frame and never writes the rows freed at the top. The
   cursor trace of the switching frame writes nothing on rows
   0, 1, 2, 4, 6.
9. **Kova / Terminal.app difference on INPUT** (probe, same gesture):
   Kova 676 reports in 17.8 s of which 250 exact duplicates (same button, same
   column, same line) less than 2 ms apart, i.e. 37%; Terminal.app 147 reports,
   0 duplicates, one 12-byte write per report. Rate ~38/s vs ~19/s.
10. **Cause of the doubling**: `setAcceptsMouseMovedEvents(true)` on the window
    AND an `NSTrackingArea` with `MouseMoved` on the view → `mouseMoved:` delivered
    twice per physical movement. Fixed by removing the window flag
    (`window.rs:6154`); the tracking area is the more precise source (bounded
    to the view, key window only). The ~35 remaining wheel duplicates
    come from the one-event-per-line loop (`window.rs:826-831`), not
    touched.

## Not elucidated

- **Causal link doubling → hole: NOT proven.** What is established is
  a measured behavior difference between Kova and a terminal where the bug
  was never observed, in the expected direction (the app receives twice as many
  commands). The validation is the test below.
- **7.2 s without any event when the probe was launched**, although the pointer
  was indeed in the pane. The Kova log only records scrolling in the
  scrollback branch (`window.rs:840-846`), so the mouse branch is silent and
  we cannot tell "macOS delivered nothing" from "Kova filtered it".
  Readable but unproven suspect: `scroll_axis_lock` is window-global
  and is only reset on trackpad phases (`window.rs:782-801`) — a
  wheel mouse never sets it back to `None`, and vertical is ignored while
  it is `Horizontal`. Settling it would need a log in the mouse
  branch.

## Blind spot of the round 5 safeguards

`resize_settle` and `post_restore_checks` are only armed in the resize
path (`window.rs:4797` and the winsize restore). A hole born from a scroll is
therefore never detected or repaired — consistent with the observation that it stays
on screen until you scroll back down.

## Tooling

`scripts/mouse-probe.py`: enables modes 1000/1002/1003/1006, puts the tty in
raw mode, decodes the SGR reports and traces for each one the gap in milliseconds and
the size of the write. Trace in `~/Downloads/mouse-probe-<label>.log`. Used
to compare two terminals on the same gesture — it is what produced the 37%.

## To validate (next action)

Restart Kova (the running binary predates the fix), run the probe again →
zero motion duplicates expected, then scroll up again in a well-filled
Claude Code session. If the hole comes back anyway, next suspect: the
burst of one event per wheel line.

---

# Round 7 (2026-09-23): mixed-up letters ("Les 1410Gmide") — a hook was writing to the tty

Symptom: a few wrong characters in the middle of a Claude Code line (`Les 1410Gmide trajet`
for `Les 14 min de trajet`), fixed as soon as the text is highlighted with the mouse.

Cause (VERIFIED, `pty-capture-4025-106.raw`, offset ~164,000): the stream contains
`ESC [ 7G 14 ESC [` **`ESC ] 133;D BEL BEL`** `10G min`. Claude Code's `Stop` hook did
`printf '\033]133;D\007\a' > /dev/$PARENT_TTY` while Claude Code was still writing its frame:
two writers on the same tty, the OSC's `ESC` aborts the CSI in progress and `10G` goes out as text.
Kova parses correctly (that is the VT semantics); the grid really is wrong, and Claude's
differential rendering never repaints the line. Selection fixes it because Claude Code has
mouse reporting active: it redraws the selected line itself ("copied 13 chars").
Measurement: 30 OSC 133;D landing in the middle of a CSI in the captures of a single day.

Fix: IPC command `mark-completed` (same effect as 133;D, `TerminalState::mark_command_completed`),
and the hook calls a small status script that sends it over IPC instead of writing to the tty. Same treatment for the
three hooks that wrote a bare `\a` (`Stop`, `permission_prompt`, `idle_prompt`): IPC command
`bell`, because a BEL in the middle of a Claude OSC (title, link) would terminate it too — that case
was not verified in the captures. No hook writes to the tty any more.

**Follow-up, 2026-09-24 — the fix was not active.** Same symptom in pane #2 ("B✻Waiting",
spaces replaced by the letters of the previous line). `pty-capture-76193-2.raw`, offset ~1,758,061:
`CSI 1` cut by `ESC ] 133;D BEL BEL`, the `B` goes out as text. Cause: the hook fix had only been
written to the version-controlled copy of the Claude Code settings, while `~/.claude/settings.json` was no longer
the symlink to it but an ordinary file, still on the old
`printf … > /dev/$PARENT_TTY`. The link had been broken by a script doing
`jq … > $TMP.out && mv "$TMP.out" ~/.claude/settings.json`: `mv`
replaces a symlink instead of writing through it. Claude Code is not to blame
for it: Edit refuses to write to a symlink (tested in 2.1.281). The four hooks were fixed
in the live file. The sentence "no hook writes to the tty any more" was only true from
then on. Lesson: after a hook fix, check the file Claude Code actually reads
(`grep PARENT_TTY ~/.claude/settings.json`), not the one in the repo. Repaired the same day: the
live version copied back into the repo, symlink restored, and a guard now refuses any
`mv`/`rm`/`sed -i` that would target a symlink under `~/.claude`. A tab-renaming command also wrote an
OSC 1 to the tty (same possible collision): it now goes through `rename-pane`.
