//! The DJ controller. The backend listens to it and does what its controls are bound to; the
//! UI shows what it reports and sends the commands of the Settings page.

use crate::*;

pub(crate) fn handle(state: &mut State, message: Message) -> Task<Message> {
    match message {
        Message::ControllerState(Ok(report)) => {
            if let Some(track) = state.controller.update(report)
                && state.playing_id() == Some(track)
            {
                // A pad on the controller changed the cues of the song in the player.
                state.cues.release(track);
                return state.fetch_cues();
            }
        }
        Message::ControllerState(Err(error)) => state.controller.error = Some(error),
        Message::ControllerLearn(action) => {
            return ask(state, move |backend| {
                backend::learn_control(backend, action)
            });
        }
        Message::ControllerLearnCancel => return ask(state, backend::cancel_learning),
        Message::ControllerClear(action) => {
            return ask(state, move |backend| {
                backend::clear_binding(backend, action)
            });
        }
        Message::ControllerEnabledToggled(enabled) => {
            state.config_error = state
                .config
                .set_controller_enabled(enabled)
                .err()
                .map(|e| e.to_string());
            return ask(state, move |backend| {
                backend::set_controller_enabled(backend, enabled)
            });
        }
        _ => {}
    }
    Task::none()
}

/// Makes a request about the controller; the answer is the state it leaves.
pub(crate) fn ask<F>(state: &State, request: impl FnOnce(Backend) -> F) -> Task<Message>
where
    F: std::future::Future<Output = Result<ControllerState, String>> + Send + 'static,
{
    match state.backend() {
        Some(backend) => Task::perform(request(backend), Message::ControllerState),
        None => Task::none(),
    }
}

/// What to do once the backend is ready: follow the controller's state, and have it listen if
/// the setting says so.
pub(crate) fn start(state: &State, backend: &Backend) -> Task<Message> {
    let watch = Task::run(
        backend::watch_controller(backend.clone()),
        Message::ControllerState,
    );
    if state.config.effective().controller_enabled {
        let backend = backend.clone();
        Task::batch([
            watch,
            Task::perform(
                backend::set_controller_enabled(backend, true),
                Message::ControllerState,
            ),
        ])
    } else {
        watch
    }
}
