//! Starts the real backend process and talks to it over gRPC, the way the UI does.
//! Nothing here needs a sound card: tracks that can't be played fail before the audio
//! device is touched.

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::time::{Duration, Instant};

use open_engine_api::v1::backend_service_client::BackendServiceClient;
use open_engine_api::v1::controller_service_client::ControllerServiceClient;
use open_engine_api::v1::library_service_client::LibraryServiceClient;
use open_engine_api::v1::playback_service_client::PlaybackServiceClient;
use open_engine_api::v1::{
    CancelLearnRequest, ClearBindingRequest, CloseLibraryRequest, ControllerAction,
    CreateSmartPlaylistRequest, Cue, GetAlbumArtRequest, GetCuesRequest, GetInfoRequest,
    GetLoopsRequest, GetProgressRequest, GetSyncDiffRequest, GetWaveformRequest, LearnRequest,
    ListPlaylistsRequest, ListTracksRequest, Loop, MigrateOriginsRequest, OpenLibraryRequest,
    OpenLocalFolderRequest, PlayRequest, PlaybackErrorKind, PlaybackState, PullFromSourceRequest,
    SaveLibraryAsRequest, SetControllerEnabledRequest, SetCuesRequest, SetLoopsRequest,
    SetRatingRequest, SmartRule, SmartRules, SyncToSourceRequest, UpdateSmartPlaylistRequest,
    WatchControllerRequest,
};
use open_engine_api::{API_VERSION, AUTHORIZATION, LISTENING_PREFIX, TOKEN_ENV};
use tonic::service::interceptor::InterceptedService;
use tonic::transport::{Channel, Endpoint};
use tonic::{Code, Request, Status};

const TOKEN: &str = "test-token";

const FIXTURE: &str = "CREATE TABLE Track (id INTEGER PRIMARY KEY, title TEXT, artist TEXT, album TEXT,
        genre TEXT, bpmAnalyzed REAL, bpm INTEGER, length INTEGER, path TEXT,
        streamingSource TEXT, originDatabaseUuid TEXT, originTrackId INTEGER, albumArtId INTEGER);
    CREATE TABLE AlbumArt (id INTEGER PRIMARY KEY, hash, albumArt);
    CREATE TABLE Playlist (id INTEGER PRIMARY KEY, title TEXT, parentListId INTEGER, nextListId INTEGER);
    CREATE TABLE PlaylistEntity (id INTEGER PRIMARY KEY, listId INTEGER, trackId INTEGER,
        databaseUuid TEXT, nextEntityId INTEGER);
    INSERT INTO Track VALUES
        (1, 'One', 'A', 'X', 'House', 124.0, 124, 200, '../Music/one.flac', NULL, 'db', 1, 1),
        (2, 'Two', 'B', 'Y', 'Techno', 130.5, 130, 61, '../Music/two.mp3', NULL, 'db', 2, 1),
        (3, 'Three', 'C', 'Z', 'Pop', 0.0, 0, 90, '../tidal/three.mp4', 'TIDAL', 'db', 3, 2);
    INSERT INTO AlbumArt VALUES (1, 'abc', x'89504e470d0a1a0a0001'),
        (2, 'URI:1', CAST('image://streaming/x' AS BLOB));
    -- Roots in order: Beta -> Alpha, Alpha holds Child.
    INSERT INTO Playlist VALUES (1, 'Alpha', 0, 0), (2, 'Beta', 0, 1), (3, 'Child', 1, 0);
    INSERT INTO PlaylistEntity VALUES (10, 2, 3, 'db', 11), (11, 2, 1, 'db', 0);";

struct TempDir(PathBuf);

impl TempDir {
    /// A fresh folder per test: the tests run in parallel and clean up after themselves.
    fn new(name: &str) -> Self {
        let path =
            std::env::temp_dir().join(format!("open-engine-grpc-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn fixture_library(root: &Path) -> PathBuf {
    let library = root.join("Engine Library");
    std::fs::create_dir_all(library.join("Database2")).unwrap();
    rusqlite::Connection::open(library.join("Database2/m.db"))
        .unwrap()
        .execute_batch(FIXTURE)
        .unwrap();
    library
}

struct Running {
    child: Child,
    stdin: Option<ChildStdin>,
    address: String,
}

fn start(cache: &Path) -> Running {
    let mut child = Command::new(env!("CARGO_BIN_EXE_open-engine-backend"))
        .env(TOKEN_ENV, TOKEN)
        .env("OPEN_ENGINE_LIB_CACHE", cache)
        // Beside the cache, so that tests don't touch the user's own origins.
        .env("OPEN_ENGINE_LIB_DATA", cache.with_file_name("origins-data"))
        // Likewise for the DJ controller's bindings.
        .env("XDG_STATE_HOME", cache.with_file_name("state"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let stdin = child.stdin.take();
    let mut line = String::new();
    BufReader::new(child.stdout.take().unwrap())
        .read_line(&mut line)
        .unwrap();
    let address = line
        .strip_prefix(LISTENING_PREFIX)
        .unwrap_or_else(|| panic!("unexpected first line {line:?}"))
        .trim()
        .to_owned();
    Running {
        child,
        stdin,
        address,
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

type Connection = InterceptedService<Channel, fn(Request<()>) -> Result<Request<()>, Status>>;

fn with_token(mut request: Request<()>) -> Result<Request<()>, Status> {
    request
        .metadata_mut()
        .insert(AUTHORIZATION, format!("Bearer {TOKEN}").parse().unwrap());
    Ok(request)
}

async fn channel(running: &Running) -> Channel {
    Endpoint::from_shared(format!("http://{}", running.address))
        .unwrap()
        .connect()
        .await
        .unwrap()
}

fn code<T: std::fmt::Debug>(result: Result<T, Status>) -> Code {
    result.unwrap_err().code()
}

#[tokio::test]
async fn serves_the_api_over_grpc() {
    let temp = TempDir::new("api");
    let cache_root = temp.0.join("cache");
    let library_dir = fixture_library(&temp.0);
    let source_before = std::fs::read(library_dir.join("Database2/m.db")).unwrap();
    let running = start(&cache_root);
    let channel = channel(&running).await;

    // Requests without the launch token are refused.
    let mut anonymous = BackendServiceClient::new(channel.clone());
    assert_eq!(
        code(anonymous.get_info(GetInfoRequest {}).await),
        Code::Unauthenticated
    );

    let connection = |channel: Channel| -> Connection {
        InterceptedService::new(channel, with_token as fn(_) -> _)
    };
    let mut backend = BackendServiceClient::new(connection(channel.clone()));
    let mut library = LibraryServiceClient::new(connection(channel.clone()));
    let mut playback = PlaybackServiceClient::new(connection(channel));

    // The version handshake.
    let info = backend
        .get_info(GetInfoRequest {})
        .await
        .unwrap()
        .into_inner();
    assert!(info.supported_api_versions.contains(&API_VERSION));
    assert!(!info.backend_version.is_empty());

    // Nothing is readable before a library is open.
    assert_eq!(
        code(library.list_tracks(ListTracksRequest {}).await),
        Code::FailedPrecondition
    );
    assert_eq!(
        code(library.list_playlists(ListPlaylistsRequest {}).await),
        Code::FailedPrecondition
    );
    assert_eq!(
        code(
            playback
                .play(PlayRequest {
                    track_ids: vec![0],
                    position: 0,
                    ..Default::default()
                })
                .await
        ),
        Code::FailedPrecondition
    );

    // A folder without a library is NOT_FOUND.
    let missing = OpenLibraryRequest {
        path: temp.0.join("nowhere").display().to_string(),
    };
    assert_eq!(code(library.open_library(missing).await), Code::NotFound);

    // Opening goes through the cache; the source database is left alone.
    let opened = library
        .open_library(OpenLibraryRequest {
            path: temp.0.display().to_string(),
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(Path::new(&opened.path), library_dir);
    assert_eq!((opened.track_count, opened.playlist_count), (3, 3));
    let cache = PathBuf::from(&opened.cache_path);
    assert!(cache.starts_with(&cache_root));
    assert!(cache.join("Database2/m.db").is_file());
    assert_eq!(
        std::fs::read(library_dir.join("Database2/m.db")).unwrap(),
        source_before
    );

    let mut stream = library
        .list_tracks(ListTracksRequest {})
        .await
        .unwrap()
        .into_inner();
    let mut tracks = Vec::new();
    while let Some(batch) = stream.message().await.unwrap() {
        tracks.extend(batch.tracks);
    }
    assert_eq!(tracks.iter().map(|t| t.id).collect::<Vec<_>>(), [0, 1, 2]);
    assert_eq!(
        (
            tracks[1].title.as_str(),
            tracks[1].bpm,
            tracks[1].length_seconds
        ),
        ("Two", Some(130.5), Some(61))
    );
    assert_eq!(
        tracks[0].path, "../Music/one.flac",
        "as the library records it"
    );
    assert_eq!(tracks[2].bpm, None);
    assert!(tracks[2].streaming && !tracks[0].streaming);
    // Tracks of one album share a cover.
    assert_eq!(
        tracks.iter().map(|t| t.album_art_id).collect::<Vec<_>>(),
        [Some(1), Some(1), Some(2)]
    );

    let cover = library
        .get_album_art(GetAlbumArtRequest { id: 1 })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(cover.mime_type, "image/png");
    assert!(cover.data.starts_with(b"\x89PNG"));
    // A streaming track's cover is only a URL, and unknown ids don't exist.
    assert_eq!(
        code(library.get_album_art(GetAlbumArtRequest { id: 2 }).await),
        Code::NotFound
    );
    assert_eq!(
        code(library.get_album_art(GetAlbumArtRequest { id: 77 }).await),
        Code::NotFound
    );

    let playlists = library
        .list_playlists(ListPlaylistsRequest {})
        .await
        .unwrap()
        .into_inner()
        .playlists;
    let shape: Vec<_> = playlists
        .iter()
        .map(|p| (p.title.as_str(), p.depth, p.has_children))
        .collect();
    assert_eq!(
        shape,
        [("Beta", 0, false), ("Alpha", 0, true), ("Child", 1, false)]
    );
    assert_eq!(playlists[0].track_ids, [2, 0]);
    assert_eq!(playlists[2].parent_id, Some(1));

    // Bad play requests are rejected; tracks that can't be played are reported in the status.
    assert_eq!(
        code(
            playback
                .play(PlayRequest {
                    track_ids: vec![99],
                    position: 0,
                    ..Default::default()
                })
                .await
        ),
        Code::InvalidArgument
    );
    assert_eq!(
        code(
            playback
                .play(PlayRequest {
                    track_ids: vec![0],
                    position: 1,
                    ..Default::default()
                })
                .await
        ),
        Code::InvalidArgument
    );

    let status = playback
        .play(PlayRequest {
            track_ids: vec![0],
            position: 0,
            ..Default::default()
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(status.state(), PlaybackState::Stopped);
    let error = status.error.unwrap();
    assert_eq!(
        (error.kind(), error.track_id),
        (PlaybackErrorKind::NotFound, 0)
    );
    assert!(error.detail.ends_with("one.flac"), "{}", error.detail);

    let status = playback
        .play(PlayRequest {
            track_ids: vec![2],
            position: 0,
            ..Default::default()
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(status.error.unwrap().kind(), PlaybackErrorKind::Streaming);
    assert_eq!(
        playback
            .get_status(open_engine_api::v1::GetStatusRequest {})
            .await
            .unwrap()
            .into_inner()
            .state(),
        PlaybackState::Stopped
    );

    // Closing forgets the library.
    library.close_library(CloseLibraryRequest {}).await.unwrap();
    assert_eq!(
        code(library.list_tracks(ListTracksRequest {}).await),
        Code::FailedPrecondition
    );

    // The cache is kept, so reopening reuses it.
    let reopened = library
        .open_library(OpenLibraryRequest {
            path: library_dir.display().to_string(),
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(reopened.cache_path, opened.cache_path);
}

#[tokio::test]
async fn adds_and_changes_smart_playlists() {
    let temp = TempDir::new("smart");
    let library_dir = fixture_library(&temp.0);
    rusqlite::Connection::open(library_dir.join("Database2/m.db"))
        .unwrap()
        .execute_batch(
            "CREATE TABLE Smartlist (listUuid TEXT NOT NULL PRIMARY KEY, title TEXT,
                parentPlaylistPath TEXT, nextPlaylistPath TEXT, nextListUuid TEXT, rules TEXT,
                lastEditTime DATETIME, UNIQUE (title, parentPlaylistPath),
                UNIQUE (parentPlaylistPath, nextPlaylistPath, nextListUuid));",
        )
        .unwrap();
    let running = start(&temp.0.join("cache"));
    let channel = channel(&running).await;
    let mut library =
        LibraryServiceClient::new(InterceptedService::new(channel, with_token as fn(_) -> _));
    library
        .open_library(OpenLibraryRequest {
            path: temp.0.display().to_string(),
        })
        .await
        .unwrap();

    let rules = |genre: &str| SmartRules {
        match_all: true,
        rules: vec![SmartRule {
            column: "genre".into(),
            condition: "LIKE".into(),
            param: format!("%{genre}%"),
        }],
    };

    let created = library
        .create_smart_playlist(CreateSmartPlaylistRequest {
            title: "Houses".into(),
            rules: Some(rules("house")),
        })
        .await
        .unwrap()
        .into_inner();
    let list = &created.playlists[created.playlist_id as usize];
    assert_eq!((list.title.as_str(), list.is_smart), ("Houses", true));
    assert_eq!(list.track_ids, [0]);
    assert_eq!(list.rules, Some(rules("house")));

    let changed = library
        .update_smart_playlist(UpdateSmartPlaylistRequest {
            playlist_id: created.playlist_id,
            title: "Techs".into(),
            rules: Some(rules("techno")),
        })
        .await
        .unwrap()
        .into_inner();
    let list = &changed.playlists[changed.playlist_id as usize];
    assert_eq!(
        (list.title.as_str(), list.track_ids.as_slice()),
        ("Techs", &[1][..])
    );

    // The list ListPlaylists answers is the same one.
    let listed = library
        .list_playlists(ListPlaylistsRequest {})
        .await
        .unwrap()
        .into_inner()
        .playlists;
    assert_eq!(listed, changed.playlists);

    let refused = |result: Result<_, Status>| code(result.map(|_: tonic::Response<_>| ()));
    assert_eq!(
        refused(
            library
                .create_smart_playlist(CreateSmartPlaylistRequest {
                    title: "Techs".into(),
                    rules: Some(rules("x")),
                })
                .await
        ),
        Code::AlreadyExists
    );
    assert_eq!(
        refused(
            library
                .create_smart_playlist(CreateSmartPlaylistRequest {
                    title: " ".into(),
                    rules: Some(rules("x")),
                })
                .await
        ),
        Code::InvalidArgument
    );
    assert_eq!(
        refused(
            library
                .create_smart_playlist(CreateSmartPlaylistRequest {
                    title: "No rules".into(),
                    rules: None,
                })
                .await
        ),
        Code::InvalidArgument
    );
    // Playlist 0 is an ordinary one.
    assert_eq!(
        refused(
            library
                .update_smart_playlist(UpdateSmartPlaylistRequest {
                    playlist_id: 0,
                    title: "x".into(),
                    rules: Some(rules("x")),
                })
                .await
        ),
        Code::InvalidArgument
    );
    assert_eq!(
        refused(
            library
                .update_smart_playlist(UpdateSmartPlaylistRequest {
                    playlist_id: 99,
                    title: "x".into(),
                    rules: Some(rules("x")),
                })
                .await
        ),
        Code::NotFound
    );
}

#[tokio::test]
async fn remembers_where_tracks_were_in_its_own_database() {
    let temp = TempDir::new("origins");
    let library_dir = fixture_library(&temp.0);
    let running = start(&temp.0.join("cache"));
    let first = channel(&running).await;
    let mut library =
        LibraryServiceClient::new(InterceptedService::new(first, with_token as fn(_) -> _));
    assert_eq!(
        code(library.migrate_origins(MigrateOriginsRequest {}).await),
        Code::FailedPrecondition,
        "no library is open"
    );
    library
        .open_library(OpenLibraryRequest {
            path: temp.0.display().to_string(),
        })
        .await
        .unwrap();

    let origins = |tracks: Vec<open_engine_api::v1::Track>| -> Vec<Option<String>> {
        tracks.into_iter().map(|track| track.origin).collect()
    };
    let list = |mut library: LibraryServiceClient<Connection>| async move {
        let mut stream = library
            .list_tracks(ListTracksRequest {})
            .await
            .unwrap()
            .into_inner();
        let mut tracks = Vec::new();
        while let Some(batch) = stream.message().await.unwrap() {
            tracks.extend(batch.tracks);
        }
        tracks
    };
    assert_eq!(origins(list(library.clone()).await), [None, None, None]);

    // Nothing is running.
    let idle = library
        .get_progress(GetProgressRequest {})
        .await
        .unwrap()
        .into_inner();
    assert!(!idle.running);
    assert_eq!(
        (idle.total_bytes, idle.done_files, idle.current.as_str()),
        (0, 0, "")
    );

    // Tracks 1 and 2 have files here; 3 is a streaming track.
    let done = library
        .migrate_origins(MigrateOriginsRequest {})
        .await
        .unwrap()
        .into_inner();
    assert_eq!((done.saved, done.kept, done.skipped), (2, 0, 1));
    let music = library_dir.parent().unwrap().join("Music");
    let one = music.join("one.flac").display().to_string();
    let two = music.join("two.mp3").display().to_string();
    assert_eq!(
        done.origins
            .iter()
            .map(|origin| (origin.track_id, origin.origin.as_str()))
            .collect::<Vec<_>>(),
        [(0, one.as_str()), (1, two.as_str())]
    );
    assert_eq!(
        origins(list(library.clone()).await),
        [Some(one.clone()), Some(two.clone()), None]
    );

    // Again: nothing is new, and what is saved is kept.
    let again = library
        .migrate_origins(MigrateOriginsRequest {})
        .await
        .unwrap()
        .into_inner();
    assert_eq!((again.saved, again.kept, again.skipped), (0, 2, 1));

    // The backend can be restarted and still knows.
    drop(library);
    drop(running);
    let running = start(&temp.0.join("cache"));
    let second = channel(&running).await;
    let mut library =
        LibraryServiceClient::new(InterceptedService::new(second, with_token as fn(_) -> _));
    library
        .open_library(OpenLibraryRequest {
            path: temp.0.display().to_string(),
        })
        .await
        .unwrap();
    assert_eq!(origins(list(library).await), [Some(one), Some(two), None]);
}

#[tokio::test]
async fn pulling_replaces_the_working_copy_with_the_source() {
    let temp = TempDir::new("pull");
    let library_dir = fixture_library(&temp.0);
    // The fixture has no rating column; the working copy gets one through an edit.
    rusqlite::Connection::open(library_dir.join("Database2/m.db"))
        .unwrap()
        .execute_batch("ALTER TABLE Track ADD COLUMN rating INTEGER;")
        .unwrap();
    let running = start(&temp.0.join("cache"));
    let first = channel(&running).await;
    let mut library =
        LibraryServiceClient::new(InterceptedService::new(first, with_token as fn(_) -> _));
    library
        .open_library(OpenLibraryRequest {
            path: temp.0.display().to_string(),
        })
        .await
        .unwrap();
    let rating_of = |mut library: LibraryServiceClient<Connection>| async move {
        let mut stream = library
            .list_tracks(ListTracksRequest {})
            .await
            .unwrap()
            .into_inner();
        stream.message().await.unwrap().unwrap().tracks[0].rating
    };

    library
        .set_rating(SetRatingRequest {
            track_id: 0,
            rating: 80,
        })
        .await
        .unwrap();
    assert_eq!(rating_of(library.clone()).await, Some(80));
    // The other way round, the diff says the working copy would lose it.
    let diff = library
        .get_sync_diff(GetSyncDiffRequest { reverse: true })
        .await
        .unwrap()
        .into_inner();
    let change = diff
        .changed_tracks
        .iter()
        .find(|c| c.fields.iter().any(|f| f.field == "rating"));
    let field = &change.unwrap().fields[0];
    assert_eq!(
        (field.source_value.as_str(), field.working_value.as_str()),
        ("80", "")
    );

    let pulled = library
        .pull_from_source(PullFromSourceRequest {})
        .await
        .unwrap()
        .into_inner();
    assert!(pulled.kept_edits_path.ends_with("unsynced-edits"));
    assert!(
        Path::new(&pulled.kept_edits_path)
            .join("Database2/m.db")
            .is_file()
    );
    // The library that is open reads the new working copy: the rating is the source's.
    assert_eq!(rating_of(library.clone()).await, None);
    let diff = library
        .get_sync_diff(GetSyncDiffRequest::default())
        .await
        .unwrap()
        .into_inner();
    assert!(diff.changed_databases.is_empty());
}

#[tokio::test]
async fn shows_and_changes_what_the_controls_of_a_dj_controller_do() {
    let temp = TempDir::new("controller");
    // Bindings that an earlier run learned, one of them nonsense.
    let state_dir = temp.0.join("state/open-engine-lib");
    std::fs::create_dir_all(&state_dir).unwrap();
    let file = state_dir.join("controller.toml");
    std::fs::write(
        &file,
        r#"
            [[binding]]
            control = { kind = "note", channel = 1, number = 36 }
            action = "play_pause"
            [[binding]]
            control = { kind = "cc", channel = 40, number = 1 }
            action = "stop"
            [[binding]]
            control = { kind = "cc", channel = 2, number = 17 }
            action = "pad_3"
        "#,
    )
    .unwrap();
    let running = start(&temp.0.join("cache"));
    let mut controller = ControllerServiceClient::with_interceptor(
        channel(&running).await,
        with_token as fn(Request<()>) -> Result<Request<()>, Status>,
    );

    // It is off until asked, and knows the bindings that were valid.
    let mut watch = controller
        .watch(WatchControllerRequest {})
        .await
        .unwrap()
        .into_inner();
    let state = watch.message().await.unwrap().unwrap();
    assert!(!state.enabled && state.devices.is_empty() && state.learning.is_none());
    let bound: Vec<_> = state
        .bindings
        .iter()
        .map(|binding| (binding.action(), binding.control.unwrap().number))
        .collect();
    assert_eq!(
        bound,
        [
            (ControllerAction::PlayPause, 36),
            (ControllerAction::Pad3, 17)
        ]
    );

    // Learning is told to the watcher, and can be cancelled.
    let learning = controller
        .learn(LearnRequest {
            action: ControllerAction::Next.into(),
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(learning.learning, Some(ControllerAction::Next.into()));
    let state = watch.message().await.unwrap().unwrap();
    assert_eq!(state.learning, Some(ControllerAction::Next.into()));
    let state = controller
        .cancel_learn(CancelLearnRequest {})
        .await
        .unwrap()
        .into_inner();
    assert_eq!(state.learning, None);
    assert_eq!(
        code(
            controller
                .learn(LearnRequest {
                    action: ControllerAction::Unspecified.into(),
                })
                .await
        ),
        Code::InvalidArgument
    );

    // Clearing a binding is remembered in the file.
    let state = controller
        .clear_binding(ClearBindingRequest {
            action: ControllerAction::PlayPause.into(),
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(state.bindings.len(), 1);
    let saved = std::fs::read_to_string(&file).unwrap();
    assert!(
        saved.contains("pad_3") && !saved.contains("play_pause"),
        "{saved}"
    );

    // Turning it on starts the listening (whether there is a controller, or even a MIDI
    // sequencer, depends on the machine); turning it off forgets what was heard.
    let state = controller
        .set_enabled(SetControllerEnabledRequest { enabled: true })
        .await
        .unwrap()
        .into_inner();
    assert!(state.enabled);
    let state = controller
        .set_enabled(SetControllerEnabledRequest { enabled: false })
        .await
        .unwrap()
        .into_inner();
    assert!(!state.enabled && state.devices.is_empty() && state.error.is_none());
    assert_eq!(state.bindings.len(), 1, "the bindings stay");
}

/// Needs a DJ controller plugged in (and the audio process next to the backend): turns the
/// controller on, lists what is connected and prints the messages it sends for
/// `OPEN_ENGINE_TEST_SECONDS` (10), so press some buttons while it runs.
/// `cargo test -p open-engine-backend --test grpc hears_a_connected -- --ignored --nocapture`
#[tokio::test]
#[ignore = "needs a DJ controller"]
async fn hears_a_connected_dj_controller() {
    let temp = TempDir::new("hears");
    let running = start(&temp.0.join("cache"));
    let mut controller = ControllerServiceClient::with_interceptor(
        channel(&running).await,
        with_token as fn(Request<()>) -> Result<Request<()>, Status>,
    );
    let mut watch = controller
        .watch(WatchControllerRequest {})
        .await
        .unwrap()
        .into_inner();
    controller
        .set_enabled(SetControllerEnabledRequest { enabled: true })
        .await
        .unwrap();
    let seconds = std::env::var("OPEN_ENGINE_TEST_SECONDS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(10);
    let end = Instant::now() + Duration::from_secs(seconds);
    let mut devices = Vec::new();
    while let Ok(Ok(Some(state))) = tokio::time::timeout(
        end.saturating_duration_since(Instant::now()),
        watch.message(),
    )
    .await
    {
        if state.devices != devices {
            println!("connected: {:?}", state.devices);
            devices = state.devices.clone();
        }
        if let Some(input) = &state.last_input {
            println!("{:?}", input);
        }
        assert_eq!(state.error, None);
    }
    assert!(!devices.is_empty(), "no controller was found");
}

#[tokio::test]
async fn exits_when_the_ui_goes_away() {
    let temp = TempDir::new("exit");
    let mut running = start(&temp.0.join("cache"));

    // Closing the pipe the UI holds is how the backend learns that the UI is gone.
    drop(running.stdin.take());

    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(status) = running.child.try_wait().unwrap() {
            assert!(status.success(), "{status}");
            break;
        }
        assert!(Instant::now() < deadline, "the backend kept running");
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// The processes that have `parent` as their parent: their ids and the names of their programs.
#[cfg(target_os = "linux")]
fn children_of(parent: u32) -> Vec<(u32, String)> {
    let mut found = Vec::new();
    for entry in std::fs::read_dir("/proc").unwrap().flatten() {
        let Ok(id) = entry.file_name().to_string_lossy().parse::<u32>() else {
            continue;
        };
        let Ok(stat) = std::fs::read_to_string(entry.path().join("stat")) else {
            continue;
        };
        // "pid (name) state ppid ...": the name may hold spaces, so go from its closing bracket.
        let (Some(open), Some(close)) = (stat.find('('), stat.rfind(')')) else {
            continue;
        };
        let ppid = stat[close + 1..].split_whitespace().nth(1);
        if ppid.and_then(|ppid| ppid.parse::<u32>().ok()) == Some(parent) {
            found.push((id, stat[open + 1..close].to_owned()));
        }
    }
    found
}

/// The audio is another process, started by the backend (the UI never starts or talks to it),
/// and it goes when the backend does.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn starts_the_audio_process_and_takes_it_along_when_it_exits() {
    let temp = TempDir::new("audio-process");
    let mut running = start(&temp.0.join("cache"));
    // (The name in /proc is cut to 15 characters.)
    let (audio, _) = children_of(running.child.id())
        .into_iter()
        .find(|(_, name)| name.starts_with("open-engine-aud"))
        .expect("the backend did not start the audio process");

    drop(running.stdin.take());
    let deadline = Instant::now() + Duration::from_secs(5);
    while running.child.try_wait().unwrap().is_none() {
        assert!(Instant::now() < deadline, "the backend kept running");
        std::thread::sleep(Duration::from_millis(50));
    }
    // It reads the pipe the backend holds, which closes with the backend, so it exits by itself.
    let deadline = Instant::now() + Duration::from_secs(5);
    while Path::new(&format!("/proc/{audio}")).exists() {
        assert!(Instant::now() < deadline, "the audio process kept running");
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Smoke test against a real library and the real sound card. Not run by default:
/// `OPEN_ENGINE_TEST_LIBRARY=/path/to/drive cargo test -p open-engine-backend --test grpc -- --ignored --nocapture`
#[tokio::test]
#[ignore = "needs a real library (OPEN_ENGINE_TEST_LIBRARY) and an audio device"]
async fn plays_from_a_real_library() {
    let path = std::env::var("OPEN_ENGINE_TEST_LIBRARY").expect("OPEN_ENGINE_TEST_LIBRARY");
    let temp = TempDir::new("real");
    let running = start(&temp.0.join("cache"));
    let channel = channel(&running).await;
    let connection = |channel: Channel| -> Connection {
        InterceptedService::new(channel, with_token as fn(_) -> _)
    };
    let mut library = LibraryServiceClient::new(connection(channel.clone()));
    let mut playback = PlaybackServiceClient::new(connection(channel));

    let started = Instant::now();
    let opened = library
        .open_library(OpenLibraryRequest { path })
        .await
        .unwrap()
        .into_inner();
    println!("opened {opened:?} in {:?}", started.elapsed());

    let mut stream = library
        .list_tracks(ListTracksRequest {})
        .await
        .unwrap()
        .into_inner();
    let mut tracks = Vec::new();
    while let Some(batch) = stream.message().await.unwrap() {
        tracks.extend(batch.tracks);
    }
    let playlists = library
        .list_playlists(ListPlaylistsRequest {})
        .await
        .unwrap()
        .into_inner()
        .playlists;
    println!("{} tracks, {} playlists", tracks.len(), playlists.len());
    assert_eq!(tracks.len(), opened.track_count as usize);

    // Play the first track whose file exists.
    let mut playing = None;
    for track in tracks.iter().filter(|track| !track.streaming).take(50) {
        let status = playback
            .play(PlayRequest {
                track_ids: vec![track.id],
                position: 0,
                ..Default::default()
            })
            .await
            .unwrap()
            .into_inner();
        match status.error {
            None => {
                println!("playing {:?} - {:?}", track.artist, track.title);
                playing = Some(track.id);
                break;
            }
            Some(error) => println!("skipped {}: {:?} {}", track.id, error.kind(), error.detail),
        }
    }
    let id = playing.expect("none of the first 50 tracks could be played");

    tokio::time::sleep(Duration::from_secs(2)).await;
    let status = playback
        .get_status(open_engine_api::v1::GetStatusRequest {})
        .await
        .unwrap()
        .into_inner();
    println!("after 2s: {status:?}");
    assert_eq!(status.state(), PlaybackState::Playing);
    assert_eq!(status.track_id, Some(id));
    assert!(
        status.position_ms >= 1000,
        "position {}",
        status.position_ms
    );

    let status = playback
        .toggle_pause(open_engine_api::v1::TogglePauseRequest {})
        .await
        .unwrap()
        .into_inner();
    assert_eq!(status.state(), PlaybackState::Paused);
    let status = playback
        .seek(open_engine_api::v1::SeekRequest {
            position_ms: 30_000,
        })
        .await
        .unwrap()
        .into_inner();
    println!("after seek to 30s: {status:?}");
    let status = playback
        .stop(open_engine_api::v1::StopRequest {})
        .await
        .unwrap()
        .into_inner();
    assert_eq!(status.state(), PlaybackState::Stopped);
}

/// Fetches every cover of a real library: `OPEN_ENGINE_TEST_LIBRARY=... cargo test -p
/// open-engine-backend --test grpc covers -- --ignored --nocapture`. Silent, unlike the
/// playback test.
#[tokio::test]
#[ignore = "needs a real library (OPEN_ENGINE_TEST_LIBRARY)"]
async fn serves_covers_from_a_real_library() {
    let path = std::env::var("OPEN_ENGINE_TEST_LIBRARY").expect("OPEN_ENGINE_TEST_LIBRARY");
    let temp = TempDir::new("covers");
    let running = start(&temp.0.join("cache"));
    let channel = channel(&running).await;
    let mut library =
        LibraryServiceClient::new(InterceptedService::new(channel, with_token as fn(_) -> _));
    library
        .open_library(OpenLibraryRequest { path })
        .await
        .unwrap();

    let mut stream = library
        .list_tracks(ListTracksRequest {})
        .await
        .unwrap()
        .into_inner();
    let mut tracks = Vec::new();
    while let Some(batch) = stream.message().await.unwrap() {
        tracks.extend(batch.tracks);
    }

    let mut ids: Vec<u32> = tracks
        .iter()
        .filter_map(|track| track.album_art_id)
        .collect();
    let without = tracks.len() - ids.len();
    ids.sort_unstable();
    ids.dedup();

    let (mut png, mut jpeg, mut missing, mut bytes) = (0, 0, 0, 0);
    let started = Instant::now();
    for id in &ids {
        match library.get_album_art(GetAlbumArtRequest { id: *id }).await {
            Ok(cover) => {
                let cover = cover.into_inner();
                bytes += cover.data.len();
                match cover.mime_type.as_str() {
                    "image/png" => {
                        assert!(cover.data.starts_with(b"\x89PNG"));
                        png += 1;
                    }
                    "image/jpeg" => {
                        assert!(cover.data.starts_with(&[0xff, 0xd8, 0xff]));
                        jpeg += 1;
                    }
                    other => panic!("unexpected type {other}"),
                }
            }
            Err(status) => {
                assert_eq!(status.code(), Code::NotFound);
                missing += 1;
            }
        }
    }
    println!(
        "{} tracks: {} without a cover id; {} distinct covers: {png} png, {jpeg} jpeg, \
         {missing} not available; {bytes} bytes in {:?}",
        tracks.len(),
        without,
        ids.len(),
        started.elapsed()
    );
    let streaming_missing = tracks
        .iter()
        .filter(|track| track.streaming && track.album_art_id.is_some())
        .count();
    println!("{streaming_missing} streaming tracks have a cover id (their covers are URLs)");
}

/// Loading a track paused at a position (what restoring the last session does) must stay
/// silent: `OPEN_ENGINE_TEST_LIBRARY=... cargo test -p open-engine-backend --test grpc paused -- --ignored --nocapture`
#[tokio::test]
#[ignore = "needs a real library (OPEN_ENGINE_TEST_LIBRARY) and an audio device"]
async fn loads_a_real_track_paused_at_a_position() {
    let path = std::env::var("OPEN_ENGINE_TEST_LIBRARY").expect("OPEN_ENGINE_TEST_LIBRARY");
    let temp = TempDir::new("paused");
    let running = start(&temp.0.join("cache"));
    let channel = channel(&running).await;
    let connection = |channel: Channel| -> Connection {
        InterceptedService::new(channel, with_token as fn(_) -> _)
    };
    let mut library = LibraryServiceClient::new(connection(channel.clone()));
    let mut playback = PlaybackServiceClient::new(connection(channel));
    library
        .open_library(OpenLibraryRequest { path })
        .await
        .unwrap();

    let mut stream = library
        .list_tracks(ListTracksRequest {})
        .await
        .unwrap()
        .into_inner();
    let mut tracks = Vec::new();
    while let Some(batch) = stream.message().await.unwrap() {
        tracks.extend(batch.tracks);
    }

    for track in tracks.iter().filter(|track| !track.streaming).take(50) {
        let status = playback
            .play(PlayRequest {
                track_ids: vec![track.id],
                position: 0,
                start_paused: true,
                start_position_ms: 20_000,
            })
            .await
            .unwrap()
            .into_inner();
        if status.error.is_some() {
            continue;
        }
        println!("loaded {:?} paused: {status:?}", track.title);
        assert_eq!(status.state(), PlaybackState::Paused);
        assert_eq!(status.position_ms, 20_000);

        // Paused means paused: the position does not move.
        tokio::time::sleep(Duration::from_secs(1)).await;
        let later = playback
            .get_status(open_engine_api::v1::GetStatusRequest {})
            .await
            .unwrap()
            .into_inner();
        assert_eq!(later.state(), PlaybackState::Paused);
        assert_eq!(later.position_ms, 20_000);
        playback
            .stop(open_engine_api::v1::StopRequest {})
            .await
            .unwrap();
        return;
    }
    panic!("none of the first 50 tracks could be loaded");
}

/// A mono 16-bit WAV file: `seconds` of a tone of `hz`.
fn wav(path: &Path, tones: &[(f32, f32)]) {
    const RATE: u32 = 22_050;
    let samples: Vec<i16> = tones
        .iter()
        .flat_map(|&(hz, seconds)| {
            (0..(RATE as f32 * seconds) as usize).map(move |n| {
                ((std::f32::consts::TAU * hz * n as f32 / RATE as f32).sin()
                    * 0.8
                    * f32::from(i16::MAX)) as i16
            })
        })
        .collect();
    let data_len = (samples.len() * 2) as u32;
    let mut file = Vec::new();
    file.extend(b"RIFF");
    file.extend((36 + data_len).to_le_bytes());
    file.extend(b"WAVEfmt ");
    file.extend(16u32.to_le_bytes());
    file.extend([1u8, 0, 1, 0]); // PCM, mono
    file.extend(RATE.to_le_bytes());
    file.extend((RATE * 2).to_le_bytes());
    file.extend([2u8, 0, 16, 0]);
    file.extend(b"data");
    file.extend(data_len.to_le_bytes());
    file.extend(samples.iter().flat_map(|s| s.to_le_bytes()));
    std::fs::write(path, file).unwrap();
}

/// A `beatData` blob for a 120 BPM grid starting a quarter second in.
fn beat_blob() -> Vec<u8> {
    use std::io::Write;
    let mut data = Vec::new();
    data.extend(22_050f64.to_be_bytes());
    data.extend(66_150f64.to_be_bytes());
    data.push(1);
    for _ in 0..2 {
        data.extend(2i64.to_be_bytes());
        for (samples, beat) in [(5_512.5f64, 0i64), (5_512.5 + 11_025.0 * 4.0, 4)] {
            data.extend(samples.to_le_bytes());
            data.extend(beat.to_le_bytes());
            data.extend([0u8; 8]);
        }
    }
    let mut encoder = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
    encoder.write_all(&data).unwrap();
    let mut blob = (data.len() as u32).to_be_bytes().to_vec();
    blob.extend(encoder.finish().unwrap());
    blob
}

#[tokio::test]
async fn serves_waveforms_and_beatgrids() {
    let temp = TempDir::new("waveform");
    let library_dir = fixture_library(&temp.0);
    std::fs::create_dir_all(temp.0.join("Music")).unwrap();
    // One second of bass, then one second of treble.
    wav(
        &temp.0.join("Music/tone.wav"),
        &[(80.0, 1.0), (5_000.0, 1.0)],
    );
    wav(&temp.0.join("Music/plain.wav"), &[(2_000.0, 1.0)]);
    {
        let db = rusqlite::Connection::open(library_dir.join("Database2/m.db")).unwrap();
        db.execute_batch(
            "CREATE TABLE PerformanceData (trackId INTEGER PRIMARY KEY, beatData BLOB);
             INSERT INTO Track VALUES (4, 'Tone', 'T', 'T', 'Test', 120.0, 120, 2, '../Music/tone.wav', NULL, 'db', 4, NULL),
                 (5, 'Plain', 'T', 'T', 'Test', 0.0, 0, 1, '../Music/plain.wav', NULL, 'db', 5, NULL);",
        )
        .unwrap();
        db.execute("INSERT INTO PerformanceData VALUES (4, ?1)", [beat_blob()])
            .unwrap();
    }

    let running = start(&temp.0.join("cache"));
    let channel = channel(&running).await;
    let mut library =
        LibraryServiceClient::new(InterceptedService::new(channel, with_token as fn(_) -> _));
    assert_eq!(
        code(
            library
                .get_waveform(GetWaveformRequest {
                    track_id: 0,
                    ..Default::default()
                })
                .await
        ),
        Code::FailedPrecondition,
        "no library open"
    );
    library
        .open_library(OpenLibraryRequest {
            path: temp.0.display().to_string(),
        })
        .await
        .unwrap();

    // Track ids are positions: 0..=2 are the fixture's, 3 is Tone, 4 is Plain.
    let tone = library
        .get_waveform(GetWaveformRequest {
            track_id: 3,
            ..Default::default()
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(tone.columns_per_second, 100);
    assert!((tone.duration_seconds - 2.0).abs() < 0.01);
    assert_eq!(tone.columns.len(), 200 * 3);
    let band = |column: usize, band: usize| u32::from(tone.columns[column * 3 + band]);
    assert!(
        band(50, 0) > 150 && band(50, 2) < 60,
        "bass first: {} {}",
        band(50, 0),
        band(50, 2)
    );
    assert!(band(150, 2) > 150 && band(150, 0) < 60, "treble after");

    let grid = tone.beat_grid.expect("the track has a beatgrid");
    let markers: Vec<_> = grid
        .markers
        .iter()
        .map(|m| (m.position_seconds, m.beat_number))
        .collect();
    assert_eq!(markers, [(0.25, 0), (2.25, 4)]);

    // The same waveform without a beatgrid for a track that has none.
    let plain = library
        .get_waveform(GetWaveformRequest {
            track_id: 4,
            ..Default::default()
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(plain.columns.len(), 100 * 3);
    assert!(plain.beat_grid.is_none());

    // The bands meet at 600 Hz and 4 kHz unless told otherwise: a 2 kHz tone is mid. With
    // the high band starting at 1 kHz it is high, and with the low band reaching up to
    // 6 kHz it is low.
    let band_at_50 = |response: &open_engine_api::v1::GetWaveformResponse, band: usize| {
        u32::from(response.columns[50 * 3 + band])
    };
    assert!(
        band_at_50(&plain, 1) > band_at_50(&plain, 0)
            && band_at_50(&plain, 1) > band_at_50(&plain, 2)
    );
    let waveform_with = |low_mid_hz, mid_high_hz| GetWaveformRequest {
        track_id: 4,
        low_mid_hz: Some(low_mid_hz),
        mid_high_hz: Some(mid_high_hz),
        ..Default::default()
    };
    let high = library
        .get_waveform(waveform_with(200, 1_000))
        .await
        .unwrap()
        .into_inner();
    assert!(
        band_at_50(&high, 2) > band_at_50(&high, 1),
        "high from 1 kHz"
    );
    let low = library
        .get_waveform(waveform_with(6_000, 9_000))
        .await
        .unwrap()
        .into_inner();
    assert!(band_at_50(&low, 0) > band_at_50(&low, 1), "low up to 6 kHz");
    // Split points that make no sense are refused.
    for (low_mid_hz, mid_high_hz) in [(4_000, 600), (600, 600), (5, 4_000), (600, 40_000)] {
        assert_eq!(
            code(
                library
                    .get_waveform(waveform_with(low_mid_hz, mid_high_hz))
                    .await
            ),
            Code::InvalidArgument,
            "{low_mid_hz} / {mid_high_hz}"
        );
    }

    // The gains amplify a band (and only that one), up to full height, and the same
    // file is not analysed again for it.
    let with_gain = |gain_mid| GetWaveformRequest {
        track_id: 4,
        gain_mid: Some(gain_mid),
        ..Default::default()
    };
    let quieter = library
        .get_waveform(with_gain(0.5))
        .await
        .unwrap()
        .into_inner();
    let louder = library
        .get_waveform(with_gain(1.5))
        .await
        .unwrap()
        .into_inner();
    assert!(band_at_50(&quieter, 1) < band_at_50(&plain, 1));
    assert!(band_at_50(&louder, 1) > band_at_50(&plain, 1));
    assert_eq!(
        band_at_50(&louder, 0),
        band_at_50(&plain, 0),
        "the low band is untouched"
    );
    assert_eq!(
        band_at_50(&louder, 2),
        band_at_50(&plain, 2),
        "and so is the high band"
    );
    let loudest = library
        .get_waveform(with_gain(20.0))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(band_at_50(&loudest, 1), 255, "gain stops at full height");
    for gain in [0.0, -1.0, 21.0, f32::NAN] {
        assert_eq!(
            code(library.get_waveform(with_gain(gain)).await),
            Code::InvalidArgument,
            "gain {gain}"
        );
    }

    // Tracks without a usable file, and unknown ids.
    assert_eq!(
        code(
            library
                .get_waveform(GetWaveformRequest {
                    track_id: 0,
                    ..Default::default()
                })
                .await
        ),
        Code::NotFound
    );
    assert_eq!(
        code(
            library
                .get_waveform(GetWaveformRequest {
                    track_id: 2,
                    ..Default::default()
                })
                .await
        ),
        Code::FailedPrecondition
    );
    assert_eq!(
        code(
            library
                .get_waveform(GetWaveformRequest {
                    track_id: 99,
                    ..Default::default()
                })
                .await
        ),
        Code::InvalidArgument
    );
}

/// Waveforms and beatgrids of a real library's first tracks, with timings: `OPEN_ENGINE_TEST_LIBRARY=...
/// cargo test -p open-engine-backend --test grpc waveforms -- --ignored --nocapture`. Silent.
#[tokio::test]
#[ignore = "needs a real library (OPEN_ENGINE_TEST_LIBRARY)"]
async fn serves_waveforms_from_a_real_library() {
    let path = std::env::var("OPEN_ENGINE_TEST_LIBRARY").expect("OPEN_ENGINE_TEST_LIBRARY");
    let temp = TempDir::new("real-waveforms");
    let running = start(&temp.0.join("cache"));
    let channel = channel(&running).await;
    let mut library =
        LibraryServiceClient::new(InterceptedService::new(channel, with_token as fn(_) -> _));
    library
        .open_library(OpenLibraryRequest { path })
        .await
        .unwrap();

    let mut stream = library
        .list_tracks(ListTracksRequest {})
        .await
        .unwrap()
        .into_inner();
    let mut tracks = Vec::new();
    while let Some(batch) = stream.message().await.unwrap() {
        tracks.extend(batch.tracks);
    }

    let (mut done, mut with_grid, mut missing) = (0, 0, 0);
    for track in tracks.iter().filter(|track| !track.streaming).take(25) {
        let started = Instant::now();
        match library
            .get_waveform(GetWaveformRequest {
                track_id: track.id,
                ..Default::default()
            })
            .await
        {
            Ok(response) => {
                let response = response.into_inner();
                done += 1;
                let columns = response.columns.len() / 3;
                let expected = (f64::from(track.length_seconds.unwrap_or(0)) * 100.0) as usize;
                assert!(
                    columns.abs_diff(expected) < 300,
                    "{columns} columns for {expected} expected"
                );
                let grid = response.beat_grid.as_ref().map(|grid| {
                    let (a, b) = (grid.markers.first().unwrap(), grid.markers.last().unwrap());
                    60.0 * (b.beat_number - a.beat_number) as f64
                        / (b.position_seconds - a.position_seconds)
                });
                with_grid += usize::from(grid.is_some());

                // How the overview (the track pooled down to 1200 columns) would look: the
                // average height of each band, pooling by the loudest value of a group
                // against by its RMS.
                let bands: Vec<[f64; 3]> = response
                    .columns
                    .chunks(3)
                    .map(|c| [f64::from(c[0]), f64::from(c[1]), f64::from(c[2])])
                    .collect();
                let (mut by_max, mut by_rms) = ([0.0f64; 3], [0.0f64; 3]);
                for bin in 0..1200 {
                    let (from, to) = (
                        bin * bands.len() / 1200,
                        ((bin + 1) * bands.len() / 1200).max(bin * bands.len() / 1200 + 1),
                    );
                    for band in 0..3 {
                        let group = &bands[from..to.min(bands.len())];
                        by_max[band] += group.iter().map(|c| c[band]).fold(0.0, f64::max) / 1200.0;
                        by_rms[band] += (group.iter().map(|c| c[band] * c[band]).sum::<f64>()
                            / group.len() as f64)
                            .sqrt()
                            / 1200.0;
                    }
                }
                println!(
                    "        overview average low/mid/high: by max {:.0}/{:.0}/{:.0}, by rms {:.0}/{:.0}/{:.0}",
                    by_max[0], by_max[1], by_max[2], by_rms[0], by_rms[1], by_rms[2]
                );

                // Do the beats of the grid land on the music? Fold the low band's onsets (how
                // much louder it gets within 30 ms) over one beat: where the kicks hit shows
                // as the strongest phase, and should be at 0 ms, right on the beat.
                if let Some(grid) = response.beat_grid.as_ref() {
                    let low = |column: usize| f64::from(response.columns[column * 3]);
                    let (first, last) =
                        (grid.markers.first().unwrap(), grid.markers.last().unwrap());
                    let spacing = (last.position_seconds - first.position_seconds)
                        / (last.beat_number - first.beat_number) as f64;
                    let bins = (spacing * 100.0).round() as usize; // one per 10 ms
                    let mut onset = vec![0.0f64; bins];
                    for beat in 8..(response.duration_seconds / spacing) as usize - 8 {
                        for (bin, onset) in onset.iter_mut().enumerate() {
                            let column = ((first.position_seconds + beat as f64 * spacing) * 100.0)
                                .round() as usize
                                + bin;
                            if column >= 3 && column < columns {
                                *onset += (low(column) - low(column - 3)).max(0.0);
                            }
                        }
                    }
                    let best = (0..bins)
                        .max_by(|&a, &b| onset[a].total_cmp(&onset[b]))
                        .unwrap();
                    // Wrapped to -half..+half a beat, in ms; a kick just before the grid line
                    // shows up as the end of the beat.
                    let ms = if best * 2 > bins {
                        best as i64 - bins as i64
                    } else {
                        best as i64
                    } * 10;
                    let mean = onset.iter().sum::<f64>() / bins as f64;
                    println!(
                        "        kick onsets are strongest {ms:>4} ms from the beat (x{:.1} the average)",
                        onset[best] / mean
                    );
                }
                println!(
                    "{:>6.0?}  {:>5} s  library bpm {:>6.1?}  grid bpm {:>6.1?}  markers {}",
                    started.elapsed(),
                    response.duration_seconds as u32,
                    track.bpm,
                    grid,
                    response.beat_grid.map_or(0, |grid| grid.markers.len()),
                );
            }
            Err(status) => {
                assert_eq!(status.code(), Code::NotFound, "{status}");
                missing += 1;
            }
        }
    }
    println!("{done} waveforms ({with_grid} with a beatgrid), {missing} files not on the drive");
}

#[tokio::test]
async fn saves_the_library_to_a_new_place_and_moves_there() {
    let temp = TempDir::new("save-as");
    let library_dir = fixture_library(&temp.0);
    let source_before = std::fs::read(library_dir.join("Database2/m.db")).unwrap();
    let running = start(&temp.0.join("cache"));
    let channel = channel(&running).await;
    let mut library =
        LibraryServiceClient::new(InterceptedService::new(channel, with_token as fn(_) -> _));

    let save = |path: &Path| SaveLibraryAsRequest {
        path: path.display().to_string(),
    };
    let elsewhere = temp.0.join("elsewhere");
    assert_eq!(
        code(library.save_library_as(save(&elsewhere)).await),
        Code::FailedPrecondition,
        "nothing to save"
    );

    let opened = library
        .open_library(OpenLibraryRequest {
            path: temp.0.display().to_string(),
        })
        .await
        .unwrap()
        .into_inner();
    let track_path = |track: &open_engine_api::v1::Track| track.path.clone();
    let mut stream = library
        .list_tracks(ListTracksRequest {})
        .await
        .unwrap()
        .into_inner();
    let before: Vec<_> = stream
        .message()
        .await
        .unwrap()
        .unwrap()
        .tracks
        .iter()
        .map(track_path)
        .collect();
    assert_eq!(before[0], "../Music/one.flac");

    // Saving to the library's own place, or where one already is, is refused...
    assert_eq!(
        code(library.save_library_as(save(&temp.0)).await),
        Code::InvalidArgument
    );
    assert_eq!(
        code(library.save_library_as(save(&library_dir)).await),
        Code::InvalidArgument
    );

    // ...saving somewhere else clones it there and moves the library there.
    let saved = library
        .save_library_as(save(&elsewhere))
        .await
        .unwrap()
        .into_inner();
    let new_dir = elsewhere.join("Engine Library");
    assert_eq!(Path::new(&saved.path), new_dir);
    assert!(new_dir.join("Database2/m.db").is_file());
    assert_ne!(
        saved.cache_path, opened.cache_path,
        "it is a new place, so a new working copy"
    );
    assert!(
        Path::new(&saved.cache_path)
            .join("Database2/m.db")
            .is_file()
    );

    // The tracks are the same, in the same order, but their paths now start from the new
    // folder and lead to the same files.
    let mut stream = library
        .list_tracks(ListTracksRequest {})
        .await
        .unwrap()
        .into_inner();
    let after: Vec<_> = stream
        .message()
        .await
        .unwrap()
        .unwrap()
        .tracks
        .iter()
        .map(track_path)
        .collect();
    assert_eq!(after.len(), before.len());
    assert_eq!(after[0], "../../Music/one.flac");
    assert_eq!(after[1], "../../Music/two.mp3");
    assert_eq!(after[2], before[2], "a streaming track keeps its path");

    // The old place was only read from.
    assert_eq!(
        std::fs::read(library_dir.join("Database2/m.db")).unwrap(),
        source_before
    );

    // Where the library is now can't be saved to again, nor can any place with a library.
    assert_eq!(
        code(library.save_library_as(save(&elsewhere)).await),
        Code::InvalidArgument
    );
    assert_eq!(
        code(library.save_library_as(save(&new_dir)).await),
        Code::InvalidArgument
    );
    let again = library
        .save_library_as(save(&temp.0.join("third")))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(Path::new(&again.path), temp.0.join("third/Engine Library"));
    assert_eq!(
        code(library.save_library_as(save(&elsewhere)).await),
        Code::AlreadyExists
    );
}

/// Saves a real library to a temporary place and checks that the tracks of the copy lead to
/// the same files: `OPEN_ENGINE_TEST_LIBRARY=... cargo test -p open-engine-backend --test
/// grpc saves_a_real -- --ignored --nocapture`. Only reads the real library.
#[tokio::test]
#[ignore = "needs a real library (OPEN_ENGINE_TEST_LIBRARY)"]
async fn saves_a_real_library() {
    let path = std::env::var("OPEN_ENGINE_TEST_LIBRARY").expect("OPEN_ENGINE_TEST_LIBRARY");
    let temp = TempDir::new("save-real");
    let running = start(&temp.0.join("cache"));
    let channel = channel(&running).await;
    let mut library =
        LibraryServiceClient::new(InterceptedService::new(channel, with_token as fn(_) -> _));

    let opened = library
        .open_library(OpenLibraryRequest { path })
        .await
        .unwrap()
        .into_inner();
    let list = |mut stream: tonic::Streaming<open_engine_api::v1::TrackBatch>| async move {
        let mut tracks = Vec::new();
        while let Some(batch) = stream.message().await.unwrap() {
            tracks.extend(batch.tracks);
        }
        tracks
    };
    let before = list(
        library
            .list_tracks(ListTracksRequest {})
            .await
            .unwrap()
            .into_inner(),
    )
    .await;

    let started = Instant::now();
    let saved = library
        .save_library_as(SaveLibraryAsRequest {
            path: temp.0.join("clone").display().to_string(),
        })
        .await
        .unwrap()
        .into_inner();
    println!(
        "saved {} tracks to {} in {:?}",
        before.len(),
        saved.path,
        started.elapsed()
    );

    let after = list(
        library
            .list_tracks(ListTracksRequest {})
            .await
            .unwrap()
            .into_inner(),
    )
    .await;
    assert_eq!(before.len(), after.len());

    // Each track of the copy leads, from the copy's folder, to the file the original did
    // from the original folder.
    let resolve = |dir: &str, relative: &str| {
        let mut resolved = PathBuf::new();
        for part in Path::new(dir).join(relative).components() {
            match part {
                std::path::Component::ParentDir => {
                    resolved.pop();
                }
                std::path::Component::CurDir => {}
                other => resolved.push(other.as_os_str()),
            }
        }
        resolved
    };
    let (mut same, mut streaming) = (0, 0);
    for (old, new) in before.iter().zip(&after) {
        assert_eq!(
            (old.id, &old.title),
            (new.id, &new.title),
            "same tracks in the same order"
        );
        if old.streaming {
            assert_eq!(old.path, new.path);
            streaming += 1;
        } else {
            assert_eq!(
                resolve(&opened.path, &old.path),
                resolve(&saved.path, &new.path),
                "{}",
                old.path
            );
            same += 1;
        }
    }
    let artwork =
        std::fs::read_dir(Path::new(&saved.path).join("Artwork")).map_or(0, |dir| dir.count());
    println!(
        "{same} tracks lead to the same file, {streaming} streaming tracks untouched, {artwork} cover files copied"
    );
    assert!(same > 0);
}

#[tokio::test]
async fn shows_what_syncing_would_change_and_writes_the_working_copy_to_the_source() {
    let temp = TempDir::new("sync");
    let library_dir = fixture_library(&temp.0);
    let running = start(&temp.0.join("cache"));
    let channel = channel(&running).await;
    let mut library =
        LibraryServiceClient::new(InterceptedService::new(channel, with_token as fn(_) -> _));

    assert_eq!(
        code(library.get_sync_diff(GetSyncDiffRequest::default()).await),
        Code::FailedPrecondition
    );
    assert_eq!(
        code(library.sync_to_source(SyncToSourceRequest::default()).await),
        Code::FailedPrecondition
    );

    let opened = library
        .open_library(OpenLibraryRequest {
            path: temp.0.display().to_string(),
        })
        .await
        .unwrap()
        .into_inner();
    let diff = library
        .get_sync_diff(GetSyncDiffRequest::default())
        .await
        .unwrap()
        .into_inner();
    assert!(diff.source_available && !diff.source_changed_outside && !diff.source_busy);
    assert!(
        diff.changed_databases.is_empty(),
        "a fresh working copy is the source"
    );

    // The working copy is changed (as editing it will).
    let working = Path::new(&opened.cache_path).join("Database2/m.db");
    rusqlite::Connection::open(&working)
        .unwrap()
        .execute_batch(
            "UPDATE Track SET title = 'Renamed' WHERE id = 1; DELETE FROM Track WHERE id = 3;",
        )
        .unwrap();

    let diff = library
        .get_sync_diff(GetSyncDiffRequest::default())
        .await
        .unwrap()
        .into_inner();
    assert_eq!(diff.changed_databases, ["m.db"]);
    assert_eq!(
        (diff.tracks_added, diff.tracks_removed, diff.tracks_changed),
        (0, 1, 1)
    );
    assert_eq!(diff.removed_tracks[0].title, "Three");
    let change = &diff.changed_tracks[0];
    assert_eq!(change.fields[0].field, "title");
    assert_eq!(change.fields[0].source_value, "One");
    assert_eq!(change.fields[0].working_value, "Renamed");

    // Syncing writes it, and keeps what was there.
    let source = library_dir.join("Database2/m.db");
    let old_source = std::fs::read(&source).unwrap();
    let done = library
        .sync_to_source(SyncToSourceRequest::default())
        .await
        .unwrap()
        .into_inner();
    assert_eq!(done.files_written, 1);
    assert_eq!(
        std::fs::read(&source).unwrap(),
        std::fs::read(&working).unwrap()
    );
    assert_eq!(
        std::fs::read(Path::new(&done.backup_path).join("m.db")).unwrap(),
        old_source
    );

    let after = library
        .get_sync_diff(GetSyncDiffRequest::default())
        .await
        .unwrap()
        .into_inner();
    assert!(after.changed_databases.is_empty() && !after.source_changed_outside);

    // Someone else changes the source: that is noticed, and not overwritten unless meant.
    rusqlite::Connection::open(&source)
        .unwrap()
        .execute_batch("UPDATE Track SET title = 'Theirs' WHERE id = 2;")
        .unwrap();
    let outside = library
        .get_sync_diff(GetSyncDiffRequest::default())
        .await
        .unwrap()
        .into_inner();
    assert!(outside.source_changed_outside);
    assert_eq!(
        code(library.sync_to_source(SyncToSourceRequest::default()).await),
        Code::FailedPrecondition
    );
    let theirs = std::fs::read(&source).unwrap();
    assert_eq!(
        std::fs::read(&source).unwrap(),
        theirs,
        "refusing changes nothing"
    );
    let forced = library
        .sync_to_source(SyncToSourceRequest {
            overwrite_outside_changes: true,
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(forced.files_written, 1);
    assert_eq!(
        std::fs::read(&source).unwrap(),
        std::fs::read(&working).unwrap()
    );
}

/// Diffs a real library against its working copy, after changing the working copy in a
/// temporary cache (the real library is only read): `OPEN_ENGINE_TEST_LIBRARY=... cargo test
/// -p open-engine-backend --test grpc diffs_a_real -- --ignored --nocapture`.
#[tokio::test]
#[ignore = "needs a real library (OPEN_ENGINE_TEST_LIBRARY)"]
async fn diffs_a_real_library() {
    let path = std::env::var("OPEN_ENGINE_TEST_LIBRARY").expect("OPEN_ENGINE_TEST_LIBRARY");
    let temp = TempDir::new("diff-real");
    let running = start(&temp.0.join("cache"));
    let channel = channel(&running).await;
    let mut library =
        LibraryServiceClient::new(InterceptedService::new(channel, with_token as fn(_) -> _));
    let opened = library
        .open_library(OpenLibraryRequest { path })
        .await
        .unwrap()
        .into_inner();

    let started = Instant::now();
    let same = library
        .get_sync_diff(GetSyncDiffRequest::default())
        .await
        .unwrap()
        .into_inner();
    println!(
        "unchanged: {:?} databases differ, compared in {:?}",
        same.changed_databases,
        started.elapsed()
    );
    assert!(same.source_available && same.changed_databases.is_empty());

    // Edit the working copy: a title, a rating, a beatgrid, a track gone, a playlist renamed,
    // a track out of a playlist.
    let working = Path::new(&opened.cache_path).join("Database2/m.db");
    let db = rusqlite::Connection::open(&working).unwrap();
    db.execute_batch(
        "PRAGMA foreign_keys = OFF;
         UPDATE Track SET title = title || ' (edited)', rating = 100 WHERE id = (SELECT min(id) FROM Track);
         UPDATE PerformanceData SET beatData = x'00' WHERE trackId = (SELECT min(id) + 1 FROM Track);
         DELETE FROM Track WHERE id = (SELECT max(id) FROM Track);
         UPDATE Playlist SET title = title || ' 2' WHERE id = (SELECT min(id) FROM Playlist);
         DELETE FROM PlaylistEntity WHERE id = (SELECT min(id) FROM PlaylistEntity);",
    )
    .unwrap();
    drop(db);

    let started = Instant::now();
    let diff = library
        .get_sync_diff(GetSyncDiffRequest::default())
        .await
        .unwrap()
        .into_inner();
    println!(
        "edited: databases {:?}; tracks +{} -{} ~{}; {} playlists changed; compared in {:?}",
        diff.changed_databases,
        diff.tracks_added,
        diff.tracks_removed,
        diff.tracks_changed,
        diff.playlists_changed,
        started.elapsed()
    );
    for change in diff.changed_tracks.iter().take(3) {
        let fields: Vec<_> = change.fields.iter().map(|f| f.field.as_str()).collect();
        println!(
            "  ~ {:?}: {fields:?}",
            change.track.as_ref().map(|t| &t.title)
        );
    }
    for change in diff.changed_playlists.iter().take(3) {
        println!(
            "  playlist {:?} {:?} {:?} +{} -{}",
            change.name,
            change.kind(),
            change.detail,
            change.tracks_added,
            change.tracks_removed
        );
    }
    assert_eq!(diff.changed_databases, ["m.db"]);
    assert_eq!(
        (diff.tracks_added, diff.tracks_removed, diff.tracks_changed),
        (0, 1, 2)
    );
    assert!(
        diff.changed_playlists
            .iter()
            .any(|p| p.kind() == open_engine_api::v1::PlaylistChangeKind::Renamed)
    );
    assert!(
        !diff.source_changed_outside,
        "the source itself was not touched"
    );
}

/// Ratings and SoundSwitch lighting of a real library's tracks, counted:
/// `OPEN_ENGINE_TEST_LIBRARY=... cargo test -p open-engine-backend --test grpc ratings -- --ignored --nocapture`
#[tokio::test]
#[ignore = "needs a real library (OPEN_ENGINE_TEST_LIBRARY)"]
async fn reads_ratings_and_soundswitch_lighting_from_a_real_library() {
    use open_engine_api::v1::SoundSwitchState;
    let path = std::env::var("OPEN_ENGINE_TEST_LIBRARY").expect("OPEN_ENGINE_TEST_LIBRARY");
    let temp = TempDir::new("ratings");
    let running = start(&temp.0.join("cache"));
    let channel = channel(&running).await;
    let mut library =
        LibraryServiceClient::new(InterceptedService::new(channel, with_token as fn(_) -> _));
    library
        .open_library(OpenLibraryRequest { path })
        .await
        .unwrap();

    let mut stream = library
        .list_tracks(ListTracksRequest {})
        .await
        .unwrap()
        .into_inner();
    let mut tracks = Vec::new();
    while let Some(batch) = stream.message().await.unwrap() {
        tracks.extend(batch.tracks);
    }

    let mut stars = std::collections::BTreeMap::new();
    for track in &tracks {
        *stars
            .entry(track.rating.map(|r| (r + 10) / 20))
            .or_insert(0) += 1;
    }
    println!("stars (None = no rating): {stars:?}");

    let local: Vec<_> = tracks.iter().filter(|track| !track.streaming).collect();
    let count = |state: SoundSwitchState| local.iter().filter(|t| t.soundswitch() == state).count();
    println!(
        "{} local tracks: lighting rendered {}, analysed only {}, not in SoundSwitch {}; streaming tracks with a state: {}",
        local.len(),
        count(SoundSwitchState::Rendered),
        count(SoundSwitchState::Analysed),
        count(SoundSwitchState::Unspecified),
        tracks
            .iter()
            .filter(|t| t.streaming && t.soundswitch() != SoundSwitchState::Unspecified)
            .count(),
    );
    let projects: std::collections::BTreeSet<_> = local
        .iter()
        .flat_map(|t| t.soundswitch_projects.iter().cloned())
        .collect();
    println!("projects with lighting: {projects:?}");
    for track in local
        .iter()
        .filter(|t| t.soundswitch() == SoundSwitchState::Rendered)
        .take(3)
    {
        println!(
            "  rendered: {} — {} ({})",
            track.title, track.artist, track.path
        );
    }
    for track in local
        .iter()
        .filter(|t| t.soundswitch() == SoundSwitchState::Unspecified)
        .take(3)
    {
        println!(
            "  not in SoundSwitch: {} — {} ({})",
            track.title, track.artist, track.path
        );
    }
}

/// A SoundSwitch track map record: the path, then the marker and GUID of the track's data.
fn soundswitch_record(path: &str, guid: [u8; 16]) -> Vec<u8> {
    let mut record = vec![1, 0, 0, 0];
    record.extend((path.encode_utf16().count() as u32 + 1).to_le_bytes());
    record.extend(path.encode_utf16().flat_map(u16::to_le_bytes));
    record.extend([0, 0]);
    record.extend([
        0x80, 0, 0, 0, 0xf8, 0x18, 3, 0, 1, 2, 0, 0, 0, 1, 1, 0, 0, 0, 0x40, 0xe4,
    ]);
    record.extend([4, 0, 0, 0, 1]);
    record.extend(guid);
    record.extend([1, 0, 0, 0]);
    record
}

#[tokio::test]
async fn serves_ratings_and_whether_soundswitch_has_lighting_for_a_track() {
    use open_engine_api::v1::SoundSwitchState;

    let temp = TempDir::new("soundswitch");
    let library_dir = fixture_library(&temp.0);
    // Stars: three for the first track, a value from another system for the second.
    {
        let db = rusqlite::Connection::open(library_dir.join("Database2/m.db")).unwrap();
        db.execute_batch(
            "ALTER TABLE Track ADD COLUMN rating INTEGER;
             UPDATE Track SET rating = 60 WHERE id = 1;
             UPDATE Track SET rating = 27 WHERE id = 2;",
        )
        .unwrap();
    }
    // SoundSwitch has the drive's first track with lighting, the second only analysed.
    let project = temp.0.join("SoundSwitch/Venue.ssproj");
    std::fs::create_dir_all(&project).unwrap();
    let (lit, analysed) = (
        [
            0x0f, 0x1b, 0x5f, 0x4c, 0xab, 0x59, 0x15, 0x47, 0xbe, 0xcc, 0x9c, 0x49, 0x87, 0x27,
            0xc9, 0xdd,
        ],
        [
            0x00, 0x47, 0xa4, 0xc0, 0xb3, 0x0f, 0x7c, 0x40, 0x87, 0x64, 0xb9, 0x76, 0xc5, 0x25,
            0x98, 0x16,
        ],
    );
    let mut map = vec![0xaa, 0xaa, 0x09, 0x55, 3, 0, 0, 0];
    // As SoundSwitch on Windows knows the files: by drive letter and backslashes.
    map.extend(soundswitch_record(r"F:\Music\one.flac", lit));
    map.extend(soundswitch_record(
        r"D:\Engine Library\..\Music\two.mp3",
        analysed,
    ));
    std::fs::write(project.join("SoundSwitchTrackMap.bin"), map).unwrap();
    std::fs::write(
        project.join("{4C5F1B0F-59AB-4715-BECC-9C498727C9DD}.ssfile"),
        b"x",
    )
    .unwrap();
    std::fs::write(
        project.join("{4C5F1B0F-59AB-4715-BECC-9C498727C9DD}.ssa"),
        b"x",
    )
    .unwrap();
    std::fs::write(
        project.join("{C0A44700-0FB3-407C-8764-B976C5259816}.ssfile"),
        b"x",
    )
    .unwrap();

    let running = start(&temp.0.join("cache"));
    let channel = channel(&running).await;
    let mut library =
        LibraryServiceClient::new(InterceptedService::new(channel, with_token as fn(_) -> _));
    library
        .open_library(OpenLibraryRequest {
            path: temp.0.display().to_string(),
        })
        .await
        .unwrap();
    let mut stream = library
        .list_tracks(ListTracksRequest {})
        .await
        .unwrap()
        .into_inner();
    let tracks = stream.message().await.unwrap().unwrap().tracks;

    assert_eq!(tracks[0].rating, Some(60));
    assert_eq!(tracks[1].rating, Some(27));
    assert_eq!(tracks[2].rating, None);

    assert_eq!(tracks[0].soundswitch(), SoundSwitchState::Rendered);
    assert_eq!(tracks[0].soundswitch_projects, ["Venue"]);
    assert_eq!(tracks[1].soundswitch(), SoundSwitchState::Analysed);
    assert!(tracks[1].soundswitch_projects.is_empty());
    assert_eq!(
        tracks[2].soundswitch(),
        SoundSwitchState::Unspecified,
        "not in SoundSwitch"
    );
}

#[tokio::test]
async fn sets_a_rating_in_the_working_copy_and_syncs_it_to_the_source() {
    let temp = TempDir::new("set-rating");
    let library_dir = fixture_library(&temp.0);
    {
        let db = rusqlite::Connection::open(library_dir.join("Database2/m.db")).unwrap();
        db.execute_batch(
            "ALTER TABLE Track ADD COLUMN rating INTEGER;
             UPDATE Track SET rating = 60 WHERE id = 1;",
        )
        .unwrap();
    }
    let running = start(&temp.0.join("cache"));
    let channel = channel(&running).await;
    let mut library =
        LibraryServiceClient::new(InterceptedService::new(channel, with_token as fn(_) -> _));

    let rate = |track_id, rating| SetRatingRequest { track_id, rating };
    assert_eq!(
        code(library.set_rating(rate(0, 20)).await),
        Code::FailedPrecondition
    );

    let path = temp.0.display().to_string();
    library
        .open_library(OpenLibraryRequest { path: path.clone() })
        .await
        .unwrap();
    assert_eq!(
        code(library.set_rating(rate(0, 101)).await),
        Code::InvalidArgument
    );
    assert_eq!(code(library.set_rating(rate(99, 20)).await), Code::NotFound);

    library.set_rating(rate(0, 100)).await.unwrap();
    library.set_rating(rate(2, 40)).await.unwrap();
    library.set_rating(rate(1, 0)).await.unwrap();
    let ratings = |tracks: Vec<open_engine_api::v1::Track>| {
        tracks.iter().map(|track| track.rating).collect::<Vec<_>>()
    };
    let mut stream = library
        .list_tracks(ListTracksRequest {})
        .await
        .unwrap()
        .into_inner();
    let listed = stream.message().await.unwrap().unwrap().tracks;
    assert_eq!(ratings(listed), [Some(100), Some(0), Some(40)]);

    // The source is untouched until syncing, which shows the change first.
    // (Syncing replaces the file, so each read opens it afresh.)
    let source_rating = |id: i64| -> Option<i64> {
        rusqlite::Connection::open(library_dir.join("Database2/m.db"))
            .unwrap()
            .query_row("SELECT rating FROM Track WHERE id = ?1", [id], |row| {
                row.get(0)
            })
            .unwrap()
    };
    assert_eq!(source_rating(1), Some(60));
    let diff = library
        .get_sync_diff(GetSyncDiffRequest::default())
        .await
        .unwrap()
        .into_inner();
    assert_eq!(diff.tracks_changed, 3);
    let first = diff
        .changed_tracks
        .iter()
        .find(|change| change.track.as_ref().unwrap().title == "One")
        .unwrap();
    assert_eq!(first.fields[0].field, "rating");
    assert_eq!(first.fields[0].source_value, "60");
    assert_eq!(first.fields[0].working_value, "100");

    library
        .sync_to_source(SyncToSourceRequest::default())
        .await
        .unwrap();
    assert_eq!(source_rating(1), Some(100));
    assert_eq!(source_rating(3), Some(40));

    // Reopening gives the same ratings back.
    library
        .open_library(OpenLibraryRequest { path })
        .await
        .unwrap();
    let mut stream = library
        .list_tracks(ListTracksRequest {})
        .await
        .unwrap()
        .into_inner();
    let reopened = stream.message().await.unwrap().unwrap().tracks;
    assert_eq!(ratings(reopened), [Some(100), Some(0), Some(40)]);
}

/// A zlib blob with the 4-byte length in front, the way Engine stores the analysis.
fn analysis_blob(data: &[u8]) -> Vec<u8> {
    use std::io::Write;
    let mut encoder = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
    encoder.write_all(data).unwrap();
    let mut blob = (data.len() as u32).to_be_bytes().to_vec();
    blob.extend(encoder.finish().unwrap());
    blob
}

/// `trackData` for 200 seconds at 44.1 kHz (only the start of it matters here).
fn track_data_blob() -> Vec<u8> {
    let mut data = 44_100f64.to_be_bytes().to_vec();
    data.extend((44_100i64 * 200).to_be_bytes());
    analysis_blob(&data)
}

/// `quickCues` with eight slots, the second one set (`Cue 2` at 20 s).
fn quick_cues_blob() -> Vec<u8> {
    let mut data = 8i64.to_be_bytes().to_vec();
    for slot in 0..8 {
        if slot == 1 {
            data.push(5);
            data.extend(b"Cue 2");
            data.extend((44_100f64 * 20.0).to_be_bytes());
            data.extend([0xff, 0xef, 0x81, 0x30]);
        } else {
            data.push(0);
            data.extend((-1f64).to_be_bytes());
            data.extend([0; 4]);
        }
    }
    data.extend((-1f64).to_be_bytes());
    data.push(0);
    data.extend((-1f64).to_be_bytes());
    analysis_blob(&data)
}

#[tokio::test]
async fn lists_and_edits_cues_in_the_working_copy_and_syncs_them_to_the_source() {
    let temp = TempDir::new("cues");
    let library_dir = fixture_library(&temp.0);
    {
        let db = rusqlite::Connection::open(library_dir.join("Database2/m.db")).unwrap();
        db.execute_batch(
            "CREATE TABLE PerformanceData (trackId INTEGER PRIMARY KEY, trackData BLOB,
                overviewWaveFormData BLOB, beatData BLOB, quickCues BLOB, loops BLOB);",
        )
        .unwrap();
        // Track 1 (id 0) has a cue, track 2 (id 1) has analysis but no cues, track 3 has none.
        db.execute(
            "INSERT INTO PerformanceData (trackId, trackData, quickCues) VALUES (1, ?1, ?2), (2, ?1, NULL)",
            (track_data_blob(), quick_cues_blob()),
        )
        .unwrap();
    }
    let running = start(&temp.0.join("cache"));
    let channel = channel(&running).await;
    let mut library =
        LibraryServiceClient::new(InterceptedService::new(channel, with_token as fn(_) -> _));
    let cues_of = |track_id| GetCuesRequest { track_id };
    assert_eq!(
        code(library.get_cues(cues_of(0)).await),
        Code::FailedPrecondition
    );

    library
        .open_library(OpenLibraryRequest {
            path: temp.0.display().to_string(),
        })
        .await
        .unwrap();
    let counts = |tracks: Vec<open_engine_api::v1::Track>| {
        tracks
            .iter()
            .map(|track| track.cue_count)
            .collect::<Vec<_>>()
    };
    let list = |mut library: LibraryServiceClient<Connection>| async move {
        let mut stream = library
            .list_tracks(ListTracksRequest {})
            .await
            .unwrap()
            .into_inner();
        stream.message().await.unwrap().unwrap().tracks
    };
    assert_eq!(counts(list(library.clone()).await), [1, 0, 0]);

    let cues = library
        .get_cues(cues_of(0))
        .await
        .unwrap()
        .into_inner()
        .cues;
    assert_eq!(cues.len(), 1);
    assert_eq!((cues[0].slot, cues[0].label.as_str()), (1, "Cue 2"));
    assert!((cues[0].position_seconds - 20.0).abs() < 1e-6);
    assert_eq!(cues[0].color_argb, 0xffef_8130);
    assert!(
        library
            .get_cues(cues_of(2))
            .await
            .unwrap()
            .into_inner()
            .cues
            .is_empty()
    );
    assert_eq!(code(library.get_cues(cues_of(9)).await), Code::NotFound);

    let cue = |slot, label: &str, position_seconds| Cue {
        slot,
        label: label.to_string(),
        position_seconds,
        color_argb: 0xfff4_d338,
    };
    let set = |track_id, cues| SetCuesRequest { track_id, cues };
    // Add a cue to track 1 next to its own and change the one it had; give track 2 two.
    let after = library
        .set_cues(set(0, vec![cue(0, "Intro", 1.5), cue(1, "Drop", 30.25)]))
        .await
        .unwrap()
        .into_inner()
        .cues;
    assert_eq!(
        after
            .iter()
            .map(|cue| cue.label.as_str())
            .collect::<Vec<_>>(),
        ["Intro", "Drop"]
    );
    assert_eq!(
        after,
        library
            .get_cues(cues_of(0))
            .await
            .unwrap()
            .into_inner()
            .cues
    );
    library
        .set_cues(set(1, vec![cue(7, "", 0.0), cue(3, "End", 200.0)]))
        .await
        .unwrap();
    assert_eq!(counts(list(library.clone()).await), [2, 2, 0]);

    // What can't be saved is refused and changes nothing.
    for cues in [
        vec![cue(8, "", 1.0)],
        vec![cue(0, "", 1.0), cue(0, "", 2.0)],
        vec![cue(0, "", -1.0)],
        vec![cue(0, "", f64::NAN)],
        vec![cue(0, "", 200.5)],
        vec![cue(0, &"x".repeat(256), 1.0)],
    ] {
        assert_eq!(
            code(library.set_cues(set(1, cues)).await),
            Code::InvalidArgument
        );
    }
    assert_eq!(
        code(library.set_cues(set(2, vec![cue(0, "", 1.0)])).await),
        Code::FailedPrecondition,
        "track 3 has no analysis"
    );
    assert_eq!(code(library.set_cues(set(9, vec![])).await), Code::NotFound);
    assert_eq!(counts(list(library.clone()).await), [2, 2, 0]);

    // Removing is sending fewer.
    library
        .set_cues(set(1, vec![cue(3, "End", 200.0)]))
        .await
        .unwrap();
    assert_eq!(counts(list(library.clone()).await), [2, 1, 0]);

    // The source is untouched until syncing, which shows the change first.
    let source_blob = |id: i64| -> Option<Vec<u8>> {
        rusqlite::Connection::open(library_dir.join("Database2/m.db"))
            .unwrap()
            .query_row(
                "SELECT quickCues FROM PerformanceData WHERE trackId = ?1",
                [id],
                |row| row.get(0),
            )
            .unwrap()
    };
    assert_eq!(source_blob(1), Some(quick_cues_blob()));
    assert_eq!(source_blob(2), None);
    let diff = library
        .get_sync_diff(GetSyncDiffRequest::default())
        .await
        .unwrap()
        .into_inner();
    assert_eq!(diff.tracks_changed, 2);
    assert!(
        diff.changed_tracks
            .iter()
            .all(|change| change.fields.iter().any(|field| field.field == "cues"))
    );

    library
        .sync_to_source(SyncToSourceRequest::default())
        .await
        .unwrap();
    assert_ne!(source_blob(1), Some(quick_cues_blob()));

    // Reopening gives the same cues back.
    library
        .open_library(OpenLibraryRequest {
            path: temp.0.display().to_string(),
        })
        .await
        .unwrap();
    assert_eq!(counts(list(library.clone()).await), [2, 1, 0]);
    let cues = library
        .get_cues(cues_of(0))
        .await
        .unwrap()
        .into_inner()
        .cues;
    assert_eq!(
        (cues[0].label.as_str(), cues[0].position_seconds),
        ("Intro", 1.5)
    );
    assert_eq!(cues[1].position_seconds, 30.25);
}

/// The cues of a real library, counted:
/// `OPEN_ENGINE_TEST_LIBRARY=... cargo test -p open-engine-backend --test grpc cues_of_a_real -- --ignored --nocapture`
#[tokio::test]
#[ignore = "needs a real library (OPEN_ENGINE_TEST_LIBRARY)"]
async fn reads_the_cues_of_a_real_library() {
    let path = std::env::var("OPEN_ENGINE_TEST_LIBRARY").expect("OPEN_ENGINE_TEST_LIBRARY");
    let temp = TempDir::new("real-cues");
    let running = start(&temp.0.join("cache"));
    let channel = channel(&running).await;
    let mut library =
        LibraryServiceClient::new(InterceptedService::new(channel, with_token as fn(_) -> _));
    library
        .open_library(OpenLibraryRequest { path })
        .await
        .unwrap();
    let mut stream = library
        .list_tracks(ListTracksRequest {})
        .await
        .unwrap()
        .into_inner();
    let mut tracks = Vec::new();
    while let Some(batch) = stream.message().await.unwrap() {
        tracks.extend(batch.tracks);
    }
    let mut by_count = std::collections::BTreeMap::new();
    for track in &tracks {
        *by_count.entry(track.cue_count).or_insert(0) += 1;
    }
    println!("tracks by number of cues: {by_count:?}");
    for track in tracks.iter().filter(|track| track.cue_count > 0).take(5) {
        let cues = library
            .get_cues(GetCuesRequest { track_id: track.id })
            .await
            .unwrap()
            .into_inner()
            .cues;
        assert_eq!(cues.len() as u32, track.cue_count);
        println!("{} — {}:", track.title, track.length_seconds());
        for cue in cues {
            println!(
                "  slot {} {:?} at {:.2} s, color {:08x}",
                cue.slot + 1,
                cue.label,
                cue.position_seconds,
                cue.color_argb
            );
        }
    }
}

/// `loops` with eight slots, the second one set (`Loop 2` from 20 s to 24 s). Not compressed.
fn loops_blob() -> Vec<u8> {
    let mut data = 8i64.to_le_bytes().to_vec();
    for slot in 0..8 {
        if slot == 1 {
            data.push(6);
            data.extend(b"Loop 2");
            data.extend((44_100f64 * 20.0).to_le_bytes());
            data.extend((44_100f64 * 24.0).to_le_bytes());
            data.extend([1, 1]);
            data.extend([0xff, 0xef, 0x81, 0x30]);
        } else {
            data.push(0);
            data.extend((-1f64).to_le_bytes());
            data.extend((-1f64).to_le_bytes());
            data.extend([0, 0]);
            data.extend([0; 4]);
        }
    }
    data
}

#[tokio::test]
async fn lists_and_edits_loops_in_the_working_copy_and_syncs_them_to_the_source() {
    let temp = TempDir::new("loops");
    let library_dir = fixture_library(&temp.0);
    {
        let db = rusqlite::Connection::open(library_dir.join("Database2/m.db")).unwrap();
        db.execute_batch(
            "CREATE TABLE PerformanceData (trackId INTEGER PRIMARY KEY, trackData BLOB,
                overviewWaveFormData BLOB, beatData BLOB, quickCues BLOB, loops BLOB);",
        )
        .unwrap();
        // Track 1 (id 0) has a loop, track 2 (id 1) has analysis but no loops, track 3 has none.
        db.execute(
            "INSERT INTO PerformanceData (trackId, trackData, loops) VALUES (1, ?1, ?2), (2, ?1, NULL)",
            (track_data_blob(), loops_blob()),
        )
        .unwrap();
    }
    let running = start(&temp.0.join("cache"));
    let channel = channel(&running).await;
    let mut library =
        LibraryServiceClient::new(InterceptedService::new(channel, with_token as fn(_) -> _));
    let loops_of = |track_id| GetLoopsRequest { track_id };
    assert_eq!(
        code(library.get_loops(loops_of(0)).await),
        Code::FailedPrecondition
    );

    library
        .open_library(OpenLibraryRequest {
            path: temp.0.display().to_string(),
        })
        .await
        .unwrap();
    let counts = |tracks: Vec<open_engine_api::v1::Track>| {
        tracks
            .iter()
            .map(|track| track.loop_count)
            .collect::<Vec<_>>()
    };
    let list = |mut library: LibraryServiceClient<Connection>| async move {
        let mut stream = library
            .list_tracks(ListTracksRequest {})
            .await
            .unwrap()
            .into_inner();
        stream.message().await.unwrap().unwrap().tracks
    };
    assert_eq!(counts(list(library.clone()).await), [1, 0, 0]);

    let loops = library
        .get_loops(loops_of(0))
        .await
        .unwrap()
        .into_inner()
        .loops;
    assert_eq!(loops.len(), 1);
    assert_eq!((loops[0].slot, loops[0].label.as_str()), (1, "Loop 2"));
    assert!((loops[0].start_seconds - 20.0).abs() < 1e-6);
    assert!((loops[0].end_seconds - 24.0).abs() < 1e-6);
    assert_eq!(loops[0].color_argb, 0xffef_8130);
    assert_eq!(code(library.get_loops(loops_of(9)).await), Code::NotFound);

    let saved = |slot, label: &str, start_seconds, end_seconds| Loop {
        slot,
        label: label.to_string(),
        start_seconds,
        end_seconds,
        color_argb: 0xfff4_d338,
    };
    let set = |track_id, loops| SetLoopsRequest { track_id, loops };
    let after = library
        .set_loops(set(
            0,
            vec![saved(0, "Intro", 1.5, 3.0), saved(1, "Drop", 30.0, 38.0)],
        ))
        .await
        .unwrap()
        .into_inner()
        .loops;
    assert_eq!(
        after
            .iter()
            .map(|saved| saved.label.as_str())
            .collect::<Vec<_>>(),
        ["Intro", "Drop"]
    );
    assert_eq!(
        after,
        library
            .get_loops(loops_of(0))
            .await
            .unwrap()
            .into_inner()
            .loops
    );
    library
        .set_loops(set(
            1,
            vec![saved(7, "", 0.0, 1.0), saved(3, "End", 190.0, 200.0)],
        ))
        .await
        .unwrap();
    assert_eq!(counts(list(library.clone()).await), [2, 2, 0]);

    for loops in [
        vec![saved(8, "", 1.0, 2.0)],
        vec![saved(0, "", 1.0, 2.0), saved(0, "", 3.0, 4.0)],
        vec![saved(0, "", -1.0, 2.0)],
        vec![saved(0, "", 1.0, f64::NAN)],
        vec![saved(0, "", 2.0, 2.0)],
        vec![saved(0, "", 3.0, 2.0)],
        vec![saved(0, "", 199.0, 200.5)],
        vec![saved(0, &"x".repeat(256), 1.0, 2.0)],
    ] {
        assert_eq!(
            code(library.set_loops(set(1, loops)).await),
            Code::InvalidArgument
        );
    }
    assert_eq!(
        code(
            library
                .set_loops(set(2, vec![saved(0, "", 1.0, 2.0)]))
                .await
        ),
        Code::FailedPrecondition,
        "track 3 has no analysis"
    );
    assert_eq!(
        code(library.set_loops(set(9, vec![])).await),
        Code::NotFound
    );
    assert_eq!(counts(list(library.clone()).await), [2, 2, 0]);

    // Removing is sending fewer.
    library
        .set_loops(set(1, vec![saved(3, "End", 190.0, 200.0)]))
        .await
        .unwrap();
    assert_eq!(counts(list(library.clone()).await), [2, 1, 0]);

    // The source is untouched until syncing, which shows the change first.
    let source_blob = |id: i64| -> Option<Vec<u8>> {
        rusqlite::Connection::open(library_dir.join("Database2/m.db"))
            .unwrap()
            .query_row(
                "SELECT loops FROM PerformanceData WHERE trackId = ?1",
                [id],
                |row| row.get(0),
            )
            .unwrap()
    };
    assert_eq!(source_blob(1), Some(loops_blob()));
    assert_eq!(source_blob(2), None);
    let diff = library
        .get_sync_diff(GetSyncDiffRequest::default())
        .await
        .unwrap()
        .into_inner();
    assert_eq!(diff.tracks_changed, 2);
    assert!(
        diff.changed_tracks
            .iter()
            .all(|change| change.fields.iter().any(|field| field.field == "loops"))
    );

    library
        .sync_to_source(SyncToSourceRequest::default())
        .await
        .unwrap();
    assert_ne!(source_blob(1), Some(loops_blob()));

    // Reopening gives the same loops back.
    library
        .open_library(OpenLibraryRequest {
            path: temp.0.display().to_string(),
        })
        .await
        .unwrap();
    assert_eq!(counts(list(library.clone()).await), [2, 1, 0]);
    let loops = library
        .get_loops(loops_of(0))
        .await
        .unwrap()
        .into_inner()
        .loops;
    assert_eq!(
        (
            loops[0].label.as_str(),
            loops[0].start_seconds,
            loops[0].end_seconds
        ),
        ("Intro", 1.5, 3.0)
    );
    assert_eq!(loops[1].start_seconds, 30.0);
}

/// The loops of a real library, counted:
/// `OPEN_ENGINE_TEST_LIBRARY=... cargo test -p open-engine-backend --test grpc reads_the_loops -- --ignored --nocapture`
#[tokio::test]
#[ignore = "needs a real library (OPEN_ENGINE_TEST_LIBRARY)"]
async fn reads_the_loops_of_a_real_library() {
    let path = std::env::var("OPEN_ENGINE_TEST_LIBRARY").expect("OPEN_ENGINE_TEST_LIBRARY");
    let temp = TempDir::new("real-loops");
    let running = start(&temp.0.join("cache"));
    let channel = channel(&running).await;
    let mut library =
        LibraryServiceClient::new(InterceptedService::new(channel, with_token as fn(_) -> _));
    library
        .open_library(OpenLibraryRequest { path })
        .await
        .unwrap();
    let mut stream = library
        .list_tracks(ListTracksRequest {})
        .await
        .unwrap()
        .into_inner();
    let mut tracks = Vec::new();
    while let Some(batch) = stream.message().await.unwrap() {
        tracks.extend(batch.tracks);
    }
    let mut by_count = std::collections::BTreeMap::new();
    for track in &tracks {
        *by_count.entry(track.loop_count).or_insert(0) += 1;
    }
    println!("tracks by number of loops: {by_count:?}");
    for track in tracks.iter().filter(|track| track.loop_count > 0).take(5) {
        let loops = library
            .get_loops(GetLoopsRequest { track_id: track.id })
            .await
            .unwrap()
            .into_inner()
            .loops;
        assert_eq!(loops.len() as u32, track.loop_count);
        println!("{} — {}:", track.title, track.length_seconds());
        for saved in loops {
            println!(
                "  slot {} {:?} {:.2} s to {:.2} s, color {:08x}",
                saved.slot + 1,
                saved.label,
                saved.start_seconds,
                saved.end_seconds,
                saved.color_argb
            );
        }
    }
}

#[tokio::test]
async fn opens_a_plain_folder_of_music_as_a_library() {
    let temp = TempDir::new("local-folder");
    let music = temp.0.join("Music");
    std::fs::create_dir_all(music.join("House/Deep")).unwrap();
    wav(&music.join("top.wav"), &[(440.0, 1.0)]);
    wav(&music.join("House/one.wav"), &[(220.0, 2.0)]);
    wav(&music.join("House/Deep/two.wav"), &[(330.0, 1.0)]);
    std::fs::write(music.join("House/notes.txt"), b"not music").unwrap();

    let running = start(&temp.0.join("cache"));
    let channel = channel(&running).await;
    let mut library =
        LibraryServiceClient::new(InterceptedService::new(channel, with_token as fn(_) -> _));
    let folder = |path: &Path| OpenLocalFolderRequest {
        path: path.display().to_string(),
    };

    assert_eq!(
        code(
            library
                .open_local_folder(folder(&temp.0.join("missing")))
                .await
        ),
        Code::NotFound
    );

    let opened = library
        .open_local_folder(folder(&music))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(opened.path, music.display().to_string());
    assert_eq!((opened.track_count, opened.playlist_count), (3, 2));
    assert_eq!(opened.cache_path, "");

    // The tracks are the files, with the path relative to the folder.
    let mut stream = library
        .list_tracks(ListTracksRequest {})
        .await
        .unwrap()
        .into_inner();
    let tracks = stream.message().await.unwrap().unwrap().tracks;
    assert_eq!(
        tracks
            .iter()
            .map(|track| track.path.as_str())
            .collect::<Vec<_>>(),
        ["top.wav", "House/one.wav", "House/Deep/two.wav"]
    );
    assert_eq!(tracks[1].title, "one");
    assert_eq!(tracks[1].length_seconds, Some(2));
    assert!(tracks.iter().all(|track| {
        !track.streaming
            && track.rating.is_none()
            && track.cue_count == 0
            && track.loop_count == 0
            && track.origin.is_none()
            && track.album_art_id.is_none()
    }));

    // The folders are the playlists, each with the tracks below it.
    let playlists = library
        .list_playlists(ListPlaylistsRequest {})
        .await
        .unwrap()
        .into_inner()
        .playlists;
    let shape: Vec<_> = playlists
        .iter()
        .map(|playlist| {
            (
                playlist.title.as_str(),
                playlist.depth,
                playlist.has_children,
                playlist.track_ids.clone(),
            )
        })
        .collect();
    assert_eq!(
        shape,
        [("House", 0, true, vec![1, 2]), ("Deep", 1, false, vec![2])]
    );

    // The waveform comes from the file, and has no beatgrid and no cues.
    let waveform = library
        .get_waveform(GetWaveformRequest {
            track_id: 1,
            ..Default::default()
        })
        .await
        .unwrap()
        .into_inner();
    assert!((waveform.duration_seconds - 2.0).abs() < 0.01);
    assert!(waveform.beat_grid.is_none());
    assert!(
        library
            .get_cues(GetCuesRequest { track_id: 1 })
            .await
            .unwrap()
            .into_inner()
            .cues
            .is_empty()
    );
    assert!(
        library
            .get_loops(GetLoopsRequest { track_id: 1 })
            .await
            .unwrap()
            .into_inner()
            .loops
            .is_empty()
    );

    // What only an Engine library has is refused, and nothing is written to the folder.
    assert_eq!(
        code(
            library
                .set_rating(SetRatingRequest {
                    track_id: 0,
                    rating: 20
                })
                .await
        ),
        Code::FailedPrecondition
    );
    assert_eq!(
        code(
            library
                .set_cues(SetCuesRequest {
                    track_id: 0,
                    cues: vec![]
                })
                .await
        ),
        Code::FailedPrecondition
    );
    assert_eq!(
        code(library.get_sync_diff(GetSyncDiffRequest::default()).await),
        Code::FailedPrecondition
    );
    assert_eq!(
        code(
            library
                .save_library_as(SaveLibraryAsRequest {
                    path: temp.0.join("copy").display().to_string()
                })
                .await
        ),
        Code::FailedPrecondition
    );
    assert_eq!(
        code(library.migrate_origins(MigrateOriginsRequest {}).await),
        Code::FailedPrecondition
    );
    let mut names: Vec<_> = std::fs::read_dir(&music)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    assert_eq!(names, ["House", "top.wav"]);

    // Opening an Engine library afterwards replaces it, as one library replaces another.
    let engine = fixture_library(&temp.0);
    library
        .open_library(OpenLibraryRequest {
            path: engine.display().to_string(),
        })
        .await
        .unwrap();
    let mut stream = library
        .list_tracks(ListTracksRequest {})
        .await
        .unwrap()
        .into_inner();
    assert_eq!(stream.message().await.unwrap().unwrap().tracks.len(), 3);
}

/// A real folder of music, counted and timed:
/// `OPEN_ENGINE_TEST_FOLDER=... cargo test -p open-engine-backend --test grpc real_music_folder -- --ignored --nocapture`
#[tokio::test]
#[ignore = "needs a real folder of music (OPEN_ENGINE_TEST_FOLDER)"]
async fn scans_a_real_music_folder() {
    let path = std::env::var("OPEN_ENGINE_TEST_FOLDER").expect("OPEN_ENGINE_TEST_FOLDER");
    let temp = TempDir::new("real-folder");
    let running = start(&temp.0.join("cache"));
    let channel = channel(&running).await;
    let mut library =
        LibraryServiceClient::new(InterceptedService::new(channel, with_token as fn(_) -> _));
    let started = Instant::now();
    let opened = library
        .open_local_folder(OpenLocalFolderRequest { path })
        .await
        .unwrap()
        .into_inner();
    println!(
        "{} tracks in {} folders, opened in {:?}",
        opened.track_count,
        opened.playlist_count,
        started.elapsed()
    );
    let mut stream = library
        .list_tracks(ListTracksRequest {})
        .await
        .unwrap()
        .into_inner();
    let mut tracks = Vec::new();
    while let Some(batch) = stream.message().await.unwrap() {
        tracks.extend(batch.tracks);
    }
    let with =
        |field: fn(&open_engine_api::v1::Track) -> bool| tracks.iter().filter(|t| field(t)).count();
    println!(
        "with artist {}, album {}, genre {}, bpm {}, length {}",
        with(|t| !t.artist.is_empty()),
        with(|t| !t.album.is_empty()),
        with(|t| !t.genre.is_empty()),
        with(|t| t.bpm.is_some()),
        with(|t| t.length_seconds.is_some()),
    );
    for track in tracks.iter().take(5) {
        println!(
            "  {} — {} [{}] {:?} BPM, {:?} s ({})",
            track.artist, track.title, track.album, track.bpm, track.length_seconds, track.path
        );
    }
}
