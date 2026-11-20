# Memory and CPU audit (2026-11-19)

Read-only review after the machine saturated (figures reported by the user, not re-measured here: 36 GB, 18 GB compressed, load 34).
Each item says whether it is **measured** (command run on 2026-11-19 around 19:20, Kova pid
9991 started 09:09) or **inferred** from the code. Ranked by expected gain.

## Measured state

- `list-panes` on the socket: 93 panes, 7 tabs, 1 window. Tab 0 alone holds 74 panes.
  0 paused, 4 minimized, 40 restored asleep (`dormant_session_id` set).
- `ps` crossed with `list-panes` pids (ancestry walk): 55 `claude` processes on the machine.
  52 sit under a live pane shell. The other 3 are not Kova's: two `claude -p` children of a
  Python process, one in another app. **No orphan**: no `claude` and no `zsh` with ppid 1.
- `footprint` on the claude pids listed in `child_processes` of `list-panes`: 51 still alive
  at re-measure, 15.8 GB total (about 310 MB each). RSS (MiB): the 52 pane claudes 14 733,
  their whole process trees 15 071 across 337 processes (156 `mcp-lazy`, 151 MiB in all).
- Of the 52, 51 are not `working` (14 332 MiB of tree RSS); 11 have been `awaiting` for 2 h or
  more (3.2 GB of tree RSS).
- No duplicate `claude_session_id` across panes.
- Kova itself: `footprint` 364 MB. Malloc Small 142 MB, graphics (unmapped) 114 MB,
  IOSurface 42 MB, IOAccelerator 20 MB, Malloc Large 11 MB. `top`: 0 to 4.3 % CPU at rest,
  101 threads.
- `~/.config/kova/config.toml` does not set `scrollback` or `fps`: defaults 10 000 lines, 60 fps.
- Log of the 09:09 restart (`kova.log.1`): `Shutting down 88 PTY(s)`, then 60 lines
  `reap_child: pid N still alive after SIGKILL`. None of those 60 pids exists now.
- `kova.log` since 09:09: 46 lines `Restore: pane N shown, resuming its agent`, 2 `Pause:`.

## Conclusion first

Kova does not leak claude processes today: every one of them is a conversation held by a
pane. The ~16 GB measured tonight is **52 live agent sessions that only a manual pause puts
back to sleep** (2 pauses logged today). Kova's own cost (364 MB, a few % CPU) is about 2 %
of that. On Kova's side the big lever is the agent lifecycle, not the renderer. The rest of
the machine (another app 1.4 GB, Slack 0.9 GB, etc. by RSS) was not part of this audit.

## 1. Idle agents only sleep when paused by hand — gain up to ~14 GB (inferred from measured RSS)

- `src/window/tick.rs:564-578` `wake_shown_panes`: a restored agent resumes the first time
  its pane is drawn in the active tab, focused or not. In a 74-pane tab with horizontal
  scroll, every column scrolled past wakes its claude (inferred from the code).
- `src/surface.rs:916-950` `Surface::pause`: hangs up the agent (SIGHUP, SIGKILL after 2 s),
  keeps the shell and the conversation id, refuses while the agent works. It exists, but
  only `p` in Cmd+P and the `pause-pane` IPC call it.
- Fix A: an auto-pause sweep. A pane whose agent is not working, has no background shells,
  is not focused, and has not been focused for N hours gets `pause()`. The refusal logic is
  already there. Gain inferred: last-focus time is not exposed, so the closest measured proxy
  is `awaiting` for 2 h or more, 11 panes and 3.2 GB tonight; all 51 idle sessions are
  14.3 GB of tree RSS.
- Fix B: wake on focus only, not on "shown". Scrolling across a tab would no longer start
  claudes. This changes a behavior written in the README ("focused or not"), so it is
  your decision, not a fix.
- Risk: a paused conversation takes a few seconds to come back, and its next turn misses the
  prompt cache (token cost, inferred). An agent waiting on a long external thing it did not
  start as a background shell could be paused under it (inferred).

## 2. Scrollback cell layout — gain ~40 % of Kova's heap (inferred)

- `src/terminal/mod.rs:135-149`: `Cell` is 32 bytes; `cluster: Option<Box<str>>` takes 16 of
  them and is almost always `None`. 10 000 lines per pane (`src/config.rs:260`), enforced on
  every push (`mod.rs:1163-1172`), trailing blanks trimmed. Worst case 25 MB per 80-col pane,
  64 MB at 200 cols.
- The 142 MB Malloc Small for 93 panes fits that (about 1.5 MB per pane); the attribution to
  scrollback is inferred, nothing splits the heap per pane.
- Fix: move clusters to a per-terminal side table indexed by a `u16`/`u32` (the
  `hyperlink_id` pattern). Cell drops to about 16-20 bytes. Risk: medium, touches the whole
  model; the size test at `mod.rs:3241` guards the layout.
- Cheaper variant: a lower scrollback for panes running an agent, since the conversation
  lives in its transcript. Risk: low, but it is a visible behavior change.

## 3. Per-tick CPU with 93 panes — gain: idle CPU floor (inferred, 4 % measured)

- `src/app.rs:227-233`: one global `NSTimer` at 60 fps. `KovaView::tick` has no early-out:
  each tick sweeps every pane of every tab (bell, completion, running, unread, highlight,
  inject), builds `PaneRenderData` with string clones and `tab_titles`, then `render_panes`
  skips the GPU only if nothing is dirty. Order of magnitude: 5 to 8 lock acquisitions per
  pane per tick, so 30 000 to 45 000 per second at 93 panes (inferred).
- No occlusion or minimized-window check: `grep occlusion|isMiniaturized` finds nothing in
  `src/` (verified). A window hidden behind others still ticks and draws.
- `src/terminal/pty.rs:485`: each reader thread polls with a 100 ms timeout to see the
  shutdown flag. 93 threads = about 930 wakeups per second at rest (inferred; 101 threads
  measured).
- `src/agent_session.rs` `for_shell`: for a pane that is not Claude, the Codex, omp, tazu
  and Pi scanners each walk the whole process table (1 s cache each), plus the Claude scan
  reads every `~/.claude/sessions/*.json`. Called every 30 ticks per pane via
  `Surface::check_running`.
- Fixes, by ratio: (a) wake reader threads with a self-pipe instead of a 100 ms poll
  (low risk); (b) one shared `proc_listpids` pass per second for all scanners (low risk);
  (c) skip the full tick for occluded windows, keep reaping and IPC (low-medium risk);
  (d) drop to ~10-20 fps after 1 s without output or input, back to 60 on any event
  (medium: several timers count frames, not time).

## 4. Graphics memory — gain tens of MB (inferred)

- `src/renderer/mod.rs:685, 809-827`: 2 vertex buffers of 8 MB per window; grown to the next
  power of two (`mod.rs:1720-1743`), never shrunk.
- `src/renderer/pane_renderer.rs:391`: each rebuilt pane reserves `rows × cols × 6` vertices
  of 48 bytes (288 bytes per cell, about 3 MB for 200×50) and keeps that capacity. Only
  on-screen panes of the active tab hold a cache.
- `src/renderer/glyph_atlas.rs:1313-1388`: the atlas doubles up to 16384 px, never shrinks,
  and keeps a full CPU copy `atlas_buf`.
- The 114 MB "unmapped graphics" is not attributed by `footprint`; the share of each item
  above is unknown.
- Fixes: `shrink_to` when capacity exceeds 2× length; drop `atlas_buf` and upload sub-rects.
  Low risk.

## 5. Smaller items

- `get-pane-content`: two copies of the full text on the main thread under the terminal read
  lock (`dump_text` then `json!`, `src/ipc/host.rs:462-470`), a third on the client thread
  (`to_json` clones `data`, `src/ipc.rs:258`). Fix: consume by value, build the JSON outside
  the lock.
  Low risk, matters for agents that read panes in loops.
- `src/terminal/pty.rs:446-449, 520-523`: PTY capture on by default, one unbuffered write per
  4 KB read. 242 capture files, `~/Library/Logs/Kova` = 117 MB (measured). Fix: off by default
  or `BufWriter`. Also a privacy issue already noted in `notes/pane-open-perf.md`.
- Hyperlink table: linear dedup, never pruned (`terminal/mod.rs:538`). Low risk fix.

## 6. Process lifecycle — risks with no instance observed today

- Close pane / tab / window: `Pty::drop` (`pty.rs:861-914`) SIGHUPs the shell and reaps the
  foreground job with `killpg` SIGHUP then SIGKILL after 2 s. Sound.
- **Quit** (`pty.rs:840-859` `shutdown_all`, verified): only the shell gets SIGHUP, SIGTERM,
  SIGKILL 25 ms apart; the foreground job (claude) gets no `killpg`, it relies on zsh
  forwarding SIGHUP and on the tty hangup. The 60 "still alive after SIGKILL" warnings at
  09:09: none of those pids exists at 19:20 (measured); that they come from the 75 ms budget
  under load is inferred, the load at 09:09 was not measured.
  Fix: `killpg` SIGHUP every foreground job first, one shared 2 s deadline, then SIGKILL;
  widen the shell step. Risk: quit up to 2 s slower.
- Crash or `kill -9` of Kova: no cleanup handler (`main.rs` handles only SIGSEGV, SIGBUS,
  SIGABRT); teardown relies on the kernel hangup alone.
- Restore and Cmd+Shift+Z reopen do not call `claude_session::end_orphan` before resuming,
  unlike the search palette (`search_palette.rs:809`). A survivor from a crash would get a
  second writer on the same transcript. Fix: one `ensure_single_writer(id)` used by all
  resume paths. Low risk.
- Restore spawns a shell for every saved pane of every tab in the background
  (`tick.rs:581-640`). The cap of 4 loading shells is checked per tab, and a whole tab starts
  at once (`tick.rs:603-621`), so a 74-pane tab starts 74 shells together. 93 shells at
  launch even for tabs never opened. Claude itself stays lazy.

## What this audit did not establish

- The morning count (86 claudes, 13.7 GB) was not observed here, so whether some of them were
  outside Kova then is unknown.
- The fall to 11 after the 09:09 restart and the climb back match lazy restore (46 resumes
  logged since), but nothing here proves which panes were resumed by scrolling versus focus.
