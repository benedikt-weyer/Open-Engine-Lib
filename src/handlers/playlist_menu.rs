//! The context menu of a playlist of the tree (right click): rename, delete, edit the rules of
//! a smart playlist, and analyse the tracks of the playlist one after the other.

use crate::*;

pub(crate) fn handle(state: &mut State, message: Message) -> Task<Message> {
    match message {
        Message::PlaylistMenuOpen { playlist, at } => {
            // (While a track is analysed the menu opens, with what changes the library greyed out.)
            if (state.locked() && !state.analysing()) || state.window_open() || state.syncing() {
                return Task::none();
            }
            let LibraryState::Open(browser) = &state.library else {
                return Task::none();
            };
            let Some(list) = browser.library.playlists.get(playlist) else {
                return Task::none();
            };
            if !browser.library.is_engine() {
                return Task::none();
            }
            let files: Vec<&model::Track> = list
                .tracks
                .iter()
                .map(|&index| &browser.library.tracks[index])
                .filter(|track| {
                    !track.streaming
                        && !state
                            .analysis_queue
                            .as_ref()
                            .is_some_and(|queue| queue.contains(track.id))
                })
                .collect();
            let menu = playlist_menu::Menu {
                playlist,
                title: list.title.clone(),
                smart: list.is_smart,
                tracks: files.len(),
                unanalysed: files
                    .iter()
                    .filter(|track| browser.analysis(track) == 0)
                    .count(),
                confirm_reanalyse: false,
                at,
            };
            state.track_menu = None;
            state.folder_menu = None;
            state.playlist_menu = Some(menu);
        }
        Message::PlaylistMenuClose => state.playlist_menu = None,
        Message::PlaylistMenuRename | Message::PlaylistMenuDelete | Message::PlaylistMenuEdit => {
            let Some(menu) = state.playlist_menu.take() else {
                return Task::none();
            };
            if state.locked() {
                return Task::none();
            }
            let next = match message {
                Message::PlaylistMenuRename if !menu.smart => {
                    Message::PlaylistRename(menu.playlist)
                }
                Message::PlaylistMenuDelete if !menu.smart => {
                    Message::PlaylistDelete(menu.playlist)
                }
                Message::PlaylistMenuEdit if menu.smart => Message::SmartEdit(menu.playlist),
                _ => return Task::none(),
            };
            return crate::handle(state, next);
        }
        Message::PlaylistMenuAnalyse => {
            let Some(menu) = state.playlist_menu.take() else {
                return Task::none();
            };
            return analyse(state, &menu, false);
        }
        Message::PlaylistMenuReanalyse => {
            // The first click only asks: it replaces the analyses of all the tracks.
            if let Some(menu) = &mut state.playlist_menu
                && !menu.confirm_reanalyse
            {
                menu.confirm_reanalyse = true;
                return Task::none();
            }
            let Some(menu) = state.playlist_menu.take() else {
                return Task::none();
            };
            return analyse(state, &menu, true);
        }
        _ => {}
    }
    Task::none()
}

/// Analyses the tracks of the playlist of `menu` one after the other: with `overwrite` all of
/// them, replacing what they have, else the ones that have no analysis. While tracks are
/// being analysed they join the queue (the ones in it already stay as they are).
fn analyse(state: &mut State, menu: &playlist_menu::Menu, overwrite: bool) -> Task<Message> {
    let LibraryState::Open(browser) = &state.library else {
        return Task::none();
    };
    let Some(list) = browser.library.playlists.get(menu.playlist) else {
        return Task::none();
    };
    let tracks: Vec<analysis_queue::Entry> = list
        .tracks
        .iter()
        .map(|&index| &browser.library.tracks[index])
        .filter(|track| !track.streaming && (overwrite || browser.analysis(track) == 0))
        .map(|track| (track.id, track.title.clone()))
        .collect();
    let library = browser.library.path.clone();
    state.start_analysis(library, tracks, overwrite)
}
