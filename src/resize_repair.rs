//! The repairs that follow a resize, shared by Kova.app's window and the
//! library's panes (`embed`): the round-trip nudge, the debounced settle
//! repaint (soft reset + winsize nudge), the deferred winsize restore, and the
//! blank-band check after that restore. Moved here from `window/resize.rs` and
//! `window/tick.rs` without changing what they do.
//!
//! Time is counted in frames: Kova.app steps the countdowns by one per tick of
//! its render timer, a library pane by the number of frames (at the config's
//! fps) that elapsed since its last tick (`FrameClock`).
//!
//! Why each repair exists: a differential TUI (Claude Code) can coalesce a
//! burst of SIGWINCHs into one no-op, or answer a winsize change with a clear
//! screen followed by a partial repaint, and leave a stale blank band on
//! screen (notes/display-glitches.md).

use std::collections::HashMap;
use std::time::Instant;

use crate::surface::{PaneId, Surface};

/// Minimum height (rows) of an interior blank band before it is treated as a
/// hole worth repairing. Real holes span dozens of rows; small gaps are
/// normal UI spacing.
pub const BAND_MIN_ROWS: usize = 8;
/// Row-coverage fraction above which the settle nudge is skipped: the app
/// demonstrably repainted (almost) the whole screen since the last winsize
/// change, so nudging again is pure risk — the winsize flap it causes can
/// make Claude Code emit a clear-screen followed by a partial repaint (the
/// hole, see notes/display-glitches.md round 5).
pub const SETTLE_SKIP_COVERAGE: f32 = 0.8;
/// Frames (~0.5s at 60fps) after a winsize restore before scanning the grid
/// for a leftover hole.
pub const POST_RESTORE_CHECK_FRAMES: u32 = 30;
/// Max automatic band-repair repaints per pane between real resizes, so a
/// program that keeps answering nudges with broken frames can't loop us.
pub const MAX_BAND_REPAIRS: u32 = 2;
/// Frames between a winsize nudge and its restore (~50ms @60fps), so the
/// foreground program sees two distinct SIGWINCHs with two distinct sizes.
pub const RESTORE_FRAMES: u32 = 3;
/// How long a size stays in a pane's history for round-trip detection.
const ROUND_TRIP_WINDOW_MS: u128 = 500;

/// Frames of quiet after the last resize before the settle repaint fires
/// (~150ms at `fps`).
pub fn settle_frames(fps: u32) -> u32 {
    (fps / 6).max(4)
}

/// The rows a nudge sets the winsize to: one less, or one more for a 1-row
/// pane.
pub fn nudged_rows(rows: u16) -> u16 {
    if rows > 1 { rows - 1 } else { rows + 1 }
}

/// Pending restore of a PTY winsize after a nudge, or a pending hole check
/// after a restore. The restore must NOT happen back-to-back with the nudge:
/// two immediate TIOCSWINSZ calls coalesce their SIGWINCHs, and the foreground
/// program then reads a winsize identical to its cached one — programs that
/// compare old == new size skip the redraw entirely.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Countdown {
    pane_id: PaneId,
    remaining_frames: u32,
}

/// One debounce step for the per-pane resize-settle countdowns: decrement
/// every entry by `advance` frames and return the pane ids whose countdown
/// reached 0 (removing them from the map). A fresh resize re-arms a pane by
/// re-inserting its countdown, so the burst only fires once, after it stops.
fn step_resize_settle(map: &mut HashMap<PaneId, u32>, advance: u32) -> Vec<PaneId> {
    let mut fire: Vec<PaneId> = Vec::new();
    map.retain(|&pane_id, frames| {
        *frames = frames.saturating_sub(advance);
        if *frames == 0 {
            fire.push(pane_id);
            false
        } else {
            true
        }
    });
    fire
}

/// Decrement a list of countdowns by `advance` frames; return the panes that
/// came due, in list order, and drop them.
fn step_countdowns(list: &mut Vec<Countdown>, advance: u32) -> Vec<PaneId> {
    let mut due = Vec::new();
    for c in list.iter_mut() {
        c.remaining_frames = c.remaining_frames.saturating_sub(advance);
        if c.remaining_frames == 0 {
            due.push(c.pane_id);
        }
    }
    list.retain(|c| c.remaining_frames > 0);
    due
}

/// One slot per pane: re-arming a pane replaces its own pending entry without
/// dropping another pane's (a dropped restore would leave that PTY one row
/// short permanently).
fn arm(list: &mut Vec<Countdown>, pane_id: PaneId, frames: u32) {
    list.retain(|c| c.pane_id != pane_id);
    list.push(Countdown { pane_id, remaining_frames: frames });
}

/// Whether the post-resize settle repaint (winsize nudge) can be skipped.
/// Skip only when the app already repainted broadly AND the grid shows no
/// hole — both conditions observable, so this is strictly safer than always
/// nudging (the round-4 behavior) which can itself re-trigger the hole.
fn should_skip_settle_nudge(row_coverage: f32, has_interior_band: bool) -> bool {
    !has_interior_band && row_coverage >= SETTLE_SKIP_COVERAGE
}

/// Record that a pane left size `old` for size `new` at `now`, in its bounded
/// history. True when `new` is a size the pane had less than 500ms ago: the
/// child may coalesce the SIGWINCHs into one no-op and skip its repaint while
/// our reflow round-trip lost information. The history is cleared then.
fn note_resize(history: &mut Vec<((u16, u16), Instant)>, old: (u16, u16), new: (u16, u16), now: Instant) -> bool {
    history.retain(|&(_, t)| now.duration_since(t).as_millis() < ROUND_TRIP_WINDOW_MS);
    if history.iter().any(|&(sz, _)| sz == new) {
        history.clear();
        return true;
    }
    history.push((old, now));
    if history.len() > 8 {
        history.remove(0);
    }
    false
}

/// Finds a pane by id and hands it, with whether it is minimized, to the
/// callback; does nothing when the pane is gone.
pub type PaneLookup<'a> = dyn FnMut(PaneId, &mut dyn FnMut(&Surface, bool)) + 'a;

/// The resize-repair state of a set of panes (a Kova.app window, or one
/// library pane).
#[derive(Default)]
pub struct ResizeRepair {
    /// Deferred PTY winsize restore after a nudge.
    pty_restore: Vec<Countdown>,
    /// Recent pane sizes (bounded history per pane) for round-trip
    /// detection: a rapid return to ANY recently-seen size can coalesce the
    /// SIGWINCHs — the child reads an unchanged winsize and skips its
    /// repaint while our grid went through a lossy reflow round-trip.
    recent_resizes: HashMap<PaneId, Vec<((u16, u16), Instant)>>,
    /// Debounce countdown (frames) per pane until a post-resize robust repaint
    /// fires. Reset on every resize; when it hits 0 (the resize burst has
    /// settled) the Cmd+R repaint (soft_reset + winsize nudge) runs once,
    /// so a differential TUI (Claude Code) fully repaints against a clean
    /// state instead of leaving stale blank bands after a resize.
    resize_settle: HashMap<PaneId, u32>,
    /// Countdown (frames) per pane until a post-nudge-restore hole check runs:
    /// the app may answer the restore SIGWINCH with a clear-screen followed by
    /// a PARTIAL differential repaint (skipping rows it wrongly believes
    /// unchanged), leaving a permanent blank band. ~0.5s after each restore we
    /// scan the grid and force another repaint if a band is found.
    post_restore_checks: Vec<Countdown>,
    /// Band-repair repaints already fired per pane since its last real resize,
    /// bounding the nudge → broken frame → re-nudge loop (see MAX_BAND_REPAIRS).
    band_repair_attempts: HashMap<PaneId, u32>,
}

impl ResizeRepair {
    /// Drop expired resize histories (also clears entries of closed panes).
    pub fn prune_history(&mut self, now: Instant) {
        self.recent_resizes
            .retain(|_, h| h.iter().any(|&(_, t)| now.duration_since(t).as_millis() < ROUND_TRIP_WINDOW_MS));
    }

    /// Restore the real winsize of `pane_id` in `RESTORE_FRAMES` frames.
    pub fn schedule_restore(&mut self, pane_id: PaneId) {
        arm(&mut self.pty_restore, pane_id, RESTORE_FRAMES);
    }

    /// The grid of `surface` and its PTY just went from `old` to `new` (a real
    /// resize). Opens a fresh band-repair budget, nudges the child at once on
    /// a round trip, and (re)arms the settle repaint `settle` frames ahead.
    pub fn on_resized(&mut self, surface: &Surface, old: (u16, u16), new: (u16, u16), now: Instant, settle: u32) {
        let pane_id = surface.id;
        self.band_repair_attempts.remove(&pane_id);
        let history = self.recent_resizes.entry(pane_id).or_default();
        if note_resize(history, old, new, now) {
            surface.pty.resize(new.0, nudged_rows(new.1));
            self.schedule_restore(pane_id);
        }
        self.resize_settle.insert(pane_id, settle);
    }

    /// One step of every countdown, `advance` frames at once (Kova.app: 1),
    /// in Kova.app's frame order: due winsize restores, then settle repaints,
    /// then post-restore hole checks.
    pub fn tick(&mut self, advance: u32, lookup: &mut PaneLookup<'_>) {
        if advance == 0 {
            return;
        }
        self.fire_due_pty_restores(advance, lookup);
        self.fire_resize_settle_repaints(advance, lookup);
        self.fire_post_restore_band_checks(advance, lookup);
    }

    /// Re-send the winsize to the panes whose nudge has come due.
    fn fire_due_pty_restores(&mut self, advance: u32, lookup: &mut PaneLookup<'_>) {
        let due = step_countdowns(&mut self.pty_restore, advance);
        if due.is_empty() {
            return;
        }
        for &pane_id in &due {
            lookup(pane_id, &mut |surface, _minimized| {
                // Dims are re-read at fire time: a resize in between already
                // set the PTY to the new size, and the terminal grid is the
                // source of truth for what the winsize should be.
                let (cols, rows) = {
                    let t = surface.terminal.read();
                    (t.cols, t.rows)
                };
                surface.pty.resize(cols, rows);
                // Measure the app's response to this restore SIGWINCH from a
                // clean slate.
                surface.terminal.write().reset_rows_touched();
            });
        }
        // The app's answer to a winsize restore is the frame that can carry
        // the hole (clear + partial repaint). Schedule a grid scan once that
        // frame has certainly landed. The check is stepped once more in this
        // same tick (Kova.app's frame order); when this tick covers several
        // frames, they elapsed before the restore and must not count.
        let frames = POST_RESTORE_CHECK_FRAMES.saturating_add(advance - 1);
        for &pane_id in &due {
            arm(&mut self.post_restore_checks, pane_id, frames);
        }
    }

    /// When a pane's settle countdown reaches 0 (no resize for ~150ms), run
    /// the same robust repaint as Cmd+R so the foreground program fully
    /// repaints against a clean grid.
    fn fire_resize_settle_repaints(&mut self, advance: u32, lookup: &mut PaneLookup<'_>) {
        for pane_id in step_resize_settle(&mut self.resize_settle, advance) {
            self.repaint_settle(pane_id, lookup);
        }
    }

    /// ~0.5s after each winsize restore, scan the pane's grid: if the app
    /// answered the restore with a clear-screen + partial repaint (interior
    /// blank band in alt-screen), force another robust repaint — bounded by
    /// MAX_BAND_REPAIRS per pane between real resizes so a misbehaving app
    /// can't loop us.
    fn fire_post_restore_band_checks(&mut self, advance: u32, lookup: &mut PaneLookup<'_>) {
        for pane_id in step_countdowns(&mut self.post_restore_checks, advance) {
            let mut band = None;
            lookup(pane_id, &mut |surface, minimized| {
                let t = surface.terminal.read();
                if !minimized && t.in_alt_screen {
                    band = t.interior_blank_band(BAND_MIN_ROWS);
                }
            });
            match band {
                Some((start, end)) => {
                    let n = self.band_repair_attempts.entry(pane_id).or_insert(0);
                    if *n < MAX_BAND_REPAIRS {
                        *n += 1;
                        log::info!(
                            "post-restore check: pane {} has a {}-row blank band (rows {}..{}) — forcing repaint (attempt {}/{})",
                            pane_id, end - start + 1, start, end, *n, MAX_BAND_REPAIRS
                        );
                        self.repaint_settle(pane_id, lookup);
                    } else {
                        log::warn!(
                            "post-restore check: pane {} still holed after {} repaints — giving up until next resize",
                            pane_id, MAX_BAND_REPAIRS
                        );
                    }
                }
                None => {
                    // Clean grid or pane closed: reset the repair budget.
                    self.band_repair_attempts.remove(&pane_id);
                }
            }
        }
    }

    /// Robust repaint for a pane, as Cmd+R without its border flash: soft
    /// reset, then a winsize nudge whose restore comes `RESTORE_FRAMES` later.
    /// Skipped for a minimized pane, and when the app already repainted
    /// broadly without leaving a hole.
    fn repaint_settle(&mut self, pane_id: PaneId, lookup: &mut PaneLookup<'_>) {
        let mut nudged = false;
        lookup(pane_id, &mut |surface, minimized| {
            if minimized {
                return;
            }
            let (cols, rows) = {
                let t = surface.terminal.read();
                if should_skip_settle_nudge(t.row_coverage(), t.interior_blank_band(BAND_MIN_ROWS).is_some()) {
                    log::debug!(
                        "settle repaint skipped for pane {}: app repainted {:.0}% of rows since resize, no hole",
                        pane_id,
                        t.row_coverage() * 100.0
                    );
                    return;
                }
                (t.cols, t.rows)
            };
            {
                let mut t = surface.terminal.write();
                t.soft_reset();
                // Fresh coverage window: measure what the app repaints in
                // response to this nudge, not what came before it.
                t.reset_rows_touched();
            }
            surface.pty.resize(cols, nudged_rows(rows));
            nudged = true;
        });
        if nudged {
            self.schedule_restore(pane_id);
        }
    }

    /// Test-only views of the pending work for one pane.
    #[cfg(test)]
    pub fn pending(&self, pane_id: PaneId) -> (Option<u32>, Option<u32>, Option<u32>) {
        let find = |l: &Vec<Countdown>| l.iter().find(|c| c.pane_id == pane_id).map(|c| c.remaining_frames);
        (find(&self.pty_restore), self.resize_settle.get(&pane_id).copied(), find(&self.post_restore_checks))
    }
}

/// A frame counter for a host whose clock is not Kova's: how many frames at
/// `fps` have elapsed since the previous call, so the countdowns above mean
/// the same real time in a library pane as in Kova.app.
pub struct FrameClock {
    start: Instant,
    frames: u64,
}

impl FrameClock {
    pub fn new(now: Instant) -> Self {
        FrameClock { start: now, frames: 0 }
    }

    /// Frames elapsed since the last call (0 when the host ticks faster than
    /// `fps`). Computed from the start, so no rounding drift accumulates.
    pub fn advance(&mut self, now: Instant, fps: u32) -> u32 {
        let total = (now.saturating_duration_since(self.start).as_secs_f64() * fps.max(1) as f64).floor() as u64;
        let step = total.saturating_sub(self.frames);
        self.frames = self.frames.max(total);
        step.min(u32::MAX as u64) as u32
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn resize_settle_fires_once_when_burst_ends() {
        let mut m: HashMap<PaneId, u32> = HashMap::new();
        m.insert(7, 3);
        assert!(step_resize_settle(&mut m, 1).is_empty()); // 3 -> 2
        assert!(step_resize_settle(&mut m, 1).is_empty()); // 2 -> 1
        assert_eq!(step_resize_settle(&mut m, 1), vec![7]); // 1 -> 0, fires
        assert!(m.is_empty(), "fired pane is removed");
        assert!(step_resize_settle(&mut m, 1).is_empty(), "no re-fire");
    }

    #[test]
    fn resize_settle_rearms_and_defers_fire() {
        let mut m: HashMap<PaneId, u32> = HashMap::new();
        m.insert(1, 2);
        assert!(step_resize_settle(&mut m, 1).is_empty()); // 2 -> 1
        // A new resize re-arms the same pane before it could fire.
        m.insert(1, 2);
        assert!(step_resize_settle(&mut m, 1).is_empty()); // 2 -> 1 (not 0)
        assert_eq!(step_resize_settle(&mut m, 1), vec![1]); // 1 -> 0, fires now
    }

    #[test]
    fn resize_settle_handles_multiple_panes_independently() {
        let mut m: HashMap<PaneId, u32> = HashMap::new();
        m.insert(1, 1);
        m.insert(2, 3);
        assert_eq!(step_resize_settle(&mut m, 1), vec![1]); // pane 1 fires; pane 2: 3 -> 2
        assert!(m.contains_key(&2) && !m.contains_key(&1));
        assert!(step_resize_settle(&mut m, 1).is_empty()); // pane 2: 2 -> 1
        assert_eq!(step_resize_settle(&mut m, 1), vec![2]); // pane 2: 1 -> 0, fires
    }

    /// A host that ticks slower than the frame rate steps several frames at
    /// once: the countdown fires on the first tick that covers it.
    #[test]
    fn resize_settle_steps_several_frames_at_once() {
        let mut m: HashMap<PaneId, u32> = HashMap::new();
        m.insert(4, 10);
        assert!(step_resize_settle(&mut m, 6).is_empty()); // 10 -> 4
        assert_eq!(step_resize_settle(&mut m, 6), vec![4]); // 4 -> 0 (saturates)
    }

    #[test]
    fn settle_nudge_skipped_only_on_broad_repaint_without_hole() {
        // App fully repainted after the real resize: nudging again is the
        // exact trigger of the clear+partial-repaint hole — skip.
        assert!(should_skip_settle_nudge(1.0, false));
        assert!(should_skip_settle_nudge(SETTLE_SKIP_COVERAGE, false));
        // App barely repainted (possibly coalesced the SIGWINCH): nudge.
        assert!(!should_skip_settle_nudge(0.1, false));
        // A hole is visible: always nudge, whatever the coverage says
        // (scroll frames can accumulate coverage while the band persists).
        assert!(!should_skip_settle_nudge(1.0, true));
        assert!(!should_skip_settle_nudge(0.1, true));
    }

    #[test]
    fn settle_frames_is_about_150ms_and_never_below_four() {
        assert_eq!(settle_frames(60), 10);
        assert_eq!(settle_frames(120), 20);
        assert_eq!(settle_frames(12), 4);
    }

    #[test]
    fn a_nudge_moves_the_rows_by_one() {
        assert_eq!(nudged_rows(24), 23);
        assert_eq!(nudged_rows(1), 2);
    }

    #[test]
    fn countdowns_keep_one_slot_per_pane() {
        let mut l = Vec::new();
        arm(&mut l, 1, 3);
        arm(&mut l, 2, 3);
        arm(&mut l, 1, 5); // re-armed: pane 2 keeps its own restore
        assert_eq!(step_countdowns(&mut l, 3), vec![2]);
        assert_eq!(step_countdowns(&mut l, 1), Vec::<PaneId>::new());
        assert_eq!(step_countdowns(&mut l, 1), vec![1]);
        assert!(l.is_empty());
    }

    #[test]
    fn returning_to_a_recent_size_is_a_round_trip() {
        let t0 = Instant::now();
        let mut h = Vec::new();
        assert!(!note_resize(&mut h, (80, 24), (100, 30), t0));
        assert!(note_resize(&mut h, (100, 30), (80, 24), t0 + Duration::from_millis(100)));
        assert!(h.is_empty(), "a round trip clears the history");
    }

    #[test]
    fn a_size_older_than_500ms_is_not_a_round_trip() {
        let t0 = Instant::now();
        let mut h = Vec::new();
        assert!(!note_resize(&mut h, (80, 24), (100, 30), t0));
        assert!(!note_resize(&mut h, (100, 30), (80, 24), t0 + Duration::from_millis(600)));
        assert_eq!(h.len(), 1, "only the fresh entry is left");
    }

    #[test]
    fn the_history_keeps_eight_sizes() {
        let t0 = Instant::now();
        let mut h = Vec::new();
        for i in 0..12u16 {
            assert!(!note_resize(&mut h, (10 + i, 5), (11 + i, 5), t0));
        }
        assert_eq!(h.len(), 8);
        assert_eq!(h[0].0, (14, 5), "the oldest sizes went first");
    }

    #[test]
    fn the_frame_clock_counts_frames_without_drift() {
        let t0 = Instant::now();
        let mut c = FrameClock::new(t0);
        assert_eq!(c.advance(t0 + Duration::from_millis(10), 60), 0);
        assert_eq!(c.advance(t0 + Duration::from_millis(20), 60), 1);
        assert_eq!(c.advance(t0 + Duration::from_millis(100), 60), 5); // 6 frames in all
        assert_eq!(c.advance(t0 + Duration::from_millis(100), 60), 0);
        assert_eq!(c.advance(t0 + Duration::from_millis(1000), 60), 54);
    }

    /// The whole machine on a pane in memory: a resize arms the settle repaint,
    /// which nudges the PTY and arms the restore, which arms the hole check.
    /// The settle repaint is not skipped: the fresh grid shows no coverage.
    #[test]
    fn a_resize_settles_into_a_nudge_then_a_restore_then_a_check() {
        let (surface, _reader) = Surface::capture(80, 24);
        let id = surface.id;
        let mut lookup = |pane_id: PaneId, f: &mut dyn FnMut(&Surface, bool)| {
            if pane_id == id {
                f(&surface, false);
            }
        };
        let mut r = ResizeRepair::default();
        surface.terminal.write().resize(100, 30);
        surface.terminal.write().reset_rows_touched();
        r.on_resized(&surface, (80, 24), (100, 30), Instant::now(), 4);
        assert_eq!(r.pending(id), (None, Some(4), None));
        r.tick(3, &mut lookup);
        assert_eq!(r.pending(id), (None, Some(1), None));
        r.tick(1, &mut lookup); // settle fires: nudge, restore armed
        assert_eq!(r.pending(id), (Some(RESTORE_FRAMES), None, None));
        // Restore fires and arms the check, which the same tick already steps
        // (Kova.app's per-frame order).
        r.tick(RESTORE_FRAMES, &mut lookup);
        assert_eq!(r.pending(id), (None, None, Some(POST_RESTORE_CHECK_FRAMES - 1)));
        r.tick(POST_RESTORE_CHECK_FRAMES, &mut lookup); // clean grid: nothing more
        assert_eq!(r.pending(id), (None, None, None));
    }

    /// A minimized pane keeps its old size: the settle repaint leaves it be.
    #[test]
    fn a_minimized_pane_is_never_nudged() {
        let (surface, _reader) = Surface::capture(80, 24);
        let id = surface.id;
        let mut lookup = |pane_id: PaneId, f: &mut dyn FnMut(&Surface, bool)| {
            if pane_id == id {
                f(&surface, true);
            }
        };
        let mut r = ResizeRepair::default();
        r.on_resized(&surface, (80, 24), (100, 30), Instant::now(), 1);
        r.tick(1, &mut lookup);
        assert_eq!(r.pending(id), (None, None, None), "no nudge, so no restore");
    }

    #[test]
    fn a_round_trip_nudges_at_once() {
        let (surface, _reader) = Surface::capture(80, 24);
        let id = surface.id;
        let mut r = ResizeRepair::default();
        let t0 = Instant::now();
        r.on_resized(&surface, (80, 24), (100, 30), t0, 10);
        assert_eq!(r.pending(id).0, None);
        r.on_resized(&surface, (100, 30), (80, 24), t0 + Duration::from_millis(50), 10);
        assert_eq!(r.pending(id), (Some(RESTORE_FRAMES), Some(10), None));
    }
}
