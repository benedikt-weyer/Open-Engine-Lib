//! Moving a playlist of an Engine library to another place in the sidebar's tree by dragging
//! it.

use playlist_drag::Drag;

use crate::*;

pub(crate) fn handle(state: &mut State, message: Message) -> Task<Message> {
    match message {
        Message::PlaylistDragPress(index) => {
            // Only the playlists of an Engine library, and not while the working copy is
            // being changed or read, or a window is over the app.
            let LibraryState::Open(browser) = &state.library else {
                return Task::none();
            };
            if !browser.library.is_engine()
                || state.locked()
                || state.window_open()
                || state.syncing()
                || state.moving_playlist
                || browser
                    .library
                    .playlists
                    .get(index)
                    .is_none_or(|playlist| playlist.is_smart)
            {
                return Task::none();
            }
            state.playlist_drag = Some(Drag::pressed(index));
        }
        Message::PlaylistDragMoved(at) => {
            if let Some(drag) = &mut state.playlist_drag {
                let was_active = drag.active;
                drag.moved(at);
                if drag.active && !was_active {
                    // (A menu that was open is in the way of the drop.)
                    state.track_menu = None;
                    state.folder_menu = None;
                }
            }
        }
        Message::PlaylistDragOver(row, zone) => {
            if let Some(drag) = state.playlist_drag.as_mut().filter(|drag| drag.active) {
                drag.hover(row, zone);
            }
        }
        Message::PlaylistDragTick => {
            let (Some(drag), LibraryState::Open(browser)) =
                (&mut state.playlist_drag, &mut state.library)
            else {
                return Task::none();
            };
            let now = Instant::now();
            let open = drag.to_open(
                &browser.library.playlists,
                |row| browser.is_expanded(row),
                now,
            );
            if let Some(row) = open {
                browser.toggle(row);
                // (The next one to open is one more wait.)
                drag.over_since = now;
            }
        }
        Message::PlaylistDragCancel => state.playlist_drag = None,
        Message::PlaylistDragEnd => {
            let Some(drag) = state.playlist_drag.take() else {
                return Task::none();
            };
            let (Some(over), true) = (drag.over, drag.active) else {
                return Task::none();
            };
            let (Some(backend), LibraryState::Open(browser)) = (state.backend(), &state.library)
            else {
                return Task::none();
            };
            let Some(place) = playlist_drag::place(
                &browser.library.playlists,
                |row| browser.is_expanded(row),
                drag.playlist,
                over,
            ) else {
                return Task::none();
            };
            state.moving_playlist = true;
            let from = browser.library.playlist_span(drag.playlist);
            return Task::perform(
                backend::move_playlist(
                    backend,
                    Arc::clone(&browser.library),
                    drag.playlist,
                    place.parent,
                    place.before,
                ),
                move |result| Message::PlaylistMoved {
                    from: from.clone(),
                    result,
                },
            );
        }
        Message::PlaylistMoved { from, result } => {
            state.moving_playlist = false;
            match result {
                Ok(edited) => {
                    state.backend_error = None;
                    let change = match edited.playlist {
                        Some(to) => Change::Moved { from, to },
                        None => Change::Same,
                    };
                    state.playlists_changed(edited.playlists, change, None);
                    // The playlist is shown where it went, even if that is in a folder that
                    // was closed.
                    if let (Some(to), LibraryState::Open(browser)) =
                        (edited.playlist, &mut state.library)
                        && to < browser.library.playlists.len()
                    {
                        let chain = browser.library.playlist_chain(to);
                        browser.expand(&chain);
                    }
                }
                Err(error) => state.backend_error = Some(error),
            }
        }
        _ => {}
    }
    Task::none()
}
