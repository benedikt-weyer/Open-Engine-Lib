//! The menu of a folder of a folder of music (right click in the playlist tree) and the
//! window that imports that folder into an Engine library as a playlist.

use import_view::{Import, Options, Stage};
use open_engine_api::v1::NameOrder;

use crate::*;

pub(crate) fn handle(state: &mut State, message: Message) -> Task<Message> {
    match message {
        Message::FolderMenuOpen { playlist, at } => {
            // Only the folders of a folder of music are imported, and not while something
            // else works on the backend.
            let LibraryState::Open(browser) = &state.library else {
                return Task::none();
            };
            if browser.library.is_engine() || state.locked() || state.window_open() {
                return Task::none();
            }
            let Some(folder) = browser.library.playlists.get(playlist) else {
                return Task::none();
            };
            state.track_menu = None;
            state.folder_menu = Some(folder_menu::Menu {
                playlist,
                title: folder.title.clone(),
                at,
            });
        }
        Message::FolderMenuClose => state.folder_menu = None,
        Message::FolderMenuImport => {
            let Some(menu) = state.folder_menu.take() else {
                return Task::none();
            };
            if state.locked() || state.window_open() || state.syncing() {
                return Task::none();
            }
            let LibraryState::Open(browser) = &state.library else {
                return Task::none();
            };
            let Some(folder) = browser.library.playlists.get(menu.playlist) else {
                return Task::none();
            };
            // (A folder that has music below it has tracks; one that has none is not a list.)
            if folder.tracks.is_empty() {
                return Task::none();
            }
            state.import = Some(Import {
                playlist: menu.playlist,
                name: folder.title.clone(),
                tracks: folder.tracks.len(),
                stage: Stage::Pick,
                preview: None,
            });
        }
        Message::ImportAdd => {
            if matches!(
                state.import,
                Some(Import {
                    stage: Stage::Pick,
                    ..
                })
            ) {
                let title = state
                    .config
                    .effective()
                    .language
                    .strings()
                    .libsync_add_title
                    .clone();
                return Task::perform(
                    async move {
                        rfd::AsyncFileDialog::new()
                            .set_title(title)
                            .pick_folder()
                            .await
                            .map(|folder| folder.path().to_path_buf())
                    },
                    Message::ImportAdded,
                );
            }
        }
        Message::ImportAdded(Some(path)) => return handle(state, Message::ImportTarget(path)),
        Message::ImportAdded(None) => {}
        Message::ImportTarget(target) => {
            if let Some(import) = &mut state.import
                && matches!(import.stage, Stage::Pick)
            {
                let name_order = NameOrder::Strict;
                import.stage = Stage::Options {
                    target,
                    options: Options {
                        title: import.name.clone(),
                        copy_files: true,
                        analyse: true,
                        name_order,
                    },
                    error: None,
                };
                return fetch_preview(state, name_order);
            }
        }
        Message::ImportOrder(name_order) => {
            if let Some(Import {
                stage: Stage::Options { options, .. },
                preview,
                ..
            }) = &mut state.import
                && options.name_order != name_order
            {
                options.name_order = name_order;
                *preview = None;
                return fetch_preview(state, name_order);
            }
        }
        Message::ImportPreview(name_order, result) => {
            // (An answer for an order that is not the chosen one any more is not wanted.)
            if let Some(import) = &mut state.import
                && import
                    .stage
                    .options()
                    .is_some_and(|options| options.name_order == name_order)
            {
                import.preview = Some(result);
            }
        }
        Message::ImportTitle(title) => {
            if let Some(Import {
                stage: Stage::Options { options, .. },
                ..
            }) = &mut state.import
            {
                options.title = title;
            }
        }
        Message::ImportOption(setting, on) => {
            if let Some(Import {
                stage: Stage::Options { options, .. },
                ..
            }) = &mut state.import
            {
                *options = options.clone().with(setting, on);
            }
        }
        Message::ImportBack => {
            if let Some(import) = &mut state.import
                && matches!(import.stage, Stage::Options { .. })
            {
                import.stage = Stage::Pick;
            }
        }
        Message::ImportStart => {
            let Some(backend) = state.backend() else {
                return Task::none();
            };
            if let Some(import) = &mut state.import
                && let Stage::Options {
                    target, options, ..
                } = &import.stage
                && !options.title.trim().is_empty()
            {
                let (target, options) = (target.clone(), options.clone());
                let (playlist, title, copy_files, name_order) = (
                    import.playlist,
                    options.title.trim().to_owned(),
                    options.copy_files,
                    options.name_order,
                );
                import.stage = Stage::Importing {
                    target: target.clone(),
                    options,
                };
                state.progress = None;
                return Task::perform(
                    backend::import_folder(
                        backend, playlist, target, title, copy_files, name_order,
                    ),
                    Message::ImportFinished,
                );
            }
        }
        Message::ImportFinished(result) => {
            if let Some(import) = &mut state.import
                && let Stage::Importing { target, options } = &import.stage
            {
                let (target, options) = (target.clone(), options.clone());
                import.stage = match result {
                    Ok(done) => {
                        // A library that could be written to is worth remembering.
                        state.recents_error = state
                            .recents
                            .add(PathBuf::from(&done.target_path))
                            .err()
                            .map(|error| error.to_string());
                        Stage::Done {
                            done,
                            analyse: options.analyse,
                        }
                    }
                    Err(error) => Stage::Options {
                        target,
                        options,
                        error: Some(error),
                    },
                };
            }
            state.progress = None;
        }
        Message::ImportOpenLibrary => {
            if let Some(Import {
                stage: Stage::Done { done, analyse },
                ..
            }) = state.import.take()
            {
                state.after_open = Some(analysis_queue::AfterOpen {
                    library: done.target_path.clone(),
                    playlist: done.playlist_title.clone(),
                    added: done.added_paths.clone(),
                    analyse,
                });
                return state.open(PathBuf::from(done.target_path), None);
            }
        }
        Message::ImportClose
            if state
                .import
                .as_ref()
                .is_some_and(|import| import.stage.can_close()) =>
        {
            state.import = None;
        }
        _ => {}
    }
    Task::none()
}

/// Asks the backend for the files of the folder being imported in the order `name_order`
/// gives them.
fn fetch_preview(state: &State, name_order: NameOrder) -> Task<Message> {
    let (Some(backend), Some(import)) = (state.backend(), &state.import) else {
        return Task::none();
    };
    Task::perform(
        backend::preview_import(backend, import.playlist, name_order),
        move |result| Message::ImportPreview(name_order, result),
    )
}

/// What follows the opening of the library that was imported into: its new playlist is
/// shown, and the new tracks are analysed if that was chosen.
pub(crate) fn opened_after_import(
    state: &mut State,
    after: analysis_queue::AfterOpen,
) -> Task<Message> {
    let LibraryState::Open(browser) = &state.library else {
        return Task::none();
    };
    // (Another library than the one that was meant: nothing is done to it.)
    if browser.library.path != after.library {
        return Task::none();
    }
    let playlist = browser.library.playlists.iter().position(|playlist| {
        playlist.depth == 0 && !playlist.is_smart && playlist.title == after.playlist
    });
    let tracks = new_tracks(browser, &after.added);
    let library = browser.library.path.clone();
    let mut tasks = Vec::new();
    if let Some(playlist) = playlist {
        tasks.push(handle_show(state, playlist));
    }
    if after.analyse {
        tasks.push(state.start_analysis(library, tracks, false));
    }
    Task::batch(tasks)
}

fn handle_show(state: &mut State, playlist: usize) -> Task<Message> {
    crate::handle(state, Message::ShowPlaylist(playlist))
}

/// The tracks the import added (by their paths) that nothing analysed yet: files, not
/// streaming tracks.
fn new_tracks(browser: &Browser, added: &[String]) -> Vec<analysis_queue::Entry> {
    browser
        .library
        .tracks
        .iter()
        .filter(|track| {
            !track.streaming && browser.analysis(track) == 0 && added.contains(&track.path)
        })
        .map(|track| (track.id, track.title.clone()))
        .collect()
}
