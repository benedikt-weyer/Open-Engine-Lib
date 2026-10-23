//! The track list, ratings, the player and what is fetched for what is shown.

use crate::*;

pub(crate) fn handle(state: &mut State, message: Message) -> Task<Message> {
    match message {
        Message::ShowCollection | Message::ShowPlaylist(_) => {
            if let LibraryState::Open(browser) = &mut state.library {
                browser.show(match message {
                    Message::ShowPlaylist(index) => Source::Playlist(index),
                    _ => Source::Collection,
                });
                return browser::scroll_to_top();
            }
        }
        Message::RevealRow(row) => {
            if let LibraryState::Open(browser) = &mut state.library {
                return browser.reveal(row);
            }
        }
        Message::ToggleFolder(index) => {
            if let LibraryState::Open(browser) = &mut state.library {
                browser.toggle(index);
            }
        }
        Message::SelectTrack(row) => {
            if let LibraryState::Open(browser) = &mut state.library {
                browser.selected = Some(row);
            }
        }
        // The databases are being cloned: a rating set now might miss the clone.
        Message::SetStars { .. } if state.locked() => {}
        Message::SetStars { track, stars } => {
            let Some(backend) = state.backend() else {
                return Task::none();
            };
            if let LibraryState::Open(browser) = &mut state.library {
                // Shown at once; taken back if the backend can't do it.
                let previous = browser.rating(track);
                let rating = u32::from(stars.min(5)) * 20;
                browser.set_rating(track, Some(rating));
                return Task::perform(backend::set_rating(backend, track, rating), move |result| {
                    Message::RatingSet {
                        track,
                        previous,
                        result,
                    }
                });
            }
        }
        Message::RatingSet {
            track,
            previous,
            result,
        } => match result {
            Ok(()) => state.backend_error = None,
            Err(error) => {
                if let LibraryState::Open(browser) = &mut state.library {
                    browser.set_rating(track, previous);
                }
                state.backend_error = Some(error);
            }
        },
        Message::PlayTrack(row) => {
            if let LibraryState::Open(browser) = &mut state.library {
                browser.selected = Some(row);
            }
            return state.play_row(row);
        }
        Message::TrackListScrolled(viewport) => {
            if let LibraryState::Open(browser) = &mut state.library {
                browser.scrolled(viewport);
            }
        }

        Message::PlayPause => {
            if state.is_stopped() {
                let LibraryState::Open(browser) = &state.library else {
                    return Task::none();
                };
                let row = browser.selected.unwrap_or(0);
                return state.play_row(row);
            }
            return state.control(Control::TogglePause);
        }
        Message::Stop => return state.control(Control::Stop),
        Message::Next => return state.control(Control::Next),
        Message::Previous => return state.control(Control::Previous),
        Message::Tick => return state.control(Control::Poll),
        Message::SeekTo(seconds) => {
            return state.control(Control::Seek {
                position_ms: (seconds.max(0.0) * 1000.0) as u64,
            });
        }
        Message::Status(Ok(status)) => {
            state.status = status;
            state.clock.report(
                state.status.position_ms as f64 / 1000.0,
                state.status.state() == PlaybackState::Playing,
                Instant::now(),
            );
            state.backend_error = None;
            if state.is_stopped() {
                state.play_source = None;
            }
        }
        Message::Status(Err(error)) => state.backend_error = Some(error),
        Message::WaveformLoaded(generation, id, result) => match result {
            Ok(waveform) => state.waveforms.set(generation, id, waveform),
            Err(error) => {
                state.waveforms.set(generation, id, None);
                state.backend_error = Some(error);
            }
        },
        Message::ProgressTick => {
            if state.reporting_progress()
                && let Some(backend) = state.backend()
            {
                return Task::perform(backend::progress(backend), Message::ProgressUpdated);
            }
        }
        Message::ProgressUpdated(result) => {
            // (An answer that comes after the sync or the opening ended is not wanted.)
            if state.reporting_progress() {
                state.progress = result.ok();
            }
        }
        Message::PathsChecked(generation, answers) => state.reachable.set(generation, answers),
        Message::CoverLoaded(generation, id, result) => match result {
            Ok(image) => state.covers.set(generation, id, image),
            Err(error) => {
                state.covers.set(generation, id, None);
                state.backend_error = Some(error);
            }
        },
        _ => {}
    }
    Task::none()
}
