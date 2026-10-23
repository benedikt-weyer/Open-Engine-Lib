//! Reads and writes the cues Engine stores for a track (`PerformanceData.quickCues`).
//!
//! Layout (see `docs/engine-library-format.md`): a 4-byte big-endian length and a zlib
//! stream, holding a big-endian count (always 8 in the libraries seen: one per pad), then
//! per slot a label (`u8` length and the bytes), a position in samples (`f64`, -1.0 when
//! the slot is empty) and a color (4 bytes, ARGB), then a 17-byte tail that is not about
//! the pads (it looks like the main cue point). The tail is kept as it was when a blob is
//! changed.

use std::io::{Read, Write};

use flate2::Compression;
use flate2::read::ZlibDecoder;
use flate2::write::ZlibEncoder;

/// The pads Engine has.
pub const SLOTS: usize = 8;
/// More than any real blob needs; guards against corrupt data.
const MAX_DECOMPRESSED: u64 = 1024 * 1024;
/// The longest label a slot can have: its length is a byte.
pub const MAX_LABEL: usize = 255;
/// What Engine writes after the slots when there is no main cue point: `-1.0`, a zero
/// byte and `-1.0`.
const EMPTY_TAIL: [u8; 17] = [
    0xbf, 0xf0, 0, 0, 0, 0, 0, 0, 0, 0xbf, 0xf0, 0, 0, 0, 0, 0, 0,
];

/// A cue that is set.
#[derive(Debug, Clone, PartialEq)]
pub struct Cue {
    /// 0 to 7.
    pub slot: usize,
    pub label: String,
    pub position_seconds: f64,
    /// `0xAARRGGBB`.
    pub color: u32,
}

#[derive(Debug, Clone, PartialEq)]
struct Slot {
    label: String,
    /// In samples; negative when the slot is empty.
    position: f64,
    color: [u8; 4],
}

impl Slot {
    fn empty() -> Self {
        Self {
            label: String::new(),
            position: -1.0,
            color: [0; 4],
        }
    }

    fn is_set(&self) -> bool {
        self.position.is_finite() && self.position >= 0.0
    }
}

/// The content of a `quickCues` blob.
#[derive(Debug, Clone, PartialEq)]
pub struct QuickCues {
    slots: Vec<Slot>,
    tail: Vec<u8>,
}

impl QuickCues {
    /// A track with no cue set.
    pub fn empty() -> Self {
        Self {
            slots: vec![Slot::empty(); SLOTS],
            tail: EMPTY_TAIL.to_vec(),
        }
    }

    /// The blob's content, or `None` if it is not a `quickCues` blob.
    pub fn parse(blob: &[u8]) -> Option<Self> {
        let data = decompress(blob)?;
        let count = i64::from_be_bytes(data.get(0..8)?.try_into().ok()?);
        // (Every slot takes at least 13 bytes.)
        let count = usize::try_from(count)
            .ok()
            .filter(|count| *count <= data.len() / 13)?;

        let mut at = 8;
        let mut slots = Vec::with_capacity(count);
        for _ in 0..count {
            let length = usize::from(*data.get(at)?);
            let label = String::from_utf8(data.get(at + 1..at + 1 + length)?.to_vec()).ok()?;
            at += 1 + length;
            let position = f64::from_be_bytes(data.get(at..at + 8)?.try_into().ok()?);
            let color = data.get(at + 8..at + 12)?.try_into().ok()?;
            at += 12;
            slots.push(Slot {
                label,
                position,
                color,
            });
        }
        Some(Self {
            slots,
            tail: data[at..].to_vec(),
        })
    }

    /// The blob for this content.
    pub fn encode(&self) -> Vec<u8> {
        let mut data = Vec::new();
        data.extend((self.slots.len() as i64).to_be_bytes());
        for slot in &self.slots {
            data.push(slot.label.len() as u8);
            data.extend(slot.label.as_bytes());
            data.extend(slot.position.to_be_bytes());
            data.extend(slot.color);
        }
        data.extend(&self.tail);

        let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(&data).expect("writing to memory");
        let mut blob = (data.len() as u32).to_be_bytes().to_vec();
        blob.extend(encoder.finish().expect("writing to memory"));
        blob
    }

    /// How many slots are set.
    pub fn count(&self) -> usize {
        self.slots.iter().filter(|slot| slot.is_set()).count()
    }

    /// The cues that are set, in slot order, with positions in seconds at `sample_rate`.
    pub fn cues(&self, sample_rate: f64) -> Vec<Cue> {
        self.slots
            .iter()
            .enumerate()
            .filter(|(_, slot)| slot.is_set())
            .map(|(slot, cue)| Cue {
                slot,
                label: cue.label.clone(),
                position_seconds: cue.position / sample_rate,
                color: u32::from_be_bytes(cue.color),
            })
            .collect()
    }

    /// Makes `cues` the cues that are set, and every other slot empty. The positions are in
    /// seconds at `sample_rate`. A blob with fewer slots than there are pads gets more.
    pub fn replace(&mut self, cues: &[Cue], sample_rate: f64) {
        if self.slots.len() < SLOTS {
            self.slots.resize(SLOTS, Slot::empty());
        }
        self.slots.fill(Slot::empty());
        for cue in cues {
            self.slots[cue.slot] = Slot {
                label: cue.label.clone(),
                position: cue.position_seconds * sample_rate,
                color: cue.color.to_be_bytes(),
            };
        }
    }
}

/// The sample rate at the start of a `trackData` or `beatData` blob (both begin with it, as a
/// big-endian `f64`).
pub fn sample_rate(blob: &[u8]) -> Option<f64> {
    let data = decompress(blob)?;
    let rate = f64::from_be_bytes(data.get(0..8)?.try_into().ok()?);
    (rate.is_finite() && rate > 0.0).then_some(rate)
}

/// The total number of samples of a `trackData` blob, which follows the sample rate.
pub fn sample_count(blob: &[u8]) -> Option<f64> {
    let data = decompress(blob)?;
    let count = i64::from_be_bytes(data.get(8..16)?.try_into().ok()?);
    (count > 0).then_some(count as f64)
}

/// Checks cues that are to be saved: what is wrong with them, if anything. A cue past
/// `duration_seconds` (when that is known) is wrong too.
pub fn validate(cues: &[Cue], duration_seconds: Option<f64>) -> Result<(), String> {
    let mut seen = [false; SLOTS];
    for cue in cues {
        let number = cue.slot + 1;
        let taken = seen
            .get_mut(cue.slot)
            .ok_or_else(|| format!("cue slot {number} does not exist: there are only {SLOTS}"))?;
        if std::mem::replace(taken, true) {
            return Err(format!("cue slot {number} is given twice"));
        }
        if !(cue.position_seconds.is_finite() && cue.position_seconds >= 0.0) {
            return Err(format!(
                "cue {number} does not have a position in the track"
            ));
        }
        if duration_seconds.is_some_and(|duration| cue.position_seconds > duration) {
            return Err(format!("cue {number} is after the end of the track"));
        }
        if cue.label.len() > MAX_LABEL {
            return Err(format!(
                "the label of cue {number} is longer than {MAX_LABEL} bytes"
            ));
        }
    }
    Ok(())
}

fn decompress(blob: &[u8]) -> Option<Vec<u8>> {
    let mut data = Vec::new();
    ZlibDecoder::new(blob.get(4..)?)
        .take(MAX_DECOMPRESSED)
        .read_to_end(&mut data)
        .ok()?;
    Some(data)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A `quickCues` blob the way Engine writes it: `(label, samples, ARGB)` per slot.
    fn blob(slots: &[(&str, f64, [u8; 4])], tail: &[u8]) -> Vec<u8> {
        let mut data = (slots.len() as i64).to_be_bytes().to_vec();
        for (label, position, color) in slots {
            data.push(label.len() as u8);
            data.extend(label.as_bytes());
            data.extend(position.to_be_bytes());
            data.extend(color);
        }
        data.extend(tail);
        let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(&data).unwrap();
        let mut blob = (data.len() as u32).to_be_bytes().to_vec();
        blob.extend(encoder.finish().unwrap());
        blob
    }

    const NONE: (&str, f64, [u8; 4]) = ("", -1.0, [0; 4]);
    // The first cue of a real track.
    const CUE_1: (&str, f64, [u8; 4]) = ("Cue 1", 3_668_946.943_359_375, [0xff, 0xf4, 0xd3, 0x38]);
    // A main cue point as a real track has it.
    const TAIL: [u8; 17] = [
        0x40, 0xc3, 0x94, 0x88, 0xc0, 0, 0, 0, 0, 0, 0x40, 0xc3, 0x94, 0x88, 0xc0, 0, 0,
    ];

    fn real() -> Vec<u8> {
        let mut slots = vec![CUE_1];
        slots.extend([NONE; 7]);
        blob(&slots, &TAIL)
    }

    #[test]
    fn reads_the_cues_that_are_set_in_seconds() {
        let cues = QuickCues::parse(&real()).unwrap();
        assert_eq!(cues.count(), 1);
        let cues = cues.cues(44_100.0);
        assert_eq!(cues.len(), 1);
        assert_eq!(cues[0].slot, 0);
        assert_eq!(cues[0].label, "Cue 1");
        assert!((cues[0].position_seconds - 83.2).abs() < 0.01);
        assert_eq!(cues[0].color, 0xfff4_d338);
    }

    #[test]
    fn a_blob_survives_being_read_and_written() {
        let parsed = QuickCues::parse(&real()).unwrap();
        // Same content, so the same bytes inside (the compressor may differ from Engine's,
        // and the reader doesn't mind).
        assert_eq!(QuickCues::parse(&parsed.encode()), Some(parsed.clone()));
        assert_eq!(decompress(&parsed.encode()), decompress(&real()));
        assert_eq!(parsed.tail, TAIL);
    }

    #[test]
    fn replacing_cues_sets_some_slots_and_empties_the_rest() {
        let mut cues = QuickCues::parse(&real()).unwrap();
        cues.replace(
            &[
                Cue {
                    slot: 4,
                    label: "Drop".into(),
                    position_seconds: 2.0,
                    color: 0xff86_c64b,
                },
                Cue {
                    slot: 2,
                    label: String::new(),
                    position_seconds: 0.5,
                    color: 0xffaa_55c4,
                },
            ],
            44_100.0,
        );
        let cues = QuickCues::parse(&cues.encode()).unwrap();
        assert_eq!(cues.count(), 2);
        let listed = cues.cues(44_100.0);
        assert_eq!(
            listed.iter().map(|cue| cue.slot).collect::<Vec<_>>(),
            [2, 4]
        );
        assert_eq!(listed[1].label, "Drop");
        assert_eq!(listed[1].position_seconds, 2.0);
        assert_eq!(listed[1].color, 0xff86_c64b);
        assert_eq!(cues.slots.len(), SLOTS);
        assert_eq!(cues.slots[0], Slot::empty(), "slot 1 was emptied");
        assert_eq!(cues.tail, TAIL, "the main cue point is left alone");
    }

    #[test]
    fn an_empty_blob_has_eight_empty_slots() {
        let empty = QuickCues::empty();
        assert_eq!(empty.count(), 0);
        let parsed = QuickCues::parse(&empty.encode()).unwrap();
        assert_eq!(parsed, empty);
        assert_eq!(parsed.slots.len(), SLOTS);

        // What Engine writes for a track with no cues is the same.
        assert_eq!(
            decompress(&empty.encode()),
            decompress(&blob(&[NONE; 8], &EMPTY_TAIL))
        );
    }

    #[test]
    fn a_blob_with_fewer_slots_gets_the_missing_ones() {
        let mut cues = QuickCues::parse(&blob(&[CUE_1], &EMPTY_TAIL)).unwrap();
        cues.replace(
            &[Cue {
                slot: 7,
                label: "x".into(),
                position_seconds: 1.0,
                color: 0xff15_71e2,
            }],
            44_100.0,
        );
        assert_eq!(cues.slots.len(), SLOTS);
        assert_eq!(cues.cues(44_100.0)[0].slot, 7);
    }

    #[test]
    fn sample_rate_and_count_come_from_the_start_of_the_analysis() {
        let mut data = 48_000f64.to_be_bytes().to_vec();
        data.extend(9_600_000i64.to_be_bytes());
        let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(&data).unwrap();
        let mut blob = (data.len() as u32).to_be_bytes().to_vec();
        blob.extend(encoder.finish().unwrap());

        assert_eq!(sample_rate(&blob), Some(48_000.0));
        assert_eq!(sample_count(&blob), Some(9_600_000.0));
        assert_eq!(sample_rate(&[]), None);
        assert_eq!(sample_rate(&[0, 0, 0, 4, 1, 2]), None);
    }

    #[test]
    fn garbage_is_no_cues() {
        assert_eq!(QuickCues::parse(&[]), None);
        assert_eq!(QuickCues::parse(&[0, 0, 0, 9, 1, 2, 3]), None);
        // A valid stream cut short, and a count that promises more than there is.
        let good = real();
        assert_eq!(QuickCues::parse(&good[..good.len() / 2]), None);
        let data = i64::MAX.to_be_bytes();
        let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(&data).unwrap();
        let mut blob = 8u32.to_be_bytes().to_vec();
        blob.extend(encoder.finish().unwrap());
        assert_eq!(QuickCues::parse(&blob), None);
    }

    #[test]
    fn checks_cues_before_they_are_saved() {
        let cue = |slot, position_seconds, label: &str| Cue {
            slot,
            label: label.to_string(),
            position_seconds,
            color: 0xffff_ffff,
        };
        assert_eq!(
            validate(&[cue(0, 1.0, "a"), cue(7, 200.0, "")], Some(200.0)),
            Ok(())
        );
        assert_eq!(validate(&[], None), Ok(()));
        assert!(
            validate(&[cue(8, 1.0, "")], None)
                .unwrap_err()
                .contains("only 8")
        );
        assert!(
            validate(&[cue(1, 1.0, ""), cue(1, 2.0, "")], None)
                .unwrap_err()
                .contains("twice")
        );
        for position in [-1.0, f64::NAN, f64::INFINITY] {
            assert!(
                validate(&[cue(0, position, "")], None).is_err(),
                "{position}"
            );
        }
        assert!(
            validate(&[cue(0, 201.0, "")], Some(200.0))
                .unwrap_err()
                .contains("after the end")
        );
        assert_eq!(validate(&[cue(0, 201.0, "")], None), Ok(()));
        assert!(validate(&[cue(0, 1.0, &"x".repeat(256))], None).is_err());
        assert!(validate(&[cue(0, 1.0, &"x".repeat(255))], None).is_ok());
    }
}
