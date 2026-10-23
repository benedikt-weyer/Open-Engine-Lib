//! Reads and writes the loops Engine stores for a track (`PerformanceData.loops`).
//!
//! Layout (see `docs/engine-library-format.md`): unlike the cues the blob is not
//! compressed and is little-endian: a count (always 8 in the libraries seen: one per
//! pad), then per slot a label (`u8` length and the bytes), the start and the end in
//! samples (`f64` each, -1.0 when not set), a byte each saying whether the start and the
//! end are set, and a color (4 bytes, ARGB). Nothing follows the slots; if something
//! does, it is kept as it was when a blob is changed.

use crate::cues::MAX_LABEL;
pub use crate::cues::SLOTS;

/// A loop that is set.
#[derive(Debug, Clone, PartialEq)]
pub struct Loop {
    /// 0 to 7.
    pub slot: usize,
    pub label: String,
    pub start_seconds: f64,
    pub end_seconds: f64,
    /// `0xAARRGGBB`.
    pub color: u32,
}

#[derive(Debug, Clone, PartialEq)]
struct Slot {
    label: String,
    /// In samples; negative when not set.
    start: f64,
    end: f64,
    start_set: bool,
    end_set: bool,
    color: [u8; 4],
}

impl Slot {
    fn empty() -> Self {
        Self {
            label: String::new(),
            start: -1.0,
            end: -1.0,
            start_set: false,
            end_set: false,
            color: [0; 4],
        }
    }

    /// A loop to show: both ends are set and the end is after the start. (A slot with
    /// only one end is something Engine can write; it is not a loop to us and is left as
    /// it is.)
    fn is_set(&self) -> bool {
        self.start_set
            && self.end_set
            && self.start.is_finite()
            && self.end.is_finite()
            && self.start >= 0.0
            && self.end > self.start
    }
}

/// The content of a `loops` blob.
#[derive(Debug, Clone, PartialEq)]
pub struct QuickLoops {
    slots: Vec<Slot>,
    tail: Vec<u8>,
}

impl QuickLoops {
    /// A track with no loop set.
    pub fn empty() -> Self {
        Self {
            slots: vec![Slot::empty(); SLOTS],
            tail: Vec::new(),
        }
    }

    /// The blob's content, or `None` if it is not a `loops` blob.
    pub fn parse(blob: &[u8]) -> Option<Self> {
        let count = i64::from_le_bytes(blob.get(0..8)?.try_into().ok()?);
        // (Every slot takes at least 23 bytes.)
        let count = usize::try_from(count)
            .ok()
            .filter(|count| *count <= blob.len() / 23)?;

        let mut at = 8;
        let mut slots = Vec::with_capacity(count);
        for _ in 0..count {
            let length = usize::from(*blob.get(at)?);
            let label = String::from_utf8(blob.get(at + 1..at + 1 + length)?.to_vec()).ok()?;
            at += 1 + length;
            let start = f64::from_le_bytes(blob.get(at..at + 8)?.try_into().ok()?);
            let end = f64::from_le_bytes(blob.get(at + 8..at + 16)?.try_into().ok()?);
            let start_set = *blob.get(at + 16)? != 0;
            let end_set = *blob.get(at + 17)? != 0;
            let color = blob.get(at + 18..at + 22)?.try_into().ok()?;
            at += 22;
            slots.push(Slot {
                label,
                start,
                end,
                start_set,
                end_set,
                color,
            });
        }
        Some(Self {
            slots,
            tail: blob[at..].to_vec(),
        })
    }

    /// The blob for this content.
    pub fn encode(&self) -> Vec<u8> {
        let mut data = Vec::new();
        data.extend((self.slots.len() as i64).to_le_bytes());
        for slot in &self.slots {
            data.push(slot.label.len() as u8);
            data.extend(slot.label.as_bytes());
            data.extend(slot.start.to_le_bytes());
            data.extend(slot.end.to_le_bytes());
            data.push(u8::from(slot.start_set));
            data.push(u8::from(slot.end_set));
            data.extend(slot.color);
        }
        data.extend(&self.tail);
        data
    }

    /// How many slots hold a loop.
    pub fn count(&self) -> usize {
        self.slots.iter().filter(|slot| slot.is_set()).count()
    }

    /// The loops that are set, in slot order, with positions in seconds at `sample_rate`.
    pub fn loops(&self, sample_rate: f64) -> Vec<Loop> {
        self.slots
            .iter()
            .enumerate()
            .filter(|(_, slot)| slot.is_set())
            .map(|(slot, saved)| Loop {
                slot,
                label: saved.label.clone(),
                start_seconds: saved.start / sample_rate,
                end_seconds: saved.end / sample_rate,
                color: u32::from_be_bytes(saved.color),
            })
            .collect()
    }

    /// Makes `loops` the loops that are set: the slots they name get them, and the slots
    /// that held a loop and are not named are emptied. Slots that are only half set are
    /// left alone. The positions are in seconds at `sample_rate`. A blob with fewer slots
    /// than there are pads gets more.
    pub fn replace(&mut self, loops: &[Loop], sample_rate: f64) {
        if self.slots.len() < SLOTS {
            self.slots.resize(SLOTS, Slot::empty());
        }
        for slot in &mut self.slots {
            if slot.is_set() {
                *slot = Slot::empty();
            }
        }
        for saved in loops {
            self.slots[saved.slot] = Slot {
                label: saved.label.clone(),
                start: saved.start_seconds * sample_rate,
                end: saved.end_seconds * sample_rate,
                start_set: true,
                end_set: true,
                color: saved.color.to_be_bytes(),
            };
        }
    }
}

/// Checks loops that are to be saved: what is wrong with them, if anything. A loop that
/// ends past `duration_seconds` (when that is known) is wrong too.
pub fn validate(loops: &[Loop], duration_seconds: Option<f64>) -> Result<(), String> {
    let mut seen = [false; SLOTS];
    for saved in loops {
        let number = saved.slot + 1;
        let taken = seen
            .get_mut(saved.slot)
            .ok_or_else(|| format!("loop slot {number} does not exist: there are only {SLOTS}"))?;
        if std::mem::replace(taken, true) {
            return Err(format!("loop slot {number} is given twice"));
        }
        let (start, end) = (saved.start_seconds, saved.end_seconds);
        if !(start.is_finite() && start >= 0.0 && end.is_finite()) {
            return Err(format!("loop {number} does not have a start in the track"));
        }
        if end <= start {
            return Err(format!("loop {number} does not end after it starts"));
        }
        if duration_seconds.is_some_and(|duration| end > duration) {
            return Err(format!("loop {number} ends after the end of the track"));
        }
        if saved.label.len() > MAX_LABEL {
            return Err(format!(
                "the label of loop {number} is longer than {MAX_LABEL} bytes"
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A `loops` blob the way Engine writes it: `(label, start, end, flags, ARGB)` per slot.
    type Entry = (&'static str, f64, f64, [u8; 2], [u8; 4]);

    fn blob(slots: &[Entry]) -> Vec<u8> {
        let mut data = (slots.len() as i64).to_le_bytes().to_vec();
        for (label, start, end, flags, color) in slots {
            data.push(label.len() as u8);
            data.extend(label.as_bytes());
            data.extend(start.to_le_bytes());
            data.extend(end.to_le_bytes());
            data.extend(flags);
            data.extend(color);
        }
        data
    }

    const NONE: Entry = ("", -1.0, -1.0, [0, 0], [0; 4]);
    // The second loop of a real track, in samples (at 44.1 kHz it is 51.8 s to 53.1 s).
    const LOOP_2: Entry = (
        "Loop 2",
        2_284_156.0,
        2_342_160.0,
        [1, 1],
        [0xff, 0xef, 0x81, 0x30],
    );

    fn real() -> Vec<u8> {
        blob(&[NONE, LOOP_2, NONE, NONE, NONE, NONE, NONE, NONE])
    }

    fn saved(slot: usize, label: &str, start: f64, end: f64) -> Loop {
        Loop {
            slot,
            label: label.to_string(),
            start_seconds: start,
            end_seconds: end,
            color: 0xffaa_55c4,
        }
    }

    #[test]
    fn reads_the_loops_that_are_set_in_seconds() {
        let loops = QuickLoops::parse(&real()).unwrap();
        assert_eq!(loops.count(), 1);
        let loops = loops.loops(44_100.0);
        assert_eq!(loops.len(), 1);
        assert_eq!((loops[0].slot, loops[0].label.as_str()), (1, "Loop 2"));
        assert!((loops[0].start_seconds - 51.797).abs() < 0.01);
        assert!((loops[0].end_seconds - 53.112).abs() < 0.01);
        assert_eq!(loops[0].color, 0xffef_8130);
    }

    #[test]
    fn a_blob_survives_being_read_and_written() {
        let parsed = QuickLoops::parse(&real()).unwrap();
        // The blob is not compressed, so it comes back byte for byte.
        assert_eq!(parsed.encode(), real());
        assert_eq!(QuickLoops::empty().encode().len(), 192);
        assert_eq!(
            QuickLoops::empty().encode(),
            blob(&[NONE, NONE, NONE, NONE, NONE, NONE, NONE, NONE])
        );
    }

    #[test]
    fn replacing_loops_sets_some_slots_and_empties_the_rest() {
        let mut loops = QuickLoops::parse(&real()).unwrap();
        loops.replace(
            &[saved(4, "Drop", 2.0, 4.0), saved(2, "", 0.5, 1.0)],
            44_100.0,
        );
        let loops = QuickLoops::parse(&loops.encode()).unwrap();
        assert_eq!(loops.count(), 2);
        let listed = loops.loops(44_100.0);
        assert_eq!(
            listed.iter().map(|saved| saved.slot).collect::<Vec<_>>(),
            [2, 4]
        );
        assert_eq!(listed[1].label, "Drop");
        assert_eq!((listed[1].start_seconds, listed[1].end_seconds), (2.0, 4.0));
        assert_eq!(loops.slots.len(), SLOTS);
        assert_eq!(loops.slots[1], Slot::empty(), "slot 2 was emptied");
    }

    #[test]
    fn a_slot_that_is_half_set_is_not_a_loop_and_is_left_alone() {
        let half: Entry = ("Start", 441.0, -1.0, [1, 0], [0xff, 1, 2, 3]);
        let mut loops =
            QuickLoops::parse(&blob(&[half, LOOP_2, NONE, NONE, NONE, NONE, NONE, NONE])).unwrap();
        assert_eq!(loops.count(), 1);
        loops.replace(&[], 44_100.0);
        assert_eq!(loops.count(), 0);
        assert_eq!(loops.slots[0].label, "Start");
        assert!(loops.slots[0].start_set && !loops.slots[0].end_set);
        assert_eq!(loops.slots[1], Slot::empty());
        // An end that is not after the start is not a loop either.
        let backwards: Entry = ("", 900.0, 100.0, [1, 1], [0; 4]);
        let parsed = QuickLoops::parse(&blob(&[backwards])).unwrap();
        assert_eq!(parsed.count(), 0);
    }

    #[test]
    fn a_blob_with_fewer_slots_gets_the_missing_ones() {
        let mut loops = QuickLoops::parse(&blob(&[LOOP_2])).unwrap();
        loops.replace(&[saved(7, "x", 1.0, 2.0)], 44_100.0);
        assert_eq!(loops.slots.len(), SLOTS);
        assert_eq!(loops.loops(44_100.0)[0].slot, 7);
    }

    #[test]
    fn what_follows_the_slots_is_kept() {
        let mut data = real();
        data.extend([9, 8, 7]);
        let mut loops = QuickLoops::parse(&data).unwrap();
        loops.replace(&[saved(0, "a", 1.0, 2.0)], 44_100.0);
        assert!(loops.encode().ends_with(&[9, 8, 7]));
    }

    #[test]
    fn garbage_is_no_loops() {
        assert_eq!(QuickLoops::parse(&[]), None);
        assert_eq!(QuickLoops::parse(&[1, 2, 3]), None);
        let good = real();
        assert_eq!(QuickLoops::parse(&good[..good.len() / 2]), None);
        assert_eq!(QuickLoops::parse(&i64::MAX.to_le_bytes()), None);
        assert_eq!(QuickLoops::parse(&(-1i64).to_le_bytes()), None);
        // A label that is not UTF-8.
        let mut bad = real();
        bad[8 + 23 + 1] = 0xff;
        bad[8 + 23] = 2;
        assert_eq!(QuickLoops::parse(&bad[..]), None);
    }

    #[test]
    fn checks_loops_before_they_are_saved() {
        assert_eq!(
            validate(
                &[saved(0, "a", 1.0, 2.0), saved(7, "", 100.0, 200.0)],
                Some(200.0)
            ),
            Ok(())
        );
        assert_eq!(validate(&[], None), Ok(()));
        assert!(
            validate(&[saved(8, "", 1.0, 2.0)], None)
                .unwrap_err()
                .contains("only 8")
        );
        assert!(
            validate(&[saved(1, "", 1.0, 2.0), saved(1, "", 3.0, 4.0)], None)
                .unwrap_err()
                .contains("twice")
        );
        for (start, end) in [
            (-1.0, 2.0),
            (f64::NAN, 2.0),
            (1.0, f64::NAN),
            (1.0, f64::INFINITY),
        ] {
            assert!(
                validate(&[saved(0, "", start, end)], None).is_err(),
                "{start} {end}"
            );
        }
        for (start, end) in [(2.0, 2.0), (3.0, 2.0)] {
            assert!(
                validate(&[saved(0, "", start, end)], None)
                    .unwrap_err()
                    .contains("end after")
            );
        }
        assert!(
            validate(&[saved(0, "", 199.0, 201.0)], Some(200.0))
                .unwrap_err()
                .contains("after the end")
        );
        assert_eq!(validate(&[saved(0, "", 199.0, 201.0)], None), Ok(()));
        assert!(validate(&[saved(0, &"x".repeat(256), 1.0, 2.0)], None).is_err());
        assert!(validate(&[saved(0, &"x".repeat(255), 1.0, 2.0)], None).is_ok());
    }
}
