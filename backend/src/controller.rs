//! A DJ controller (or anything else that sends MIDI) used as a remote for the player:
//! which control does what, learning new bindings, and what the UI is told about it.
//!
//! Controllers differ, and some, like Denon DJ's SC LIVE 4, have no published list of the
//! messages their buttons send, so nothing is built in: the user picks an action and presses
//! a control ("MIDI learn"), and the pair is remembered. The bindings are state the user
//! builds up, so they live in their own file in the user state dir
//! (`<state dir>/open-engine-lib/controller.toml`), which can also be edited by hand.
//!
//! The bindings are kept in profiles with a name, so that several controllers (or several
//! ways of using one) can each have theirs and the user can switch between them. A few
//! profiles for controllers whose messages are known come with the program (`presets`); they
//! can't be changed, but a copy of one can.
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

mod presets;

const APP_DIR: &str = "open-engine-lib";
const FILE: &str = "controller.toml";
/// How many cue pads a track has.
pub const PADS: usize = crate::cues::SLOTS;
/// The name of the profile a first start (or a file from before there were profiles) gets.
const DEFAULT_PROFILE: &str = "Default";
/// The longest name a profile can have, in characters.
const NAME_MAX_CHARS: usize = 60;

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
    /// The top of the jog wheel is touched (note on) or let go (note off): holds the track
    /// for scratching, and lets it go.
    JogTouch,
    /// The position of the jog wheel, which a controller sends as a 14-bit number: its high
    /// 7 bits on the control bound here (a control change) and the low 7 bits on the one
    /// `JOG_LOW_OFFSET` above it. Scratches while the top is touched.
    JogWheel,
}

/// How far above the control with the high bits of a 14-bit value the one with the low bits
/// is (the MIDI convention: controllers 0 to 31 and their low halves 32 to 63).
const JOG_LOW_OFFSET: u8 = 32;
/// The positions a jog wheel reports run from 0 to this and wrap around: 14 bits.
const JOG_RANGE: i32 = 1 << 14;
/// How many steps of that position make a turn of the wheel. Not the whole range: the Mixxx
/// mapping for the SC LIVE 4 (which was tried on the unit) hands the steps to its scratching
/// as 1000 to a turn, and the whole range (16384 to a turn) was found far too slow here.
const JOG_STEPS_PER_TURN: f64 = 1000.0;
/// How much of the track a turn of the wheel scratches, in seconds: a record at 33⅓ turns
/// a minute takes 1.8 seconds for a turn.
pub const JOG_SECONDS_PER_TURN: f64 = 1.8;

/// How far the jog wheel went from one position it reported to the next, in seconds of the
/// track: the short way round, so a turn past the end of the scale is not a jump the other
/// way.
pub fn jog_seconds(from: u16, to: u16) -> f64 {
    let steps = i32::from(to) - i32::from(from);
    let wrapped = (steps + JOG_RANGE / 2).rem_euclid(JOG_RANGE) - JOG_RANGE / 2;
    f64::from(wrapped) / JOG_STEPS_PER_TURN * JOG_SECONDS_PER_TURN
}

/// What a message from the controller asks the player to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event {
    /// A press of the control bound to the action.
    Press(Action),
    /// The top of the jog wheel went down (true) or up.
    JogTouch(bool),
    /// The jog wheel is at this position (14 bits).
    JogPosition(u16),
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
            Action::JogTouch => "jog_touch".into(),
            Action::JogWheel => "jog_wheel".into(),
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
            "jog_touch" => Action::JogTouch,
            "jog_wheel" => Action::JogWheel,
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
            A::JogTouch => Action::JogTouch,
            A::JogWheel => Action::JogWheel,
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
            Action::JogTouch => A::JogTouch,
            Action::JogWheel => A::JogWheel,
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

/// Why a profile could not be made, changed or selected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProfileError {
    EmptyName,
    NameTooLong,
    /// Another profile has the name (names are compared without regard to case).
    NameTaken,
    /// There is no profile of that name.
    Unknown,
    /// Built-in profiles can't be changed, renamed or deleted.
    BuiltIn,
}

impl fmt::Display for ProfileError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            ProfileError::EmptyName => "a profile needs a name",
            ProfileError::NameTooLong => "the name of the profile is too long",
            ProfileError::NameTaken => "another profile has that name",
            ProfileError::Unknown => "no such profile",
            ProfileError::BuiltIn => {
                "a built-in profile can't be changed (make a copy of it to do that)"
            }
        })
    }
}

/// The control a jog wheel is bound by when `control` is one of the two it sends its
/// position on: the one with the high bits.
fn jog_high_control(control: Control) -> Control {
    if control.kind == Kind::Cc && (JOG_LOW_OFFSET..2 * JOG_LOW_OFFSET).contains(&control.number) {
        Control {
            number: control.number - JOG_LOW_OFFSET,
            ..control
        }
    } else {
        control
    }
}

/// A name without the spaces around it, if it is one a profile can have.
fn clean_name(name: &str) -> Result<String, ProfileError> {
    let name = name.trim();
    if name.is_empty() {
        Err(ProfileError::EmptyName)
    } else if name.chars().count() > NAME_MAX_CHARS {
        Err(ProfileError::NameTooLong)
    } else {
        Ok(name.to_owned())
    }
}

fn same_name(a: &str, b: &str) -> bool {
    a.to_lowercase() == b.to_lowercase()
}

/// The user's own set of bindings, with the name they know it by.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Profile {
    name: String,
    #[serde(flatten)]
    mapping: Mapping,
}

/// What the file holds.
#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(default)]
struct Store {
    /// The name of the profile in use.
    active: Option<String>,
    #[serde(rename = "profile")]
    profiles: Vec<Profile>,
    /// The bindings of before there were profiles. They are read, and become the profile
    /// `DEFAULT_PROFILE`, but never written.
    #[serde(rename = "binding", skip_serializing)]
    legacy: Vec<Binding>,
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
    /// The user's profiles (the built-in ones are in `presets`).
    profiles: Vec<Profile>,
    /// The name of the profile in use.
    active: String,
    /// The bindings of the profile in use. For the user's profile they are copied back into
    /// `profiles` when saved.
    mapping: Mapping,
    enabled: bool,
    devices: Vec<String>,
    learning: Option<Action>,
    last: Option<Input>,
    error: Option<String>,
    cues_revision: u64,
    cues_track: u32,
    /// The high 7 bits of the jog wheel's position, which the low 7 bits complete.
    jog_high: Option<u8>,
}

impl Inner {
    /// Reads what the file held. It never ends up with no profile in use: a first start, a
    /// file from before there were profiles and one that names a profile that isn't there
    /// all get one.
    fn from_store(store: Store) -> Self {
        let mut inner = Inner::default();
        for profile in store.profiles {
            let name = clean_name(&profile.name).ok();
            if let Some(name) = name
                && !inner.name_taken(&name, None)
            {
                inner.profiles.push(Profile {
                    name,
                    mapping: profile.mapping.sanitized(),
                });
            }
        }
        if store.active.is_none() && inner.profiles.is_empty() {
            inner.profiles.push(Profile {
                name: DEFAULT_PROFILE.to_owned(),
                mapping: Mapping {
                    bindings: store.legacy,
                }
                .sanitized(),
            });
        }
        let first = inner
            .profiles
            .first()
            .map(|profile| profile.name.clone())
            .unwrap_or_else(|| presets::NAMES[0].to_owned());
        let active = store
            .active
            .filter(|name| inner.profile_mapping(name).is_some())
            .unwrap_or(first);
        inner.mapping = inner.profile_mapping(&active).unwrap_or_default();
        inner.active = active;
        inner
    }

    fn to_store(&self) -> Store {
        Store {
            active: Some(self.active.clone()),
            profiles: self.profiles.clone(),
            legacy: Vec::new(),
        }
    }

    fn name_taken(&self, name: &str, except: Option<&str>) -> bool {
        presets::NAMES
            .iter()
            .copied()
            .chain(self.profiles.iter().map(|profile| profile.name.as_str()))
            .filter(|other| Some(*other) != except)
            .any(|other| same_name(other, name))
    }

    /// The bindings of the profile of that name, a built-in or the user's.
    fn profile_mapping(&self, name: &str) -> Option<Mapping> {
        presets::mapping(name).or_else(|| {
            self.profiles
                .iter()
                .find(|profile| profile.name == name)
                .map(|profile| profile.mapping.clone())
        })
    }

    /// The jog wheel's position, when `input` is the low half of it: the high half was heard
    /// just before.
    fn jog_position(&self, input: Input) -> Option<Event> {
        let wheel = self.mapping.control_for(Action::JogWheel)?;
        let low = Control {
            number: wheel.number.checked_add(JOG_LOW_OFFSET)?,
            ..wheel
        };
        if wheel.kind != Kind::Cc || input.control != low {
            return None;
        }
        let high = self.jog_high?;
        Some(Event::JogPosition(
            (u16::from(high) << 7) | u16::from(input.value),
        ))
    }

    /// Checks that the profile in use may be changed.
    fn editable(&self) -> Result<(), ProfileError> {
        if presets::is_built_in(&self.active) {
            Err(ProfileError::BuiltIn)
        } else {
            Ok(())
        }
    }

    /// Puts the working bindings back into the user's profile in use.
    fn store_mapping(&mut self) {
        if let Some(profile) = self
            .profiles
            .iter_mut()
            .find(|profile| profile.name == self.active)
        {
            profile.mapping = self.mapping.clone();
        }
    }

    /// Makes the profile of that name the one in use, and stops learning.
    fn select(&mut self, name: &str) -> Result<(), ProfileError> {
        let mapping = self.profile_mapping(name).ok_or(ProfileError::Unknown)?;
        self.store_mapping();
        self.active = name.to_owned();
        self.mapping = mapping;
        self.learning = None;
        Ok(())
    }

    /// Finds a user's profile to change. (A built-in profile is an error of its own kind.)
    fn user_profile(&self, name: &str) -> Result<usize, ProfileError> {
        match self
            .profiles
            .iter()
            .position(|profile| profile.name == name)
        {
            Some(index) => Ok(index),
            None if presets::is_built_in(name) => Err(ProfileError::BuiltIn),
            None => Err(ProfileError::Unknown),
        }
    }
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

    /// Reads the file. A missing or unreadable one gives an empty profile: they are only a
    /// convenience, so they must never stop the backend from starting.
    pub fn load(file: PathBuf) -> Self {
        let store = fs::read_to_string(&file)
            .ok()
            .and_then(|text| toml::from_str::<Store>(&text).ok())
            .unwrap_or_default();
        Self {
            file,
            inner: Mutex::new(Inner::from_store(store)),
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

    /// Like `change`, for a change that can be refused: the watchers are only told if it
    /// was made.
    fn try_change<T>(
        &self,
        change: impl FnOnce(&mut Inner) -> Result<T, ProfileError>,
    ) -> Result<T, ProfileError> {
        let result = change(&mut self.lock())?;
        self.changed.send_modify(|count| *count += 1);
        Ok(result)
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
        let profiles = presets::NAMES
            .iter()
            .map(|name| api::ControllerProfile {
                name: (*name).to_owned(),
                built_in: true,
            })
            .chain(inner.profiles.iter().map(|profile| api::ControllerProfile {
                name: profile.name.clone(),
                built_in: false,
            }))
            .collect();
        api::ControllerState {
            enabled: inner.enabled,
            devices: inner.devices.clone(),
            learning: inner.learning.map(|action| action.to_api().into()),
            bindings,
            last_input: inner.last.map(Input::to_api),
            error: inner.error.clone(),
            cues_revision: inner.cues_revision,
            cues_track_id: inner.cues_track,
            profiles,
            active_profile: inner.active.clone(),
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
    /// action to carry out, and the jog wheel's touch and position are told as they come.
    pub fn input(&self, input: Input) -> Option<Event> {
        self.change(|inner| {
            inner.last = Some(input);
            if let Some(action) = inner.learning {
                if input.pressed() {
                    inner.learning = None;
                    // (A built-in profile is never learned into: learning can't start.)
                    if inner.editable().is_ok() {
                        // The wheel sends both halves of its position; either may be the
                        // first heard.
                        let control = if action == Action::JogWheel {
                            jog_high_control(input.control)
                        } else {
                            input.control
                        };
                        inner.mapping.bind(control, action);
                        self.persist(inner);
                    }
                }
                return None;
            }
            match inner.mapping.action_for(input.control) {
                Some(Action::JogTouch) => Some(Event::JogTouch(input.pressed())),
                Some(Action::JogWheel) => {
                    inner.jog_high = Some(input.value);
                    None
                }
                Some(action) => input.pressed().then_some(Event::Press(action)),
                None => inner.jog_position(input),
            }
        })
    }

    /// The next press is to be bound to `action`. Not in a built-in profile.
    pub fn learn(&self, action: Action) -> Result<(), ProfileError> {
        self.try_change(|inner| {
            inner.editable()?;
            inner.learning = Some(action);
            Ok(())
        })
    }

    pub fn cancel_learning(&self) {
        self.change(|inner| inner.learning = None);
    }

    pub fn clear(&self, action: Action) -> Result<(), ProfileError> {
        self.try_change(|inner| {
            inner.editable()?;
            if inner.learning == Some(action) {
                inner.learning = None;
            }
            inner.mapping.unbind(action);
            self.persist(inner);
            Ok(())
        })
    }

    /// Makes the profile of that name the one in use.
    pub fn select_profile(&self, name: &str) -> Result<(), ProfileError> {
        self.try_change(|inner| {
            inner.select(name)?;
            self.persist(inner);
            Ok(())
        })
    }

    /// Adds a profile, empty or with the bindings of another, and makes it the one in use.
    pub fn create_profile(&self, name: &str, copy_of: Option<&str>) -> Result<(), ProfileError> {
        let name = clean_name(name)?;
        self.try_change(|inner| {
            if inner.name_taken(&name, None) {
                return Err(ProfileError::NameTaken);
            }
            let mapping = match copy_of {
                Some(source) => inner.profile_mapping(source).ok_or(ProfileError::Unknown)?,
                None => Mapping::default(),
            };
            inner.store_mapping();
            inner.profiles.push(Profile {
                name: name.clone(),
                mapping,
            });
            inner.select(&name)?;
            self.persist(inner);
            Ok(())
        })
    }

    pub fn rename_profile(&self, name: &str, new_name: &str) -> Result<(), ProfileError> {
        let new_name = clean_name(new_name)?;
        self.try_change(|inner| {
            let index = inner.user_profile(name)?;
            if inner.name_taken(&new_name, Some(name)) {
                return Err(ProfileError::NameTaken);
            }
            inner.store_mapping();
            inner.profiles[index].name = new_name.clone();
            if inner.active == name {
                inner.active = new_name;
            }
            self.persist(inner);
            Ok(())
        })
    }

    /// Takes a profile away. If it was in use, the user's profile next to it takes its place,
    /// or the first built-in one if there is none.
    pub fn delete_profile(&self, name: &str) -> Result<(), ProfileError> {
        self.try_change(|inner| {
            let index = inner.user_profile(name)?;
            inner.store_mapping();
            inner.profiles.remove(index);
            if inner.active == name {
                let next = inner
                    .profiles
                    .get(index.min(inner.profiles.len().saturating_sub(1)))
                    .map_or(presets::NAMES[0], |profile| profile.name.as_str())
                    .to_owned();
                // (The profile in use is gone, so there is nothing to put back.)
                inner.active = String::new();
                inner.select(&next)?;
            }
            self.persist(inner);
            Ok(())
        })
    }

    /// A pad changed the cues of `track`: whoever shows them has to fetch them again.
    pub fn cues_changed(&self, track: u32) {
        self.change(|inner| {
            inner.cues_revision += 1;
            inner.cues_track = track;
        });
    }

    /// Saves the profiles, and remembers why that failed (the bindings are only a
    /// convenience, so it is shown, not made an error of the request).
    fn persist(&self, inner: &mut Inner) {
        inner.store_mapping();
        inner.error = self
            .save(&inner.to_store())
            .err()
            .map(|error| error.to_string());
    }

    fn save(&self, store: &Store) -> io::Result<()> {
        if let Some(parent) = self.file.parent() {
            fs::create_dir_all(parent)?;
        }
        let text = toml::to_string(store).map_err(io::Error::other)?;
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
        .chain([Action::JogTouch, Action::JogWheel])
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
        assert_eq!(
            controller.input(press(note(36))),
            Some(Event::Press(Action::PlayPause))
        );
        assert_eq!(controller.input(release(note(36))), None);
        assert_eq!(controller.input(press(note(37))), None);
        assert_eq!(controller.lock().last, Some(press(note(37))));
    }

    #[test]
    fn learning_binds_the_first_press_and_does_nothing_else() {
        let file = TempFile::new("learn");
        let controller = Controller::load(file.0.clone());
        controller.lock().mapping.bind(note(36), Action::PlayPause);
        controller.learn(Action::Stop).unwrap();
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
        controller.learn(Action::Stop).unwrap();
        controller.cancel_learning();
        assert_eq!(controller.input(press(note(5))), None);
        assert_eq!(mapping_of(&controller), Mapping::default());
        controller.learn(Action::Stop).unwrap();
        controller.clear(Action::Stop).unwrap();
        assert_eq!(controller.lock().learning, None);
    }

    #[test]
    fn bindings_are_saved_and_read_back() {
        let file = TempFile::new("save");
        let controller = Controller::load(file.0.clone());
        controller.learn(Action::Pad(2)).unwrap();
        controller.input(press(note(51)));
        controller.learn(Action::SeekBack).unwrap();
        controller.input(press(cc(7)));
        let written = fs::read_to_string(&file.0).unwrap();
        assert!(
            written.contains("pad_3") && written.contains("seek_back"),
            "{written}"
        );
        let reloaded = Controller::load(file.0.clone());
        assert_eq!(mapping_of(&reloaded), mapping_of(&controller));
        controller.clear(Action::Pad(2)).unwrap();
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
        controller.learn(Action::Next).unwrap();
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

    fn names(controller: &Controller) -> Vec<String> {
        controller
            .snapshot()
            .profiles
            .into_iter()
            .map(|profile| profile.name)
            .collect()
    }

    #[test]
    fn a_first_start_has_an_empty_profile_to_learn_into() {
        let file = TempFile::new("first");
        let controller = Controller::load(file.0.clone());
        let state = controller.snapshot();
        assert_eq!(state.active_profile, DEFAULT_PROFILE);
        assert_eq!(
            names(&controller),
            [presets::NAMES[0], presets::NAMES[1], DEFAULT_PROFILE]
        );
        assert!(state.profiles[0].built_in && !state.profiles[2].built_in);
        assert!(state.bindings.is_empty());
        controller.learn(Action::Next).unwrap();
    }

    #[test]
    fn bindings_from_before_there_were_profiles_become_the_default_profile() {
        let file = TempFile::new("legacy");
        fs::create_dir_all(file.0.parent().unwrap()).unwrap();
        fs::write(
            &file.0,
            r#"
                [[binding]]
                control = { kind = "note", channel = 1, number = 5 }
                action = "play_pause"
            "#,
        )
        .unwrap();
        let controller = Controller::load(file.0.clone());
        assert_eq!(controller.snapshot().active_profile, DEFAULT_PROFILE);
        assert_eq!(
            mapping_of(&controller).control_for(Action::PlayPause),
            Some(note(5))
        );
        // The first change writes the new format, and nothing is lost.
        controller.learn(Action::Stop).unwrap();
        controller.input(press(note(6)));
        let written = fs::read_to_string(&file.0).unwrap();
        assert!(written.contains("[[profile]]"), "{written}");
        let reloaded = Controller::load(file.0.clone());
        assert_eq!(mapping_of(&reloaded), mapping_of(&controller));
        assert_eq!(mapping_of(&reloaded).bindings.len(), 2);
    }

    #[test]
    fn profiles_are_made_switched_between_and_kept_apart() {
        let file = TempFile::new("profiles");
        let controller = Controller::load(file.0.clone());
        controller.learn(Action::Next).unwrap();
        controller.input(press(note(1)));

        // A new profile starts empty and is the one in use...
        controller.create_profile("  Booth  ", None).unwrap();
        assert_eq!(controller.snapshot().active_profile, "Booth");
        assert!(mapping_of(&controller).bindings.is_empty());
        controller.learn(Action::Stop).unwrap();
        controller.input(press(note(2)));
        // ...and what is learned in it stays out of the others.
        controller.select_profile(DEFAULT_PROFILE).unwrap();
        let mapping = mapping_of(&controller);
        assert_eq!(mapping.control_for(Action::Next), Some(note(1)));
        assert_eq!(mapping.control_for(Action::Stop), None);
        controller.select_profile("Booth").unwrap();
        assert_eq!(
            mapping_of(&controller).control_for(Action::Stop),
            Some(note(2))
        );

        // The one in use, and all the bindings, are still there after a restart.
        let reloaded = Controller::load(file.0.clone());
        assert_eq!(reloaded.snapshot().active_profile, "Booth");
        assert_eq!(mapping_of(&reloaded), mapping_of(&controller));
        assert_eq!(
            names(&reloaded),
            [
                presets::NAMES[0],
                presets::NAMES[1],
                DEFAULT_PROFILE,
                "Booth"
            ]
        );
    }

    #[test]
    fn selecting_a_profile_stops_learning_and_an_unknown_one_changes_nothing() {
        let file = TempFile::new("select");
        let controller = Controller::load(file.0.clone());
        controller.learn(Action::Next).unwrap();
        let watcher = controller.subscribe();
        assert_eq!(
            controller.select_profile("Nobody"),
            Err(ProfileError::Unknown)
        );
        assert!(!watcher.has_changed().unwrap(), "a refusal is no change");
        assert_eq!(
            controller.snapshot().learning,
            Some(api::ControllerAction::Next as i32)
        );
        controller.select_profile(presets::NAMES[0]).unwrap();
        assert_eq!(controller.snapshot().learning, None);
        assert!(watcher.has_changed().unwrap());
    }

    #[test]
    fn a_built_in_profile_can_be_used_and_copied_but_not_changed() {
        let file = TempFile::new("built-in");
        let controller = Controller::load(file.0.clone());
        let built_in = presets::NAMES[1];
        controller.select_profile(built_in).unwrap();
        assert_eq!(controller.snapshot().bindings.len(), 6 + 2 + PADS);
        // A press of its play button does what it should; learning is not possible.
        let play = presets::mapping(built_in)
            .unwrap()
            .control_for(Action::PlayPause)
            .unwrap();
        assert_eq!(
            controller.input(press(play)),
            Some(Event::Press(Action::PlayPause))
        );
        assert_eq!(controller.learn(Action::Next), Err(ProfileError::BuiltIn));
        assert_eq!(controller.clear(Action::Next), Err(ProfileError::BuiltIn));
        assert_eq!(
            controller.rename_profile(built_in, "Mine"),
            Err(ProfileError::BuiltIn)
        );
        assert_eq!(
            controller.delete_profile(built_in),
            Err(ProfileError::BuiltIn)
        );
        assert_eq!(controller.snapshot().bindings.len(), 6 + 2 + PADS);

        // A copy is the way to change it.
        controller.create_profile("My SC", Some(built_in)).unwrap();
        assert_eq!(controller.snapshot().bindings.len(), 6 + 2 + PADS);
        controller.clear(Action::Next).unwrap();
        assert_eq!(controller.snapshot().bindings.len(), 5 + 2 + PADS);
        controller.select_profile(built_in).unwrap();
        assert_eq!(controller.snapshot().bindings.len(), 6 + 2 + PADS);
    }

    #[test]
    fn names_are_checked_and_unique_without_regard_to_case() {
        let file = TempFile::new("names");
        let controller = Controller::load(file.0.clone());
        assert_eq!(
            controller.create_profile("  ", None),
            Err(ProfileError::EmptyName)
        );
        assert_eq!(
            controller.create_profile(&"x".repeat(NAME_MAX_CHARS + 1), None),
            Err(ProfileError::NameTooLong)
        );
        assert_eq!(
            controller.create_profile("default", None),
            Err(ProfileError::NameTaken)
        );
        // Built-in names are taken too.
        assert_eq!(
            controller.create_profile(&presets::NAMES[0].to_uppercase(), None),
            Err(ProfileError::NameTaken)
        );
        assert_eq!(
            controller.create_profile("Copy", Some("Nobody")),
            Err(ProfileError::Unknown)
        );
        assert_eq!(
            names(&controller).len(),
            presets::NAMES.len() + 1,
            "nothing was made"
        );

        controller.create_profile("Booth", None).unwrap();
        assert_eq!(
            controller.rename_profile("Booth", "DEFAULT"),
            Err(ProfileError::NameTaken)
        );
        assert_eq!(
            controller.rename_profile("Nobody", "Studio"),
            Err(ProfileError::Unknown)
        );
        // Only the case of its own name changes.
        controller.rename_profile("Booth", "booth").unwrap();
        controller.rename_profile("booth", "Studio").unwrap();
        // The profile in use keeps being in use, under its new name.
        assert_eq!(controller.snapshot().active_profile, "Studio");
        assert_eq!(
            Controller::load(file.0.clone()).snapshot().active_profile,
            "Studio"
        );
    }

    #[test]
    fn deleting_the_profile_in_use_selects_its_neighbour_or_a_built_in_one() {
        let file = TempFile::new("delete");
        let controller = Controller::load(file.0.clone());
        controller.create_profile("A", None).unwrap();
        controller.create_profile("B", None).unwrap();
        controller.create_profile("C", None).unwrap();

        // Another profile than the one in use: nothing else changes.
        controller.delete_profile("A").unwrap();
        assert_eq!(controller.snapshot().active_profile, "C");
        controller.select_profile("B").unwrap();
        controller.delete_profile("B").unwrap();
        assert_eq!(controller.snapshot().active_profile, "C");
        controller.delete_profile("C").unwrap();
        assert_eq!(controller.snapshot().active_profile, DEFAULT_PROFILE);

        // Deleting the last of the user's profiles leaves the built-in ones, and that sticks:
        // the empty profile of a first start does not come back.
        controller.delete_profile(DEFAULT_PROFILE).unwrap();
        assert_eq!(controller.snapshot().active_profile, presets::NAMES[0]);
        assert_eq!(controller.delete_profile("C"), Err(ProfileError::Unknown));
        let reloaded = Controller::load(file.0.clone());
        assert_eq!(reloaded.snapshot().active_profile, presets::NAMES[0]);
        assert_eq!(names(&reloaded), presets::NAMES);
    }

    #[test]
    fn a_hand_edited_file_with_profiles_is_cleaned_up() {
        let file = TempFile::new("profiles-edited");
        fs::create_dir_all(file.0.parent().unwrap()).unwrap();
        fs::write(
            &file.0,
            r#"
                active = "Nowhere"
                [[profile]]
                name = "One"
                [[profile.binding]]
                control = { kind = "note", channel = 1, number = 5 }
                action = "play_pause"
                [[profile.binding]]
                control = { kind = "note", channel = 1, number = 5 }
                action = "stop"
                [[profile]]
                name = "one"
                [[profile]]
                name = "   "
                [[profile]]
                name = "Two"
            "#,
        )
        .unwrap();
        let controller = Controller::load(file.0.clone());
        // A name nobody has falls back to the first profile; repeated names and empty ones
        // are left out, and so are bindings that clash.
        assert_eq!(controller.snapshot().active_profile, "One");
        assert_eq!(
            names(&controller),
            [presets::NAMES[0], presets::NAMES[1], "One", "Two"]
        );
        assert_eq!(mapping_of(&controller).bindings.len(), 1);
    }

    fn wheel(number: u8, value: u8) -> Input {
        Input {
            control: cc(number),
            value,
        }
    }

    #[test]
    fn the_jog_wheel_is_told_as_a_touch_and_a_position_of_14_bits() {
        let file = TempFile::new("jog");
        let controller = Controller::load(file.0.clone());
        {
            let mut inner = controller.lock();
            inner.mapping.bind(note(0x21), Action::JogTouch);
            inner.mapping.bind(cc(0x11), Action::JogWheel);
        }
        assert_eq!(
            controller.input(press(note(0x21))),
            Some(Event::JogTouch(true))
        );
        assert_eq!(
            controller.input(release(note(0x21))),
            Some(Event::JogTouch(false))
        );

        // The high bits alone say nothing yet; the low bits complete the position.
        assert_eq!(controller.input(wheel(0x11, 0x01)), None);
        assert_eq!(
            controller.input(wheel(0x31, 0x05)),
            Some(Event::JogPosition(0x85))
        );
        // The same high bits serve for the next low bits.
        assert_eq!(
            controller.input(wheel(0x31, 0x00)),
            Some(Event::JogPosition(0x80))
        );
        assert_eq!(controller.input(wheel(0x11, 0)), None);
        assert_eq!(
            controller.input(wheel(0x31, 0)),
            Some(Event::JogPosition(0)),
            "a position of 0 is a position"
        );
        // Another control with those numbers, or another channel, is not the wheel.
        assert_eq!(controller.input(wheel(0x32, 1)), None);
        let elsewhere = Control {
            channel: 3,
            ..cc(0x31)
        };
        assert_eq!(
            controller.input(Input {
                control: elsewhere,
                value: 1
            }),
            None
        );
    }

    #[test]
    fn the_low_half_of_the_wheel_is_nothing_before_the_high_half_is_heard() {
        let file = TempFile::new("jog-first");
        let controller = Controller::load(file.0.clone());
        controller.lock().mapping.bind(cc(0x11), Action::JogWheel);
        assert_eq!(controller.input(wheel(0x31, 9)), None);
    }

    #[test]
    fn the_jog_wheel_is_learned_by_either_half_of_its_position() {
        for turned in [0x11, 0x31] {
            let file = TempFile::new("jog-learn");
            let controller = Controller::load(file.0.clone());
            controller.learn(Action::JogWheel).unwrap();
            assert_eq!(controller.input(wheel(turned, 3)), None);
            assert_eq!(
                controller.lock().mapping.control_for(Action::JogWheel),
                Some(cc(0x11)),
                "turned {turned:#x}"
            );
        }
        // Another action is bound to the control as it is.
        let file = TempFile::new("jog-learn-other");
        let controller = Controller::load(file.0.clone());
        controller.learn(Action::Stop).unwrap();
        controller.input(wheel(0x31, 3));
        assert_eq!(
            controller.lock().mapping.control_for(Action::Stop),
            Some(cc(0x31))
        );
    }

    #[test]
    fn the_wheel_moves_the_short_way_round() {
        let turn = JOG_SECONDS_PER_TURN;
        assert_eq!(jog_seconds(100, 100), 0.0);
        // A thousand steps are a turn.
        assert!((jog_seconds(0, 1000) - turn).abs() < 1e-9);
        assert!((jog_seconds(1000, 250) + turn * 0.75).abs() < 1e-9);
        // Past the end of the scale, and back over it.
        assert!((jog_seconds(16_380, 4) - 8.0 / 1000.0 * turn).abs() < 1e-9);
        assert!((jog_seconds(4, 16_380) + 8.0 / 1000.0 * turn).abs() < 1e-9);
        // Half the range is as far as a move can be.
        assert!(jog_seconds(0, 8_191) > 0.0);
        assert!(jog_seconds(0, 8_193) < 0.0);
    }
}
