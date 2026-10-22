//! Audio output: plays one file at a time through the system's default device.

use std::fmt;
use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use rodio::{Decoder, DeviceSinkBuilder, MixerDeviceSink};

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
}

/// The real output. The device is opened on the first file and kept until `close`,
/// so switching files does not reopen it.
#[derive(Default)]
pub struct RodioOutput {
    device: Option<MixerDeviceSink>,
    current: Option<rodio::Player>,
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
        Ok(())
    }

    fn is_paused(&self) -> bool {
        self.current.as_ref().is_some_and(rodio::Player::is_paused)
    }

    fn toggle_pause(&mut self) {
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
        self.current
            .as_ref()
            .map_or(Duration::ZERO, rodio::Player::get_pos)
    }

    fn seek(&mut self, to: Duration) -> bool {
        self.current
            .as_ref()
            .is_some_and(|player| player.try_seek(to).is_ok())
    }

    fn stop(&mut self) {
        // Dropping the rodio player stops it.
        self.current = None;
    }

    fn close(&mut self) {
        self.stop();
        self.device = None;
    }
}
