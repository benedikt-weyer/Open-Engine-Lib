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

/// Where the bands are split, in Hz.
const LOW_MID_HZ: f32 = 200.0;
const MID_HIGH_HZ: f32 = 2_000.0;
/// Each band is scaled so that this share of its columns is at or below full height,
/// which keeps a single loud click from flattening the rest.
const FULL_SCALE_PERCENTILE: f64 = 0.99;
/// Below 1, so that quiet parts stay visible next to loud ones.
const GAMMA: f32 = 0.8;
/// A quiet band is boosted to show up next to a loud one (treble is always much softer
/// than bass), but at most this far: otherwise a track with no treble at all would
/// show its filter leakage at full height.
const MIN_BAND_SCALE: f32 = 0.3;

#[derive(Debug, Clone, PartialEq)]
pub struct Waveform {
    pub duration_seconds: f64,
    /// Low, mid and high loudness per column, 0 to 255.
    pub columns: Vec<[u8; 3]>,
}

#[derive(Debug)]
pub enum AnalyzeError {
    NotFound,
    Failed(String),
}

/// Decodes the whole file at `path` and computes its waveform.
pub fn analyze_file(path: &Path) -> Result<Waveform, AnalyzeError> {
    let file = File::open(path).map_err(|error| match error.kind() {
        io::ErrorKind::NotFound => AnalyzeError::NotFound,
        _ => AnalyzeError::Failed(error.to_string()),
    })?;
    let source =
        Decoder::try_from(file).map_err(|error| AnalyzeError::Failed(error.to_string()))?;
    let channels = source.channels().get();
    let sample_rate = source.sample_rate().get();
    Ok(analyze(source, channels, sample_rate))
}

/// The waveform of interleaved `samples`.
pub fn analyze(samples: impl Iterator<Item = f32>, channels: u16, sample_rate: u32) -> Waveform {
    let channels = usize::from(channels.max(1));
    // Low is what gets through a low-pass at the first split, high what gets through
    // a high-pass at the second, and mid what gets through both a high-pass at the
    // first and a low-pass at the second.
    let mut low_pass = Biquad::low_pass(LOW_MID_HZ, sample_rate);
    let mut mid_high_pass = Biquad::high_pass(LOW_MID_HZ, sample_rate);
    let mut mid_low_pass = Biquad::low_pass(MID_HIGH_HZ, sample_rate);
    let mut high_pass = Biquad::high_pass(MID_HIGH_HZ, sample_rate);

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

    Waveform {
        duration_seconds: frame_count as f64 / f64::from(sample_rate),
        columns: normalize(&peaks),
    }
}

/// Scales every band to its own loud parts (see `MIN_BAND_SCALE`) and to 0..=255.
fn normalize(peaks: &[[f32; 3]]) -> Vec<[u8; 3]> {
    let mut scale = [1.0f32; 3];
    for (band, scale) in scale.iter_mut().enumerate() {
        let mut values: Vec<f32> = peaks.iter().map(|peak| peak[band]).collect();
        values.sort_by(f32::total_cmp);
        let at = ((values.len() as f64 * FULL_SCALE_PERCENTILE) as usize)
            .min(values.len().saturating_sub(1));
        *scale = values.get(at).copied().unwrap_or(1.0);
    }
    let loudest = scale.iter().copied().fold(0.0, f32::max);
    // The floor also keeps silence (all scales near 0) from being blown up.
    let floor = (loudest * MIN_BAND_SCALE).max(1e-3);
    for scale in &mut scale {
        *scale = scale.max(floor);
    }

    peaks
        .iter()
        .map(|peak| {
            let mut column = [0u8; 3];
            for band in 0..3 {
                let level = (peak[band] / scale[band]).clamp(0.0, 1.0).powf(GAMMA);
                column[band] = (level * 255.0).round() as u8;
            }
            column
        })
        .collect()
}

/// A second-order (12 dB per octave) Butterworth filter, in transposed direct form II.
struct Biquad {
    b0: f32,
    b1: f32,
    b2: f32,
    a1: f32,
    a2: f32,
    z1: f32,
    z2: f32,
}

impl Biquad {
    fn low_pass(cutoff_hz: f32, sample_rate: u32) -> Self {
        Self::new(cutoff_hz, sample_rate, |cos, alpha| {
            let b = (1.0 - cos) / 2.0;
            (b, 1.0 - cos, b, 1.0 + alpha)
        })
    }

    fn high_pass(cutoff_hz: f32, sample_rate: u32) -> Self {
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

    fn process(&mut self, input: f32) -> f32 {
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

    /// The average of a band over a range of columns.
    fn average(waveform: &Waveform, band: usize, columns: std::ops::Range<usize>) -> f32 {
        let values: Vec<f32> = waveform.columns[columns]
            .iter()
            .map(|c| f32::from(c[band]))
            .collect();
        values.iter().sum::<f32>() / values.len() as f32
    }

    #[test]
    fn has_one_column_per_ten_milliseconds() {
        let waveform = analyze(tone(440.0, 2.0).into_iter(), 1, RATE);
        assert_eq!(waveform.columns.len(), 200);
        assert!((waveform.duration_seconds - 2.0).abs() < 1e-3);
    }

    #[test]
    fn tells_bass_from_treble() {
        // One second of bass, then one second of treble.
        let samples: Vec<f32> = tone(80.0, 1.0)
            .into_iter()
            .chain(tone(6_000.0, 1.0))
            .collect();
        let waveform = analyze(samples.into_iter(), 1, RATE);

        // Skip the first columns of each half, where the filters are still settling.
        let (bass, treble) = (20..90, 120..190);
        assert!(
            average(&waveform, 0, bass.clone()) > 200.0,
            "bass is loud in the low band"
        );
        assert!(
            average(&waveform, 2, bass.clone()) < 40.0,
            "and not in the high band"
        );
        assert!(
            average(&waveform, 2, treble.clone()) > 200.0,
            "treble is loud in the high band"
        );
        assert!(
            average(&waveform, 0, treble) < 40.0,
            "and not in the low band"
        );
    }

    #[test]
    fn puts_a_midrange_tone_in_the_middle_band() {
        let waveform = analyze(tone(800.0, 2.0).into_iter(), 1, RATE);
        let mid = average(&waveform, 1, 20..180);
        assert!(mid > 200.0);
        assert!(average(&waveform, 0, 20..180) < mid && average(&waveform, 2, 20..180) < mid);
    }

    #[test]
    fn mixes_channels_down_and_follows_the_sample_rate() {
        // Interleaved stereo at 22.05 kHz: a second is still 100 columns.
        let mono = tone(440.0, 1.0);
        let stereo: Vec<f32> = mono.iter().flat_map(|s| [*s, *s]).collect();
        let waveform = analyze(stereo.into_iter(), 2, RATE);
        assert_eq!(waveform.columns.len(), 100);

        let slow = analyze((0..22_050).map(|n| (n as f32 * 0.1).sin()), 1, 22_050);
        assert_eq!(slow.columns.len(), 100);
        assert!((slow.duration_seconds - 1.0).abs() < 1e-3);
    }

    #[test]
    fn silence_and_empty_input_are_flat_and_empty() {
        let silent = analyze(std::iter::repeat_n(0.0, 44_100), 1, RATE);
        assert!(silent.columns.iter().all(|c| *c == [0, 0, 0]));

        let empty = analyze(std::iter::empty(), 2, RATE);
        assert!(empty.columns.is_empty());
        assert_eq!(empty.duration_seconds, 0.0);
    }

    #[test]
    fn a_missing_file_is_not_found() {
        assert!(matches!(
            analyze_file(Path::new("/nonexistent/track.flac")),
            Err(AnalyzeError::NotFound)
        ));
    }
}
