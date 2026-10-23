//! The window that changes the folder the saved origins start with.

use crate::*;

pub(crate) fn handle(state: &mut State, message: Message) -> Task<Message> {
    match message {
        Message::OriginPrefixOpen => {
            if !state.locked()
                && !state.window_open()
                && !state.syncing()
                && matches!(state.library, LibraryState::Open(_))
                && let Some(backend) = state.backend()
            {
                state.origin_prefix = Some(origin_prefix_dialog::Dialog::new());
                return Task::perform(
                    backend::get_origin_prefix(backend),
                    Message::OriginPrefixLoaded,
                );
            }
        }
        Message::OriginPrefixLoaded(result) => {
            let strings = state.config.effective().language.strings();
            if let Some(dialog) = &mut state.origin_prefix {
                match result {
                    Ok(found) => dialog.loaded(found.prefix, found.count),
                    Err(error) => {
                        dialog.found = Some((String::new(), 0));
                        dialog.error = Some(fill(
                            &strings.origin_prefix_load_failed,
                            &[("error", &error)],
                        ));
                    }
                }
            }
        }
        Message::OriginPrefixDialog(event) => {
            if let Some(dialog) = state.origin_prefix.as_mut().filter(|dialog| !dialog.busy) {
                dialog.update(event);
            }
        }
        Message::OriginPrefixBrowse => {
            if let Some(dialog) = state.origin_prefix.as_ref().filter(|dialog| !dialog.busy) {
                let start = PathBuf::from(dialog.new.trim());
                return Task::perform(
                    async move {
                        let mut picker = rfd::AsyncFileDialog::new();
                        if start.is_dir() {
                            picker = picker.set_directory(start);
                        }
                        picker
                            .pick_folder()
                            .await
                            .map(|folder| folder.path().to_path_buf())
                    },
                    Message::OriginPrefixPicked,
                );
            }
        }
        Message::OriginPrefixPicked(Some(folder)) => {
            if let Some(dialog) = state.origin_prefix.as_mut().filter(|dialog| !dialog.busy) {
                dialog.update(origin_prefix_dialog::Event::New(
                    folder.display().to_string(),
                ));
            }
        }
        Message::OriginPrefixPicked(None) => {}
        Message::OriginPrefixApply => {
            let backend = state.backend();
            if let Some(dialog) = state.origin_prefix.as_mut().filter(|dialog| !dialog.busy)
                && let Some((from, to)) = dialog.change()
                && let Some(backend) = backend
            {
                let (from, to) = (from.to_string(), to.to_string());
                dialog.busy = true;
                dialog.error = None;
                return Task::perform(
                    backend::replace_origin_prefix(backend, from, to),
                    Message::OriginPrefixApplied,
                );
            }
        }
        Message::OriginPrefixApplied(result) => {
            let strings = state.config.effective().language.strings();
            match result {
                Ok(done) => {
                    state.backend_error = None;
                    state.origin_prefix = None;
                    if let LibraryState::Open(browser) = &mut state.library {
                        browser
                            .set_origins(done.origins.into_iter().map(|o| (o.track_id, o.origin)));
                        browser.set_origin_notice(fill(
                            &strings.origin_prefix_changed,
                            &[("changed", &done.changed)],
                        ));
                    }
                }
                Err(error) => {
                    if let Some(dialog) = &mut state.origin_prefix {
                        dialog.busy = false;
                        dialog.error =
                            Some(fill(&strings.origin_prefix_failed, &[("error", &error)]));
                    }
                }
            }
        }
        Message::OriginPrefixCancel
            if state
                .origin_prefix
                .as_ref()
                .is_some_and(|dialog| !dialog.busy) =>
        {
            state.origin_prefix = None;
        }
        _ => {}
    }
    Task::none()
}
