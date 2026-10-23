//! The DJ controller. The backend listens to it and does what its controls are bound to; the
//! UI shows what it reports and sends the commands of the Settings page.

use crate::*;

pub(crate) fn handle(state: &mut State, message: Message) -> Task<Message> {
    match message {
        Message::ControllerState(Ok(report)) => return report_state(state, report),
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
        Message::ControllerProfileSelected(name) => {
            state.controller.confirm_delete = None;
            return change_profiles(state, false, move |backend| {
                backend::select_profile(backend, name)
            });
        }
        Message::ControllerProfileNameChanged(name) => state.controller.name_draft = name,
        Message::ControllerProfileCreate(copy) => {
            let Some(name) = state.controller.typed_name() else {
                return Task::none();
            };
            let copy_of = copy
                .then(|| state.controller.active_profile().map(str::to_owned))
                .flatten();
            return change_profiles(state, true, move |backend| {
                backend::create_profile(backend, name, copy_of)
            });
        }
        Message::ControllerProfileRename => {
            let (Some(name), Some(current)) = (
                state.controller.typed_name(),
                state.controller.active_profile().map(str::to_owned),
            ) else {
                return Task::none();
            };
            return change_profiles(state, true, move |backend| {
                backend::rename_profile(backend, current, name)
            });
        }
        Message::ControllerProfileDelete => {
            let Some(current) = state.controller.active_profile().map(str::to_owned) else {
                return Task::none();
            };
            // A profile is a lot of learning: the first press only asks.
            if state.controller.confirm_delete.as_ref() != Some(&current) {
                state.controller.confirm_delete = Some(current);
                return Task::none();
            }
            state.controller.confirm_delete = None;
            return change_profiles(state, false, move |backend| {
                backend::delete_profile(backend, current)
            });
        }
        Message::ControllerProfileChanged(used_name, Ok(report)) => {
            state.controller.profile_error = None;
            if used_name {
                state.controller.name_draft.clear();
            }
            return report_state(state, report);
        }
        Message::ControllerProfileChanged(_, Err(error)) => {
            state.controller.profile_error = Some(error);
        }
        _ => {}
    }
    Task::none()
}

/// Takes what the backend reports of the controller.
fn report_state(state: &mut State, report: ControllerState) -> Task<Message> {
    if let Some(track) = state.controller.update(report)
        && state.playing_id() == Some(track)
    {
        // A pad on the controller changed the cues of the song in the player.
        state.cues.release(track);
        return state.fetch_cues();
    }
    Task::none()
}

/// Asks to change the profiles. Unlike the other requests, a refusal (a name that is taken)
/// is shown as that, not as the controller being out of reach.
fn change_profiles<F>(
    state: &State,
    used_name: bool,
    request: impl FnOnce(Backend) -> F,
) -> Task<Message>
where
    F: std::future::Future<Output = Result<ControllerState, String>> + Send + 'static,
{
    match state.backend() {
        Some(backend) => Task::perform(request(backend), move |result| {
            Message::ControllerProfileChanged(used_name, result)
        }),
        None => Task::none(),
    }
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
