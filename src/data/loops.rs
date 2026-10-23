//! The saved loops (eight pads per track, like the cues) of the tracks played, as the
//! backend has them. The colors, the position text and the store of what was fetched are
//! the ones of the cues (`cues.rs`).

use iced::Color;

use crate::cues::{self, Fetched, PALETTE, SLOTS};

/// How many beats a loop set from a pad has, as Engine's own default.
pub const DEFAULT_BEATS: f64 = 4.0;
/// How long such a loop is, in seconds, for a track whose tempo is not known.
pub const FALLBACK_SECONDS: f64 = 2.0;

#[derive(Debug, Clone, PartialEq)]
pub struct Loop {
    /// The pad, 0 to 7.
    pub slot: usize,
    pub label: String,
    /// Seconds into the track.
    pub start: f64,
    pub end: f64,
    /// `0xAARRGGBB`.
    pub color: u32,
}

impl Loop {
    /// A loop on `slot` from `start` to `end`, named and colored the way Engine does it.
    pub fn new(slot: usize, start: f64, end: f64) -> Self {
        Self {
            slot,
            label: default_label(slot),
            start,
            end,
            color: PALETTE[slot.min(SLOTS - 1)],
        }
    }

    pub fn color(&self) -> Color {
        cues::argb(self.color)
    }
}

/// What Engine calls the loop of a pad when the user has not named it. (It is saved with
/// the loop, so it is not translated.)
pub fn default_label(slot: usize) -> String {
    format!("Loop {}", slot + 1)
}

/// How long a loop set from a pad is: four beats at `bpm`, else a fixed time.
pub fn default_length(bpm: Option<f64>) -> f64 {
    match bpm {
        Some(bpm) if bpm.is_finite() && bpm > 0.0 => DEFAULT_BEATS * 60.0 / bpm,
        _ => FALLBACK_SECONDS,
    }
}

/// `list` with `saved` in its slot (replacing the loop that was there), in slot order.
pub fn with(list: &[Loop], saved: Loop) -> Vec<Loop> {
    let mut list: Vec<Loop> = list
        .iter()
        .filter(|other| other.slot != saved.slot)
        .cloned()
        .collect();
    list.push(saved);
    list.sort_by_key(|saved| saved.slot);
    list
}

/// `list` without the loop in `slot`.
pub fn without(list: &[Loop], slot: usize) -> Vec<Loop> {
    list.iter()
        .filter(|saved| saved.slot != slot)
        .cloned()
        .collect()
}

/// The loops fetched for the tracks played.
pub type TrackLoops = Fetched<Loop>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_new_loop_is_named_and_colored_for_its_pad() {
        let saved = Loop::new(2, 10.0, 12.0);
        assert_eq!(
            (saved.slot, saved.label.as_str(), saved.start, saved.end),
            (2, "Loop 3", 10.0, 12.0)
        );
        assert_eq!(saved.color, 0xffaa_55c4);
    }

    #[test]
    fn a_loop_from_a_pad_is_four_beats_long() {
        assert_eq!(default_length(Some(120.0)), 2.0);
        assert!((default_length(Some(128.0)) - 1.875).abs() < 1e-9);
        for bpm in [None, Some(0.0), Some(-1.0), Some(f64::NAN)] {
            assert_eq!(default_length(bpm), FALLBACK_SECONDS, "{bpm:?}");
        }
    }

    #[test]
    fn loops_are_put_in_and_taken_out_by_slot() {
        let list = [Loop::new(1, 1.0, 2.0), Loop::new(5, 3.0, 4.0)];
        let slots = |list: &[Loop]| list.iter().map(|saved| saved.slot).collect::<Vec<_>>();

        assert_eq!(slots(&with(&list, Loop::new(3, 5.0, 6.0))), [1, 3, 5]);
        let moved = with(&list, Loop::new(5, 9.0, 10.0));
        assert_eq!(slots(&moved), [1, 5]);
        assert_eq!(moved[1].start, 9.0);
        assert_eq!(slots(&without(&list, 1)), [5]);
        assert_eq!(slots(&without(&list, 0)), [1, 5]);
    }
}
