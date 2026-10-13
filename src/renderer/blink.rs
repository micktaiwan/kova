//! The cursor blink, kept by the host. Its phase is a function of real time
//! since the focused pane's cursor last moved, not of how many frames were
//! drawn, so it does not depend on how often (or how regularly) a host draws.
//!
//! The config still states the period in frames (`cursor_blink_frames`), the
//! unit it had when the blink was a frame counter. A frame is one tick of the
//! run-loop timer, `1 / fps` seconds (`start_global_timer`, `app.rs`). The
//! elapsed time is turned back into that tick count, so the blink keeps the
//! exact pattern the counter drew whenever the timer ticked on time:
//!
//! - old: the counter is reset to 0 on a cursor move, then incremented before
//!   use, so the frame that sees the move is frame 1, the next tick frame 2…
//! - new: frame = 1 + round(elapsed × fps), elapsed counted from that same
//!   frame. With ticks every 1/fps s, the k-th tick after the move has
//!   elapsed = (k − 1)/fps and lands on frame k. Rounding (not flooring)
//!   keeps that true for a tick early or late by less than half an interval
//!   relative to the tick that started the count.
//! - on = frame % N < N/2, unchanged. With the defaults (N = 60 frames,
//!   fps = 60) the cycle is N/fps = 1 s: on through frame 29 (elapsed
//!   < 28.5/60 s ≈ 475 ms), off from frame 30 to 59, on again from frame 60
//!   (elapsed ≥ 58.5/60 s ≈ 975 ms), and so on with a 1 s period.
//!
//! Where the counter drifted (ticks dropped under load slowed the blink), the
//! clock does not: that is the one intended difference.

use std::time::Instant;

pub(super) struct CursorBlink {
    /// The frame-1 instant: the first sample, or the last cursor move.
    origin: Option<Instant>,
    last_epoch: u32,
    /// Frame number of the previous sample (0 right after a reset).
    last_frame: u64,
}

impl CursorBlink {
    pub(super) fn new() -> Self {
        CursorBlink { origin: None, last_epoch: 0, last_frame: 0 }
    }

    /// Phase of the blink at `now`: (cursor shown, phase may have flipped
    /// since the previous sample). `cursor_epoch` is the focused pane's
    /// `cursor_move_epoch`, `None` when no pane is focused; a new value
    /// restarts the blink with the cursor shown. `period_frames` below 2
    /// means no blink.
    pub(super) fn sample(
        &mut self,
        now: Instant,
        cursor_epoch: Option<u32>,
        period_frames: u32,
        fps: u32,
    ) -> (bool, bool) {
        // Reset blink on cursor movement of focused pane
        if let Some(epoch) = cursor_epoch {
            if epoch != self.last_epoch {
                self.last_epoch = epoch;
                self.origin = Some(now);
                self.last_frame = 0;
            }
        }
        let origin = *self.origin.get_or_insert(now);
        let frame = frame_at(now.saturating_duration_since(origin).as_nanos(), fps);
        let previous = std::mem::replace(&mut self.last_frame, frame);
        blink_phase(frame, previous, period_frames)
    }
}

/// The frame a sample `elapsed_nanos` after frame 1 falls on, at `fps` ticks
/// per second: 1 + round(elapsed × fps). Integer arithmetic, so the boundary
/// between two frames does not depend on float rounding.
fn frame_at(elapsed_nanos: u128, fps: u32) -> u64 {
    const NANOS: u128 = 1_000_000_000;
    let ticks = (elapsed_nanos * fps.max(1) as u128 + NANOS / 2) / NANOS;
    1 + ticks.min(u64::MAX as u128 - 1) as u64
}

/// (on, changed) at `frame`, `previous` being the frame of the last sample.
/// `on` is the counter's rule. The counter flagged a change on the frames
/// where `frame % half == 0`; a clock can skip frames, so `changed` says
/// whether such a frame lies in (previous, frame] — the same frames when
/// every tick is sampled.
fn blink_phase(frame: u64, previous: u64, period_frames: u32) -> (bool, bool) {
    if period_frames >= 2 {
        let n = period_frames as u64;
        let half = n / 2;
        (frame % n < half, frame / half > previous / half)
    } else {
        (true, false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// The frame counter this replaces, verbatim: reset to 0 on a cursor
    /// move, incremented on every frame, phase read from the count.
    struct OldCounter {
        counter: u32,
        last_epoch: u32,
    }

    impl OldCounter {
        fn frame(&mut self, epoch: Option<u32>, n: u32) -> (bool, bool) {
            if let Some(epoch) = epoch {
                if epoch != self.last_epoch {
                    self.last_epoch = epoch;
                    self.counter = 0;
                }
            }
            self.counter = self.counter.wrapping_add(1);
            if n >= 2 {
                let half = n / 2;
                (self.counter % n < half, (self.counter % half) == 0)
            } else {
                (true, false)
            }
        }
    }

    /// Drive both with a timer ticking every 1/fps s, `jitter(tick)` nanos
    /// off schedule, and the cursor moving on the ticks listed in `moves`.
    fn compare(n: u32, fps: u32, ticks: u64, moves: &[u64], jitter: impl Fn(u64) -> i64) {
        let start = Instant::now();
        let mut old = OldCounter { counter: 0, last_epoch: 0 };
        let mut new = CursorBlink::new();
        let mut epoch = 0u32;
        for tick in 0..ticks {
            if moves.contains(&tick) {
                epoch += 1;
            }
            let nanos = (tick as i128 * 1_000_000_000 / fps as i128 + jitter(tick) as i128).max(0);
            let now = start + Duration::from_nanos(nanos as u64);
            let want = old.frame(Some(epoch), n);
            let got = new.sample(now, Some(epoch), n, fps);
            assert_eq!(got, want, "tick {tick} (n={n}, fps={fps})");
        }
    }

    #[test]
    fn real_time_blink_matches_the_frame_counter_on_a_steady_timer() {
        let moves = [7, 8, 45, 200, 201, 202, 333];
        for (n, fps) in [(60, 60), (60, 120), (30, 60), (2, 60), (3, 60), (4, 30), (61, 60)] {
            compare(n, fps, 600, &moves, |_| 0);
        }
    }

    #[test]
    fn real_time_blink_absorbs_timer_jitter_under_half_a_tick() {
        // 1/60 s is 16_666_666 ns. Ticks drift by up to ±4 ms, so two ticks
        // are never more than 8 ms (under half an interval) apart from their
        // schedule relative to each other — the move tick included.
        let jitter = |t: u64| [0i64, 4_000_000, -4_000_000, 2_000_000, -3_500_000][(t % 5) as usize];
        compare(60, 60, 600, &[0, 90, 91, 400], jitter);
    }

    #[test]
    fn default_blink_flips_every_half_second() {
        // N = 60 frames at 60 fps: on for frames 1..=29, off for 30..=59.
        let start = Instant::now();
        let mut b = CursorBlink::new();
        let at = |ms: u64| start + Duration::from_millis(ms);
        assert_eq!(b.sample(at(0), Some(1), 60, 60), (true, false));
        assert_eq!(b.sample(at(470), Some(1), 60, 60), (true, false));
        assert_eq!(b.sample(at(480), Some(1), 60, 60), (false, true));
        assert_eq!(b.sample(at(970), Some(1), 60, 60), (false, false));
        assert_eq!(b.sample(at(980), Some(1), 60, 60), (true, true));
        // A cursor move shows the cursor again, whatever the phase was.
        assert_eq!(b.sample(at(1500), Some(2), 60, 60).0, true);
    }

    #[test]
    fn a_late_frame_still_reports_the_flip_it_skipped_over() {
        let start = Instant::now();
        let mut b = CursorBlink::new();
        b.sample(start, Some(1), 60, 60);
        b.sample(start + Duration::from_millis(400), Some(1), 60, 60);
        // Next sample a full second later: back in the "on" half, but two
        // flips happened in between — the host must hear about it.
        let (on, changed) = b.sample(start + Duration::from_millis(1400), Some(1), 60, 60);
        assert!(on);
        assert!(changed);
    }

    #[test]
    fn no_blink_below_two_frames() {
        let start = Instant::now();
        let mut b = CursorBlink::new();
        for ms in [0, 10, 500, 5000] {
            assert_eq!(b.sample(start + Duration::from_millis(ms), Some(1), 1, 60), (true, false));
        }
    }
}
