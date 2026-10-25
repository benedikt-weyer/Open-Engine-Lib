//! Moving a track of the shown playlist to another place in it by dragging its row.

use playlist_drag::Drag;

use crate::*;

pub(crate) fn handle(state: &mut State, message: Message) -> Task<Message> {
    match message {
        Message::TrackDragPress(row) => {
            // Only the tracks of an ordinary playlist of an Engine library in its own order,
            // and not while the working copy is being changed or read, or a window is over
            // the app.
            let LibraryState::Open(browser) = &state.library else {
                return Task::none();
            };
            if !browser.reorderable()
                || row >= browser.tracks().len()
                || state.locked()
                || state.window_open()
                || state.syncing()
                || state.moving_track
                || state.playlist_drag.is_some()
            {
                return Task::none();
            }
            state.track_drag = Some(Drag::pressed(row));
        }
        Message::TrackDragMoved(at) => {
            if let Some(drag) = &mut state.track_drag {
                let was_active = drag.active;
                drag.moved(at);
                if drag.active && !was_active {
                    // (A menu that was open is in the way of the drop.)
                    state.track_menu = None;
                    state.folder_menu = None;
                }
            }
        }
        Message::TrackDragOver(row, hover) => {
            if let Some(drag) = state.track_drag.as_mut().filter(|drag| drag.active) {
                drag.hover(row, hover);
            }
        }
        // The list moves while the pointer is at its edge.
        Message::TrackDragTick => {
            if let (Some(drag), LibraryState::Open(browser)) =
                (&state.track_drag, &mut state.library)
                && drag.active
                && drag.edge != 0
            {
                return browser.scroll_by(f32::from(drag.edge) * TRACK_SCROLL_STEP);
            }
        }
        Message::TrackDragCancel => state.track_drag = None,
        Message::TrackDragEnd => {
            let Some(drag) = state.track_drag.take() else {
                return Task::none();
            };
            let Some(at) = playlist_drag::insertion(&drag) else {
                return Task::none();
            };
            let (Some(backend), LibraryState::Open(browser)) = (state.backend(), &state.library)
            else {
                return Task::none();
            };
            let Source::Playlist(playlist) = browser.source else {
                return Task::none();
            };
            let list = browser.tracks();
            let id_of = |row: usize| list.get(row).map(|&track| browser.library.tracks[track].id);
            let Some(track) = id_of(drag.dragged) else {
                return Task::none();
            };
            let before = id_of(at);
            // Where it is once it is moved: the rows it passes move up by one.
            let row = if at > drag.dragged { at - 1 } else { at };
            state.moving_track = true;
            return Task::perform(
                backend::move_track_in_playlist(
                    backend,
                    Arc::clone(&browser.library),
                    playlist,
                    track,
                    before,
                ),
                move |result| Message::TrackMoved { row, result },
            );
        }
        Message::TrackMoved { row, result } => {
            state.moving_track = false;
            match result {
                Ok(edited) => {
                    state.backend_error = None;
                    state.playlists_changed(edited.playlists, Change::Same, None);
                    // The track that was moved stays the selected one.
                    if let LibraryState::Open(browser) = &mut state.library {
                        browser.selected = Some(row);
                    }
                }
                Err(error) => state.backend_error = Some(error),
            }
        }
        _ => {}
    }
    Task::none()
}
