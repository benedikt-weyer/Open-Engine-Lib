//! Publishes what is playing on the desktop's media interface (MPRIS, over D-Bus), so media
//! keys, the shell's media widget and tools like `playerctl` can show the track and control it.
//!
//! The UI owns this because it knows the track's tags, its cover and the playback state. The
//! D-Bus service runs inside a subscription: it hands the UI a [`Handle`] to push state into
//! and turns what other programs ask for into [`Command`]s. Only Linux has MPRIS; elsewhere
//! the subscription does nothing and the handle is never made.

use iced::Subscription;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Playback {
    Playing,
    Paused,
    Stopped,
}

/// The track in the player, as the desktop is told about it.
#[derive(Debug, Clone, PartialEq)]
pub struct TrackInfo {
    pub id: u32,
    pub title: String,
    pub artist: String,
    pub album: String,
    pub genre: String,
    /// In microseconds.
    pub length_us: Option<i64>,
    /// The audio file, if it is on this machine.
    pub location: Option<String>,
    /// The cover's id and its encoded image.
    pub art: Option<(u32, Vec<u8>)>,
}

/// Everything the desktop is told, apart from the position as it moves.
#[derive(Debug, Clone, PartialEq)]
pub struct Info {
    pub state: Playback,
    pub track: Option<TrackInfo>,
    /// Where in the track the player is, in microseconds.
    pub position_us: i64,
}

/// What another program asked the player to do.
#[derive(Debug, Clone, PartialEq)]
pub enum Command {
    Play,
    Pause,
    PlayPause,
    Stop,
    Next,
    Previous,
    /// Move by this many microseconds, backwards if negative.
    SeekBy(i64),
    /// Jump to a place in the track with this id.
    SeekTo {
        track: u32,
        position_us: i64,
    },
}

#[derive(Debug, Clone)]
pub enum Event {
    /// The service is up; this is how to tell it what plays.
    Ready(Handle),
    Command(Command),
}

#[cfg(target_os = "linux")]
pub use linux::{Handle, subscription};

#[cfg(not(target_os = "linux"))]
pub use other::{Handle, subscription};

#[cfg(not(target_os = "linux"))]
mod other {
    use super::*;

    #[derive(Debug, Clone)]
    pub struct Handle;

    impl Handle {
        pub fn set(&self, _info: Info) {}
        pub fn set_position(&self, _position_us: i64) {}
    }

    pub fn subscription() -> Subscription<Event> {
        Subscription::none()
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use std::collections::HashMap;
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex, MutexGuard};
    use std::time::Instant;

    use iced::futures::channel::mpsc;
    use iced::futures::{SinkExt, Stream, StreamExt, stream};
    use zbus::fdo::Properties;
    use zbus::names::InterfaceName;
    use zbus::object_server::SignalEmitter;
    use zbus::zvariant::{ObjectPath, Value};
    use zbus::{Connection, connection, interface};

    use super::*;

    const PATH: &str = "/org/mpris/MediaPlayer2";
    const PLAYER_INTERFACE: &str = "org.mpris.MediaPlayer2.Player";
    const NAME: &str = "org.mpris.MediaPlayer2.open-engine-lib";
    const NO_TRACK: &str = "/org/mpris/MediaPlayer2/TrackList/NoTrack";
    /// A position this far from where the player would be by itself is a seek.
    const SEEK_THRESHOLD_US: i64 = 500_000;

    #[derive(Debug, Clone)]
    enum Update {
        Info(Info),
        Position(i64),
    }

    /// How the UI tells the service what plays.
    #[derive(Debug, Clone)]
    pub struct Handle(mpsc::UnboundedSender<Update>);

    impl Handle {
        pub fn set(&self, info: Info) {
            let _ = self.0.unbounded_send(Update::Info(info));
        }

        pub fn set_position(&self, position_us: i64) {
            let _ = self.0.unbounded_send(Update::Position(position_us));
        }
    }

    /// What the interfaces answer with, kept up to date by the task that owns them.
    struct Shared {
        info: Info,
        /// The cover as a file the desktop can load, and the id of the cover it holds.
        art: Option<(u32, PathBuf)>,
        /// When `info.position_us` was true.
        at: Instant,
    }

    impl Shared {
        fn position(&self) -> i64 {
            let moved = match self.info.state {
                Playback::Playing => self.at.elapsed().as_micros() as i64,
                _ => 0,
            };
            let position = self.info.position_us + moved;
            match self.info.track.as_ref().and_then(|track| track.length_us) {
                Some(length) => position.min(length),
                None => position,
            }
            .max(0)
        }

        fn metadata(&self) -> HashMap<&'static str, Value<'static>> {
            let mut metadata = HashMap::new();
            let Some(track) = &self.info.track else {
                let none = ObjectPath::from_static_str_unchecked(NO_TRACK);
                metadata.insert("mpris:trackid", Value::from(none));
                return metadata;
            };
            metadata.insert("mpris:trackid", Value::from(track_path(track.id)));
            if let Some(length) = track.length_us {
                metadata.insert("mpris:length", Value::from(length));
            }
            if let Some((_, path)) = &self.art {
                metadata.insert(
                    "mpris:artUrl",
                    Value::from(file_url(&path.to_string_lossy())),
                );
            }
            metadata.insert("xesam:title", Value::from(track.title.clone()));
            if !track.artist.is_empty() {
                metadata.insert("xesam:artist", Value::from(vec![track.artist.clone()]));
            }
            if !track.album.is_empty() {
                metadata.insert("xesam:album", Value::from(track.album.clone()));
            }
            if !track.genre.is_empty() {
                metadata.insert("xesam:genre", Value::from(vec![track.genre.clone()]));
            }
            if let Some(location) = &track.location {
                metadata.insert("xesam:url", Value::from(file_url(location)));
            }
            metadata
        }
    }

    fn track_path(id: u32) -> ObjectPath<'static> {
        ObjectPath::try_from(format!("/org/openenginelib/track/{id}"))
            .expect("digits make a valid object path")
    }

    /// The id of a track path made by `track_path`.
    fn track_of_path(path: &ObjectPath<'_>) -> Option<u32> {
        path.as_str()
            .strip_prefix("/org/openenginelib/track/")?
            .parse()
            .ok()
    }

    fn file_url(path: &str) -> String {
        let mut url = String::from("file://");
        for byte in path.bytes() {
            match byte {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' => {
                    url.push(byte as char)
                }
                _ => url.push_str(&format!("%{byte:02X}")),
            }
        }
        url
    }

    fn lock(shared: &Mutex<Shared>) -> MutexGuard<'_, Shared> {
        shared
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    struct Root;

    #[interface(name = "org.mpris.MediaPlayer2")]
    impl Root {
        fn raise(&self) {}

        fn quit(&self) {}

        #[zbus(property)]
        fn can_quit(&self) -> bool {
            false
        }

        #[zbus(property)]
        fn can_raise(&self) -> bool {
            false
        }

        #[zbus(property)]
        fn has_track_list(&self) -> bool {
            false
        }

        #[zbus(property)]
        fn identity(&self) -> &str {
            "Open Engine Lib"
        }

        #[zbus(property)]
        fn desktop_entry(&self) -> &str {
            "open-engine-lib"
        }

        #[zbus(property)]
        fn supported_uri_schemes(&self) -> Vec<String> {
            Vec::new()
        }

        #[zbus(property)]
        fn supported_mime_types(&self) -> Vec<String> {
            Vec::new()
        }
    }

    struct Player {
        shared: Arc<Mutex<Shared>>,
        commands: mpsc::UnboundedSender<Command>,
    }

    impl Player {
        fn send(&self, command: Command) {
            let _ = self.commands.unbounded_send(command);
        }

        fn state(&self) -> Playback {
            lock(&self.shared).info.state
        }
    }

    #[interface(name = "org.mpris.MediaPlayer2.Player")]
    impl Player {
        fn next(&self) {
            self.send(Command::Next);
        }

        fn previous(&self) {
            self.send(Command::Previous);
        }

        fn pause(&self) {
            self.send(Command::Pause);
        }

        fn play_pause(&self) {
            self.send(Command::PlayPause);
        }

        fn stop(&self) {
            self.send(Command::Stop);
        }

        fn play(&self) {
            self.send(Command::Play);
        }

        fn seek(&self, offset: i64) {
            self.send(Command::SeekBy(offset));
        }

        fn set_position(&self, track_id: ObjectPath<'_>, position: i64) {
            if let Some(track) = track_of_path(&track_id) {
                self.send(Command::SeekTo {
                    track,
                    position_us: position,
                });
            }
        }

        fn open_uri(&self, _uri: &str) {}

        #[zbus(signal)]
        async fn seeked(emitter: &SignalEmitter<'_>, position: i64) -> zbus::Result<()>;

        #[zbus(property)]
        fn playback_status(&self) -> &'static str {
            match self.state() {
                Playback::Playing => "Playing",
                Playback::Paused => "Paused",
                Playback::Stopped => "Stopped",
            }
        }

        #[zbus(property)]
        fn rate(&self) -> f64 {
            1.0
        }

        #[zbus(property)]
        fn minimum_rate(&self) -> f64 {
            1.0
        }

        #[zbus(property)]
        fn maximum_rate(&self) -> f64 {
            1.0
        }

        #[zbus(property)]
        fn metadata(&self) -> HashMap<&'static str, Value<'static>> {
            lock(&self.shared).metadata()
        }

        #[zbus(property(emits_changed_signal = "false"))]
        fn position(&self) -> i64 {
            lock(&self.shared).position()
        }

        #[zbus(property)]
        fn can_go_next(&self) -> bool {
            self.state() != Playback::Stopped
        }

        #[zbus(property)]
        fn can_go_previous(&self) -> bool {
            self.state() != Playback::Stopped
        }

        #[zbus(property)]
        fn can_play(&self) -> bool {
            true
        }

        #[zbus(property)]
        fn can_pause(&self) -> bool {
            self.state() != Playback::Stopped
        }

        #[zbus(property)]
        fn can_seek(&self) -> bool {
            self.state() != Playback::Stopped
        }

        #[zbus(property)]
        fn can_control(&self) -> bool {
            true
        }
    }

    enum Wake {
        Command(Command),
        Update(Update),
    }

    /// The MPRIS service: the UI is told when it is up (`Event::Ready`) and what the desktop
    /// asks for. It gives up quietly if there is no session bus.
    pub fn subscription() -> Subscription<Event> {
        Subscription::run(events)
    }

    fn events() -> impl Stream<Item = Event> {
        iced::stream::channel(16, async |mut output| {
            let (command_tx, command_rx) = mpsc::unbounded();
            let (update_tx, update_rx) = mpsc::unbounded();
            let shared = Arc::new(Mutex::new(Shared {
                info: Info {
                    state: Playback::Stopped,
                    track: None,
                    position_us: 0,
                },
                art: None,
                at: Instant::now(),
            }));
            let connection = match serve(&shared, command_tx, NAME).await {
                Ok(connection) => connection,
                Err(error) => {
                    eprintln!("MPRIS is not available: {error}");
                    return;
                }
            };
            let art_dir = art_dir();
            if output.send(Event::Ready(Handle(update_tx))).await.is_err() {
                return;
            }

            let mut wakes =
                stream::select(command_rx.map(Wake::Command), update_rx.map(Wake::Update));
            while let Some(wake) = wakes.next().await {
                match wake {
                    Wake::Command(command) => {
                        if output.send(Event::Command(command)).await.is_err() {
                            return;
                        }
                    }
                    Wake::Update(update) => {
                        let result = match update {
                            Update::Info(info) => {
                                apply_info(&connection, &shared, &art_dir, info).await
                            }
                            Update::Position(position) => {
                                apply_position(&connection, &shared, position).await
                            }
                        };
                        if let Err(error) = result {
                            eprintln!("could not tell the desktop what plays: {error}");
                        }
                    }
                }
            }
        })
    }

    /// Takes the bus name, or one with the process id if another Open Engine Lib has it.
    async fn serve(
        shared: &Arc<Mutex<Shared>>,
        commands: mpsc::UnboundedSender<Command>,
        name: &str,
    ) -> zbus::Result<Connection> {
        let build = |name: String| {
            let player = Player {
                shared: Arc::clone(shared),
                commands: commands.clone(),
            };
            async move {
                connection::Builder::session()?
                    .name(name)?
                    .serve_at(PATH, Root)?
                    .serve_at(PATH, player)?
                    .build()
                    .await
            }
        };
        match build(name.to_string()).await {
            Err(zbus::Error::NameTaken) => {
                build(format!("{name}.instance{}", std::process::id())).await
            }
            other => other,
        }
    }

    /// Where covers are put for the desktop to load; it has no way to take the bytes.
    fn art_dir() -> Option<PathBuf> {
        let parent = dirs::runtime_dir()
            .or_else(dirs::cache_dir)?
            .join("open-engine-lib");
        // The folders of runs that ended (one per process, so two windows don't clash).
        for entry in std::fs::read_dir(&parent).into_iter().flatten().flatten() {
            let name = entry.file_name();
            let pid = name.to_str().and_then(|name| name.strip_prefix("mpris-"));
            if pid.is_some_and(|pid| !std::path::Path::new("/proc").join(pid).exists()) {
                let _ = std::fs::remove_dir_all(entry.path());
            }
        }
        let dir = parent.join(format!("mpris-{}", std::process::id()));
        std::fs::create_dir_all(&dir).ok()?;
        Some(dir)
    }

    fn extension(image: &[u8]) -> &'static str {
        if image.starts_with(b"\x89PNG") {
            "png"
        } else if image.starts_with(b"\xff\xd8") {
            "jpg"
        } else if image.starts_with(b"GIF8") {
            "gif"
        } else if image.starts_with(b"RIFF") {
            "webp"
        } else {
            "img"
        }
    }

    /// Writes the cover down and returns where, removing the previous cover's file.
    async fn write_art(
        dir: PathBuf,
        id: u32,
        image: Vec<u8>,
        previous: Option<PathBuf>,
    ) -> Option<PathBuf> {
        tokio::task::spawn_blocking(move || {
            let path = dir.join(format!("cover-{id}.{}", extension(&image)));
            std::fs::write(&path, &image).ok()?;
            if let Some(previous) = previous.filter(|previous| *previous != path) {
                let _ = std::fs::remove_file(previous);
            }
            Some(path)
        })
        .await
        .ok()
        .flatten()
    }

    async fn apply_info(
        connection: &Connection,
        shared: &Arc<Mutex<Shared>>,
        art_dir: &Option<PathBuf>,
        info: Info,
    ) -> zbus::Result<()> {
        // The cover is only written when it is another one than the desktop has.
        let held = lock(shared).art.clone();
        let wanted = info.track.as_ref().and_then(|track| track.art.as_ref());
        let art = match (wanted, art_dir) {
            (Some((id, _)), _) if held.as_ref().is_some_and(|(held, _)| held == id) => held,
            (Some((id, image)), Some(dir)) => {
                let previous = held.map(|(_, path)| path);
                write_art(dir.clone(), *id, image.clone(), previous)
                    .await
                    .map(|path| (*id, path))
            }
            _ => None,
        };

        let (changed, seeked) = {
            let mut shared = lock(shared);
            let before_state = shared.info.state;
            let before_metadata = shared.metadata();
            let expected = shared.position();
            let same_track = shared.info.track.as_ref().map(|track| track.id)
                == info.track.as_ref().map(|track| track.id);
            shared.art = art;
            shared.info = Info {
                track: info.track.map(|mut track| {
                    // (The image is kept on disk; no need to hold it here too.)
                    track.art = None;
                    track
                }),
                ..info
            };
            shared.at = Instant::now();

            let mut changed: HashMap<&str, Value<'_>> = HashMap::new();
            if shared.info.state != before_state {
                changed.insert(
                    "PlaybackStatus",
                    Value::from(match shared.info.state {
                        Playback::Playing => "Playing",
                        Playback::Paused => "Paused",
                        Playback::Stopped => "Stopped",
                    }),
                );
                let active = shared.info.state != Playback::Stopped;
                changed.insert("CanGoNext", Value::from(active));
                changed.insert("CanGoPrevious", Value::from(active));
                changed.insert("CanPause", Value::from(active));
                changed.insert("CanSeek", Value::from(active));
            }
            let metadata = shared.metadata();
            if metadata != before_metadata {
                changed.insert("Metadata", Value::from(metadata));
            }
            let seeked = same_track
                && shared.info.state != Playback::Stopped
                && (shared.info.position_us - expected).abs() > SEEK_THRESHOLD_US;
            (changed, seeked.then_some(shared.info.position_us))
        };

        let emitter = SignalEmitter::new(connection, PATH)?;
        if !changed.is_empty() {
            Properties::properties_changed(
                &emitter,
                InterfaceName::from_static_str_unchecked(PLAYER_INTERFACE),
                changed,
                Vec::new().into(),
            )
            .await?;
        }
        if let Some(position) = seeked {
            Player::seeked(&emitter, position).await?;
        }
        Ok(())
    }

    /// The player moved on: tells the desktop only if it jumped.
    async fn apply_position(
        connection: &Connection,
        shared: &Arc<Mutex<Shared>>,
        position_us: i64,
    ) -> zbus::Result<()> {
        let seeked = {
            let mut shared = lock(shared);
            if shared.info.state == Playback::Stopped {
                return Ok(());
            }
            let jumped = (position_us - shared.position()).abs() > SEEK_THRESHOLD_US;
            shared.info.position_us = position_us;
            shared.at = Instant::now();
            jumped
        };
        if seeked {
            let emitter = SignalEmitter::new(connection, PATH)?;
            Player::seeked(&emitter, position_us).await?;
        }
        Ok(())
    }
    #[cfg(test)]
    mod tests {
        use super::*;
        use zbus::Proxy;
        use zbus::proxy::{Builder, CacheProperties};

        fn shared() -> Arc<Mutex<Shared>> {
            Arc::new(Mutex::new(Shared {
                info: Info {
                    state: Playback::Stopped,
                    track: None,
                    position_us: 0,
                },
                art: None,
                at: Instant::now(),
            }))
        }

        fn info(state: Playback, position_us: i64) -> Info {
            Info {
                state,
                track: Some(TrackInfo {
                    id: 7,
                    title: "Song".into(),
                    artist: "Artist".into(),
                    album: "Album".into(),
                    genre: "House".into(),
                    length_us: Some(200_000_000),
                    location: Some("/music/a b.flac".into()),
                    art: None,
                }),
                position_us,
            }
        }

        #[test]
        fn file_urls_escape_what_a_path_can_hold() {
            assert_eq!(
                file_url("/music/a b#é.flac"),
                "file:///music/a%20b%23%C3%A9.flac"
            );
        }

        #[test]
        fn track_paths_name_the_track_back() {
            assert_eq!(track_of_path(&track_path(42)), Some(42));
            assert_eq!(
                track_of_path(&ObjectPath::try_from(NO_TRACK).unwrap()),
                None
            );
        }

        /// Needs a session bus; passes without one.
        #[test]
        fn the_desktop_sees_what_plays_and_can_control_it() {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_time()
                .build()
                .unwrap();
            runtime.block_on(async {
                let (commands, mut asked) = mpsc::unbounded();
                let shared = shared();
                let name = format!(
                    "org.mpris.MediaPlayer2.open_engine_lib_test{}",
                    std::process::id()
                );
                let Ok(connection) = serve(&shared, commands, &name).await else {
                    return;
                };
                let client = Connection::session().await.unwrap();
                let player: Proxy = Builder::new(&client)
                    .destination(name.as_str())
                    .unwrap()
                    .path(PATH)
                    .unwrap()
                    .interface(PLAYER_INTERFACE)
                    .unwrap()
                    .cache_properties(CacheProperties::No)
                    .build()
                    .await
                    .unwrap();

                let status: String = player.get_property("PlaybackStatus").await.unwrap();
                assert_eq!(status, "Stopped");

                apply_info(
                    &connection,
                    &shared,
                    &None,
                    info(Playback::Playing, 1_000_000),
                )
                .await
                .unwrap();
                let status: String = player.get_property("PlaybackStatus").await.unwrap();
                assert_eq!(status, "Playing");
                let metadata: HashMap<String, zbus::zvariant::OwnedValue> =
                    player.get_property("Metadata").await.unwrap();
                let title: String = metadata["xesam:title"]
                    .try_clone()
                    .unwrap()
                    .try_into()
                    .unwrap();
                assert_eq!(title, "Song");
                let length: i64 = metadata["mpris:length"]
                    .try_clone()
                    .unwrap()
                    .try_into()
                    .unwrap();
                assert_eq!(length, 200_000_000);
                let url: String = metadata["xesam:url"]
                    .try_clone()
                    .unwrap()
                    .try_into()
                    .unwrap();
                assert_eq!(url, "file:///music/a%20b.flac");
                let can_seek: bool = player.get_property("CanSeek").await.unwrap();
                assert!(can_seek);
                let position: i64 = player.get_property("Position").await.unwrap();
                assert!(position >= 1_000_000);

                player.call_method("PlayPause", &()).await.unwrap();
                assert_eq!(asked.next().await, Some(Command::PlayPause));
                player.call_method("Seek", &(5_000_000i64,)).await.unwrap();
                assert_eq!(asked.next().await, Some(Command::SeekBy(5_000_000)));
                player
                    .call_method("SetPosition", &(track_path(7), 9_000_000i64))
                    .await
                    .unwrap();
                assert_eq!(
                    asked.next().await,
                    Some(Command::SeekTo {
                        track: 7,
                        position_us: 9_000_000
                    })
                );

                apply_info(&connection, &shared, &None, info(Playback::Stopped, 0))
                    .await
                    .unwrap();
                let status: String = player.get_property("PlaybackStatus").await.unwrap();
                assert_eq!(status, "Stopped");
            });
        }
    }
}
