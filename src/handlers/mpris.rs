//! The desktop's media interface (MPRIS): what it asks for, and telling it what plays.

use crate::*;
use iced::widget::image::Handle;

/// What the desktop was last told, apart from the position as it moves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Told {
    state: mpris::Playback,
    track: Option<u32>,
    /// The cover and the exact length (from the waveform) come in after the track starts.
    cover: bool,
    duration: bool,
}

pub(crate) fn handle(state: &mut State, message: Message) -> Task<Message> {
    let Message::Mpris(event) = message else {
        return Task::none();
    };
    let playing = state.status.state() == PlaybackState::Playing;
    let stopped = state.is_stopped();
    let forward = |state: &mut State, message| super::playback::handle(state, message);
    match event {
        mpris::Event::Ready(handle) => {
            state.mpris = Some(handle);
            state.mpris_told = None;
        }
        mpris::Event::Command(command) => match command {
            mpris::Command::Play if !playing => return forward(state, Message::PlayPause),
            mpris::Command::Pause if playing => return forward(state, Message::PlayPause),
            mpris::Command::PlayPause => return forward(state, Message::PlayPause),
            mpris::Command::Stop => return forward(state, Message::Stop),
            mpris::Command::Next => return forward(state, Message::Next),
            mpris::Command::Previous => return forward(state, Message::Previous),
            mpris::Command::SeekBy(offset_us) if !stopped => {
                let duration = state.playing_duration().unwrap_or(f64::INFINITY);
                let now = state.clock.position(Instant::now(), duration);
                let seconds = now + offset_us as f64 / 1_000_000.0;
                return forward(state, Message::SeekTo(seconds.clamp(0.0, duration) as f32));
            }
            mpris::Command::SeekTo { track, position_us } if state.playing_id() == Some(track) => {
                return forward(
                    state,
                    Message::SeekTo((position_us as f64 / 1_000_000.0) as f32),
                );
            }
            _ => {}
        },
    }
    Task::none()
}

impl State {
    /// Tells the desktop what plays if that changed since it was last told. `reported` is
    /// whether the backend just said where the player is, which is what moves the position.
    pub(crate) fn tell_desktop(&mut self, reported: bool) {
        // (While the track is held by hand it is paused and jumps about; the desktop hears of
        // where it ends up.)
        if self.scratch.is_some() || self.status.scratching {
            return;
        }
        let Some(handle) = &self.mpris else {
            return;
        };
        let playback = match self.status.state() {
            PlaybackState::Playing => mpris::Playback::Playing,
            PlaybackState::Paused => mpris::Playback::Paused,
            _ => mpris::Playback::Stopped,
        };
        let track = match (&self.library, self.playing_id()) {
            (LibraryState::Open(browser), Some(id)) => browser.library.track_by_id(id),
            _ => None,
        };
        let cover = track
            .and_then(|track| track.album_art)
            .and_then(|id| self.covers.get(id).map(|handle| (id, handle)));
        let duration = track.and(self.playing_duration());
        let told = Told {
            state: playback,
            track: track.map(|track| track.id),
            cover: cover.is_some(),
            duration: duration.is_some(),
        };
        let position_us = self.status.position_ms as i64 * 1000;
        if self.mpris_told == Some(told) {
            if reported {
                handle.set_position(position_us);
            }
            return;
        }
        handle.set(mpris::Info {
            state: playback,
            track: track.map(|track| mpris::TrackInfo {
                id: track.id,
                title: track.title.clone(),
                artist: track.artist.clone(),
                album: track.album.clone(),
                genre: track.genre.clone(),
                length_us: duration
                    .map(|seconds| (seconds * 1_000_000.0) as i64)
                    .or_else(|| track.length.map(|seconds| i64::from(seconds) * 1_000_000)),
                location: (!track.location.is_empty()).then(|| track.location.clone()),
                art: cover.and_then(|(id, handle)| match handle {
                    Handle::Bytes(_, bytes) => Some((id, bytes.to_vec())),
                    _ => None,
                }),
            }),
            position_us,
        });
        self.mpris_told = Some(told);
    }
}
