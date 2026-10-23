//! Settings, window and backend start-up.

use crate::*;

pub(crate) fn handle(state: &mut State, message: Message) -> Task<Message> {
    match message {
        Message::Navigate(page) => state.page = page,
        Message::ThemeSelected(theme) => {
            state.config_error = state.config.set_theme(theme).err().map(|e| e.to_string());
        }
        Message::LanguageSelected(language) => {
            state.config_error = state
                .config
                .set_language(language)
                .err()
                .map(|e| e.to_string());
        }
        Message::WaveformColorsSelected(colors) => {
            state.config_error = state
                .config
                .set_waveform_colors(colors)
                .err()
                .map(|e| e.to_string());
        }
        Message::ResizeStarted(column) => {
            state.resize = Some(Resize {
                column,
                width: state.columns.widths.get(column),
                grab: None,
            });
        }
        Message::ResizeMoved(x) => {
            if let Some(resize) = &mut state.resize {
                let grab = *resize.grab.get_or_insert(x);
                state
                    .columns
                    .widths
                    .set(resize.column, resize.width + (x - grab));
            }
        }
        Message::ResizeEnded => {
            if state.resize.take().is_some() {
                // Not being able to remember the widths is not worth bothering the user about.
                if let Err(error) = state.columns.save() {
                    eprintln!("could not save the column widths: {error}");
                }
            }
        }
        Message::WaveformGainDragged(band, gain) => state.gain_draft = Some((band, gain)),
        Message::WaveformGainReleased => {
            if let Some((band, gain)) = state.gain_draft.take() {
                state.config_error = state
                    .config
                    .set_waveform_gain(band, gain)
                    .err()
                    .map(|e| e.to_string());
                // The waveform of the track in the player is drawn again with the new gain.
                state.waveforms.clear();
            }
        }
        Message::ResetWaveformGains => {
            state.gain_draft = None;
            state.config_error = state
                .config
                .reset_waveform_gains()
                .err()
                .map(|e| e.to_string());
            state.waveforms.clear();
        }
        Message::WaveformDelayDragged(delay) => state.delay_draft = Some(delay),
        Message::WaveformDelayReleased => {
            if let Some(delay) = state.delay_draft.take() {
                state.config_error = state
                    .config
                    .set_waveform_delay_ms(delay)
                    .err()
                    .map(|e| e.to_string());
            }
        }
        Message::ResetWaveformDelay => {
            state.delay_draft = None;
            state.config_error = state
                .config
                .set_waveform_delay_ms(0)
                .err()
                .map(|e| e.to_string());
        }
        Message::ReopenLibraryToggled(reopen) => {
            state.config_error = state
                .config
                .set_reopen_library(reopen)
                .err()
                .map(|e| e.to_string());
        }
        Message::RestorePositionToggled(restore) => {
            state.config_error = state
                .config
                .set_restore_position(restore)
                .err()
                .map(|e| e.to_string());
        }
        Message::CloseRequested(_) => return state.exit(),
        Message::ResetOverrides => {
            state.config_error = state.config.reset_overrides().err().map(|e| e.to_string());
        }

        Message::BackendStarted(Ok(backend)) => {
            state.backend = BackendState::Ready(backend);
            if let Some((path, restore)) = state.open_when_ready.take() {
                return state.open(path, restore);
            }
        }
        Message::BackendStarted(Err(error)) => state.backend = BackendState::Failed(error),
        Message::RetryBackend => {
            state.backend = BackendState::Starting;
            state.backend_error = None;
            return start_backend();
        }

        _ => {}
    }
    Task::none()
}
