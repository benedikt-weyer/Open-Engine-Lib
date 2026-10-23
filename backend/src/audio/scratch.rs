//! Scratching: dragging the playing track forwards and backwards by hand, like a record.
//!
//! A track that streams from its decoder can't be played backwards or at a changing speed,
//! so when a scratch begins a stretch of the track around the playhead is decoded into
//! memory (`Window`), and a source (`ScratchSource`) plays it from a position that follows
//! the hand. The sound is the window read at the speed the hand moves, so it is faster and
//! higher when the hand is quick, backwards when it moves back, and silent when it holds
//! still, which is what a record does. Only that stretch can be scratched.

use std::fs::File;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use rodio::{ChannelCount, Decoder, Sample, SampleRate, Source};

/// How much of the track around the place it is held can be scratched, each way.
const WINDOW_RADIUS: Duration = Duration::from_secs(15);
/// How long the sound takes to catch up with the hand (the time constant, in seconds).
/// Short enough to feel direct, long enough that a jump of the hand is a swoosh and not a
/// click.
const FOLLOW_SECONDS: f64 = 0.006;
/// The fastest the window is read, in frames of the track per frame of output. (Further
/// than that, a jump of the hand takes a few output frames to be caught up with.)
const MAX_SPEED: f64 = 8.0;
/// At this speed or above the sound is at full volume; below it is faded down to silence,
/// so that a hand that comes to rest doesn't leave a constant level (a hum) behind.
const FULL_VOLUME_SPEED: f64 = 0.03;

/// A stretch of a track, decoded.
pub struct Window {
    /// Interleaved.
    samples: Vec<Sample>,
    channels: ChannelCount,
    rate: SampleRate,
    /// Where in the track the first frame is.
    start: Duration,
}

impl Window {
    /// Decodes the stretch around `at` from the file.
    pub fn decode(path: &Path, at: Duration) -> Result<Self, String> {
        let file = File::open(path).map_err(|error| error.to_string())?;
        let mut decoder = Decoder::try_from(file).map_err(|error| error.to_string())?;
        let (channels, rate) = (decoder.channels(), decoder.sample_rate());

        let start = at.saturating_sub(WINDOW_RADIUS);
        if !start.is_zero() {
            decoder
                .try_seek(start)
                .map_err(|error| format!("can't seek to scratch: {error}"))?;
        }
        let wanted = (2.0 * WINDOW_RADIUS.as_secs_f64() * f64::from(rate.get())) as usize
            * usize::from(channels.get());
        let mut samples: Vec<Sample> = decoder.take(wanted).collect();
        // Whole frames only.
        samples.truncate(samples.len() / usize::from(channels.get()) * usize::from(channels.get()));
        if samples.is_empty() {
            return Err("there is nothing to scratch here".to_owned());
        }
        Ok(Self {
            samples,
            channels,
            rate,
            start,
        })
    }

    fn frames(&self) -> usize {
        self.samples.len() / usize::from(self.channels.get())
    }
}

/// Where the hand is, in frames of the window. Written by the thread that handles the
/// requests, read by the audio thread.
#[derive(Default)]
struct Hand(AtomicU64);

impl Hand {
    fn get(&self) -> f64 {
        f64::from_bits(self.0.load(Ordering::Relaxed))
    }

    fn set(&self, frames: f64) {
        self.0.store(frames.to_bits(), Ordering::Relaxed);
    }
}

/// The scratch as the engine sees it: where the hand is.
pub struct Scratch {
    hand: Arc<Hand>,
    position: f64,
    frames: usize,
    rate: f64,
    start: Duration,
}

impl Scratch {
    /// Begins at `at` in the track (which should be inside `window`). The source is to be
    /// played; it never ends.
    pub fn begin(window: Window, at: Duration) -> (Self, ScratchSource) {
        let rate = f64::from(window.rate.get());
        let frames = window.frames();
        let position =
            (at.saturating_sub(window.start).as_secs_f64() * rate).clamp(0.0, (frames - 1) as f64);
        let hand = Arc::new(Hand::default());
        hand.set(position);
        let scratch = Self {
            hand: Arc::clone(&hand),
            position,
            frames,
            rate,
            start: window.start,
        };
        let source = ScratchSource {
            frame: vec![0.0; usize::from(window.channels.get())],
            next: 0,
            follow_frames: (FOLLOW_SECONDS * rate).max(1.0),
            position,
            hand,
            window,
        };
        (scratch, source)
    }

    /// Moves the hand by `seconds` of the track (negative is backwards), not past the ends
    /// of the window.
    pub fn move_by(&mut self, seconds: f64) {
        if seconds.is_finite() {
            self.position =
                (self.position + seconds * self.rate).clamp(0.0, (self.frames - 1) as f64);
            self.hand.set(self.position);
        }
    }

    /// Where in the track the hand is.
    pub fn position(&self) -> Duration {
        self.start + Duration::from_secs_f64(self.position / self.rate)
    }
}

/// The sound of a scratch: the window, read from a position that runs after the hand.
pub struct ScratchSource {
    window: Window,
    hand: Arc<Hand>,
    /// Where the sound is, in frames of the window.
    position: f64,
    follow_frames: f64,
    /// The frame being played out, and which of its channels is next.
    frame: Vec<Sample>,
    next: usize,
}

impl ScratchSource {
    /// Moves on by one frame of output and works out what it sounds like.
    fn advance(&mut self) {
        let behind = self.hand.get() - self.position;
        let speed = (behind / self.follow_frames).clamp(-MAX_SPEED, MAX_SPEED);
        self.position += speed;
        if (self.hand.get() - self.position).abs() < 1e-3 {
            self.position = self.hand.get();
        }

        let channels = usize::from(self.window.channels.get());
        let frames = self.window.frames();
        if self.position < 0.0 || self.position > (frames - 1) as f64 {
            self.frame.fill(0.0);
            return;
        }
        let gain = (speed.abs() / FULL_VOLUME_SPEED).min(1.0) as Sample;
        let index = self.position.floor() as usize;
        let after = (index + 1).min(frames - 1);
        let fraction = (self.position - index as f64) as Sample;
        for (channel, out) in self.frame.iter_mut().enumerate() {
            let a = self.window.samples[index * channels + channel];
            let b = self.window.samples[after * channels + channel];
            *out = (a + (b - a) * fraction) * gain;
        }
    }
}

impl Iterator for ScratchSource {
    type Item = Sample;

    fn next(&mut self) -> Option<Sample> {
        if self.next == 0 {
            self.advance();
        }
        let sample = self.frame[self.next];
        self.next = (self.next + 1) % self.frame.len();
        Some(sample)
    }
}

impl Source for ScratchSource {
    fn current_span_len(&self) -> Option<usize> {
        None
    }

    fn channels(&self) -> ChannelCount {
        self.window.channels
    }

    fn sample_rate(&self) -> SampleRate {
        self.window.rate
    }

    fn total_duration(&self) -> Option<Duration> {
        None
    }
}

#[cfg(test)]
mod tests {
    use std::num::NonZero;

    use super::*;

    const RATE: u32 = 8_000;

    /// A mono window whose sample at frame `n` is `n / 1000`: reading it back shows where
    /// and how fast it was read.
    fn ramp(frames: usize) -> Window {
        Window {
            samples: (0..frames).map(|n| n as f32 / 1000.0).collect(),
            channels: NonZero::new(1).unwrap(),
            rate: NonZero::new(RATE).unwrap(),
            start: Duration::from_secs(10),
        }
    }

    /// Plays `count` frames while the hand moves `per_frame` frames each, and gives the
    /// last `keep` samples.
    fn play(source: &mut ScratchSource, hand: &Hand, per_frame: f64, count: usize) -> Vec<f32> {
        (0..count)
            .map(|_| {
                hand.set(hand.get() + per_frame);
                source.next().unwrap()
            })
            .collect()
    }

    fn source_at(frames: usize, seconds: f64) -> (Scratch, ScratchSource) {
        Scratch::begin(
            ramp(frames),
            Duration::from_secs(10) + Duration::from_secs_f64(seconds),
        )
    }

    #[test]
    fn a_hand_that_holds_still_is_silent() {
        let (_, mut source) = source_at(40_000, 1.0);
        assert!(
            (0..2_000)
                .map(|_| source.next().unwrap())
                .all(|sample| sample == 0.0)
        );
    }

    #[test]
    fn the_window_is_read_at_the_speed_of_the_hand() {
        let (scratch, mut source) = source_at(40_000, 1.0);
        let hand = Arc::clone(&scratch.hand);

        // At the pace of the track the ramp rises one frame per frame...
        let normal = play(&mut source, &hand, 1.0, 3_000);
        let rise =
            |samples: &[f32]| (samples[samples.len() - 1] - samples[samples.len() - 101]) / 100.0;
        assert!((rise(&normal) - 0.001).abs() < 0.0001, "{}", rise(&normal));

        // ...twice as fast, twice as steeply...
        let fast = play(&mut source, &hand, 2.0, 3_000);
        assert!((rise(&fast) - 0.002).abs() < 0.0002, "{}", rise(&fast));

        // ...and backwards it falls.
        let back = play(&mut source, &hand, -1.0, 6_000);
        assert!((rise(&back) + 0.001).abs() < 0.0001, "{}", rise(&back));
    }

    #[test]
    fn the_sound_comes_to_rest_where_the_hand_does() {
        let (scratch, mut source) = source_at(40_000, 1.0);
        let hand = Arc::clone(&scratch.hand);
        play(&mut source, &hand, 1.0, 1_000);
        // The hand stops; the sound runs on a little and settles at the hand, quietly.
        let settled = play(&mut source, &hand, 0.0, 4_000);
        assert_eq!(settled[settled.len() - 1], 0.0);
        assert!((source.position - hand.get()).abs() < 0.01);
    }

    #[test]
    fn outside_the_window_it_is_silent_and_the_hand_stays_inside() {
        let (mut scratch, mut source) = source_at(8_000, 0.5);
        scratch.move_by(100.0);
        assert_eq!(
            scratch.position(),
            Duration::from_secs(10) + Duration::from_secs_f64(7_999.0 / 8_000.0)
        );
        scratch.move_by(-100.0);
        assert_eq!(scratch.position(), Duration::from_secs(10));
        scratch.move_by(f64::NAN);
        assert_eq!(scratch.position(), Duration::from_secs(10));
        // (The sound is still inside the window while it follows.)
        assert!((0..100).all(|_| source.next().unwrap().is_finite()));
    }

    #[test]
    fn the_hand_starts_where_the_track_was_held() {
        let (scratch, _) = source_at(40_000, 2.5);
        assert_eq!(scratch.position(), Duration::from_secs_f64(12.5));
        // A track held before the start of the window (the window is from the start of the
        // track) starts at its beginning.
        let (early, _) = Scratch::begin(ramp(1_000), Duration::from_secs(3));
        assert_eq!(early.position(), Duration::from_secs(10));
    }

    #[test]
    fn stereo_frames_stay_together() {
        let window = Window {
            samples: (0..4_000).flat_map(|n| [n as f32, -(n as f32)]).collect(),
            channels: NonZero::new(2).unwrap(),
            rate: NonZero::new(RATE).unwrap(),
            start: Duration::ZERO,
        };
        let (scratch, mut source) = Scratch::begin(window, Duration::from_secs_f64(0.1));
        let hand = Arc::clone(&scratch.hand);
        for _ in 0..500 {
            hand.set(hand.get() + 1.0);
            let (left, right) = (source.next().unwrap(), source.next().unwrap());
            assert_eq!(left, -right);
        }
    }

    /// A WAV file of 16-bit mono samples, which `rodio`'s decoder reads.
    fn wav(samples: &[i16]) -> Vec<u8> {
        let data = samples.len() as u32 * 2;
        let mut bytes = b"RIFF".to_vec();
        bytes.extend((36 + data).to_le_bytes());
        bytes.extend(b"WAVEfmt ");
        bytes.extend(16u32.to_le_bytes());
        bytes.extend(1u16.to_le_bytes()); // PCM
        bytes.extend(1u16.to_le_bytes()); // mono
        bytes.extend(RATE.to_le_bytes());
        bytes.extend((RATE * 2).to_le_bytes());
        bytes.extend(2u16.to_le_bytes());
        bytes.extend(16u16.to_le_bytes());
        bytes.extend(b"data");
        bytes.extend(data.to_le_bytes());
        bytes.extend(samples.iter().flat_map(|sample| sample.to_le_bytes()));
        bytes
    }

    #[test]
    fn a_window_is_decoded_from_around_the_position() {
        let path =
            std::env::temp_dir().join(format!("open-engine-scratch-{}.wav", std::process::id()));
        // A minute of ramp that wraps every 20000 frames: a frame's value says which it is.
        let seconds = 60;
        let samples: Vec<i16> = (0..RATE as usize * seconds)
            .map(|n| (n % 20_000) as i16)
            .collect();
        std::fs::write(&path, wav(&samples)).unwrap();

        let result = Window::decode(&path, Duration::from_secs(30));
        let early = Window::decode(&path, Duration::from_secs(5));
        std::fs::remove_file(&path).unwrap();

        let window = result.unwrap();
        assert_eq!(window.start, Duration::from_secs(15));
        // 30 seconds of it.
        assert_eq!(window.frames(), 30 * RATE as usize);
        // The first frame is frame 15 s * 8000 of the track.
        let expected = (15 * RATE as usize % 20_000) as f32 / f32::from(i16::MAX);
        assert!(
            (window.samples[0] - expected).abs() < 1e-3,
            "{}",
            window.samples[0]
        );

        // Near the start of the track the window starts with the track.
        assert_eq!(early.unwrap().start, Duration::ZERO);
    }

    #[test]
    fn a_missing_file_gives_an_error() {
        assert!(Window::decode(Path::new("/nonexistent/track.flac"), Duration::ZERO).is_err());
    }
}
