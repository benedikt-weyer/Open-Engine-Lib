//! A DJ controller (or anything else that sends MIDI) used as a remote for the player:
//! which control does what, learning new bindings, and what the UI is told about it.
//!
//! Controllers differ, and some, like Denon DJ's SC LIVE 4, have no published list of the
//! messages their buttons send, so nothing is built in: the user picks an action and presses
//! a control ("MIDI learn"), and the pair is remembered. The bindings are state the user
//! builds up, so they live in their own file in the user state dir
//! (`<state dir>/open-engine-lib/controller.toml`), which can also be edited by hand.
//!
//! The hardware is the audio process's (`audio/midi.rs`); the actions are carried out by
//! `service/controller.rs`. This is the part in between that knows no hardware and no player.

use std::fmt;
use std::fs;
use std::io;
use std::path::PathBuf;
use std::sync::Mutex;

use open_engine_api::v1 as api;
use serde::{Deserialize, Serialize};
use tokio::sync::watch;

const APP_DIR: &str = "open-engine-lib";
const FILE: &str = "controller.toml";
/// How many cue pads a track has.
pub const PADS: usize = crate::cues::SLOTS;

/// What a control of the controller does to the player.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Action {
    PlayPause,
    Stop,
    Next,
    Previous,
    SeekBack,
    SeekForward,
    /// Cue pad `0..PADS`: jumps to its cue, or sets one at the playhead.
    Pad(usize),
}

impl Action {
    /// The name in the file.
    fn key(self) -> String {
        match self {
            Action::PlayPause => "play_pause".into(),
            Action::Stop => "stop".into(),
            Action::Next => "next".into(),
            Action::Previous => "previous".into(),
            Action::SeekBack => "seek_back".into(),
            Action::SeekForward => "seek_forward".into(),
            Action::Pad(slot) => format!("pad_{}", slot + 1),
        }
    }

    fn from_key(key: &str) -> Option<Action> {
        Some(match key {
            "play_pause" => Action::PlayPause,
            "stop" => Action::Stop,
            "next" => Action::Next,
            "previous" => Action::Previous,
            "seek_back" => Action::SeekBack,
            "seek_forward" => Action::SeekForward,
            _ => {
                let number: usize = key.strip_prefix("pad_")?.parse().ok()?;
                Action::Pad(number.checked_sub(1).filter(|slot| *slot < PADS)?)
            }
        })
    }

    pub fn from_api(action: api::ControllerAction) -> Option<Action> {
        use api::ControllerAction as A;
        Some(match action {
            A::Unspecified => return None,
            A::PlayPause => Action::PlayPause,
            A::Stop => Action::Stop,
            A::Previous => Action::Previous,
            A::Next => Action::Next,
            A::SeekBack => Action::SeekBack,
            A::SeekForward => Action::SeekForward,
            pad => Action::Pad((pad as usize).checked_sub(A::Pad1 as usize)?),
        })
        .filter(|action| !matches!(action, Action::Pad(slot) if *slot >= PADS))
    }

    fn to_api(self) -> api::ControllerAction {
        use api::ControllerAction as A;
        match self {
            Action::PlayPause => A::PlayPause,
            Action::Stop => A::Stop,
            Action::Previous => A::Previous,
            Action::Next => A::Next,
            Action::SeekBack => A::SeekBack,
            Action::SeekForward => A::SeekForward,
            Action::Pad(slot) => {
                A::try_from(A::Pad1 as i32 + slot as i32).unwrap_or(A::Unspecified)
            }
        }
    }
}

impl Serialize for Action {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.key())
    }
}

impl<'de> Deserialize<'de> for Action {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let key = String::deserialize(deserializer)?;
        Action::from_key(&key)
            .ok_or_else(|| serde::de::Error::custom(format!("unknown action `{key}`")))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    /// A key or button: note on and note off.
    Note,
    /// A control change: a button or a knob.
    Cc,
}

/// Which control of the controller a message comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Control {
    pub kind: Kind,
    /// The MIDI channel, 1 to 16 as controller manuals number them.
    pub channel: u8,
    /// The note or controller number, 0 to 127.
    pub number: u8,
}

impl Control {
    fn is_valid(&self) -> bool {
        (1..=16).contains(&self.channel) && self.number < 128
    }

    pub fn from_api(control: &api::Control) -> Option<Control> {
        let control = Control {
            kind: match control.kind() {
                api::ControlKind::Note => Kind::Note,
                api::ControlKind::Cc => Kind::Cc,
                api::ControlKind::Unspecified => return None,
            },
            channel: u8::try_from(control.channel).ok()?,
            number: u8::try_from(control.number).ok()?,
        };
        control.is_valid().then_some(control)
    }

    fn to_api(self) -> api::Control {
        api::Control {
            kind: match self.kind {
                Kind::Note => api::ControlKind::Note,
                Kind::Cc => api::ControlKind::Cc,
            }
            .into(),
            channel: self.channel.into(),
            number: self.number.into(),
        }
    }
}

/// A message from the controller.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Input {
    pub control: Control,
    /// 0 to 127. A button is down when it is above 0.
    pub value: u8,
}

impl Input {
    pub fn from_api(input: &api::ControllerInput) -> Option<Input> {
        Some(Input {
            control: Control::from_api(input.control.as_ref()?)?,
            value: u8::try_from(input.value)
                .ok()
                .filter(|value| *value < 128)?,
        })
    }

    fn pressed(&self) -> bool {
        self.value > 0
    }

    fn to_api(self) -> api::ControllerInput {
        api::ControllerInput {
            control: Some(self.control.to_api()),
            value: self.value.into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Binding {
    control: Control,
    action: Action,
}

/// Which control does what. A control does one thing, and an action has one control.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Mapping {
    #[serde(rename = "binding")]
    bindings: Vec<Binding>,
}

impl Mapping {
    pub fn action_for(&self, control: Control) -> Option<Action> {
        self.bindings
            .iter()
            .find(|binding| binding.control == control)
            .map(|binding| binding.action)
    }

    pub fn control_for(&self, action: Action) -> Option<Control> {
        self.bindings
            .iter()
            .find(|binding| binding.action == action)
            .map(|binding| binding.control)
    }

    /// Makes `control` do `action`, taking both away from what they did before.
    pub fn bind(&mut self, control: Control, action: Action) {
        self.bindings
            .retain(|binding| binding.control != control && binding.action != action);
        self.bindings.push(Binding { control, action });
    }

    pub fn unbind(&mut self, action: Action) {
        self.bindings.retain(|binding| binding.action != action);
    }

    /// Leaves out what a hand-edited file got wrong: controls that can't exist and a
    /// control or an action that is listed twice (the first one counts).
    fn sanitized(self) -> Self {
        let mut clean = Mapping::default();
        for binding in self.bindings {
            if binding.control.is_valid()
                && clean.action_for(binding.control).is_none()
                && clean.control_for(binding.action).is_none()
            {
                clean.bindings.push(binding);
            }
        }
        clean
    }
}

impl fmt::Display for Control {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let kind = match self.kind {
            Kind::Note => "note",
            Kind::Cc => "CC",
        };
        write!(f, "{kind} {} (channel {})", self.number, self.channel)
    }
}

#[derive(Default)]
struct Inner {
    mapping: Mapping,
    enabled: bool,
    devices: Vec<String>,
    learning: Option<Action>,
    last: Option<Input>,
    error: Option<String>,
    cues_revision: u64,
    cues_track: u32,
}

/// The controller: what its controls do, what is connected, what it said last, and whether the
/// next press is to be learned. Shared between the task that listens and the calls of the UI.
pub struct Controller {
    file: PathBuf,
    inner: Mutex<Inner>,
    /// Counts the changes, so that whoever watches knows when to look again.
    changed: watch::Sender<u64>,
}

impl Controller {
    pub fn discover() -> Self {
        let file = dirs::state_dir()
            .or_else(dirs::data_local_dir)
            .unwrap_or_default()
            .join(APP_DIR)
            .join(FILE);
        Self::load(file)
    }

    /// Reads the file. A missing or unreadable one gives no bindings: they are only a
    /// convenience, so they must never stop the backend from starting.
    pub fn load(file: PathBuf) -> Self {
        let mapping = fs::read_to_string(&file)
            .ok()
            .and_then(|text| toml::from_str::<Mapping>(&text).ok())
            .map(Mapping::sanitized)
            .unwrap_or_default();
        Self {
            file,
            inner: Mutex::new(Inner {
                mapping,
                ..Inner::default()
            }),
            changed: watch::channel(0).0,
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().expect("controller lock")
    }

    /// Changes the state and lets the watchers know.
    fn change<T>(&self, change: impl FnOnce(&mut Inner) -> T) -> T {
        let result = change(&mut self.lock());
        self.changed.send_modify(|count| *count += 1);
        result
    }

    pub fn subscribe(&self) -> watch::Receiver<u64> {
        self.changed.subscribe()
    }

    pub fn snapshot(&self) -> api::ControllerState {
        let inner = self.lock();
        let mut bindings: Vec<_> = inner
            .mapping
            .bindings
            .iter()
            .map(|binding| api::ControllerBinding {
                action: binding.action.to_api().into(),
                control: Some(binding.control.to_api()),
            })
            .collect();
        bindings.sort_by_key(|binding| binding.action);
        api::ControllerState {
            enabled: inner.enabled,
            devices: inner.devices.clone(),
            learning: inner.learning.map(|action| action.to_api().into()),
            bindings,
            last_input: inner.last.map(Input::to_api),
            error: inner.error.clone(),
            cues_revision: inner.cues_revision,
            cues_track_id: inner.cues_track,
        }
    }

    /// Turning it off forgets what was heard of the controllers.
    pub fn set_enabled(&self, enabled: bool) {
        self.change(|inner| {
            inner.enabled = enabled;
            inner.learning = None;
            if !enabled {
                inner.devices.clear();
                inner.last = None;
                inner.error = None;
            }
        });
    }

    pub fn set_devices(&self, devices: Vec<String>) {
        self.change(|inner| {
            inner.devices = devices;
            inner.error = None;
        });
    }

    pub fn unavailable(&self, error: String) {
        self.change(|inner| {
            inner.devices.clear();
            inner.error = Some(error);
        });
    }

    /// Takes a message from the controller. While an action is waiting for its control, the
    /// first press is bound to it and nothing else happens; otherwise a press says which
    /// action to carry out.
    pub fn input(&self, input: Input) -> Option<Action> {
        self.change(|inner| {
            inner.last = Some(input);
            if let Some(action) = inner.learning {
                if input.pressed() {
                    inner.learning = None;
                    inner.mapping.bind(input.control, action);
                    inner.error = self
                        .save(&inner.mapping)
                        .err()
                        .map(|error| error.to_string());
                }
                return None;
            }
            input
                .pressed()
                .then(|| inner.mapping.action_for(input.control))
                .flatten()
        })
    }

    pub fn learn(&self, action: Action) {
        self.change(|inner| inner.learning = Some(action));
    }

    pub fn cancel_learning(&self) {
        self.change(|inner| inner.learning = None);
    }

    pub fn clear(&self, action: Action) {
        self.change(|inner| {
            if inner.learning == Some(action) {
                inner.learning = None;
            }
            inner.mapping.unbind(action);
            inner.error = self
                .save(&inner.mapping)
                .err()
                .map(|error| error.to_string());
        });
    }

    /// A pad changed the cues of `track`: whoever shows them has to fetch them again.
    pub fn cues_changed(&self, track: u32) {
        self.change(|inner| {
            inner.cues_revision += 1;
            inner.cues_track = track;
        });
    }

    fn save(&self, mapping: &Mapping) -> io::Result<()> {
        if let Some(parent) = self.file.parent() {
            fs::create_dir_all(parent)?;
        }
        let text = toml::to_string(mapping).map_err(io::Error::other)?;
        // Write beside the file and move it into place, so a crash can't leave half of it.
        let temporary = self.file.with_extension("toml.tmp");
        fs::write(&temporary, text)?;
        fs::rename(&temporary, &self.file)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;

    fn note(number: u8) -> Control {
        Control {
            kind: Kind::Note,
            channel: 1,
            number,
        }
    }

    fn cc(number: u8) -> Control {
        Control {
            kind: Kind::Cc,
            channel: 2,
            number,
        }
    }

    fn press(control: Control) -> Input {
        Input {
            control,
            value: 100,
        }
    }

    fn release(control: Control) -> Input {
        Input { control, value: 0 }
    }

    /// A file in a folder of its own that is taken away again.
    struct TempFile(PathBuf);

    impl TempFile {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "open-engine-lib-controller-{name}-{}",
                std::process::id()
            ));
            let _ = fs::remove_dir_all(&dir);
            Self(dir.join("state").join(FILE))
        }
    }

    impl Drop for TempFile {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(self.0.parent().unwrap().parent().unwrap());
        }
    }

    fn mapping_of(controller: &Controller) -> Mapping {
        controller.lock().mapping.clone()
    }

    #[test]
    fn every_action_has_its_own_name_and_protocol_value_that_read_back() {
        let all: Vec<Action> = [
            Action::PlayPause,
            Action::Stop,
            Action::Previous,
            Action::Next,
            Action::SeekBack,
            Action::SeekForward,
        ]
        .into_iter()
        .chain((0..PADS).map(Action::Pad))
        .collect();
        for action in &all {
            assert_eq!(Action::from_key(&action.key()), Some(*action));
            assert_eq!(Action::from_api(action.to_api()), Some(*action));
        }
        let keys: HashSet<String> = all.iter().map(|action| action.key()).collect();
        assert_eq!(keys.len(), all.len());
        assert_eq!(Action::from_key("pad_0"), None);
        assert_eq!(Action::from_key("pad_9"), None);
        assert_eq!(Action::from_key("fly"), None);
        assert_eq!(Action::from_api(api::ControllerAction::Unspecified), None);
    }

    #[test]
    fn controls_from_the_protocol_are_checked() {
        let control = |kind, channel, number| api::Control {
            kind: kind as i32,
            channel,
            number,
        };
        assert_eq!(
            Control::from_api(&control(api::ControlKind::Note, 1, 36)),
            Some(note(36))
        );
        assert_eq!(
            Control::from_api(&control(api::ControlKind::Cc, 0, 36)),
            None
        );
        assert_eq!(
            Control::from_api(&control(api::ControlKind::Cc, 17, 36)),
            None
        );
        assert_eq!(
            Control::from_api(&control(api::ControlKind::Cc, 1, 128)),
            None
        );
        assert_eq!(
            Control::from_api(&control(api::ControlKind::Unspecified, 1, 1)),
            None
        );
    }

    #[test]
    fn binding_replaces_what_the_control_or_the_action_had() {
        let mut mapping = Mapping::default();
        mapping.bind(note(1), Action::PlayPause);
        mapping.bind(note(2), Action::Stop);
        // The same control for another action takes it over...
        mapping.bind(note(1), Action::Next);
        assert_eq!(mapping.action_for(note(1)), Some(Action::Next));
        assert_eq!(mapping.control_for(Action::PlayPause), None);
        // ...and another control for the same action moves it.
        mapping.bind(note(3), Action::Next);
        assert_eq!(mapping.action_for(note(1)), None);
        assert_eq!(mapping.control_for(Action::Next), Some(note(3)));
        assert_eq!(mapping.action_for(note(2)), Some(Action::Stop));
        mapping.unbind(Action::Stop);
        assert_eq!(mapping.action_for(note(2)), None);
    }

    #[test]
    fn pressing_a_bound_control_gives_its_action_and_releasing_gives_nothing() {
        let file = TempFile::new("press");
        let controller = Controller::load(file.0.clone());
        controller.lock().mapping.bind(note(36), Action::PlayPause);
        assert_eq!(controller.input(press(note(36))), Some(Action::PlayPause));
        assert_eq!(controller.input(release(note(36))), None);
        assert_eq!(controller.input(press(note(37))), None);
        assert_eq!(controller.lock().last, Some(press(note(37))));
    }

    #[test]
    fn learning_binds_the_first_press_and_does_nothing_else() {
        let file = TempFile::new("learn");
        let controller = Controller::load(file.0.clone());
        controller.lock().mapping.bind(note(36), Action::PlayPause);
        controller.learn(Action::Stop);
        // A release is not a press, and a bound control doesn't act while learning.
        assert_eq!(controller.input(release(note(40))), None);
        assert_eq!(controller.lock().learning, Some(Action::Stop));
        assert_eq!(controller.input(press(note(36))), None);
        let inner = controller.lock();
        assert_eq!(inner.learning, None);
        assert_eq!(inner.mapping.control_for(Action::Stop), Some(note(36)));
        assert_eq!(inner.mapping.control_for(Action::PlayPause), None);
        assert_eq!(inner.error, None);
    }

    #[test]
    fn learning_can_be_cancelled_and_clearing_a_binding_ends_its_learning() {
        let file = TempFile::new("cancel");
        let controller = Controller::load(file.0.clone());
        controller.learn(Action::Stop);
        controller.cancel_learning();
        assert_eq!(controller.input(press(note(5))), None);
        assert_eq!(mapping_of(&controller), Mapping::default());
        controller.learn(Action::Stop);
        controller.clear(Action::Stop);
        assert_eq!(controller.lock().learning, None);
    }

    #[test]
    fn bindings_are_saved_and_read_back() {
        let file = TempFile::new("save");
        let controller = Controller::load(file.0.clone());
        controller.learn(Action::Pad(2));
        controller.input(press(note(51)));
        controller.learn(Action::SeekBack);
        controller.input(press(cc(7)));
        let written = fs::read_to_string(&file.0).unwrap();
        assert!(
            written.contains("pad_3") && written.contains("seek_back"),
            "{written}"
        );
        let reloaded = Controller::load(file.0.clone());
        assert_eq!(mapping_of(&reloaded), mapping_of(&controller));
        controller.clear(Action::Pad(2));
        let reloaded = Controller::load(file.0.clone());
        assert_eq!(mapping_of(&reloaded).control_for(Action::Pad(2)), None);
        assert_eq!(
            mapping_of(&reloaded).control_for(Action::SeekBack),
            Some(cc(7))
        );
    }

    #[test]
    fn the_state_tells_the_bindings_the_last_message_and_the_changes_to_cues() {
        let file = TempFile::new("state");
        let controller = Controller::load(file.0.clone());
        let watcher = controller.subscribe();
        assert!(!watcher.has_changed().unwrap());
        controller.set_enabled(true);
        controller.set_devices(vec!["SC LIVE 4".into()]);
        controller.learn(Action::Next);
        controller.input(press(cc(9)));
        controller.cues_changed(42);
        assert!(watcher.has_changed().unwrap());

        let state = controller.snapshot();
        assert!(state.enabled);
        assert_eq!(state.devices, ["SC LIVE 4"]);
        assert_eq!(state.learning, None);
        assert_eq!(state.bindings.len(), 1);
        assert_eq!(state.bindings[0].action(), api::ControllerAction::Next);
        assert_eq!(state.bindings[0].control, Some(cc(9).to_api()));
        assert_eq!(state.last_input, Some(press(cc(9)).to_api()));
        assert_eq!((state.cues_revision, state.cues_track_id), (1, 42));

        // Turning it off forgets the devices and the last message, not the bindings.
        controller.set_enabled(false);
        let state = controller.snapshot();
        assert!(!state.enabled && state.devices.is_empty() && state.last_input.is_none());
        assert_eq!(state.bindings.len(), 1);
    }

    #[test]
    fn a_hand_edited_file_is_cleaned_up() {
        let text = r#"
            [[binding]]
            control = { kind = "note", channel = 1, number = 5 }
            action = "play_pause"
            [[binding]]
            control = { kind = "note", channel = 1, number = 5 }
            action = "stop"
            [[binding]]
            control = { kind = "cc", channel = 17, number = 5 }
            action = "next"
            [[binding]]
            control = { kind = "cc", channel = 1, number = 200 }
            action = "previous"
        "#;
        let mapping = toml::from_str::<Mapping>(text).unwrap().sanitized();
        assert_eq!(mapping.bindings.len(), 1);
        assert_eq!(mapping.action_for(note(5)), Some(Action::PlayPause));
        // An action that doesn't exist makes the whole file unreadable, which means no bindings.
        assert!(toml::from_str::<Mapping>(&text.replace("stop", "dance")).is_err());
    }

    #[test]
    fn an_unreadable_file_means_no_bindings() {
        let file = TempFile::new("garbage");
        fs::create_dir_all(file.0.parent().unwrap()).unwrap();
        fs::write(&file.0, "this is not toml [").unwrap();
        assert_eq!(
            mapping_of(&Controller::load(file.0.clone())),
            Mapping::default()
        );
    }
}
