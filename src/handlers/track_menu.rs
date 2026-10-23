//! The context menu of a track of the list: taking the track out of the playlist that is
//! shown, or out of the library. (Taking out the streaming songs, which works the same, is
//! with the playlists'.)

use crate::*;

pub(crate) fn handle(state: &mut State, message: Message) -> Task<Message> {
    match message {
        Message::TrackMenuOpen { row, at } => {
            if state.locked() {
                return Task::none();
            }
            let LibraryState::Open(browser) = &mut state.library else {
                return Task::none();
            };
            let Some(&track) = browser.tracks().get(row) else {
                return Task::none();
            };
            if !browser.library.is_engine() {
                return Task::none();
            }
            // The row clicked is the one the menu is about.
            browser.selected = Some(row);
            let track = &browser.library.tracks[track];
            // An ordinary playlist can lose the track; "All tracks" and a smart playlist (its
            // tracks come from its rules) can't.
            let playlist = match browser.source {
                Source::Playlist(index)
                    if browser
                        .library
                        .playlists
                        .get(index)
                        .is_some_and(|playlist| !playlist.is_smart) =>
                {
                    Some(index)
                }
                _ => None,
            };
            state.track_menu = Some(track_menu::Menu {
                row,
                track: track.id,
                title: if track.title.is_empty() {
                    track.path.clone()
                } else {
                    track.title.clone()
                },
                playlist,
                at,
                confirm_library: false,
            });
        }
        Message::TrackMenuClose => state.track_menu = None,
        Message::TrackMenuRemoveFromPlaylist => {
            let (Some(menu), LibraryState::Open(browser)) = (&state.track_menu, &state.library)
            else {
                return Task::none();
            };
            let (Some(playlist), Some(backend)) = (menu.playlist, state.backend()) else {
                return Task::none();
            };
            let task = Task::perform(
                backend::remove_track_from_playlist(
                    backend,
                    Arc::clone(&browser.library),
                    playlist,
                    menu.track,
                ),
                Message::TrackRemovedFromPlaylist,
            );
            state.track_menu = None;
            return task;
        }
        Message::TrackRemovedFromPlaylist(result) => match result {
            Ok(edited) => {
                state.backend_error = None;
                state.playlists_changed(edited.playlists, Change::Same, None);
                // The row that was selected is another track now.
                if let LibraryState::Open(browser) = &mut state.library {
                    browser.selected = None;
                }
            }
            Err(error) => state.backend_error = Some(error),
        },
        Message::TrackMenuRemoveFromLibrary => {
            let Some(menu) = &mut state.track_menu else {
                return Task::none();
            };
            // Deleting a track takes a second click; the first one only asks.
            if !menu.confirm_library {
                menu.confirm_library = true;
                return Task::none();
            }
            let track = menu.track;
            let Some(backend) = state.backend() else {
                return Task::none();
            };
            state.track_menu = None;
            // Nothing else may change the library until it has been read again.
            state.reloading = true;
            return Task::perform(
                backend::remove_tracks(backend, vec![track]),
                Message::TracksRemoved,
            );
        }
        Message::TracksRemoved(result) => {
            return removal_finished(
                state,
                result.map(|done| (done.removed, done.playlist_entries)),
                |strings| &strings.track_deleted,
            );
        }
        _ => {}
    }
    Task::none()
}

/// What follows a removal of tracks from the library, which the backend did in the working
/// copy and then read again: tell what went (`message` is the text for the count of tracks
/// and of playlist entries) and fetch the library again, as the ids of the tracks after the
/// first one removed changed and the backend stopped the player. A removal that found nothing
/// to remove only unlocks the library, and a failure is shown.
pub(crate) fn removal_finished(
    state: &mut State,
    result: Result<(u32, u32), String>,
    message: fn(&i18n::Strings) -> &String,
) -> Task<Message> {
    match result {
        Ok((removed, entries)) if removed > 0 => {
            state.backend_error = None;
            let strings = state.config.effective().language.strings();
            state.reload_result = Some((
                fill(
                    message(strings),
                    &[("tracks", &removed), ("entries", &entries)],
                ),
                false,
            ));
            Task::batch([state.reload_library(), state.control(Control::Poll)])
        }
        Ok(_) => {
            state.reloading = false;
            Task::none()
        }
        Err(error) => {
            state.reloading = false;
            state.backend_error = Some(error);
            Task::none()
        }
    }
}
