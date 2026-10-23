//! The DJ controller as the UI shows it. The backend does all the work (listening to the
//! hardware, which control does what, learning a new binding, carrying the actions out: see
//! `ControllerService`); this only keeps the latest state it reported, for the Settings page.

use open_engine_api::v1::{Control, ControlKind, ControllerAction, ControllerState};

/// In the order they are shown.
pub const ACTIONS: [ControllerAction; 14] = [
    ControllerAction::PlayPause,
    ControllerAction::Stop,
    ControllerAction::Previous,
    ControllerAction::Next,
    ControllerAction::SeekBack,
    ControllerAction::SeekForward,
    ControllerAction::Pad1,
    ControllerAction::Pad2,
    ControllerAction::Pad3,
    ControllerAction::Pad4,
    ControllerAction::Pad5,
    ControllerAction::Pad6,
    ControllerAction::Pad7,
    ControllerAction::Pad8,
];

/// The pad (0 to 7) an action is for.
pub fn pad_of(action: ControllerAction) -> Option<usize> {
    (action as i32)
        .checked_sub(ControllerAction::Pad1 as i32)
        .and_then(|slot| usize::try_from(slot).ok())
        .filter(|slot| *slot < 8)
}

#[derive(Debug, Default)]
pub struct Controller {
    /// What the backend said last. `None` until it has said anything.
    state: Option<ControllerState>,
    /// Why the backend can't be asked about the controller.
    pub error: Option<String>,
}

impl Controller {
    /// Takes the state the backend reported. Gives the track whose cues a pad on the
    /// controller changed since the last one, so they can be fetched again.
    pub fn update(&mut self, state: ControllerState) -> Option<u32> {
        let cues_changed = self
            .state
            .as_ref()
            .filter(|before| before.cues_revision != state.cues_revision)
            .map(|_| state.cues_track_id);
        self.state = Some(state);
        self.error = None;
        cues_changed
    }

    pub fn devices(&self) -> &[String] {
        self.state.as_ref().map_or(&[], |state| &state.devices)
    }

    /// The action waiting for a control to be pressed.
    pub fn learning(&self) -> Option<ControllerAction> {
        self.state
            .as_ref()?
            .learning
            .and_then(|action| ControllerAction::try_from(action).ok())
    }

    pub fn control_for(&self, action: ControllerAction) -> Option<&Control> {
        self.state
            .as_ref()?
            .bindings
            .iter()
            .find(|binding| binding.action() == action)
            .and_then(|binding| binding.control.as_ref())
    }

    /// The latest message from a controller, as a control and a value.
    pub fn last_input(&self) -> Option<(&Control, u32)> {
        let input = self.state.as_ref()?.last_input.as_ref()?;
        Some((input.control.as_ref()?, input.value))
    }

    /// Why the controllers can't be used, as the backend or the connection to it says.
    pub fn problem(&self) -> Option<&str> {
        self.error
            .as_deref()
            .or_else(|| self.state.as_ref()?.error.as_deref())
    }
}

/// How a control is told to a person: the kind decides which text.
pub fn is_note(control: &Control) -> bool {
    control.kind() == ControlKind::Note
}

#[cfg(test)]
mod tests {
    use open_engine_api::v1::{ControllerBinding, ControllerInput};

    use super::*;

    fn control(number: u32) -> Control {
        Control {
            kind: ControlKind::Cc.into(),
            channel: 2,
            number,
        }
    }

    #[test]
    fn the_pads_are_told_from_the_other_actions() {
        assert_eq!(pad_of(ControllerAction::Pad1), Some(0));
        assert_eq!(pad_of(ControllerAction::Pad8), Some(7));
        assert_eq!(pad_of(ControllerAction::Next), None);
        assert_eq!(pad_of(ControllerAction::Unspecified), None);
        assert_eq!(ACTIONS.iter().filter_map(|a| pad_of(*a)).count(), 8);
    }

    #[test]
    fn nothing_is_known_before_the_backend_reports() {
        let controller = Controller::default();
        assert!(controller.devices().is_empty());
        assert_eq!(controller.learning(), None);
        assert_eq!(controller.control_for(ControllerAction::Next), None);
        assert!(controller.last_input().is_none() && controller.problem().is_none());
    }

    #[test]
    fn the_state_is_read_and_a_change_of_cues_is_told_once() {
        let mut controller = Controller::default();
        let state = ControllerState {
            enabled: true,
            devices: vec!["SC LIVE 4".into()],
            learning: Some(ControllerAction::Stop.into()),
            bindings: vec![ControllerBinding {
                action: ControllerAction::Next.into(),
                control: Some(control(9)),
            }],
            last_input: Some(ControllerInput {
                control: Some(control(9)),
                value: 127,
            }),
            error: Some("no sequencer".into()),
            cues_revision: 3,
            cues_track_id: 5,
        };
        // The first state is not a change.
        assert_eq!(controller.update(state.clone()), None);
        assert_eq!(controller.devices(), ["SC LIVE 4"]);
        assert_eq!(controller.learning(), Some(ControllerAction::Stop));
        assert_eq!(
            controller.control_for(ControllerAction::Next),
            Some(&control(9))
        );
        assert_eq!(controller.control_for(ControllerAction::Stop), None);
        assert_eq!(controller.last_input(), Some((&control(9), 127)));
        assert_eq!(controller.problem(), Some("no sequencer"));

        assert_eq!(controller.update(state.clone()), None);
        let changed = ControllerState {
            cues_revision: 4,
            cues_track_id: 8,
            ..state
        };
        assert_eq!(controller.update(changed), Some(8));

        // A failing connection is shown over what the backend said, until it reports again.
        controller.error = Some("gone".into());
        assert_eq!(controller.problem(), Some("gone"));
    }
}
