//! The cues (Engine's hot cues, eight pads per track) of the tracks played, as the
//! backend has them, and what the UI needs to show and write them.

use std::collections::{HashMap, HashSet};

use iced::Color;

/// How many pads a track has.
pub const SLOTS: usize = 8;

/// The color Engine gives a new cue on each pad (0xAARRGGBB). The ones for the pads 1 to 6
/// and 8 are from a real library; pad 7's is a guess, as none of its tracks had it set.
pub const PALETTE: [u32; SLOTS] = [
    0xfff4_d338,
    0xffef_8130,
    0xffaa_55c4,
    0xffce_3239,
    0xff86_c64b,
    0xff20_c670,
    0xffe0_559e,
    0xff15_71e2,
];

#[derive(Debug, Clone, PartialEq)]
pub struct Cue {
    /// The pad, 0 to 7.
    pub slot: usize,
    pub label: String,
    /// Seconds into the track.
    pub position: f64,
    /// `0xAARRGGBB`.
    pub color: u32,
}

impl Cue {
    /// A cue on `slot` at `position`, named and colored the way Engine does it.
    pub fn new(slot: usize, position: f64) -> Self {
        Self {
            slot,
            label: default_label(slot),
            position,
            color: PALETTE[slot.min(SLOTS - 1)],
        }
    }

    pub fn color(&self) -> Color {
        argb(self.color)
    }
}

/// What Engine calls the cue of a pad when the user has not named it. (It is saved with
/// the cue, so it is not translated.)
pub fn default_label(slot: usize) -> String {
    format!("Cue {}", slot + 1)
}

/// An `0xAARRGGBB` color. A cue is drawn opaque whatever its alpha says.
pub fn argb(color: u32) -> Color {
    let [_, r, g, b] = color.to_be_bytes();
    Color::from_rgb8(r, g, b)
}

/// Black or white, whichever can be read on `color`.
pub fn text_on(color: Color) -> Color {
    // Perceived brightness (Rec. 601).
    if 0.299 * color.r + 0.587 * color.g + 0.114 * color.b > 0.6 {
        Color::BLACK
    } else {
        Color::WHITE
    }
}

/// `list` with `cue` in its slot (replacing the cue that was there), in slot order.
pub fn with(list: &[Cue], cue: Cue) -> Vec<Cue> {
    let mut list: Vec<Cue> = list
        .iter()
        .filter(|other| other.slot != cue.slot)
        .cloned()
        .collect();
    list.push(cue);
    list.sort_by_key(|cue| cue.slot);
    list
}

/// `list` without the cue in `slot`.
pub fn without(list: &[Cue], slot: usize) -> Vec<Cue> {
    list.iter()
        .filter(|cue| cue.slot != slot)
        .cloned()
        .collect()
}

/// A position as `m:ss.mmm`, the way it is edited.
pub fn format_position(seconds: f64) -> String {
    let millis = (seconds.max(0.0) * 1000.0).round() as u64;
    format!(
        "{}:{:02}.{:03}",
        millis / 60_000,
        millis / 1000 % 60,
        millis % 1000
    )
}

/// A position typed as `m:ss.mmm` (or `h:mm:ss.mmm`, or just seconds). `None` if it is
/// none of those.
pub fn parse_position(text: &str) -> Option<f64> {
    let mut seconds = 0.0;
    let parts: Vec<&str> = text.trim().split(':').collect();
    if parts.is_empty() || parts.len() > 3 {
        return None;
    }
    for (index, part) in parts.iter().enumerate() {
        let last = index == parts.len() - 1;
        let value: f64 = part.trim().replace(',', ".").parse().ok()?;
        // Only the seconds can have a fraction, and the minutes and seconds in
        // `h:mm:ss` stay below 60.
        let whole = last || value.fract() == 0.0;
        let in_range = index == 0 || value < 60.0;
        if !(value.is_finite() && value >= 0.0 && whole && in_range) {
            return None;
        }
        seconds = seconds * 60.0 + value;
    }
    Some(seconds)
}

/// What was fetched for the tracks played (their cues, their loops): a list per track.
pub struct Fetched<T> {
    entries: HashMap<u32, Vec<T>>,
    /// Tracks that were asked for.
    asked: HashSet<u32>,
    /// Changes with the library, so a late answer about another library's track (whose id
    /// means another track now) is told apart and dropped.
    generation: u32,
}

impl<T> Default for Fetched<T> {
    fn default() -> Self {
        Self {
            entries: HashMap::new(),
            asked: HashSet::new(),
            generation: 0,
        }
    }
}

impl<T> Fetched<T> {
    pub fn generation(&self) -> u32 {
        self.generation
    }

    pub fn clear(&mut self) {
        self.entries.clear();
        self.asked.clear();
        self.generation += 1;
    }

    /// The list of track `id`, once known.
    pub fn get(&self, id: u32) -> Option<&[T]> {
        self.entries.get(&id).map(Vec::as_slice)
    }

    /// Whether the list of `id` has not been asked for yet. If so it counts as being
    /// loaded from now on.
    pub fn claim(&mut self, id: u32) -> bool {
        self.asked.insert(id)
    }

    /// Stores what the backend said for `id`. Answers for an earlier library are ignored.
    pub fn set(&mut self, generation: u32, id: u32, list: Vec<T>) {
        if generation == self.generation {
            self.entries.insert(id, list);
        }
    }
}

/// The cues fetched for the tracks played.
pub type TrackCues = Fetched<Cue>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_new_cue_is_named_and_colored_for_its_pad() {
        let cue = Cue::new(3, 12.5);
        assert_eq!(
            (cue.slot, cue.label.as_str(), cue.position),
            (3, "Cue 4", 12.5)
        );
        assert_eq!(cue.color, 0xffce_3239);
        let color = cue.color();
        assert_eq!(
            [color.r, color.g, color.b].map(|c| (c * 255.0).round() as u8),
            [0xce, 0x32, 0x39]
        );
    }

    #[test]
    fn cues_are_put_in_and_taken_out_by_slot() {
        let list = [Cue::new(1, 2.0), Cue::new(5, 1.0)];
        let slots = |list: &[Cue]| list.iter().map(|cue| cue.slot).collect::<Vec<_>>();

        let added = with(&list, Cue::new(3, 3.0));
        assert_eq!(slots(&added), [1, 3, 5]);
        let moved = with(&list, Cue::new(5, 9.0));
        assert_eq!(slots(&moved), [1, 5]);
        assert_eq!(moved[1].position, 9.0);
        assert_eq!(slots(&without(&list, 1)), [5]);
        assert_eq!(slots(&without(&list, 0)), [1, 5]);
    }

    #[test]
    fn positions_are_written_and_read_back() {
        assert_eq!(format_position(0.0), "0:00.000");
        assert_eq!(format_position(83.2), "1:23.200");
        assert_eq!(format_position(3_599.999_6), "60:00.000");
        assert_eq!(format_position(-3.0), "0:00.000");

        for seconds in [0.0, 0.001, 59.999, 83.2, 600.5, 3661.25] {
            let back = parse_position(&format_position(seconds)).unwrap();
            assert!((back - seconds).abs() < 0.0005, "{seconds} {back}");
        }
        assert_eq!(parse_position("12"), Some(12.0));
        assert_eq!(parse_position(" 12,5 "), Some(12.5));
        assert_eq!(parse_position("1:30"), Some(90.0));
        assert_eq!(parse_position("1:05:30.5"), Some(3930.5));
        assert_eq!(
            parse_position("70"),
            Some(70.0),
            "plain seconds can be any number"
        );
    }

    #[test]
    fn what_is_not_a_position_is_refused() {
        for text in [
            "", "abc", "1:70", "1.5:30", "-1", "1:-1", "1:2:3:4", "NaN", "inf", ":",
        ] {
            assert_eq!(parse_position(text), None, "{text:?}");
        }
    }

    #[test]
    fn answers_are_kept_for_the_library_they_were_asked_in() {
        let mut cues = TrackCues::default();
        assert!(cues.claim(4));
        assert!(!cues.claim(4), "asked once");
        let generation = cues.generation();
        assert_eq!(cues.get(4), None);

        cues.set(generation, 4, vec![Cue::new(0, 1.0)]);
        assert_eq!(cues.get(4).map(<[Cue]>::len), Some(1));

        // Another library: the answer that was on its way is of no use.
        cues.clear();
        cues.set(generation, 4, vec![Cue::new(0, 1.0)]);
        assert_eq!(cues.get(4), None);
        assert!(cues.claim(4));
    }
}
