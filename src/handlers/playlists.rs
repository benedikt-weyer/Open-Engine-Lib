//! Adding, changing and deleting playlists and smart playlists, and saving origins.

use crate::*;

pub(crate) fn handle(state: &mut State, message: Message) -> Task<Message> {
    match message {
        // One window at a time, and none while the databases are being cloned.
        Message::SmartNew
        | Message::SmartEdit(_)
        | Message::PlaylistNew
        | Message::PlaylistRename(_)
        | Message::PlaylistDelete(_)
            if state.locked() || state.window_open() => {}
        Message::SmartNew => {
            if matches!(state.library, LibraryState::Open(_)) {
                state.smart = Some(smart_editor::Editor::new());
            }
        }
        Message::SmartEdit(index) => {
            if let LibraryState::Open(browser) = &state.library
                && let Some(playlist) = browser.library.playlists.get(index)
                && playlist.is_smart
            {
                state.smart = Some(smart_editor::Editor::edit(
                    index,
                    &playlist.title,
                    playlist.rules.as_ref(),
                ));
            }
        }
        Message::SmartEditor(event) => {
            if let Some(editor) = state.smart.as_mut().filter(|editor| !editor.saving) {
                editor.update(event);
            }
        }
        Message::SmartSave => {
            let strings = state.config.effective().language.strings();
            let backend = state.backend();
            if let (Some(editor), LibraryState::Open(browser)) =
                (state.smart.as_mut(), &state.library)
                && !editor.saving
            {
                match editor.finished(strings) {
                    Err(error) => editor.error = Some(error),
                    Ok((title, rules)) => {
                        let Some(backend) = backend else {
                            return Task::none();
                        };
                        editor.saving = true;
                        let library = Arc::clone(&browser.library);
                        return match editor.target {
                            Some(index) => Task::perform(
                                backend::update_smart_playlist(
                                    backend, library, index, title, rules,
                                ),
                                Message::SmartSaved,
                            ),
                            None => Task::perform(
                                backend::create_smart_playlist(backend, library, title, rules),
                                Message::SmartSaved,
                            ),
                        };
                    }
                }
            }
        }
        Message::SmartCancel => {
            if state.smart.as_ref().is_some_and(|editor| !editor.saving) {
                state.smart = None;
            }
        }
        Message::SmartSaved(result) => match result {
            Ok(edited) => {
                let change = match state.smart.as_ref().and_then(|editor| editor.target) {
                    Some(_) => Change::Same,
                    None => Change::Inserted(edited.playlist),
                };
                state.smart = None;
                state.backend_error = None;
                state.playlists_changed(edited.playlists, change, Some(edited.playlist));
            }
            Err(error) => {
                if let Some(editor) = &mut state.smart {
                    let strings = state.config.effective().language.strings();
                    editor.saving = false;
                    editor.error = Some(fill(&strings.smart_save_failed, &[("error", &error)]));
                }
            }
        },

        Message::MigrateOrigins => {
            if matches!(state.library, LibraryState::Open(_))
                && let Some(backend) = state.backend()
            {
                return Task::perform(backend::migrate_origins(backend), Message::OriginsMigrated);
            }
        }
        Message::OriginsMigrated(result) => match result {
            Ok(done) => {
                state.backend_error = None;
                let strings = state.config.effective().language.strings();
                if let LibraryState::Open(browser) = &mut state.library {
                    browser.set_origins(done.origins.into_iter().map(|o| (o.track_id, o.origin)));
                    browser.set_origin_notice(fill(
                        &strings.origin_migrated,
                        &[
                            ("saved", &done.saved),
                            ("kept", &done.kept),
                            ("skipped", &done.skipped),
                        ],
                    ));
                }
            }
            Err(error) => state.backend_error = Some(error),
        },

        Message::PlaylistNew => {
            if matches!(state.library, LibraryState::Open(_)) {
                state.playlist_dialog = Some(playlist_dialog::Dialog::create());
            }
        }
        Message::PlaylistRename(index) => {
            if let LibraryState::Open(browser) = &state.library
                && let Some(playlist) = browser.library.playlists.get(index)
                && !playlist.is_smart
            {
                state.playlist_dialog =
                    Some(playlist_dialog::Dialog::rename(index, &playlist.title));
            }
        }
        Message::PlaylistDelete(index) => {
            if let LibraryState::Open(browser) = &state.library
                && browser
                    .library
                    .playlists
                    .get(index)
                    .is_some_and(|playlist| !playlist.is_smart)
            {
                state.playlist_dialog = Some(playlist_dialog::Dialog::delete(index));
            }
        }
        Message::PlaylistDialog(event) => {
            if let Some(dialog) = state.playlist_dialog.as_mut().filter(|dialog| !dialog.busy) {
                dialog.update(event);
            }
        }
        Message::PlaylistConfirm => {
            let strings = state.config.effective().language.strings();
            let backend = state.backend();
            if let (Some(dialog), LibraryState::Open(browser)) =
                (state.playlist_dialog.as_mut(), &state.library)
                && !dialog.busy
            {
                let name = dialog.name.trim().to_string();
                if dialog.needs_name() && name.is_empty() {
                    dialog.error = Some(strings.smart_name_missing.clone());
                    return Task::none();
                }
                let Some(backend) = backend else {
                    return Task::none();
                };
                dialog.busy = true;
                let library = Arc::clone(&browser.library);
                return match dialog.kind {
                    playlist_dialog::Kind::Create { parent } => Task::perform(
                        backend::create_playlist(backend, library, name, parent),
                        Message::PlaylistsEdited,
                    ),
                    playlist_dialog::Kind::Rename(index) => Task::perform(
                        backend::rename_playlist(backend, library, index, name),
                        Message::PlaylistsEdited,
                    ),
                    playlist_dialog::Kind::Delete(index) => Task::perform(
                        backend::delete_playlist(backend, library, index),
                        Message::PlaylistsEdited,
                    ),
                };
            }
        }
        Message::PlaylistCancel => {
            if state
                .playlist_dialog
                .as_ref()
                .is_some_and(|dialog| !dialog.busy)
            {
                state.playlist_dialog = None;
            }
        }
        Message::PlaylistsEdited(result) => match result {
            Ok(edited) => {
                state.backend_error = None;
                let kind = state.playlist_dialog.take().map(|dialog| dialog.kind);
                let (change, show) = match (kind, edited.playlist, &state.library) {
                    (
                        Some(playlist_dialog::Kind::Delete(index)),
                        _,
                        LibraryState::Open(browser),
                    ) => (Change::Removed(browser.library.playlist_span(index)), None),
                    (_, Some(added), _) => (Change::Inserted(added), Some(added)),
                    _ => (Change::Same, None),
                };
                state.playlists_changed(edited.playlists, change, show);
            }
            Err(error) => {
                if let Some(dialog) = &mut state.playlist_dialog {
                    let strings = state.config.effective().language.strings();
                    dialog.busy = false;
                    dialog.error = Some(fill(&strings.playlist_failed, &[("error", &error)]));
                }
            }
        },
        _ => {}
    }
    Task::none()
}
