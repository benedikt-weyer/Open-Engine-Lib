//! Profiles that come with the program, for controllers whose messages are known. They are
//! not saved anywhere and can't be changed (a copy can).
//!
//! **Denon DJ SC LIVE 4:** the notes are the ones the Mixxx mapping for it sends and expects
//! (Jammes Luckett / Art Mechanix, `Denon-SC-Live-4-scripts.js` of mixxxdj/mixxx#17201, which
//! was tried on the hardware there, not here). In controller mode each side of the unit
//! talks on a MIDI channel of its own: the left side (decks 1 and 3) on channel 5 and the
//! right side (decks 2 and 4) on channel 6, with the same notes. Of the player's actions the
//! transport buttons have their counterparts: play/pause, cue (as stop), the two track skip
//! buttons, the two beat jump buttons (as seeking) and the eight performance pads, whatever
//! pad mode the unit is in. The top of the jog wheel scratches: touching it is note 0x21 and
//! its position is a 14-bit number on controllers 0x11 (high bits) and 0x31 (low bits), again
//! from the Mixxx mapping. That only reads the position, so how many steps make a turn of the
//! wheel (`JOG_STEPS_PER_TURN`) is taken from how that mapping scratches. The side of the wheel, which bends the pitch in
//! Mixxx, is not used.

use super::{Action, Control, Kind, Mapping, PADS};

const SC_LIVE_4_LEFT: &str = "SC LIVE 4 – left (decks 1, 3)";
const SC_LIVE_4_RIGHT: &str = "SC LIVE 4 – right (decks 2, 4)";

/// The names of the built-in profiles, in the order they are listed.
pub const NAMES: [&str; 2] = [SC_LIVE_4_LEFT, SC_LIVE_4_RIGHT];

pub fn is_built_in(name: &str) -> bool {
    NAMES.contains(&name)
}

/// The bindings of a built-in profile.
pub fn mapping(name: &str) -> Option<Mapping> {
    match name {
        SC_LIVE_4_LEFT => Some(sc_live_4(5)),
        SC_LIVE_4_RIGHT => Some(sc_live_4(6)),
        _ => None,
    }
}

/// The notes of one side of the unit, which sends on `channel`.
fn sc_live_4(channel: u8) -> Mapping {
    let note = |number| Control {
        kind: Kind::Note,
        channel,
        number,
    };
    let mut mapping = Mapping::default();
    mapping.bind(note(0x21), Action::JogTouch);
    mapping.bind(
        Control {
            kind: Kind::Cc,
            channel,
            number: 0x11,
        },
        Action::JogWheel,
    );
    for (action, number) in [
        (Action::Previous, 0x04),
        (Action::Next, 0x05),
        (Action::SeekBack, 0x06),
        (Action::SeekForward, 0x07),
        (Action::Stop, 0x09),
        (Action::PlayPause, 0x0A),
    ] {
        mapping.bind(note(number), action);
    }
    // The pads are notes 0x0F to 0x16, left to right, the top row first.
    for slot in 0..PADS {
        mapping.bind(note(0x0F + slot as u8), Action::Pad(slot));
    }
    mapping
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_built_in_profile_is_there_and_binds_valid_controls() {
        for name in NAMES {
            assert!(is_built_in(name));
            let mapping = mapping(name).unwrap();
            assert_eq!(mapping.bindings.len(), 6 + 2 + PADS, "{name}");
            // Nothing a hand-edited file would have cleaned up.
            assert_eq!(mapping.clone().sanitized(), mapping, "{name}");
        }
        assert!(!is_built_in("Default"));
        assert_eq!(mapping("Default"), None);
    }

    #[test]
    fn the_two_sides_of_the_sc_live_4_differ_only_in_their_channel() {
        let left = mapping(SC_LIVE_4_LEFT).unwrap();
        let right = mapping(SC_LIVE_4_RIGHT).unwrap();
        let control = |mapping: &Mapping, action| mapping.control_for(action).unwrap();
        assert_eq!(control(&left, Action::PlayPause).channel, 5);
        assert_eq!(control(&right, Action::PlayPause).channel, 6);
        assert_eq!(control(&left, Action::PlayPause).number, 0x0A);
        assert_eq!(control(&right, Action::Pad(7)).number, 0x16);
        for action in [Action::Next, Action::Pad(0), Action::SeekBack] {
            assert_eq!(
                control(&left, action).number,
                control(&right, action).number
            );
        }
    }

    #[test]
    fn the_jog_wheel_scratches_on_the_notes_of_its_side() {
        for (name, channel) in [(SC_LIVE_4_LEFT, 5), (SC_LIVE_4_RIGHT, 6)] {
            let mapping = mapping(name).unwrap();
            let touch = mapping.control_for(Action::JogTouch).unwrap();
            let wheel = mapping.control_for(Action::JogWheel).unwrap();
            assert_eq!(
                (touch.kind, touch.channel, touch.number),
                (Kind::Note, channel, 0x21)
            );
            assert_eq!(
                (wheel.kind, wheel.channel, wheel.number),
                (Kind::Cc, channel, 0x11)
            );
        }
    }
}
