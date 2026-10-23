//! The DJ controller: the service the UI uses to show and set it up, the task that listens to
//! the audio process's MIDI messages, and what a press on a bound control does to the player.
//! (Which control does what is `crate::controller`; the hardware is the audio process's.)

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use open_engine_api::CONTROLLER_SEEK_SECONDS;
use open_engine_api::v1 as api;
use open_engine_api::v1::controller_service_server::ControllerService;
use open_engine_api::v1::midi_event::Event as MidiEvent;
use tokio::sync::{Notify, mpsc};
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status};

use super::{Service, edit_status};
use crate::controller::{Action, Event, Input, ProfileError, jog_seconds};
use crate::cues::Cue;

/// Changes that come in quick succession (a jog wheel makes hundreds of messages a second)
/// are told to a watcher as one.
const WATCH_INTERVAL: Duration = Duration::from_millis(100);

/// Where the jog wheel is and whether a hand is on its top.
#[derive(Debug, Default)]
struct Jog {
    /// The last position it reported, which the next one is measured from. (Kept while
    /// nothing is touching it too, so the first move after a touch is a move.)
    position: Option<u16>,
    /// A hand is on it, so turning it moves the track.
    touching: bool,
}

/// What is asked of the audio process for the jog wheel.
#[derive(Debug, Clone, Copy, PartialEq)]
enum JogCall {
    Begin,
    /// Seconds of the track, negative for backwards.
    Move(f64),
    End,
}

/// The calls for the jog wheel, in order, waiting to be made. A wheel sends hundreds of
/// messages a second, and a call for each (the listener waiting for every answer) would fall
/// behind and make the track lag after the hand; moves that wait next to each other are made
/// as one call.
#[derive(Default)]
struct JogCalls {
    waiting: Mutex<VecDeque<JogCall>>,
    wake: Notify,
}

impl JogCalls {
    fn push(&self, call: JogCall) {
        {
            let mut waiting = self.waiting.lock().expect("jog calls lock");
            match (call, waiting.back_mut()) {
                (JogCall::Move(seconds), Some(JogCall::Move(last))) => *last += seconds,
                _ => waiting.push_back(call),
            }
        }
        self.wake.notify_one();
    }

    fn next(&self) -> Option<JogCall> {
        self.waiting.lock().expect("jog calls lock").pop_front()
    }
}

/// Makes the calls as they come, one at a time and in order. Stops with the task that owns it.
struct JogSender(tokio::task::JoinHandle<()>);

impl JogSender {
    fn start(service: &Service, calls: Arc<JogCalls>) -> Self {
        let service = service.clone();
        Self(tokio::spawn(async move {
            loop {
                while let Some(call) = calls.next() {
                    if let Err(status) = service.make(call).await {
                        eprintln!("open-engine-backend: {call:?}: {}", status.message());
                    }
                }
                calls.wake.notified().await;
            }
        }))
    }
}

impl Drop for JogSender {
    fn drop(&mut self) {
        self.0.abort();
    }
}

impl Service {
    /// Starts the task that listens to the controllers, unless one is running.
    fn start_listening(&self) {
        let mut listener = self.0.listener.lock().expect("listener lock");
        if listener.as_ref().is_none_or(|task| task.is_finished()) {
            *listener = Some(tokio::spawn(listen(self.clone())));
        }
    }

    /// Stops the task. (The audio process stops listening to the hardware once it is gone.)
    fn stop_listening(&self) {
        if let Some(task) = self.0.listener.lock().expect("listener lock").take() {
            task.abort();
            // A hand on the jog wheel that is never let go of would hold the track for good.
            if let Ok(mut audio) = self.audio() {
                tokio::spawn(async move { audio.end_scratch(api::EndScratchRequest {}).await });
            }
        }
    }

    /// Does what a message from the controller asks for. The jog wheel's are only queued,
    /// for `JogSender`: nothing waits for the audio process while the wheel is turned.
    async fn handle(&self, event: Event, jog: &mut Jog, calls: &JogCalls) -> Result<(), Status> {
        match event {
            Event::Press(action) => self.perform(action).await?,
            Event::JogTouch(down) => {
                jog.touching = down;
                calls.push(if down { JogCall::Begin } else { JogCall::End });
            }
            Event::JogPosition(position) => {
                let before = jog.position.replace(position);
                if let (true, Some(before)) = (jog.touching, before) {
                    let seconds = jog_seconds(before, position);
                    if seconds != 0.0 {
                        calls.push(JogCall::Move(seconds));
                    }
                }
            }
        }
        Ok(())
    }

    /// Makes a call for the jog wheel.
    async fn make(&self, call: JogCall) -> Result<(), Status> {
        let mut audio = self.audio()?;
        match call {
            JogCall::Begin => audio.begin_scratch(api::BeginScratchRequest {}).await?,
            JogCall::Move(delta_seconds) => {
                audio
                    .move_scratch(api::MoveScratchRequest { delta_seconds })
                    .await?
            }
            JogCall::End => audio.end_scratch(api::EndScratchRequest {}).await?,
        };
        Ok(())
    }

    /// Does what a press on the control bound to `action` asks for.
    async fn perform(&self, action: Action) -> Result<(), Status> {
        let mut audio = self.audio()?;
        match action {
            // (Nothing happens while the player is stopped: there is no song to pause.)
            Action::PlayPause => {
                audio.toggle_pause(api::TogglePauseRequest {}).await?;
            }
            Action::Stop => {
                audio.stop(api::StopRequest {}).await?;
            }
            Action::Next => {
                audio.next(api::NextRequest {}).await?;
            }
            Action::Previous => {
                audio.previous(api::PreviousRequest {}).await?;
            }
            Action::SeekBack | Action::SeekForward => {
                let status = audio
                    .get_status(api::GetStatusRequest {})
                    .await?
                    .into_inner();
                if status.track_id.is_none() {
                    return Ok(());
                }
                let step = u64::from(CONTROLLER_SEEK_SECONDS) * 1000;
                let position_ms = if action == Action::SeekBack {
                    status.position_ms.saturating_sub(step)
                } else {
                    status.position_ms.saturating_add(step)
                };
                audio.seek(api::SeekRequest { position_ms }).await?;
            }
            Action::Pad(slot) => self.press_pad(slot).await?,
            // (They are told as `Event::Jog...`, never as presses.)
            Action::JogTouch | Action::JogWheel => {}
        }
        Ok(())
    }

    /// A cue pad of the playing track: a pad with a cue jumps to it, an empty one gets a cue
    /// at the playhead (and whoever shows the cues is told to fetch them again).
    async fn press_pad(&self, slot: usize) -> Result<(), Status> {
        let mut audio = self.audio()?;
        let status = audio
            .get_status(api::GetStatusRequest {})
            .await?
            .into_inner();
        let Some(track) = status.track_id else {
            return Ok(());
        };
        let library = self.library()?;
        if library.is_folder() {
            return Ok(());
        }

        let mut cues = {
            let library = library.clone();
            tokio::task::spawn_blocking(move || library.cues(track as usize))
                .await
                .map_err(|error| Status::internal(error.to_string()))?
                .map_err(|error| Status::internal(error.to_string()))?
        };
        if let Some(cue) = cues.iter().find(|cue| cue.slot == slot) {
            let position_ms = (cue.position_seconds.max(0.0) * 1000.0) as u64;
            audio.seek(api::SeekRequest { position_ms }).await?;
            return Ok(());
        }
        cues.push(Cue::for_pad(slot, status.position_ms as f64 / 1000.0));
        tokio::task::spawn_blocking(move || library.set_cues(track as usize, &cues))
            .await
            .map_err(|error| Status::internal(error.to_string()))?
            .map_err(edit_status)?;
        self.0.controller.cues_changed(track);
        Ok(())
    }
}

/// Listens to what the audio process hears of the controllers, until it is aborted or the
/// audio process stops.
async fn listen(service: Service) {
    let controller = &service.0.controller;
    let mut messages = match service.audio() {
        Ok(mut audio) => match audio.watch_midi(api::WatchMidiRequest {}).await {
            Ok(response) => response.into_inner(),
            Err(status) => return controller.unavailable(status.message().to_owned()),
        },
        Err(status) => return controller.unavailable(status.message().to_owned()),
    };
    let mut jog = Jog::default();
    let calls = Arc::new(JogCalls::default());
    let _sender = JogSender::start(&service, Arc::clone(&calls));
    loop {
        let event = match messages.message().await {
            Ok(Some(event)) => event,
            Ok(None) => return controller.unavailable("the audio process stopped".to_owned()),
            Err(status) => return controller.unavailable(status.message().to_owned()),
        };
        match event.event {
            Some(MidiEvent::Input(input)) => {
                let event = Input::from_api(&input).and_then(|input| controller.input(input));
                if let Some(event) = event
                    && let Err(status) = service.handle(event, &mut jog, &calls).await
                {
                    eprintln!("open-engine-backend: {event:?}: {}", status.message());
                }
            }
            Some(MidiEvent::Devices(devices)) => controller.set_devices(devices.names),
            Some(MidiEvent::Unavailable(reason)) => return controller.unavailable(reason),
            None => {}
        }
    }
}

#[tonic::async_trait]
impl ControllerService for Service {
    type WatchStream = ReceiverStream<Result<api::ControllerState, Status>>;

    async fn watch(
        &self,
        _: Request<api::WatchControllerRequest>,
    ) -> Result<Response<Self::WatchStream>, Status> {
        let service = self.clone();
        let mut changes = service.0.controller.subscribe();
        let (sender, receiver) = mpsc::channel(4);
        tokio::spawn(async move {
            loop {
                if sender
                    .send(Ok(service.0.controller.snapshot()))
                    .await
                    .is_err()
                {
                    return;
                }
                if changes.changed().await.is_err() {
                    return;
                }
                tokio::time::sleep(WATCH_INTERVAL).await;
            }
        });
        Ok(Response::new(ReceiverStream::new(receiver)))
    }

    async fn set_enabled(
        &self,
        request: Request<api::SetControllerEnabledRequest>,
    ) -> Result<Response<api::ControllerState>, Status> {
        if request.into_inner().enabled {
            self.0.controller.set_enabled(true);
            self.start_listening();
        } else {
            self.stop_listening();
            self.0.controller.set_enabled(false);
        }
        Ok(Response::new(self.0.controller.snapshot()))
    }

    async fn learn(
        &self,
        request: Request<api::LearnRequest>,
    ) -> Result<Response<api::ControllerState>, Status> {
        let action = Action::from_api(request.into_inner().action())
            .ok_or_else(|| Status::invalid_argument("no such action"))?;
        self.0.controller.learn(action).map_err(profile_status)?;
        Ok(Response::new(self.0.controller.snapshot()))
    }

    async fn cancel_learn(
        &self,
        _: Request<api::CancelLearnRequest>,
    ) -> Result<Response<api::ControllerState>, Status> {
        self.0.controller.cancel_learning();
        Ok(Response::new(self.0.controller.snapshot()))
    }

    async fn clear_binding(
        &self,
        request: Request<api::ClearBindingRequest>,
    ) -> Result<Response<api::ControllerState>, Status> {
        let action = Action::from_api(request.into_inner().action())
            .ok_or_else(|| Status::invalid_argument("no such action"))?;
        self.0.controller.clear(action).map_err(profile_status)?;
        Ok(Response::new(self.0.controller.snapshot()))
    }

    async fn select_profile(
        &self,
        request: Request<api::SelectProfileRequest>,
    ) -> Result<Response<api::ControllerState>, Status> {
        let controller = &self.0.controller;
        controller
            .select_profile(&request.into_inner().name)
            .map_err(profile_status)?;
        Ok(Response::new(controller.snapshot()))
    }

    async fn create_profile(
        &self,
        request: Request<api::CreateProfileRequest>,
    ) -> Result<Response<api::ControllerState>, Status> {
        let request = request.into_inner();
        let controller = &self.0.controller;
        controller
            .create_profile(&request.name, request.copy_of.as_deref())
            .map_err(profile_status)?;
        Ok(Response::new(controller.snapshot()))
    }

    async fn rename_profile(
        &self,
        request: Request<api::RenameProfileRequest>,
    ) -> Result<Response<api::ControllerState>, Status> {
        let request = request.into_inner();
        let controller = &self.0.controller;
        controller
            .rename_profile(&request.name, &request.new_name)
            .map_err(profile_status)?;
        Ok(Response::new(controller.snapshot()))
    }

    async fn delete_profile(
        &self,
        request: Request<api::DeleteProfileRequest>,
    ) -> Result<Response<api::ControllerState>, Status> {
        let controller = &self.0.controller;
        controller
            .delete_profile(&request.into_inner().name)
            .map_err(profile_status)?;
        Ok(Response::new(controller.snapshot()))
    }
}

fn profile_status(error: ProfileError) -> Status {
    let message = error.to_string();
    match error {
        ProfileError::EmptyName | ProfileError::NameTooLong => Status::invalid_argument(message),
        ProfileError::NameTaken => Status::already_exists(message),
        ProfileError::Unknown => Status::not_found(message),
        ProfileError::BuiltIn => Status::failed_precondition(message),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn drain(calls: &JogCalls) -> Vec<JogCall> {
        std::iter::from_fn(|| calls.next()).collect()
    }

    #[test]
    fn moves_that_wait_next_to_each_other_are_made_as_one() {
        let calls = JogCalls::default();
        calls.push(JogCall::Begin);
        for _ in 0..100 {
            calls.push(JogCall::Move(0.01));
        }
        calls.push(JogCall::Move(-0.25));
        let [JogCall::Begin, JogCall::Move(seconds)] = drain(&calls)[..] else {
            panic!("not one begin and one move");
        };
        assert!((seconds - 0.75).abs() < 1e-9, "{seconds}");
    }

    #[test]
    fn begin_and_end_keep_their_place_between_moves() {
        let calls = JogCalls::default();
        calls.push(JogCall::Move(1.0));
        calls.push(JogCall::End);
        calls.push(JogCall::Begin);
        calls.push(JogCall::Move(2.0));
        calls.push(JogCall::Move(3.0));
        assert_eq!(
            drain(&calls),
            [
                JogCall::Move(1.0),
                JogCall::End,
                JogCall::Begin,
                JogCall::Move(5.0)
            ]
        );
        assert_eq!(calls.next(), None);
    }
}
