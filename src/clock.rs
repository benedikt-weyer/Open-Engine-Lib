//! A clock for the playhead that runs between the backend's position reports.
//!
//! The backend reports the position a few times a second, and each report is a little
//! early or late (it was measured a moment ago, and the audio is played in chunks). If
//! the display jumped to every report, the waveform would hop forward and back with
//! that jitter. Instead the clock runs on by itself and only leans towards each report,
//! and jumps only when the report is far off, which is a real jump: a seek, another
//! track, or the audio stalling.

use std::time::Instant;

/// A report further than this (seconds) from the clock is believed at once.
const SNAP_AFTER: f64 = 0.3;
/// How much of the difference to a nearby report is taken over, per report. A small
/// share irons out the jitter and still pulls the clock back to the truth within a
/// couple of seconds, should it run a little fast or slow.
const LEAN: f64 = 0.15;

#[derive(Debug, Clone, Copy)]
pub struct Clock {
    /// Seconds into the track at `at`.
    position: f64,
    at: Instant,
    running: bool,
}

impl Clock {
    pub fn new(now: Instant) -> Self {
        Self {
            position: 0.0,
            at: now,
            running: false,
        }
    }

    /// Seconds into the track at `now`, at most `duration`.
    pub fn position(&self, now: Instant, duration: f64) -> f64 {
        let moved = if self.running {
            now.saturating_duration_since(self.at).as_secs_f64()
        } else {
            0.0
        };
        (self.position + moved).clamp(0.0, duration)
    }

    /// The same clock, `seconds` behind: for drawing the waveform later than the player
    /// reports it, to line up with the sound. (Negative runs ahead.)
    pub fn delayed(mut self, seconds: f64) -> Self {
        self.position -= seconds;
        self
    }

    pub fn is_running(&self) -> bool {
        self.running
    }

    /// Takes the backend's report: `reported` seconds into the track, playing or not.
    pub fn report(&mut self, reported: f64, running: bool, now: Instant) {
        let estimate = self.position(now, f64::INFINITY);
        let smooth = running && self.running && (reported - estimate).abs() <= SNAP_AFTER;
        self.position = if smooth {
            estimate + (reported - estimate) * LEAN
        } else {
            reported
        };
        self.at = now;
        self.running = running;
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    fn after(start: Instant, seconds: f64) -> Instant {
        start + Duration::from_secs_f64(seconds)
    }

    #[test]
    fn runs_on_by_itself_while_playing_and_stands_still_when_not() {
        let start = Instant::now();
        let mut clock = Clock::new(start);
        clock.report(10.0, true, start);
        assert!((clock.position(after(start, 0.5), 300.0) - 10.5).abs() < 1e-9);

        clock.report(10.5, false, after(start, 0.5));
        assert_eq!(clock.position(after(start, 5.0), 300.0), 10.5);
    }

    #[test]
    fn stays_within_the_track() {
        let start = Instant::now();
        let mut clock = Clock::new(start);
        clock.report(99.9, true, start);
        assert_eq!(clock.position(after(start, 10.0), 100.0), 100.0);
        assert_eq!(Clock::new(start).position(start, 100.0), 0.0);
    }

    #[test]
    fn jitter_in_the_reports_barely_moves_the_playhead() {
        let start = Instant::now();
        let mut clock = Clock::new(start);
        clock.report(0.0, true, start);

        // Reports every quarter second, alternately 80 ms early and 80 ms late.
        let mut worst_jump: f64 = 0.0;
        for poll in 1..=40 {
            let now = after(start, poll as f64 * 0.25);
            let before = clock.position(now, 300.0);
            let jitter = if poll % 2 == 0 { 0.08 } else { -0.08 };
            clock.report(poll as f64 * 0.25 + jitter, true, now);
            worst_jump = worst_jump.max((clock.position(now, 300.0) - before).abs());
        }
        // Following the reports would hop by 80 ms (nearly 14 px at 170 px a second).
        assert!(worst_jump < 0.02, "{worst_jump}");
    }

    #[test]
    fn follows_the_truth_when_the_clock_runs_off() {
        let start = Instant::now();
        let mut clock = Clock::new(start);
        clock.report(0.0, true, start);

        // The real playback is 1% faster than the clock; the reports pull it along.
        for poll in 1..=40 {
            let seconds = poll as f64 * 0.25;
            clock.report(seconds * 1.01, true, after(start, seconds));
        }
        let error = (clock.position(after(start, 10.0), 300.0) - 10.1).abs();
        assert!(error < 0.05, "{error}");
    }

    #[test]
    fn a_real_jump_is_believed_at_once() {
        let start = Instant::now();
        let mut clock = Clock::new(start);
        clock.report(10.0, true, start);

        // A seek.
        clock.report(95.0, true, after(start, 0.25));
        assert_eq!(clock.position(after(start, 0.25), 300.0), 95.0);
        // Pausing and resuming start from what is reported.
        clock.report(96.0, false, after(start, 1.0));
        assert_eq!(clock.position(after(start, 2.0), 300.0), 96.0);
        clock.report(96.0, true, after(start, 2.0));
        assert_eq!(clock.position(after(start, 2.0), 300.0), 96.0);
    }

    #[test]
    fn a_delayed_clock_trails_and_stays_within_the_track() {
        let start = Instant::now();
        let mut clock = Clock::new(start);
        clock.report(10.0, true, start);
        let later = after(start, 1.0);
        assert!((clock.delayed(0.25).position(later, 300.0) - 10.75).abs() < 1e-9);
        assert!((clock.delayed(-0.25).position(later, 300.0) - 11.25).abs() < 1e-9);
        // Near the start it can't go before the beginning, nor past the end near the end.
        assert_eq!(clock.delayed(30.0).position(start, 300.0), 0.0);
        assert_eq!(clock.delayed(-300.0).position(start, 300.0), 300.0);
    }
}
