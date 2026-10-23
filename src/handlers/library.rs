//! Opening, closing, reloading and saving libraries, the File menu and the sidebar.

use crate::*;

pub(crate) fn handle(state: &mut State, message: Message) -> Task<Message> {
    match message {
        Message::MenuSelected(item) => match item {
            MenuItem::Open => return handle(state, Message::ChooseLibrary),
            MenuItem::SaveAs => {
                if !state.locked() && matches!(state.library, LibraryState::Open(_)) {
                    state.save_error = None;
                    let strings = state.config.effective().language.strings();
                    let title = strings.save_as_title.clone();
                    return Task::perform(
                        async move {
                            rfd::AsyncFileDialog::new()
                                .set_title(title)
                                .pick_folder()
                                .await
                                .map(|folder| folder.path().to_path_buf())
                        },
                        Message::SaveAsChosen,
                    );
                }
            }
            MenuItem::Reload => return state.reload_from_disk(),
            MenuItem::Exit => return state.exit(),
        },
        Message::Reloaded(result) => {
            let strings = state.config.effective().language.strings();
            match result {
                // The backend reads the new working copy; so must this.
                Ok(done) => {
                    let message = if done.kept_edits_path.is_empty() {
                        strings.reload_done.clone()
                    } else {
                        fill(
                            &strings.reload_done_kept,
                            &[("path", &done.kept_edits_path)],
                        )
                    };
                    state.reload_result = Some((message, false));
                    return state.reload_library();
                }
                Err(error) => {
                    state.reloading = false;
                    state.reload_result =
                        Some((fill(&strings.reload_failed, &[("error", &error)]), true));
                }
            }
        }
        Message::SaveAsChosen(Some(path)) => {
            if let (Some(backend), LibraryState::Open(_), None) =
                (state.backend(), &state.library, &state.saving)
            {
                state.saving = Some(path.clone());
                return Task::perform(
                    backend::save_library_as(backend, path),
                    Message::LibrarySaved,
                );
            }
        }
        Message::SaveAsChosen(None) => {}
        Message::LibrarySaved(result) => {
            state.saving = None;
            match result {
                // The library is now at its new place. Everything else carries on as it
                // was: the tracks and playlists are the same, in the same order, and the
                // player was never interrupted.
                Ok(saved) => {
                    state.save_error = None;
                    if let LibraryState::Open(browser) = &mut state.library {
                        state.recents_error = state
                            .recents
                            .add(PathBuf::from(&saved.path))
                            .err()
                            .map(|error| error.to_string());
                        browser.library = saved;
                    }
                }
                Err(error) => state.save_error = Some(error),
            }
        }
        Message::ChooseLibrary
        | Message::CloseLibrary
        | Message::OpenRecent(_)
        | Message::OpenDrive(_)
        | Message::AddLocalFolder
        | Message::OpenLocalFolder(_)
            if state.locked() => {}
        Message::AddLocalFolder => {
            let title = state
                .config
                .effective()
                .language
                .strings()
                .local_choose
                .clone();
            return Task::perform(
                async move {
                    rfd::AsyncFileDialog::new()
                        .set_title(title)
                        .pick_folder()
                        .await
                        .map(|folder| folder.path().to_path_buf())
                },
                Message::LocalFolderChosen,
            );
        }
        Message::LocalFolderChosen(Some(_)) if state.locked() => {}
        Message::LocalFolderChosen(Some(path)) => {
            state.recents_error = state
                .local_folders
                .add(path.clone())
                .err()
                .map(|error| error.to_string());
            return state.open_folder(path);
        }
        Message::LocalFolderChosen(None) => {}
        Message::OpenLocalFolder(path) => {
            // One library at a time: the backend would end up with the other one.
            if !matches!(state.library, LibraryState::Opening(_)) {
                return state.open_folder(path);
            }
        }
        Message::RemoveLocalFolder(path) => {
            state.recents_error = state
                .local_folders
                .remove(&path)
                .err()
                .map(|error| error.to_string());
            // Taking the open folder out of the list closes it.
            let open = matches!(
                &state.library,
                LibraryState::Open(browser)
                    if !browser.library.is_engine() && Path::new(&browser.library.path) == path
            );
            if open && !state.locked() {
                return handle(state, Message::CloseLibrary);
            }
        }
        Message::ChooseLibrary => {
            let title = state
                .config
                .effective()
                .language
                .strings()
                .choose_library
                .clone();
            return Task::perform(
                async move {
                    rfd::AsyncFileDialog::new()
                        .set_title(title)
                        .pick_folder()
                        .await
                        .map(|folder| folder.path().to_path_buf())
                },
                Message::LibraryChosen,
            );
        }
        Message::LibraryChosen(Some(_)) if state.locked() => {}
        Message::LibraryChosen(Some(path)) => return state.open(path, None),
        Message::LibraryChosen(None) => {}
        Message::LibraryOpened(result) => {
            if matches!(state.library, LibraryState::Opening(_)) {
                state.library = match result {
                    Ok(library) => {
                        // A folder of music is kept in the "Local music" section, not among
                        // the recent libraries.
                        let path = PathBuf::from(&library.path);
                        state.recents_error = if library.is_engine() {
                            state.recents.add(path)
                        } else {
                            state.local_folders.add(path)
                        }
                        .err()
                        .map(|error| error.to_string());
                        LibraryState::Open(Browser::new(library))
                    }
                    Err(error) => {
                        state.restore = None;
                        LibraryState::Closed { error: Some(error) }
                    }
                };
                if let Some(session) = state.restore.take() {
                    return state.restore_session(session);
                }
            }
        }
        Message::CloseLibrary => {
            state.library = LibraryState::Closed { error: None };
            state.reset_playback();
            state.covers.clear();
            state.reachable.clear();
            state.waveforms.clear();
            state.cues.clear();
            state.loops.clear();
            state.sync = None;
            state.library_sync = None;
            state.reload_result = None;
            // Closing a library on purpose means the next start doesn't reopen it.
            state.save(&Session::default(), true);
            if let Some(backend) = state.backend() {
                return Task::perform(backend::close_library(backend), Message::LibraryClosed);
            }
        }
        Message::LibraryClosed(result) => state.backend_error = result.err(),
        Message::ScanDrives => {
            if !state.scanning_drives {
                state.scanning_drives = true;
                return scan_drives();
            }
        }
        Message::DrivesFound(drives) => {
            state.scanning_drives = false;
            // A drive coming or going changes what can be reached.
            if state.drives != drives {
                state.reachable.clear();
            }
            state.drives = drives;
        }
        Message::OpenRecent(path) | Message::OpenDrive(path) => {
            // One library at a time: the backend would end up with the other one.
            if !matches!(state.library, LibraryState::Opening(_)) {
                return state.open(path, None);
            }
        }
        Message::ForgetRecent(path) => {
            state.recents_error = state
                .recents
                .forget(&path)
                .err()
                .map(|error| error.to_string());
        }

        _ => {}
    }
    Task::none()
}
