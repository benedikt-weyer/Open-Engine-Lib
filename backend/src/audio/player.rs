//! Audio output: plays one file at a time through the system's default device.

use std::fmt;
use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use rodio::{Decoder, DeviceSinkBuilder, MixerDeviceSink};

use crate::scratch::{Scratch, Window};

#[derive(Debug)]
pub enum PlayError {
    /// There is no file at the path the library records.
    NotFound(PathBuf),
    /// The file or the audio device could not be opened.
    Failed(String),
}

impl fmt::Display for PlayError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PlayError::NotFound(path) => write!(f, "{} does not exist", path.display()),
            PlayError::Failed(error) => f.write_str(error),
        }
    }
}

/// How a track starts.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Start {
    /// Start paused: nothing is heard until the track is resumed.
    pub paused: bool,
    /// Where in the track to start.
    pub at: Duration,
}

/// What the playback engine needs from an audio output. Behind a trait so the
/// queue logic can be tested without a sound card.
pub trait Output {
    /// Stops whatever is playing and starts the file at `path`.
    fn play(&mut self, path: &Path, start: Start) -> Result<(), PlayError>;
    fn is_paused(&self) -> bool;
    fn toggle_pause(&mut self);
    /// True once the current file has played to its end.
    fn finished(&self) -> bool;
    fn position(&self) -> Duration;
    /// Jumps within the current file. Returns false if the format can't seek.
    fn seek(&mut self, to: Duration) -> bool;
    fn stop(&mut self);
    /// Stops and releases the audio device.
    fn close(&mut self);
    /// Holds the track where it is and starts scratching it (see `scratch`): it is silent
    /// and paused until `scratch_end`, but for what `scratch_move` makes heard. Already
    /// scratching is fine. Says why if it can't (nothing is playing, or the file can't be
    /// read again).
    fn scratch_begin(&mut self) -> Result<(), String>;
    /// Moves the hand by `seconds` of the track, negative for backwards. Does nothing
    /// unless scratching.
    fn scratch_move(&mut self, seconds: f64);
    /// Lets go: the track goes on from where the hand is, and plays again if it was
    /// playing when the scratch began.
    fn scratch_end(&mut self);
    fn is_scratching(&self) -> bool;
}

/// A scratch that is going on: its sound, where the hand is, and what the track was doing
/// before.
struct Scratching {
    /// Plays the scratch's sound. Dropping it ends the sound.
    _sound: rodio::Player,
    scratch: Scratch,
    /// The track was playing when it was held, so it plays on when let go.
    resume: bool,
}

/// The real output. The device is opened on the first file and kept until `close`,
/// so switching files does not reopen it.
#[derive(Default)]
pub struct RodioOutput {
    device: Option<MixerDeviceSink>,
    current: Option<rodio::Player>,
    /// The file `current` plays, to decode from again for scratching.
    path: Option<PathBuf>,
    /// While scratching, `current` is held (paused) where the scratch began.
    scratching: Option<Scratching>,
}

impl RodioOutput {
    /// Ends a scratch without moving the track to where the hand was; it plays on from
    /// where it was held if it was playing.
    fn cancel_scratch(&mut self) {
        if let Some(scratching) = self.scratching.take()
            && scratching.resume
            && let Some(player) = &self.current
        {
            player.play();
        }
    }
}

impl Output for RodioOutput {
    fn play(&mut self, path: &Path, start: Start) -> Result<(), PlayError> {
        self.stop();

        let file = File::open(path).map_err(|error| match error.kind() {
            io::ErrorKind::NotFound => PlayError::NotFound(path.to_path_buf()),
            _ => PlayError::Failed(error.to_string()),
        })?;
        let source =
            Decoder::try_from(file).map_err(|error| PlayError::Failed(error.to_string()))?;

        if self.device.is_none() {
            let mut device = DeviceSinkBuilder::open_default_sink()
                .map_err(|error| PlayError::Failed(error.to_string()))?;
            device.log_on_drop(false);
            self.device = Some(device);
        }
        let device = self.device.as_ref().expect("device was just opened");

        let player = rodio::Player::connect_new(device.mixer());
        // Pause before the track is added, so that nothing is heard of it.
        if start.paused {
            player.pause();
        }
        player.append(source);
        if !start.at.is_zero() {
            // A format that can't seek just starts from the beginning.
            let _ = player.try_seek(start.at);
        }
        self.current = Some(player);
        self.path = Some(path.to_path_buf());
        Ok(())
    }

    fn is_paused(&self) -> bool {
        self.scratching.is_some() || self.current.as_ref().is_some_and(rodio::Player::is_paused)
    }

    fn toggle_pause(&mut self) {
        // (The hand holds the track.)
        if self.scratching.is_some() {
            return;
        }
        if let Some(player) = &self.current {
            if player.is_paused() {
                player.play();
            } else {
                player.pause();
            }
        }
    }

    fn finished(&self) -> bool {
        self.current.as_ref().is_some_and(rodio::Player::empty)
    }

    fn position(&self) -> Duration {
        if let Some(scratching) = &self.scratching {
            return scratching.scratch.position();
        }
        self.current
            .as_ref()
            .map_or(Duration::ZERO, rodio::Player::get_pos)
    }

    fn seek(&mut self, to: Duration) -> bool {
        // A jump while the hand is on the track (a cue pad pressed) wins over the scratch.
        self.cancel_scratch();
        self.current
            .as_ref()
            .is_some_and(|player| player.try_seek(to).is_ok())
    }

    fn stop(&mut self) {
        // Dropping the rodio player stops it.
        self.scratching = None;
        self.current = None;
        self.path = None;
    }

    fn close(&mut self) {
        self.stop();
        self.device = None;
    }

    fn scratch_begin(&mut self) -> Result<(), String> {
        self.begin_scratching()
    }

    fn scratch_move(&mut self, seconds: f64) {
        if let Some(scratching) = &mut self.scratching {
            scratching.scratch.move_by(seconds);
        }
    }

    fn scratch_end(&mut self) {
        let Some(scratching) = self.scratching.take() else {
            return;
        };
        if let Some(player) = &self.current {
            // A format that can't seek stays where it was held.
            let _ = player.try_seek(scratching.scratch.position());
            if scratching.resume {
                player.play();
            }
        }
    }

    fn is_scratching(&self) -> bool {
        self.scratching.is_some()
    }
}

impl RodioOutput {
    fn begin_scratching(&mut self) -> Result<(), String> {
        if self.scratching.is_some() {
            return Ok(());
        }
        let (Some(player), Some(path), Some(device)) = (&self.current, &self.path, &self.device)
        else {
            return Err("nothing is playing".to_owned());
        };
        let resume = !player.is_paused();
        player.pause();
        let at = player.get_pos();
        let window = match Window::decode(path, at) {
            Ok(window) => window,
            Err(error) => {
                if resume {
                    player.play();
                }
                return Err(error);
            }
        };
        let (scratch, source) = Scratch::begin(window, at);
        let sound = rodio::Player::connect_new(device.mixer());
        sound.append(source);
        self.scratching = Some(Scratching {
            _sound: sound,
            scratch,
            resume,
        });
        Ok(())
    }
}
