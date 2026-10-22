//! The playback queue and the thread that owns the audio output.
//!
//! The queue logic in [`Playback`] is plain and generic over the output. [`Engine`]
//! runs one on its own thread, because the audio device handle must stay on the
//! thread that opened it, and the gRPC handlers talk to it through messages.

use std::path::PathBuf;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use tokio::sync::oneshot;

use crate::player::{Output, PlayError, RodioOutput, Start};

/// How often the engine checks whether the current track has ended.
const TICK: Duration = Duration::from_millis(100);

#[derive(Debug, Clone)]
pub struct QueueItem {
    pub track_id: u32,
    pub path: PathBuf,
    pub streaming: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum State {
    #[default]
    Stopped,
    Playing,
    Paused,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    NotFound,
    Streaming,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrackError {
    pub kind: ErrorKind,
    pub track_id: u32,
    pub detail: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Status {
    pub state: State,
    pub queue_position: Option<u32>,
    pub track_id: Option<u32>,
    pub position: Duration,
    /// The last track that failed to play since playback was last started.
    pub error: Option<TrackError>,
}

#[derive(Debug)]
pub enum Command {
    Play {
        queue: Vec<QueueItem>,
        position: usize,
        start: Start,
    },
    TogglePause,
    Stop,
    Next,
    Previous,
    Seek(Duration),
    /// Do nothing but answer with the status as it is right now. (The status the engine
    /// publishes by itself is up to a tick old, which is too stale to draw a moving
    /// playhead from.)
    Poll,
    /// Stop and release the audio device.
    Close,
}

pub struct Playback<O> {
    output: O,
    queue: Vec<QueueItem>,
    /// Index of the playing track in `queue`; `None` while stopped.
    position: Option<usize>,
    error: Option<TrackError>,
}

impl<O: Output> Playback<O> {
    pub fn new(output: O) -> Self {
        Self {
            output,
            queue: Vec::new(),
            position: None,
            error: None,
        }
    }

    pub fn handle(&mut self, command: Command) {
        match command {
            Command::Play {
                queue,
                position,
                start,
            } => {
                self.error = None;
                self.queue = queue;
                self.position = None;
                if position < self.queue.len() {
                    self.start(position, start);
                }
            }
            Command::TogglePause => {
                if self.position.is_some() {
                    self.output.toggle_pause();
                }
            }
            Command::Stop => self.stop(),
            Command::Next => {
                if let Some(position) = self.position.filter(|p| p + 1 < self.queue.len()) {
                    self.start(position + 1, Start::default());
                }
            }
            Command::Previous => {
                if let Some(position) = self.position {
                    self.start(position.saturating_sub(1), Start::default());
                }
            }
            Command::Seek(to) => {
                if self.position.is_some() {
                    self.output.seek(to);
                }
            }
            Command::Poll => {}
            Command::Close => {
                self.stop();
                self.error = None;
                self.output.close();
            }
        }
    }

    /// Moves on when the current track has ended, skipping tracks that can't be played.
    pub fn tick(&mut self) {
        let Some(position) = self.position else {
            return;
        };
        if !self.output.finished() {
            return;
        }

        self.position = None;
        for next in position + 1..self.queue.len() {
            if self.start(next, Start::default()) {
                return;
            }
        }
        self.stop();
    }

    pub fn status(&self) -> Status {
        let item = self.position.map(|position| &self.queue[position]);
        Status {
            state: match item {
                None => State::Stopped,
                Some(_) if self.output.is_paused() => State::Paused,
                Some(_) => State::Playing,
            },
            queue_position: self.position.map(|position| position as u32),
            track_id: item.map(|item| item.track_id),
            position: if item.is_some() {
                self.output.position()
            } else {
                Duration::ZERO
            },
            error: self.error.clone(),
        }
    }

    fn stop(&mut self) {
        self.output.stop();
        self.position = None;
        self.queue.clear();
    }

    /// Starts `queue[position]`. On failure records the error and stays stopped.
    fn start(&mut self, position: usize, start: Start) -> bool {
        let item = &self.queue[position];
        let result = if item.streaming {
            self.output.stop();
            Err((ErrorKind::Streaming, String::new()))
        } else {
            self.output
                .play(&item.path, start)
                .map_err(|error| match error {
                    PlayError::NotFound(path) => (ErrorKind::NotFound, path.display().to_string()),
                    PlayError::Failed(error) => (ErrorKind::Failed, error),
                })
        };

        match result {
            Ok(()) => {
                self.position = Some(position);
                true
            }
            Err((kind, detail)) => {
                self.position = None;
                self.error = Some(TrackError {
                    kind,
                    track_id: item.track_id,
                    detail,
                });
                false
            }
        }
    }
}

struct Message {
    command: Command,
    reply: oneshot::Sender<Status>,
}

/// Handle to the playback thread.
pub struct Engine {
    commands: mpsc::Sender<Message>,
    status: Arc<Mutex<Status>>,
}

impl Engine {
    pub fn spawn() -> Self {
        let (commands, receiver) = mpsc::channel();
        let status = Arc::new(Mutex::new(Status::default()));

        let shared = Arc::clone(&status);
        thread::Builder::new()
            .name("playback".into())
            .spawn(move || run(Playback::new(RodioOutput::default()), receiver, shared))
            .expect("spawning the playback thread");

        Self { commands, status }
    }

    /// Runs a command and returns the status after it.
    pub async fn send(&self, command: Command) -> Status {
        let (reply, receiver) = oneshot::channel();
        if self.commands.send(Message { command, reply }).is_err() {
            return self.status();
        }
        match receiver.await {
            Ok(status) => status,
            Err(_) => self.status(),
        }
    }

    pub fn status(&self) -> Status {
        self.status.lock().expect("status lock").clone()
    }
}

fn run<O: Output>(
    mut playback: Playback<O>,
    receiver: mpsc::Receiver<Message>,
    shared: Arc<Mutex<Status>>,
) {
    loop {
        let message = match receiver.recv_timeout(TICK) {
            Ok(message) => Some(message),
            Err(RecvTimeoutError::Timeout) => {
                playback.tick();
                None
            }
            Err(RecvTimeoutError::Disconnected) => break,
        };

        if let Some(Message { command, reply }) = message {
            playback.handle(command);
            let status = playback.status();
            *shared.lock().expect("status lock") = status.clone();
            let _ = reply.send(status);
        } else {
            *shared.lock().expect("status lock") = playback.status();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::collections::HashSet;
    use std::path::Path;
    use std::rc::Rc;

    use super::*;

    /// Records what is played and lets a test end the current track.
    #[derive(Default)]
    struct FakeOutput {
        playing: Option<PathBuf>,
        paused: bool,
        finished: Rc<Cell<bool>>,
        missing: HashSet<PathBuf>,
        seeked: Option<Duration>,
        closed: bool,
        /// How the last track was started.
        started: Start,
    }

    impl Output for FakeOutput {
        fn play(&mut self, path: &Path, start: Start) -> Result<(), PlayError> {
            self.playing = None;
            self.finished.set(false);
            if self.missing.contains(path) {
                return Err(PlayError::NotFound(path.to_path_buf()));
            }
            self.started = start;
            self.paused = start.paused;
            self.playing = Some(path.to_path_buf());
            Ok(())
        }
        fn is_paused(&self) -> bool {
            self.paused
        }
        fn toggle_pause(&mut self) {
            self.paused = !self.paused;
        }
        fn finished(&self) -> bool {
            self.finished.get()
        }
        fn position(&self) -> Duration {
            Duration::from_secs(5)
        }
        fn seek(&mut self, to: Duration) -> bool {
            self.seeked = Some(to);
            true
        }
        fn stop(&mut self) {
            self.playing = None;
        }
        fn close(&mut self) {
            self.closed = true;
        }
    }

    fn item(id: u32, streaming: bool) -> QueueItem {
        QueueItem {
            track_id: id,
            path: PathBuf::from(format!("/music/{id}.flac")),
            streaming,
        }
    }

    fn playback(missing: &[u32]) -> (Playback<FakeOutput>, Rc<Cell<bool>>) {
        let finished = Rc::new(Cell::new(false));
        let output = FakeOutput {
            finished: Rc::clone(&finished),
            missing: missing
                .iter()
                .map(|id| PathBuf::from(format!("/music/{id}.flac")))
                .collect(),
            ..FakeOutput::default()
        };
        (Playback::new(output), finished)
    }

    fn play(playback: &mut Playback<FakeOutput>, ids: &[u32], position: usize) {
        playback.handle(Command::Play {
            queue: ids.iter().map(|&id| item(id, false)).collect(),
            position,
            start: Start::default(),
        });
    }

    #[test]
    fn starts_the_requested_track() {
        let (mut playback, _) = playback(&[]);
        play(&mut playback, &[10, 11, 12], 1);

        let status = playback.status();
        assert_eq!(status.state, State::Playing);
        assert_eq!(
            (status.queue_position, status.track_id),
            (Some(1), Some(11))
        );
        assert_eq!(status.position, Duration::from_secs(5));
        assert_eq!(
            playback.output.playing,
            Some(PathBuf::from("/music/11.flac"))
        );
    }

    #[test]
    fn can_load_a_track_paused_at_a_position() {
        let (mut playback, _) = playback(&[]);
        let at = Duration::from_secs(95);
        playback.handle(Command::Play {
            queue: vec![item(1, false), item(2, false)],
            position: 1,
            start: Start { paused: true, at },
        });

        let status = playback.status();
        assert_eq!((status.state, status.track_id), (State::Paused, Some(2)));
        assert_eq!(playback.output.started, Start { paused: true, at });

        // Resuming plays on; the next track starts normally.
        playback.handle(Command::TogglePause);
        assert_eq!(playback.status().state, State::Playing);
        playback.handle(Command::Previous);
        assert_eq!(playback.output.started, Start::default());
    }

    #[test]
    fn pauses_and_resumes_only_while_playing() {
        let (mut playback, _) = playback(&[]);
        playback.handle(Command::TogglePause);
        assert_eq!(playback.status().state, State::Stopped);

        play(&mut playback, &[1], 0);
        playback.handle(Command::TogglePause);
        assert_eq!(playback.status().state, State::Paused);
        playback.handle(Command::TogglePause);
        assert_eq!(playback.status().state, State::Playing);
    }

    #[test]
    fn moves_on_when_a_track_ends_and_stops_after_the_last() {
        let (mut playback, finished) = playback(&[]);
        play(&mut playback, &[1, 2], 0);

        playback.tick();
        assert_eq!(playback.status().track_id, Some(1), "not finished yet");

        finished.set(true);
        playback.tick();
        assert_eq!(playback.status().track_id, Some(2));

        finished.set(true);
        playback.tick();
        let status = playback.status();
        assert_eq!(status.state, State::Stopped);
        assert_eq!(status.queue_position, None);
        assert_eq!(status.error, None);
    }

    #[test]
    fn skips_unplayable_tracks_when_advancing() {
        let (mut playback, finished) = playback(&[2]);
        playback.handle(Command::Play {
            queue: vec![
                item(1, false),
                item(2, false),
                item(3, true),
                item(4, false),
            ],
            position: 0,
            start: Start::default(),
        });

        finished.set(true);
        playback.tick();

        let status = playback.status();
        assert_eq!(status.track_id, Some(4));
        let error = status.error.expect("the streaming track was skipped last");
        assert_eq!((error.kind, error.track_id), (ErrorKind::Streaming, 3));
    }

    #[test]
    fn a_failing_track_stops_playback_and_is_reported() {
        let (mut playback, _) = playback(&[7]);
        play(&mut playback, &[7, 8], 0);

        let status = playback.status();
        assert_eq!(status.state, State::Stopped);
        let error = status.error.unwrap();
        assert_eq!((error.kind, error.track_id), (ErrorKind::NotFound, 7));
        assert_eq!(error.detail, "/music/7.flac");

        // The next Play clears the error.
        play(&mut playback, &[8], 0);
        assert_eq!(playback.status().error, None);
    }

    #[test]
    fn next_and_previous_stay_inside_the_queue() {
        let (mut playback, _) = playback(&[]);
        play(&mut playback, &[1, 2, 3], 1);

        playback.handle(Command::Next);
        assert_eq!(playback.status().track_id, Some(3));
        playback.handle(Command::Next);
        assert_eq!(
            playback.status().track_id,
            Some(3),
            "no next after the last"
        );

        playback.handle(Command::Previous);
        playback.handle(Command::Previous);
        assert_eq!(playback.status().track_id, Some(1));
        playback.handle(Command::Previous);
        assert_eq!(
            playback.status().track_id,
            Some(1),
            "restarts the first track"
        );
    }

    #[test]
    fn stop_empties_the_queue_and_close_releases_the_device() {
        let (mut playback, _) = playback(&[]);
        play(&mut playback, &[1, 2], 0);

        playback.handle(Command::Stop);
        assert_eq!(playback.status().state, State::Stopped);
        playback.handle(Command::Next);
        assert_eq!(playback.status().state, State::Stopped, "queue is gone");

        play(&mut playback, &[1], 0);
        playback.handle(Command::Close);
        assert!(playback.output.closed);
        assert_eq!(playback.status().state, State::Stopped);
    }

    #[test]
    fn polling_reads_the_status_without_changing_it() {
        let (mut playback, _) = playback(&[]);
        play(&mut playback, &[1, 2], 1);
        let before = playback.status();
        playback.handle(Command::Poll);
        assert_eq!(playback.status(), before);

        let (mut idle, _) = super::tests::playback(&[]);
        idle.handle(Command::Poll);
        assert_eq!(idle.status().state, State::Stopped);
    }

    #[test]
    fn seek_reaches_the_output_only_while_playing() {
        let (mut playback, _) = playback(&[]);
        playback.handle(Command::Seek(Duration::from_secs(9)));
        assert_eq!(playback.output.seeked, None);

        play(&mut playback, &[1], 0);
        playback.handle(Command::Seek(Duration::from_secs(9)));
        assert_eq!(playback.output.seeked, Some(Duration::from_secs(9)));
    }
}
