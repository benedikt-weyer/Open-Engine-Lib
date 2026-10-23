//! The MIDI controllers that are connected, and what they send.
//!
//! Only Linux is supported: `listen` subscribes to every hardware MIDI port of the ALSA
//! sequencer (so a controller can be used by other programs at the same time, which a raw
//! MIDI device could not), and looks again every few seconds for ones that were plugged in.
//! Elsewhere it reports that MIDI is unavailable.
//!
//! This only reports what the hardware sends. What a control means is for the library
//! process to say.

use open_engine_api::v1::midi_event::Event;
use open_engine_api::v1::{Control, ControlKind, ControllerInput, MidiDevices, MidiEvent};
use tokio::sync::mpsc::Sender;
use tonic::Status;

pub type Events = Sender<Result<MidiEvent, Status>>;

/// Reports the controllers and their messages to `events` until its receiver is dropped.
/// Blocks, so it runs on a thread of its own.
pub fn listen(events: Events) {
    #[cfg(target_os = "linux")]
    {
        if let Err(error) = linux::run(&events) {
            let _ = events.blocking_send(Ok(unavailable(error.to_string())));
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = events.blocking_send(Ok(unavailable(
            "MIDI controllers are only supported on Linux".to_owned(),
        )));
    }
}

fn unavailable(reason: String) -> MidiEvent {
    MidiEvent {
        event: Some(Event::Unavailable(reason)),
    }
}

fn devices(names: Vec<String>) -> MidiEvent {
    MidiEvent {
        event: Some(Event::Devices(MidiDevices { names })),
    }
}

fn input(kind: ControlKind, channel: u8, number: u32, value: u32) -> MidiEvent {
    MidiEvent {
        event: Some(Event::Input(ControllerInput {
            control: Some(Control {
                kind: kind.into(),
                // Channels are numbered from 1.
                channel: u32::from(channel.min(15)) + 1,
                number: number.min(127),
            }),
            value: value.min(127),
        })),
    }
}

/// A note on is a press, unless its velocity is 0, which means a note off.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn note(channel: u8, note: u8, velocity: u8, on: bool) -> MidiEvent {
    input(
        ControlKind::Note,
        channel,
        u32::from(note),
        if on { u32::from(velocity) } else { 0 },
    )
}

/// A control change; one whose number can't exist is not reported.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn controller(channel: u8, number: u32, value: i32) -> Option<MidiEvent> {
    (number < 128).then(|| input(ControlKind::Cc, channel, number, value.clamp(0, 127) as u32))
}

#[cfg(target_os = "linux")]
mod linux {
    use std::collections::HashSet;
    use std::time::{Duration, Instant};

    use alsa::Direction;
    use alsa::poll::{self, Descriptors};
    use alsa::seq::{
        Addr, ClientIter, EvCtrl, EvNote, EventType, PortCap, PortIter, PortSubscribe, PortType,
        Seq,
    };

    use super::{Events, controller, devices, note};

    /// How often to look for controllers that were plugged in or pulled out.
    const SCAN_INTERVAL: Duration = Duration::from_secs(2);
    /// The sequencer's own client (timer and announcements), which has no controller.
    const SYSTEM_CLIENT: i32 = 0;
    /// How long to wait for a message before checking that someone still wants them.
    const POLL_MS: i32 = 200;

    /// Runs until the receiver of `events` is gone.
    pub fn run(events: &Events) -> alsa::Result<()> {
        let seq = Seq::open(None, Some(Direction::Capture), true)?;
        seq.set_client_name(c"Open Engine Lib")?;
        let port = seq.create_simple_port(
            c"Controller",
            PortCap::WRITE | PortCap::SUBS_WRITE,
            PortType::MIDI_GENERIC | PortType::APPLICATION,
        )?;
        let me = Addr {
            client: seq.client_id()?,
            port,
        };
        let mut input = seq.input();
        let mut fds = (&seq, Some(Direction::Capture)).get()?;

        let mut connected = HashSet::new();
        let mut reported = None;
        let mut scanned = None::<Instant>;
        loop {
            if events.is_closed() {
                return Ok(());
            }
            if scanned.is_none_or(|at| at.elapsed() >= SCAN_INTERVAL) {
                scanned = Some(Instant::now());
                let found = connect(&seq, me, &mut connected);
                if reported.as_ref() != Some(&found) {
                    // (Waits if the library process is behind: this one must not be lost.)
                    if events.blocking_send(Ok(devices(found.clone()))).is_err() {
                        return Ok(());
                    }
                    reported = Some(found);
                }
            }
            poll::poll(&mut fds, POLL_MS)?;
            while input.event_input_pending(true).unwrap_or(0) > 0 {
                let Ok(event) = input.event_input() else {
                    break;
                };
                let message = match event.get_type() {
                    EventType::Noteon => event
                        .get_data::<EvNote>()
                        .map(|n| note(n.channel, n.note, n.velocity, true)),
                    EventType::Noteoff => event
                        .get_data::<EvNote>()
                        .map(|n| note(n.channel, n.note, 0, false)),
                    EventType::Controller => event
                        .get_data::<EvCtrl>()
                        .and_then(|c| controller(c.channel, c.param, c.value)),
                    _ => None,
                };
                // A full channel means the library process is behind; dropping a message of a
                // jog wheel beats waiting.
                if let Some(message) = message {
                    let _ = events.try_send(Ok(message));
                }
            }
        }
    }

    /// Subscribes to the hardware MIDI ports that are not subscribed to yet and forgets the
    /// ones that are gone. Gives the names of the devices that are connected, sorted.
    fn connect(seq: &Seq, me: Addr, connected: &mut HashSet<Addr>) -> Vec<String> {
        let mut present = HashSet::new();
        let mut names = Vec::new();
        for client in ClientIter::new(seq) {
            let id = client.get_client();
            if id == me.client || id == SYSTEM_CLIENT {
                continue;
            }
            let mut any = false;
            for info in PortIter::new(seq, id) {
                let capability = info.get_capability();
                if !capability.contains(PortCap::READ | PortCap::SUBS_READ)
                    || capability.contains(PortCap::NO_EXPORT)
                    || !info
                        .get_type()
                        .contains(PortType::HARDWARE | PortType::MIDI_GENERIC)
                {
                    continue;
                }
                let from = info.addr();
                if !connected.contains(&from) && subscribe(seq, from, me).is_err() {
                    continue;
                }
                present.insert(from);
                any = true;
            }
            if any && let Ok(name) = client.get_name() {
                names.push(name.to_owned());
            }
        }
        *connected = present;
        names.sort();
        names.dedup();
        names
    }

    fn subscribe(seq: &Seq, from: Addr, to: Addr) -> alsa::Result<()> {
        let subscription = PortSubscribe::empty()?;
        subscription.set_sender(from);
        subscription.set_dest(to);
        seq.subscribe_port(&subscription)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn heard(event: MidiEvent) -> (ControlKind, u32, u32, u32) {
        let Some(Event::Input(input)) = event.event else {
            panic!("not a message: {event:?}");
        };
        let control = input.control.unwrap();
        (control.kind(), control.channel, control.number, input.value)
    }

    #[test]
    fn a_note_on_with_no_velocity_is_a_release() {
        assert_eq!(
            heard(note(0, 36, 100, true)),
            (ControlKind::Note, 1, 36, 100)
        );
        assert_eq!(heard(note(0, 36, 0, true)).3, 0);
        assert_eq!(heard(note(0, 36, 64, false)).3, 0);
        // Channels are numbered from 1.
        assert_eq!(heard(note(9, 36, 1, true)).1, 10);
    }

    #[test]
    fn control_changes_are_clamped_and_numbers_checked() {
        assert_eq!(
            heard(controller(1, 20, 500).unwrap()),
            (ControlKind::Cc, 2, 20, 127)
        );
        assert_eq!(heard(controller(1, 20, -3).unwrap()).3, 0);
        assert!(controller(1, 128, 1).is_none());
    }

    /// Needs a controller plugged in:
    /// `cargo test -p open-engine-backend --bin open-engine-audio a_connected_controller -- --ignored --nocapture`.
    /// Lists what is connected and prints every message for `OPEN_ENGINE_TEST_SECONDS` (10),
    /// so press some buttons while it runs.
    #[test]
    #[ignore]
    fn a_connected_controller_is_heard() {
        let seconds = std::env::var("OPEN_ENGINE_TEST_SECONDS")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(10);
        let (sender, mut receiver) = tokio::sync::mpsc::channel(64);
        std::thread::spawn(move || listen(sender));
        let end = std::time::Instant::now() + std::time::Duration::from_secs(seconds);
        let mut names = None;
        while std::time::Instant::now() < end {
            match receiver.try_recv() {
                Ok(Ok(event)) => match event.event {
                    Some(Event::Devices(found)) => {
                        println!("connected: {:?}", found.names);
                        names = Some(found.names);
                    }
                    Some(Event::Unavailable(reason)) => panic!("MIDI unavailable: {reason}"),
                    Some(Event::Input(input)) => println!("{input:?}"),
                    None => {}
                },
                Ok(Err(status)) => panic!("{status}"),
                Err(_) => std::thread::sleep(std::time::Duration::from_millis(20)),
            }
        }
        assert!(names.is_some(), "the listener never reported the devices");
    }
}
