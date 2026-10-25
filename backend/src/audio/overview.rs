//! The overview of a track: how loud its low, mid and high frequencies are, in 1024 steps
//! over the whole track, in the form Engine keeps it in `PerformanceData.overviewWaveFormData`
//! (see `docs/engine-library-format.md`). Players draw the strip of the whole track from it.
//!
//! Engine's own filters and scaling are not known, so this is an approximation made from what
//! the stored overviews of real libraries look like: the bands meet at about 250 Hz and
//! 1.8 kHz, an entry is the RMS of its band over its stretch of the track, times a gain per
//! band chosen so that a track comes out as loud, on average, as Engine's overviews of tracks
//! in a real library are.

use std::fs::File;
use std::io;
use std::path::Path;

use rodio::{Decoder, Source};

use super::waveform::{AnalyzeError, Biquad};

/// How many steps the track is cut into.
pub const STEPS: usize = 1024;
/// Where the bands meet, in Hz.
const LOW_MID_HZ: f32 = 250.0;
const MID_HIGH_HZ: f32 = 1800.0;
/// A filter can't work close to half the sample rate.
const NYQUIST_SHARE: f32 = 0.45;
/// What an entry is, per band, in 255ths of the band's RMS times this (a full-scale sine's RMS
/// is 0.707). Chosen with `calibrates_against_music` so that the tracks of a folder of techno
/// come out like the overviews of a real library: Engine's mean entry is 90, 65 and 19, and its
/// largest 180, 125 and 49 on average. The low band is the one that depends most on the music
/// (that folder is bass-heavy), so its gain is between what the means and the maxima ask for.
const GAIN: [f32; 3] = [1.3, 2.15, 0.85];
/// The time the measuring is done in before it is cut into steps: 10 ms.
const COLUMNS_PER_SECOND: u64 = 100;

/// The energy of each band in each 10 ms of a track.
pub struct Levels {
    /// The sum of the squares of the band's samples, and how many there are.
    columns: Vec<([f64; 3], u32)>,
}

/// Decodes the whole file at `path` and measures it.
pub fn analyze_file(path: &Path) -> Result<Levels, AnalyzeError> {
    let file = File::open(path).map_err(|error| match error.kind() {
        io::ErrorKind::NotFound => AnalyzeError::NotFound,
        _ => AnalyzeError::Failed(error.to_string()),
    })?;
    let source =
        Decoder::try_from(file).map_err(|error| AnalyzeError::Failed(error.to_string()))?;
    let channels = source.channels().get();
    let sample_rate = source.sample_rate().get();
    Ok(measure(source, channels, sample_rate))
}

/// The levels of interleaved `samples`.
pub fn measure(samples: impl Iterator<Item = f32>, channels: u16, sample_rate: u32) -> Levels {
    let channels = usize::from(channels.max(1));
    let ceiling = sample_rate as f32 * NYQUIST_SHARE;
    let high_hz = MID_HIGH_HZ.min(ceiling);
    let low_hz = LOW_MID_HZ.min(high_hz / 2.0);
    let mut low_pass = Biquad::low_pass(low_hz, sample_rate);
    let mut mid_high_pass = Biquad::high_pass(low_hz, sample_rate);
    let mut mid_low_pass = Biquad::low_pass(high_hz, sample_rate);
    let mut high_pass = Biquad::high_pass(high_hz, sample_rate);

    let mut columns: Vec<([f64; 3], u32)> = Vec::new();
    let (mut frame, mut sum, mut in_frame) = (0u64, 0.0f32, 0usize);
    for sample in samples {
        sum += sample;
        in_frame += 1;
        if in_frame < channels {
            continue;
        }
        let mono = sum / channels as f32;
        (sum, in_frame) = (0.0, 0);
        let bands = [
            low_pass.process(mono),
            mid_low_pass.process(mid_high_pass.process(mono)),
            high_pass.process(mono),
        ];
        let column = (frame * COLUMNS_PER_SECOND / u64::from(sample_rate)) as usize;
        if column >= columns.len() {
            columns.resize(column + 1, ([0.0; 3], 0));
        }
        for (total, band) in columns[column].0.iter_mut().zip(bands) {
            *total += f64::from(band) * f64::from(band);
        }
        columns[column].1 += 1;
        frame += 1;
    }
    Levels { columns }
}

impl Levels {
    /// The entries of the overview: `STEPS` of them, one per stretch of the track, low, mid
    /// and high, and then one more, the largest of each band, which is how Engine ends it.
    /// Empty for a track without sound data.
    pub fn entries(&self) -> Vec<[u8; 3]> {
        let count = self.columns.len();
        if count == 0 {
            return Vec::new();
        }
        let mut entries = Vec::with_capacity(STEPS + 1);
        let mut largest = [0u8; 3];
        for step in 0..STEPS {
            let start = (step * count / STEPS).min(count - 1);
            let end = ((step + 1) * count / STEPS).clamp(start + 1, count);
            let mut sums = [0.0f64; 3];
            let mut samples = 0u64;
            for (squares, n) in &self.columns[start..end] {
                for (sum, square) in sums.iter_mut().zip(squares) {
                    *sum += square;
                }
                samples += u64::from(*n);
            }
            let mut entry = [0u8; 3];
            for band in 0..3 {
                let rms = if samples == 0 {
                    0.0
                } else {
                    (sums[band] / samples as f64).sqrt() as f32
                };
                entry[band] = (255.0 * GAIN[band] * rms).round().clamp(0.0, 255.0) as u8;
                largest[band] = largest[band].max(entry[band]);
            }
            entries.push(entry);
        }
        entries.push(largest);
        entries
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine(hz: f32, seconds: f32, amplitude: f32, sample_rate: u32) -> Vec<f32> {
        (0..(seconds * sample_rate as f32) as usize)
            .map(|n| amplitude * (std::f32::consts::TAU * hz * n as f32 / sample_rate as f32).sin())
            .collect()
    }

    #[test]
    fn each_band_hears_its_own_frequencies_and_the_last_entry_is_the_largest() {
        let rate = 44_100;
        // 100 Hz, 800 Hz and 6 kHz, each for a third of the track.
        let mut samples = sine(100.0, 3.0, 0.5, rate);
        samples.extend(sine(800.0, 3.0, 0.5, rate));
        samples.extend(sine(6000.0, 3.0, 0.5, rate));
        let entries = measure(samples.into_iter(), 1, rate).entries();
        assert_eq!(entries.len(), STEPS + 1);

        let loudest = |entry: &[u8; 3]| (0..3).max_by_key(|&band| entry[band]).unwrap();
        assert_eq!(loudest(&entries[STEPS / 6]), 0, "low in the first third");
        assert_eq!(loudest(&entries[STEPS / 2]), 1, "mid in the second");
        assert_eq!(loudest(&entries[STEPS * 5 / 6]), 2, "high in the last");
        // A sine of amplitude 0.5 has the RMS 0.354.
        let expected = 255.0 * GAIN[0] * 0.354;
        assert!(
            (f32::from(entries[STEPS / 6][0]) - expected).abs() < 6.0,
            "{:?} for {expected}",
            entries[STEPS / 6]
        );
        let largest = entries[STEPS];
        for band in 0..3 {
            assert_eq!(
                largest[band],
                entries[..STEPS]
                    .iter()
                    .map(|entry| entry[band])
                    .max()
                    .unwrap()
            );
        }
    }

    #[test]
    fn silence_is_zero_and_a_stereo_file_is_its_mix() {
        let rate = 44_100;
        let silent = measure(std::iter::repeat_n(0.0, rate as usize * 2), 2, rate).entries();
        assert!(silent.iter().all(|entry| *entry == [0, 0, 0]));
        assert!(measure(std::iter::empty(), 2, rate).entries().is_empty());

        // The same tone on both channels is as loud as on one.
        let one = sine(100.0, 2.0, 0.25, rate);
        let both: Vec<f32> = one.iter().flat_map(|&s| [s, s]).collect();
        let mono = measure(one.into_iter(), 1, rate).entries();
        let stereo = measure(both.into_iter(), 2, rate).entries();
        assert_eq!(mono[STEPS / 2], stereo[STEPS / 2]);
    }

    #[test]
    fn a_short_track_still_fills_every_step() {
        let rate = 8_000;
        let entries = measure(sine(100.0, 0.5, 0.5, rate).into_iter(), 1, rate).entries();
        assert_eq!(entries.len(), STEPS + 1);
        assert!(entries[..STEPS].iter().all(|entry| entry[0] > 0));
    }

    /// The mean of what the overview of each track of a folder of music comes to, to compare
    /// with real libraries (90, 65 and 19 on average, and 180, 125 and 49 for the largest
    /// entry of a track):
    /// `OPEN_ENGINE_OVERVIEW_DIR=~/Music/tiddl cargo test -p open-engine-backend --bin open-engine-audio overview::tests::calibrates -- --ignored --nocapture`
    #[test]
    #[ignore = "needs a folder of music (OPEN_ENGINE_OVERVIEW_DIR)"]
    fn calibrates_against_music() {
        fn files(dir: &Path, found: &mut Vec<std::path::PathBuf>) {
            for entry in std::fs::read_dir(dir).unwrap().flatten() {
                let path = entry.path();
                if path.is_dir() {
                    files(&path, found);
                } else if matches!(
                    path.extension().and_then(|e| e.to_str()),
                    Some("flac" | "mp3" | "wav")
                ) {
                    found.push(path);
                }
            }
        }
        let dir = std::env::var("OPEN_ENGINE_OVERVIEW_DIR").expect("OPEN_ENGINE_OVERVIEW_DIR");
        let mut found = Vec::new();
        files(Path::new(&dir), &mut found);
        let (mut means, mut maxima, mut tracks) = ([0.0f64; 3], [0.0f64; 3], 0.0f64);
        for path in found {
            let Ok(levels) = analyze_file(&path) else {
                continue;
            };
            let entries = levels.entries();
            if entries.is_empty() {
                continue;
            }
            for band in 0..3 {
                means[band] += entries[..STEPS]
                    .iter()
                    .map(|e| f64::from(e[band]))
                    .sum::<f64>()
                    / STEPS as f64;
                maxima[band] += f64::from(entries[STEPS][band]);
            }
            tracks += 1.0;
        }
        println!(
            "{tracks} tracks: mean entry {:.1} {:.1} {:.1}, largest entry {:.1} {:.1} {:.1}",
            means[0] / tracks,
            means[1] / tracks,
            means[2] / tracks,
            maxima[0] / tracks,
            maxima[1] / tracks,
            maxima[2] / tracks,
        );
    }
}
