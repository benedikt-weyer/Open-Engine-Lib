//! The waveform of a track: how loud its low, mid and high frequencies are over time,
//! in small steps, for drawing it like a DJ player does.
//!
//! The library only stores a coarse overview, which is too rough to zoom into, so the
//! waveform is computed from the audio file.

use std::fs::File;
use std::io;
use std::path::Path;

use rodio::{Decoder, Source};

/// Columns per second of audio: one per 10 ms.
pub const COLUMNS_PER_SECOND: u32 = 100;

/// Where the bands are split by default, in Hz: low below about 600 Hz, mid up to about
/// 4 kHz, high above that.
pub const DEFAULT_LOW_MID_HZ: u32 = open_engine_api::WAVEFORM_LOW_MID_HZ;
pub const DEFAULT_MID_HIGH_HZ: u32 = open_engine_api::WAVEFORM_MID_HIGH_HZ;
/// The split points can be set anywhere in this range (the audible one), as long as the
/// first is below the second.
pub const MIN_SPLIT_HZ: u32 = open_engine_api::WAVEFORM_MIN_SPLIT_HZ;
pub const MAX_SPLIT_HZ: u32 = open_engine_api::WAVEFORM_MAX_SPLIT_HZ;
/// A filter can't work close to half the sample rate, so split points are kept below
/// this share of it.
const NYQUIST_SHARE: f32 = 0.45;
/// The curve from a band's level (0 to 1) to the height it is drawn at: below 1, so that
/// quiet parts stay visible next to loud ones. The same for every track and band.
const GAMMA: f32 = 0.8;

/// Where the low, mid and high bands meet, in Hz.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Split {
    pub low_mid_hz: u32,
    pub mid_high_hz: u32,
}

impl Default for Split {
    fn default() -> Self {
        Self {
            low_mid_hz: DEFAULT_LOW_MID_HZ,
            mid_high_hz: DEFAULT_MID_HIGH_HZ,
        }
    }
}

impl Split {
    /// Checks that the split points are in range and in order.
    pub fn validate(self) -> Result<(), String> {
        let in_range = |hz: u32| (MIN_SPLIT_HZ..=MAX_SPLIT_HZ).contains(&hz);
        if !in_range(self.low_mid_hz) || !in_range(self.mid_high_hz) {
            return Err(format!(
                "the split frequencies must be between {MIN_SPLIT_HZ} and {MAX_SPLIT_HZ} Hz"
            ));
        }
        if self.low_mid_hz >= self.mid_high_hz {
            return Err("the low/mid split must be below the mid/high split".to_owned());
        }
        Ok(())
    }

    /// The split points as the filters get them: for a file with a low sample rate, points
    /// near or above what it can hold are pulled down so that the bands still exist.
    fn for_sample_rate(self, sample_rate: u32) -> (f32, f32) {
        let ceiling = sample_rate as f32 * NYQUIST_SHARE;
        let high = (self.mid_high_hz as f32).min(ceiling);
        let low = (self.low_mid_hz as f32).min(high / 2.0);
        (low, high)
    }
}

/// How much each band is amplified before it is drawn (see `Peaks::scaled`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Gains {
    pub low: f32,
    pub mid: f32,
    pub high: f32,
}

impl Gains {
    /// Gains that leave the levels as they are.
    #[cfg(test)]
    pub const UNITY: Gains = Gains {
        low: 1.0,
        mid: 1.0,
        high: 1.0,
    };

    /// Checks that every gain is a positive number that isn't absurd.
    pub fn validate(self) -> Result<(), String> {
        let sensible = |gain: f32| gain.is_finite() && gain > 0.0 && gain <= MAX_GAIN;
        if [self.low, self.mid, self.high].into_iter().all(sensible) {
            Ok(())
        } else {
            Err(format!("the gains must be above 0 and at most {MAX_GAIN}"))
        }
    }
}

impl Default for Gains {
    fn default() -> Self {
        Self {
            low: open_engine_api::WAVEFORM_GAIN_LOW,
            mid: open_engine_api::WAVEFORM_GAIN_MID,
            high: open_engine_api::WAVEFORM_GAIN_HIGH,
        }
    }
}

const MAX_GAIN: f32 = open_engine_api::WAVEFORM_MAX_GAIN;

/// The loudest sample of each band in every 10 ms of a track, as a share of full scale
/// (a full-scale sine wave in a band is 1). Raw: nothing is scaled to the track.
#[derive(Debug, Clone, PartialEq)]
pub struct Peaks {
    pub duration_seconds: f64,
    /// Low, mid and high peak per column.
    pub columns: Vec<[f32; 3]>,
}

impl Peaks {
    /// The heights to draw, 0 to 255 per band: the peak times the band's gain, up to full
    /// height, then curved a little to show quiet parts. The scale is fixed, not fitted to
    /// the track, so a quiet track is drawn lower than a loud one and a band the track
    /// has little of is drawn low.
    pub fn scaled(&self, gains: Gains) -> Vec<[u8; 3]> {
        let gains = [gains.low, gains.mid, gains.high];
        self.columns
            .iter()
            .map(|peak| {
                let mut column = [0u8; 3];
                for band in 0..3 {
                    let level = (peak[band] * gains[band]).clamp(0.0, 1.0).powf(GAMMA);
                    column[band] = (level * 255.0).round() as u8;
                }
                column
            })
            .collect()
    }
}

#[derive(Debug)]
pub enum AnalyzeError {
    NotFound,
    Failed(String),
}

/// Decodes the whole file at `path` and measures it.
pub fn analyze_file(path: &Path, split: Split) -> Result<Peaks, AnalyzeError> {
    let file = File::open(path).map_err(|error| match error.kind() {
        io::ErrorKind::NotFound => AnalyzeError::NotFound,
        _ => AnalyzeError::Failed(error.to_string()),
    })?;
    let source =
        Decoder::try_from(file).map_err(|error| AnalyzeError::Failed(error.to_string()))?;
    let channels = source.channels().get();
    let sample_rate = source.sample_rate().get();
    Ok(analyze(source, channels, sample_rate, split))
}

/// The peaks of interleaved `samples`.
pub fn analyze(
    samples: impl Iterator<Item = f32>,
    channels: u16,
    sample_rate: u32,
    split: Split,
) -> Peaks {
    let channels = usize::from(channels.max(1));
    let (low_mid_hz, mid_high_hz) = split.for_sample_rate(sample_rate);
    // Low is what gets through a low-pass at the first split, high what gets through
    // a high-pass at the second, and mid what gets through both a high-pass at the
    // first and a low-pass at the second.
    let mut low_pass = Biquad::low_pass(low_mid_hz, sample_rate);
    let mut mid_high_pass = Biquad::high_pass(low_mid_hz, sample_rate);
    let mut mid_low_pass = Biquad::low_pass(mid_high_hz, sample_rate);
    let mut high_pass = Biquad::high_pass(mid_high_hz, sample_rate);

    let mut peaks: Vec<[f32; 3]> = Vec::new();
    let mut frame_count: u64 = 0;
    let mut sum = 0.0;
    let mut in_frame = 0;

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

        let column =
            (frame_count * u64::from(COLUMNS_PER_SECOND) / u64::from(sample_rate)) as usize;
        if column >= peaks.len() {
            peaks.resize(column + 1, [0.0; 3]);
        }
        for (peak, band) in peaks[column].iter_mut().zip(bands) {
            *peak = peak.max(band.abs());
        }
        frame_count += 1;
    }

    Peaks {
        duration_seconds: frame_count as f64 / f64::from(sample_rate),
        columns: peaks,
    }
}

/// A second-order (12 dB per octave) Butterworth filter, in transposed direct form II.
pub(super) struct Biquad {
    b0: f32,
    b1: f32,
    b2: f32,
    a1: f32,
    a2: f32,
    z1: f32,
    z2: f32,
}

impl Biquad {
    pub(super) fn low_pass(cutoff_hz: f32, sample_rate: u32) -> Self {
        Self::new(cutoff_hz, sample_rate, |cos, alpha| {
            let b = (1.0 - cos) / 2.0;
            (b, 1.0 - cos, b, 1.0 + alpha)
        })
    }

    pub(super) fn high_pass(cutoff_hz: f32, sample_rate: u32) -> Self {
        Self::new(cutoff_hz, sample_rate, |cos, alpha| {
            let b = (1.0 + cos) / 2.0;
            (b, -(1.0 + cos), b, 1.0 + alpha)
        })
    }

    /// `numerator` gives `(b0, b1, b2, a0)` from the cosine of the cutoff and `alpha`
    /// (the RBJ audio EQ cookbook formulas, with the Butterworth Q).
    fn new(
        cutoff_hz: f32,
        sample_rate: u32,
        numerator: impl Fn(f32, f32) -> (f32, f32, f32, f32),
    ) -> Self {
        let omega = std::f32::consts::TAU * cutoff_hz / sample_rate as f32;
        let alpha = omega.sin() / (2.0 * std::f32::consts::FRAC_1_SQRT_2);
        let (b0, b1, b2, a0) = numerator(omega.cos(), alpha);
        Self {
            b0: b0 / a0,
            b1: b1 / a0,
            b2: b2 / a0,
            a1: -2.0 * omega.cos() / a0,
            a2: (1.0 - alpha) / a0,
            z1: 0.0,
            z2: 0.0,
        }
    }

    pub(super) fn process(&mut self, input: f32) -> f32 {
        let output = self.b0 * input + self.z1;
        self.z1 = self.b1 * input - self.a1 * output + self.z2;
        self.z2 = self.b2 * input - self.a2 * output;
        output
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RATE: u32 = 44_100;

    fn tone(hz: f32, seconds: f32) -> Vec<f32> {
        (0..(RATE as f32 * seconds) as usize)
            .map(|n| (std::f32::consts::TAU * hz * n as f32 / RATE as f32).sin() * 0.8)
            .collect()
    }

    fn peaks(samples: Vec<f32>, split: Split) -> Peaks {
        analyze(samples.into_iter(), 1, RATE, split)
    }

    /// The average of a band's drawn height over a range of columns, with unscaled gains.
    fn average(peaks: &Peaks, band: usize, columns: std::ops::Range<usize>) -> f32 {
        let scaled = peaks.scaled(Gains::UNITY);
        let values: Vec<f32> = scaled[columns].iter().map(|c| f32::from(c[band])).collect();
        values.iter().sum::<f32>() / values.len() as f32
    }

    #[test]
    fn has_one_column_per_ten_milliseconds() {
        let peaks = peaks(tone(440.0, 2.0), Split::default());
        assert_eq!(peaks.columns.len(), 200);
        assert!((peaks.duration_seconds - 2.0).abs() < 1e-3);
    }

    #[test]
    fn tells_bass_from_treble() {
        // One second of bass, then one second of treble.
        let samples: Vec<f32> = tone(80.0, 1.0)
            .into_iter()
            .chain(tone(6_000.0, 1.0))
            .collect();
        let peaks = peaks(samples, Split::default());

        // Skip the first columns of each half, where the filters are still settling.
        let (bass, treble) = (20..90, 120..190);
        assert!(
            average(&peaks, 0, bass.clone()) > 150.0,
            "bass is tall in the low band"
        );
        assert!(average(&peaks, 2, bass) < 40.0, "and not in the high band");
        assert!(
            average(&peaks, 2, treble.clone()) > 150.0,
            "treble is tall in the high band"
        );
        assert!(average(&peaks, 0, treble) < 40.0, "and not in the low band");
    }

    #[test]
    fn puts_a_midrange_tone_in_the_middle_band() {
        let peaks = peaks(tone(1_500.0, 2.0), Split::default());
        let mid = average(&peaks, 1, 20..180);
        assert!(mid > 150.0);
        assert!(average(&peaks, 0, 20..180) < mid / 2.0 && average(&peaks, 2, 20..180) < mid / 2.0);
    }

    #[test]
    fn the_split_frequencies_are_where_the_bands_meet() {
        // 600 Hz and 4 kHz by default.
        assert_eq!(Split::default().low_mid_hz, 600);
        assert_eq!(Split::default().mid_high_hz, 4_000);

        // A 300 Hz tone is low by default, but mid when the low band ends at 100 Hz.
        let by_default = peaks(tone(300.0, 2.0), Split::default());
        let moved = peaks(
            tone(300.0, 2.0),
            Split {
                low_mid_hz: 100,
                mid_high_hz: 4_000,
            },
        );
        assert!(average(&by_default, 0, 20..180) > average(&by_default, 1, 20..180));
        assert!(average(&moved, 1, 20..180) > average(&moved, 0, 20..180));

        // A 2.5 kHz tone is mid by default, but high when the high band starts at 1 kHz.
        let by_default = peaks(tone(2_500.0, 2.0), Split::default());
        let moved = peaks(
            tone(2_500.0, 2.0),
            Split {
                low_mid_hz: 200,
                mid_high_hz: 1_000,
            },
        );
        assert!(average(&by_default, 1, 20..180) > average(&by_default, 2, 20..180));
        assert!(average(&moved, 2, 20..180) > average(&moved, 1, 20..180));
    }

    #[test]
    fn the_scale_is_fixed_and_not_fitted_to_the_track() {
        // The same tone, loud and at a fifth of the level: the quiet one is drawn lower
        // (nothing makes it as tall as the loud one).
        let loud = peaks(tone(80.0, 2.0), Split::default());
        let quiet = peaks(
            tone(80.0, 2.0).into_iter().map(|s| s * 0.2).collect(),
            Split::default(),
        );
        let (loud_height, quiet_height) = (average(&loud, 0, 20..180), average(&quiet, 0, 20..180));
        assert!(
            quiet_height < loud_height * 0.6,
            "{quiet_height} against {loud_height}"
        );

        // And a band is exactly as tall as its peak says: 0.5 gives 0.5^0.8 of full height.
        let half = Peaks {
            duration_seconds: 0.01,
            columns: vec![[0.5, 0.5, 0.5]],
        };
        let expected = (0.5f32.powf(GAMMA) * 255.0).round() as u8;
        assert_eq!(half.scaled(Gains::UNITY), [[expected; 3]]);
    }

    #[test]
    fn the_gains_amplify_each_band_by_itself_and_stop_at_full_height() {
        let columns = Peaks {
            duration_seconds: 0.01,
            columns: vec![[0.1, 0.1, 0.1]],
        };
        let plain = columns.scaled(Gains::UNITY)[0];
        let boosted = columns.scaled(Gains {
            low: 1.0,
            mid: 3.0,
            high: 10.0,
        })[0];
        assert_eq!(boosted[0], plain[0], "an untouched band stays");
        assert!(boosted[1] > plain[1] && boosted[2] > boosted[1]);
        assert_eq!(boosted[2], 255, "10 x 0.1 is full height, and no more");

        let loud = Peaks {
            duration_seconds: 0.01,
            columns: vec![[0.9, 0.9, 0.9]],
        };
        assert_eq!(
            loud.scaled(Gains {
                low: 5.0,
                mid: 5.0,
                high: 5.0
            })[0],
            [255; 3]
        );
        assert_eq!(
            loud.scaled(Gains {
                low: 0.0001,
                mid: 0.0001,
                high: 0.0001
            })[0],
            [0; 3]
        );
    }

    #[test]
    fn checks_the_gains() {
        let gains = |low, mid, high| Gains { low, mid, high };
        assert!(Gains::default().validate().is_ok());
        assert!(gains(0.5, 2.0, MAX_GAIN).validate().is_ok());
        assert!(gains(0.0, 1.0, 1.0).validate().is_err(), "silences a band");
        assert!(gains(-1.0, 1.0, 1.0).validate().is_err());
        assert!(gains(1.0, MAX_GAIN + 1.0, 1.0).validate().is_err());
        assert!(gains(1.0, f32::NAN, 1.0).validate().is_err());
        assert!(gains(1.0, 1.0, f32::INFINITY).validate().is_err());
    }

    #[test]
    fn checks_the_split_frequencies() {
        let split = |low_mid_hz, mid_high_hz| Split {
            low_mid_hz,
            mid_high_hz,
        };
        assert!(split(600, 4_000).validate().is_ok());
        assert!(split(MIN_SPLIT_HZ, MAX_SPLIT_HZ).validate().is_ok());
        assert!(split(4_000, 600).validate().is_err(), "in the wrong order");
        assert!(split(600, 600).validate().is_err(), "no mid band");
        assert!(split(5, 4_000).validate().is_err(), "too low");
        assert!(split(600, 30_000).validate().is_err(), "too high");
        assert!(split(0, 0).validate().is_err());
    }

    #[test]
    fn a_low_sample_rate_pulls_the_split_points_down() {
        // 4 kHz is too high for a 8 kHz file: the filters need room below half the rate.
        let (low, high) = Split::default().for_sample_rate(8_000);
        assert!(high <= 8_000.0 * 0.5);
        assert!(low < high);
        assert_eq!(Split::default().for_sample_rate(44_100), (600.0, 4_000.0));
    }

    #[test]
    fn mixes_channels_down_and_follows_the_sample_rate() {
        // Interleaved stereo: a second is still 100 columns.
        let stereo: Vec<f32> = tone(440.0, 1.0).iter().flat_map(|s| [*s, *s]).collect();
        let peaks = analyze(stereo.into_iter(), 2, RATE, Split::default());
        assert_eq!(peaks.columns.len(), 100);

        let slow = analyze(
            (0..22_050).map(|n| (n as f32 * 0.1).sin()),
            1,
            22_050,
            Split::default(),
        );
        assert_eq!(slow.columns.len(), 100);
        assert!((slow.duration_seconds - 1.0).abs() < 1e-3);
    }

    #[test]
    fn silence_and_empty_input_are_flat_and_empty() {
        let silent = analyze(std::iter::repeat_n(0.0, 44_100), 1, RATE, Split::default());
        assert!(
            silent
                .scaled(Gains::default())
                .iter()
                .all(|c| *c == [0, 0, 0])
        );

        let empty = analyze(std::iter::empty(), 2, RATE, Split::default());
        assert!(empty.columns.is_empty());
        assert_eq!(empty.duration_seconds, 0.0);
    }

    #[test]
    fn a_missing_file_is_not_found() {
        assert!(matches!(
            analyze_file(Path::new("/nonexistent/track.flac"), Split::default()),
            Err(AnalyzeError::NotFound)
        ));
    }

    /// Prints the raw levels of real files, to choose sensible default gains. The variable
    /// names a text file with one audio file per line:
    /// `OPEN_ENGINE_TEST_AUDIO=/list.txt cargo test -p open-engine-backend levels -- --ignored --nocapture`
    #[test]
    #[ignore = "needs real audio files (OPEN_ENGINE_TEST_AUDIO)"]
    fn levels_of_real_files() {
        let list = std::env::var("OPEN_ENGINE_TEST_AUDIO").expect("OPEN_ENGINE_TEST_AUDIO");
        let files = std::fs::read_to_string(list).unwrap();
        for file in files.lines() {
            let peaks = analyze_file(Path::new(file), Split::default()).unwrap();
            let mut line = String::new();
            for (band, name) in ["low", "mid", "high"].into_iter().enumerate() {
                let mut levels: Vec<f32> = peaks.columns.iter().map(|c| c[band]).collect();
                levels.sort_by(f32::total_cmp);
                let at = |share: f64| {
                    levels[((levels.len() as f64 * share) as usize).min(levels.len() - 1)]
                };
                line += &format!(
                    "{name}: median {:.2} p90 {:.2} p99 {:.2} max {:.2}   ",
                    at(0.5),
                    at(0.9),
                    at(0.99),
                    at(1.0)
                );
            }
            println!("{line}");
        }
    }
}
