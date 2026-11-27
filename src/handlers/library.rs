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
        | Message::TabClose(_)
        | Message::TabSelect(_)
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
            if !matches!(state.library, LibraryState::Opening(_)) {
                // One that is open already (shown or parked) is not read again: its tab is
                // shown.
                if let Some(id) = state.open_library_at(&path) {
                    return state.switch_to(id);
                }
                return state.open_folder(path);
            }
        }
        Message::RemoveLocalFolder(path) => {
            state.recents_error = state
                .local_folders
                .remove(&path)
                .err()
                .map(|error| error.to_string());
            // Taking an open folder out of the list closes its tab (shown or not).
            let open = state
                .browsers()
                .find(|browser| {
                    !browser.library.is_engine() && Path::new(&browser.library.path) == path
                })
                .map(|browser| browser.library.id);
            if let Some(id) = open {
                return state.close_tab(id);
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
                state.progress = None;
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
                        let mut browser = Browser::new(library);
                        // A library that was open already (opened again by another way to
                        // its path, say after an import into it) was replaced in the
                        // backend, so its old tab goes and the new one takes its place.
                        let old = state.parked.iter().position(|parked| {
                            parked.library.path == browser.library.path
                                && parked.library.kind == browser.library.kind
                        });
                        browser.order = match old {
                            Some(position) => state.parked.remove(position).order,
                            None => {
                                state.next_tab += 1;
                                state.next_tab - 1
                            }
                        };
                        LibraryState::Open(browser)
                    }
                    Err(error) => {
                        state.restore = None;
                        LibraryState::Closed { error: Some(error) }
                    }
                };
                if let Some(session) = state.restore.take() {
                    return state.restore_session(session);
                }
                // The library that a folder was imported into: its new playlist is shown and
                // the new tracks are analysed, if that was chosen.
                if let Some(after) = state.after_open.take() {
                    return super::import::opened_after_import(state, after);
                }
            }
        }
        // The "Close library" button closes the shown one; a tab's cross closes its own.
        Message::CloseLibrary => {
            if let Some(id) = state.shown_id() {
                return state.close_tab(id);
            }
        }
        Message::TabSelect(id) => return state.switch_to(id),
        Message::TabClose(id) => return state.close_tab(id),
        Message::UnsavedTick => return state.check_unsaved(),
        Message::UnsavedChecked(id, result) => {
            // (An error is not worth showing: the next look asks again.)
            if let Ok(unsaved) = result
                && let Some(browser) = state
                    .parked
                    .iter_mut()
                    .chain(match &mut state.library {
                        LibraryState::Open(browser) => Some(browser),
                        _ => None,
                    })
                    .find(|browser| browser.library.id == id)
            {
                browser.unsaved = unsaved;
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
        Message::UnmountDrive(device, name) => {
            state.drive_error = None;
            return Task::perform(drives::unmount(device), move |result| {
                Message::DriveUnmounted {
                    name: name.clone(),
                    result,
                }
            });
        }
        Message::DriveUnmounted { name, result } => match result {
            // It is gone from the list at once, not at the next look.
            Ok(()) => {
                if !state.scanning_drives {
                    state.scanning_drives = true;
                    return scan_drives();
                }
            }
            Err(error) => {
                let language = state.config.effective().language;
                state.drive_error = Some(fill(
                    &language.strings().drive_unmount_failed,
                    &[("name", &name), ("error", &error)],
                ));
            }
        },
        Message::OpenRecent(path) | Message::OpenDrive(path) => {
            if !matches!(state.library, LibraryState::Opening(_)) {
                // One that is open already (shown or parked) is not read again: its tab is
                // shown.
                if let Some(id) = state.open_library_at(&path) {
                    return state.switch_to(id);
                }
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
