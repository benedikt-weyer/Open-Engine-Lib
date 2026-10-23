//! The window that syncs the playlists to another library.

use crate::*;

pub(crate) fn handle(state: &mut State, message: Message) -> Task<Message> {
    match message {
        Message::LibrarySyncOpen => {
            if !state.locked()
                && !state.window_open()
                && matches!(state.library, LibraryState::Open(_))
            {
                state.library_sync = Some(LibrarySyncStage::Pick);
            }
        }
        Message::LibrarySyncAdd => {
            if matches!(state.library_sync, Some(LibrarySyncStage::Pick)) {
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
                    Message::LibrarySyncAdded,
                );
            }
        }
        Message::LibrarySyncAdded(Some(path)) => {
            return handle(state, Message::LibrarySyncTarget(path));
        }
        Message::LibrarySyncAdded(None) => {}
        Message::LibrarySyncTarget(target) => {
            return state.compare_libraries(target, library_sync_view::Options::default(), true);
        }
        Message::LibrarySyncOption(setting, on) => {
            if let Some(LibrarySyncStage::Ready {
                target, options, ..
            }) = &state.library_sync
            {
                let (target, options) = (target.clone(), options.with(setting, on));
                return state.compare_libraries(target, options, false);
            }
        }
        Message::LibrarySyncDiffLoaded {
            target: loaded,
            options: loaded_options,
            initial,
            result,
        } => {
            // (An answer to a comparison that was given up on is not wanted.)
            if let Some(LibrarySyncStage::Loading {
                target, options, ..
            }) = &state.library_sync
                && *target == loaded
                && *options == loaded_options
            {
                // Somewhere without a library: it gets one, with the tracks (what else
                // would be in it?), unless that is taken back.
                if let Ok(diff) = &result
                    && initial
                    && diff.target_is_new
                    && !loaded_options.copy_tracks
                {
                    let options = loaded_options.with(library_sync_view::Setting::CopyTracks, true);
                    return state.compare_libraries(loaded, options, false);
                }
                state.library_sync = Some(match result {
                    Ok(diff) => {
                        // A library that could be compared is worth remembering (one that
                        // is not there yet is once it is made).
                        if !diff.target_is_new {
                            state.recents_error = state
                                .recents
                                .add(PathBuf::from(&diff.target_path))
                                .err()
                                .map(|error| error.to_string());
                        }
                        LibrarySyncStage::Ready {
                            target: loaded,
                            options: loaded_options,
                            diff,
                            error: None,
                        }
                    }
                    Err(error) => LibrarySyncStage::Failed {
                        target: loaded,
                        error,
                    },
                });
            }
        }
        Message::LibrarySyncStart => {
            if let Some(LibrarySyncStage::Ready {
                target,
                options,
                diff,
                ..
            }) = &state.library_sync
                && library_sync_view::has_changes(diff)
                && let Some(backend) = state.backend()
            {
                let (target, options, diff) = (target.clone(), *options, diff.clone());
                state.progress = None;
                state.library_sync = Some(LibrarySyncStage::Syncing {
                    target: target.clone(),
                    options,
                    diff,
                });
                return Task::perform(
                    backend::sync_to_library(backend, target, options),
                    Message::LibrarySyncFinished,
                );
            }
        }
        Message::LibrarySyncFinished(result) => {
            if let Some(LibrarySyncStage::Syncing {
                target,
                options,
                diff,
            }) = state.library_sync.take()
            {
                let mut reload = false;
                state.library_sync = Some(match result {
                    Ok(done) => {
                        // The other way round the working copy was changed, and the
                        // backend reads it again.
                        reload = options.reverse && !done.backup_path.is_empty();
                        if done.created {
                            state.recents_error = state
                                .recents
                                .add(PathBuf::from(&done.target_path))
                                .err()
                                .map(|error| error.to_string());
                        }
                        LibrarySyncStage::Done(done, options.reverse)
                    }
                    Err(error) => LibrarySyncStage::Ready {
                        target,
                        options,
                        diff,
                        error: Some(error),
                    },
                });
                if reload {
                    return state.reload_library();
                }
            }
        }
        Message::LibrarySyncBack => {
            if state
                .library_sync
                .as_ref()
                .is_some_and(LibrarySyncStage::can_close)
            {
                state.library_sync = Some(LibrarySyncStage::Pick);
            }
        }
        Message::LibrarySyncClose
            if state
                .library_sync
                .as_ref()
                .is_some_and(LibrarySyncStage::can_close) =>
        {
            state.library_sync = None;
        }
        _ => {}
    }
    Task::none()
}
