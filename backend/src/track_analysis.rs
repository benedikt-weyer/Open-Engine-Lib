//! What the analysis of a track (`audio/analysis.rs`: the beats from Beat This!, the key from
//! libKeyFinder) is written as in an Engine library: the beatgrid (`beatData`), the track
//! data (`trackData`) and the key and tempo of the track. See `docs/engine-library-format.md`
//! for the layouts.
//!
//! The beatgrid Engine stores is two markers, one before the start of the audio and one after
//! its end, with the beats evenly spaced between them, so the beats found are fitted with
//! one steady tempo. A track whose tempo changes keeps the average.

use std::io::{Read, Write};

use flate2::Compression;
use flate2::read::ZlibDecoder;
use flate2::write::ZlibEncoder;

/// What is saved as having made the analysis.
pub const TOOL: &str = "Beat This! (beats) and libKeyFinder (key)";

/// The slowest and fastest tempo (beats per minute) that is taken as a tempo.
const MIN_BPM: f64 = 40.0;
const MAX_BPM: f64 = 300.0;
/// Fewer beats than this are no grid.
const MIN_BEATS: usize = 8;
/// The length of `trackData` Engine writes.
const TRACK_DATA_SIZE: usize = 44;
/// More than any `trackData` needs; guards against corrupt data.
const MAX_DECOMPRESSED: u64 = 4 * 1024 * 1024;
/// A marker is a position, a beat number and two more numbers.
const MARKER_BYTES: usize = 24;

/// The name of a key of libKeyFinder, as musicians write it.
pub fn key_name(key: u32) -> Option<&'static str> {
    const NAMES: [&str; 24] = [
        "A major",
        "A minor",
        "B♭ major",
        "B♭ minor",
        "B major",
        "B minor",
        "C major",
        "C minor",
        "D♭ major",
        "D♭ minor",
        "D major",
        "D minor",
        "E♭ major",
        "E♭ minor",
        "E major",
        "E minor",
        "F major",
        "F minor",
        "G♭ major",
        "G♭ minor",
        "G major",
        "G minor",
        "A♭ major",
        "A♭ minor",
    ];
    NAMES.get(key as usize).copied()
}

/// The number Engine gives the key (`Track.key`) that libKeyFinder's `key` (0 to 23: major
/// and minor for each semitone from A) names. Engine numbers the keys along the circle of
/// fifths from C major, each major key followed by its relative minor: 0 C major, 1 A minor,
/// 2 G major, 3 E minor, 4 D major, ... 22 F major, 23 D minor. (Worked out from the keys of
/// 150 tracks that Engine and libKeyFinder both analysed: 95 got the same number, and most
/// of the others were close on the circle: they are two analyses that disagree.)
pub fn engine_key(key: u32) -> Option<i32> {
    if key >= 24 {
        return None;
    }
    let (semitones_from_a, minor) = (key / 2, key % 2);
    let root = (9 + semitones_from_a) % 12; // (C is 0)
    let major_root = if minor == 1 { (root + 3) % 12 } else { root };
    let along_the_fifths = (major_root * 7) % 12;
    Some((2 * along_the_fifths + minor) as i32)
}

/// What the audio process found in a track.
#[derive(Debug, Clone, PartialEq)]
pub struct Found {
    /// The time of every beat, in seconds.
    pub beats: Vec<f64>,
    /// The time of the beats that start a bar.
    pub downbeats: Vec<f64>,
    /// The sample rate of the file, and how many samples (per channel) it has.
    pub sample_rate: u32,
    pub frames: u64,
    /// libKeyFinder's key, 0 to 23, if it heard one.
    pub key: Option<u32>,
}

/// A steady beat grid: the tempo and the two markers Engine stores.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Grid {
    pub bpm: f64,
    /// A marker: the position in samples and the beat number.
    pub first: (f64, i64),
    pub last: (f64, i64),
}

/// Fits one steady tempo to the beats (`beats` and `downbeats` are times in seconds, the
/// audio has `frames` samples at `sample_rate`). Beat numbers count from the first downbeat,
/// which is beat 0, so that multiples of 4 start a bar like in Engine's grids. The first
/// marker is the multiple of 4 before the start of the audio, the last the first beat after
/// its end. `None` if there are too few beats or the tempo is not one.
pub fn fit_grid(beats: &[f64], downbeats: &[f64], sample_rate: u32, frames: u64) -> Option<Grid> {
    if beats.len() < MIN_BEATS || sample_rate == 0 {
        return None;
    }
    let mut intervals: Vec<f64> = beats
        .windows(2)
        .map(|pair| pair[1] - pair[0])
        .filter(|interval| *interval > 0.0)
        .collect();
    intervals.sort_by(f64::total_cmp);
    let mut period = *intervals.get(intervals.len() / 2)?;

    // Least squares of the time of a beat over its number in the row. The beats are counted
    // one after the other (the network says its times in steps of 20 ms, so a period taken from
    // them is off by a little, which would add up if the beats were numbered by dividing their
    // time by it); a gap of a beat the network missed counts as the beats it holds, and a beat
    // too close to the one before it (the same beat found twice) is left out. Done twice, as
    // the first period is only the median.
    let mut counted: Vec<(f64, f64)> = Vec::new();
    let mut start = beats[0];
    for _ in 0..2 {
        counted.clear();
        let (mut number, mut last) = (0.0, beats[0]);
        counted.push((0.0, beats[0]));
        for &time in &beats[1..] {
            let steps = ((time - last) / period).round();
            if steps < 1.0 {
                continue;
            }
            number += steps;
            last = time;
            counted.push((number, time));
        }
        let count = counted.len() as f64;
        let (sum_k, sum_t, sum_kk, sum_kt) = counted.iter().fold(
            (0.0, 0.0, 0.0, 0.0),
            |(sum_k, sum_t, sum_kk, sum_kt), &(k, t)| {
                (sum_k + k, sum_t + t, sum_kk + k * k, sum_kt + k * t)
            },
        );
        let denominator = count * sum_kk - sum_k * sum_k;
        if denominator.abs() < f64::EPSILON {
            return None;
        }
        period = (count * sum_kt - sum_k * sum_t) / denominator;
        start = (sum_t - period * sum_k) / count;
        if !(period.is_finite() && period > 0.0) {
            return None;
        }
    }
    let bpm = 60.0 / period;
    if !(MIN_BPM..=MAX_BPM).contains(&bpm) {
        return None;
    }

    // The first downbeat is beat 0 (without downbeats, the first beat). It is one of the
    // beats, so it is the one that is nearest, whatever the tempo did meanwhile.
    let beat_0 = match downbeats.first() {
        Some(&downbeat) => {
            let nearest = counted
                .iter()
                .min_by(|a, b| (a.1 - downbeat).abs().total_cmp(&(b.1 - downbeat).abs()))
                .map_or(0.0, |&(k, _)| k);
            start + period * nearest
        }
        None => start,
    };
    let at = |number: i64| (beat_0 + period * number as f64) * f64::from(sample_rate);

    // (Strictly before the start: a marker at exactly 0 is not before it.)
    let before = ((beat_0 / (4.0 * period)).floor() + 1.0).max(1.0) as i64;
    let first = -4 * before;
    let seconds = frames as f64 / f64::from(sample_rate);
    let last = ((seconds - beat_0) / period).ceil().max((first + 1) as f64) as i64;
    Some(Grid {
        bpm,
        first: (at(first), first),
        last: (at(last), last),
    })
}

fn compress(data: &[u8]) -> Vec<u8> {
    let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(data).expect("writing to memory");
    let mut blob = (data.len() as u32).to_be_bytes().to_vec();
    blob.extend(encoder.finish().expect("writing to memory"));
    blob
}

fn decompress(blob: &[u8]) -> Option<Vec<u8>> {
    let mut data = Vec::new();
    ZlibDecoder::new(blob.get(4..)?)
        .take(MAX_DECOMPRESSED)
        .read_to_end(&mut data)
        .ok()?;
    Some(data)
}

/// The `beatData` blob of `grid` for audio with `frames` samples at `sample_rate`: the same
/// grid as analysed and as adjusted.
pub fn beat_data(sample_rate: u32, frames: u64, grid: &Grid) -> Vec<u8> {
    let mut data = Vec::new();
    data.extend(f64::from(sample_rate).to_be_bytes());
    data.extend((frames as f64).to_be_bytes());
    data.push(1); // the grid is set
    for _ in 0..2 {
        data.extend(2i64.to_be_bytes());
        for ((position, number), beats_until_next) in [
            (grid.first, (grid.last.1 - grid.first.1) as i32),
            (grid.last, 0),
        ] {
            let before = data.len();
            data.extend(position.to_le_bytes());
            data.extend(number.to_le_bytes());
            data.extend(beats_until_next.to_le_bytes());
            data.extend(0i32.to_le_bytes());
            debug_assert_eq!(data.len() - before, MARKER_BYTES);
        }
    }
    // A byte and a number that Engine writes as zeros for most tracks, see the format notes.
    data.extend([0u8; 9]);
    compress(&data)
}

/// The `trackData` blob for audio with `frames` samples at `sample_rate` and the Engine
/// number of its `key`. The three band levels that follow the key in the blob are kept from
/// `existing` (when the track already has track data), else they are zeros, which 76 % of the
/// tracks of the library they were looked at in have too; so is whatever follows them.
pub fn track_data(
    existing: Option<&[u8]>,
    sample_rate: u32,
    frames: u64,
    key: Option<i32>,
) -> Vec<u8> {
    let mut data = existing
        .and_then(decompress)
        .filter(|data| data.len() >= TRACK_DATA_SIZE)
        .unwrap_or_else(|| {
            let mut fresh = vec![0u8; TRACK_DATA_SIZE];
            // (No key is -1 in Engine.)
            fresh[16..20].copy_from_slice(&(-1i32).to_be_bytes());
            fresh
        });
    data[0..8].copy_from_slice(&f64::from(sample_rate).to_be_bytes());
    data[8..16].copy_from_slice(&(frames as i64).to_be_bytes());
    if let Some(key) = key {
        data[16..20].copy_from_slice(&key.to_be_bytes());
    }
    compress(&data)
}

/// A number that tells blobs apart, to notice that a grid is not the one that was written.
pub fn fingerprint(blob: &[u8]) -> i64 {
    i64::from(crc32fast::hash(blob))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::beatgrid;

    /// The beats of a steady `bpm`, `count` of them from `first`.
    fn steady(first: f64, bpm: f64, count: usize) -> Vec<f64> {
        (0..count).map(|k| first + k as f64 * 60.0 / bpm).collect()
    }

    #[test]
    fn keys_have_names_and_engine_numbers() {
        assert_eq!(key_name(0), Some("A major"));
        assert_eq!(key_name(23), Some("A♭ minor"));
        assert_eq!(key_name(24), None);

        // Engine numbers the keys along the circle of fifths from C major, each major
        // followed by its relative minor.
        let engine = |name: &str| {
            (0..24)
                .find(|key| key_name(*key) == Some(name))
                .and_then(engine_key)
        };
        assert_eq!(engine("C major"), Some(0));
        assert_eq!(engine("A minor"), Some(1));
        assert_eq!(engine("G major"), Some(2));
        assert_eq!(engine("E minor"), Some(3));
        assert_eq!(engine("D major"), Some(4));
        assert_eq!(engine("B minor"), Some(5));
        assert_eq!(engine("E major"), Some(8));
        assert_eq!(engine("D♭ minor"), Some(9));
        assert_eq!(engine("E♭ minor"), Some(13));
        assert_eq!(engine("B♭ minor"), Some(15));
        assert_eq!(engine("C minor"), Some(19));
        assert_eq!(engine("F major"), Some(22));
        assert_eq!(engine("D minor"), Some(23));
        // Every key gets its own number.
        let mut all: Vec<i32> = (0..24).map(|key| engine_key(key).unwrap()).collect();
        all.sort();
        assert_eq!(all, (0..24).collect::<Vec<_>>());
        assert_eq!(engine_key(24), None);
    }

    #[test]
    fn a_steady_tempo_gives_markers_like_engines() {
        // 126 BPM from a first beat a little after the start, a 4 minute track at 44.1 kHz.
        let beats = steady(0.31, 126.0, 480);
        let downbeats: Vec<f64> = beats.iter().copied().step_by(4).collect();
        let frames = 4 * 60 * 44_100;
        let grid = fit_grid(&beats, &downbeats, 44_100, frames).unwrap();

        assert!((grid.bpm - 126.0).abs() < 1e-6, "{}", grid.bpm);
        // The first downbeat is beat 0, so the marker before the start is beat -4, with a
        // negative position four beats earlier than beat 0.
        assert_eq!(grid.first.1, -4);
        let period = 60.0 / 126.0;
        assert!((grid.first.0 / 44_100.0 - (0.31 - 4.0 * period)).abs() < 1e-6);
        // The last marker is the first beat after the end.
        assert!(grid.last.0 >= frames as f64);
        assert!(grid.last.0 - frames as f64 <= period * 44_100.0);
        assert!(
            beats[0] + period * grid.last.1 as f64 >= 240.0,
            "after the end"
        );
    }

    #[test]
    fn the_bar_starts_at_the_first_downbeat_and_missing_beats_do_not_bend_the_grid() {
        // The first downbeat is the third beat; the network missed two beats on the way.
        let mut beats = steady(1.1, 120.0, 100);
        beats.remove(40);
        beats.remove(41);
        let downbeats = vec![beats[2]];
        let grid = fit_grid(&beats, &downbeats, 48_000, 60 * 48_000).unwrap();
        assert!((grid.bpm - 120.0).abs() < 1e-6, "{}", grid.bpm);
        // Beat 0 is at 2.1 s (1.1 + 2 * 0.5); the marker at beat -4 is at 0.1 s, which is
        // not before the start, so the next multiple of 4 back is used, at -1.9 s.
        assert_eq!(grid.first.1, -8);
        assert!((grid.first.0 / 48_000.0 - -1.9).abs() < 1e-6);
        assert_eq!(grid.first.1 % 4, 0);
    }

    /// What the network gives: a beat time in steps of 20 ms.
    fn quantised(beats: &[f64]) -> Vec<f64> {
        beats
            .iter()
            .map(|time| (time / 0.02).round() * 0.02)
            .collect()
    }

    #[test]
    fn times_in_steps_of_twenty_milliseconds_still_give_the_real_tempo() {
        // 135 BPM is 0.4444 s a beat, which becomes 0.44 and 0.46 s: the median interval is
        // 136.4 BPM, and a grid numbered by it would be a whole percent too fast.
        let beats = quantised(&steady(0.222, 135.0, 450));
        let grid = fit_grid(&beats, &[], 44_100, 44_100 * 200).unwrap();
        assert!((grid.bpm - 135.0).abs() < 0.05, "{}", grid.bpm);
    }

    #[test]
    fn a_beat_found_twice_and_a_beat_missed_do_not_bend_the_grid() {
        let mut beats = quantised(&steady(0.222, 135.0, 450));
        // A beat that is found twice (a little after the first time) ...
        beats.insert(101, beats[100] + 0.12);
        // ... and two missed in a row.
        beats.remove(300);
        beats.remove(300);
        let grid = fit_grid(&beats, &[beats[0]], 44_100, 44_100 * 200).unwrap();
        assert!((grid.bpm - 135.0).abs() < 0.05, "{}", grid.bpm);
        // Beat 0 is the first beat, still at 0.222 s: the marker before it is four beats back.
        let period = 60.0 / 135.0;
        assert!((grid.first.0 / 44_100.0 - (0.222 - 4.0 * period)).abs() < 0.02);
    }

    #[test]
    fn no_grid_without_a_tempo() {
        assert!(fit_grid(&[], &[], 44_100, 44_100).is_none());
        assert!(fit_grid(&steady(0.0, 120.0, 7), &[], 44_100, 441_000).is_none());
        // 20 BPM and 400 BPM are no tempos of a track.
        assert!(fit_grid(&steady(0.0, 20.0, 40), &[], 44_100, 44_100 * 100).is_none());
        assert!(fit_grid(&steady(0.0, 400.0, 400), &[], 44_100, 44_100 * 100).is_none());
        assert!(fit_grid(&steady(0.0, 120.0, 40), &[], 0, 100).is_none());
    }

    #[test]
    fn the_beat_data_can_be_read_back_like_any_engine_grid() {
        let beats = steady(0.31, 126.0, 480);
        let grid = fit_grid(&beats, &[beats[0]], 44_100, 10_584_000).unwrap();
        let blob = beat_data(44_100, 10_584_000, &grid);

        let markers = beatgrid::parse(&blob).expect("a grid");
        assert_eq!(markers.len(), 2);
        assert_eq!(markers[0].beat_number, grid.first.1);
        assert_eq!(markers[1].beat_number, grid.last.1);
        assert!((markers[0].position_seconds - grid.first.0 / 44_100.0).abs() < 1e-9);
        let bpm = 60.0 * (markers[1].beat_number - markers[0].beat_number) as f64
            / (markers[1].position_seconds - markers[0].position_seconds);
        assert!((bpm - 126.0).abs() < 1e-6, "{bpm}");

        // The sample rate and count come first, as in Engine's blobs.
        let data = decompress(&blob).unwrap();
        assert_eq!(f64::from_be_bytes(data[0..8].try_into().unwrap()), 44_100.0);
        assert_eq!(
            f64::from_be_bytes(data[8..16].try_into().unwrap()),
            10_584_000.0
        );
        assert_eq!(data[16], 1);
        // Two lists of two markers, and nine bytes after them: 17 + 2 * (8 + 2 * 24) + 9.
        assert_eq!(data.len(), 17 + 2 * (8 + 2 * 24) + 9);
        let declared = u32::from_be_bytes(blob[0..4].try_into().unwrap()) as usize;
        assert_eq!(declared, data.len());
    }

    #[test]
    fn track_data_has_the_rate_the_samples_and_the_key() {
        let blob = track_data(None, 44_100, 9_084_600, Some(13));
        let data = decompress(&blob).unwrap();
        assert_eq!(data.len(), TRACK_DATA_SIZE);
        assert_eq!(f64::from_be_bytes(data[0..8].try_into().unwrap()), 44_100.0);
        assert_eq!(
            i64::from_be_bytes(data[8..16].try_into().unwrap()),
            9_084_600
        );
        assert_eq!(i32::from_be_bytes(data[16..20].try_into().unwrap()), 13);
        assert!(data[20..].iter().all(|byte| *byte == 0), "no band levels");

        // No key heard: none, which is -1 in Engine.
        let data = decompress(&track_data(None, 44_100, 1, None)).unwrap();
        assert_eq!(i32::from_be_bytes(data[16..20].try_into().unwrap()), -1);
    }

    #[test]
    fn existing_track_data_keeps_what_the_analysis_does_not_know() {
        // The 68-byte form with band levels, as Engine writes it.
        let mut old = vec![0u8; 68];
        old[16..20].copy_from_slice(&5i32.to_be_bytes());
        old[20..28].copy_from_slice(&0.75f64.to_be_bytes());
        old[44..52].copy_from_slice(&0.25f64.to_be_bytes());
        let old = compress(&old);

        let new = decompress(&track_data(Some(&old), 48_000, 4_800_000, Some(9))).unwrap();
        assert_eq!(new.len(), 68, "the form it had");
        assert_eq!(f64::from_be_bytes(new[0..8].try_into().unwrap()), 48_000.0);
        assert_eq!(i32::from_be_bytes(new[16..20].try_into().unwrap()), 9);
        assert_eq!(f64::from_be_bytes(new[20..28].try_into().unwrap()), 0.75);
        assert_eq!(f64::from_be_bytes(new[44..52].try_into().unwrap()), 0.25);

        // Without a key found, the key it had stays.
        let kept = decompress(&track_data(Some(&old), 48_000, 4_800_000, None)).unwrap();
        assert_eq!(i32::from_be_bytes(kept[16..20].try_into().unwrap()), 5);

        // Track data that can't be read is replaced by fresh data.
        let fresh = decompress(&track_data(Some(b"junk"), 44_100, 1, Some(2))).unwrap();
        assert_eq!(fresh.len(), TRACK_DATA_SIZE);
    }

    #[test]
    fn a_fingerprint_changes_with_the_blob() {
        assert_ne!(fingerprint(b"one"), fingerprint(b"two"));
        assert_eq!(fingerprint(b"one"), fingerprint(b"one"));
    }
}
