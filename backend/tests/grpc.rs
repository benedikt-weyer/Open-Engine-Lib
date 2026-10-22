//! Starts the real backend process and talks to it over gRPC, the way the UI does.
//! Nothing here needs a sound card: tracks that can't be played fail before the audio
//! device is touched.

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::time::{Duration, Instant};

use open_engine_api::v1::backend_service_client::BackendServiceClient;
use open_engine_api::v1::library_service_client::LibraryServiceClient;
use open_engine_api::v1::playback_service_client::PlaybackServiceClient;
use open_engine_api::v1::{
    CloseLibraryRequest, GetInfoRequest, ListPlaylistsRequest, ListTracksRequest,
    OpenLibraryRequest, PlayRequest, PlaybackErrorKind, PlaybackState,
};
use open_engine_api::{API_VERSION, AUTHORIZATION, LISTENING_PREFIX, TOKEN_ENV};
use tonic::service::interceptor::InterceptedService;
use tonic::transport::{Channel, Endpoint};
use tonic::{Code, Request, Status};

const TOKEN: &str = "test-token";

const FIXTURE: &str = "CREATE TABLE Track (id INTEGER PRIMARY KEY, title TEXT, artist TEXT, album TEXT,
        genre TEXT, bpmAnalyzed REAL, bpm INTEGER, length INTEGER, path TEXT,
        streamingSource TEXT, originDatabaseUuid TEXT, originTrackId INTEGER);
    CREATE TABLE Playlist (id INTEGER PRIMARY KEY, title TEXT, parentListId INTEGER, nextListId INTEGER);
    CREATE TABLE PlaylistEntity (id INTEGER PRIMARY KEY, listId INTEGER, trackId INTEGER,
        databaseUuid TEXT, nextEntityId INTEGER);
    INSERT INTO Track VALUES
        (1, 'One', 'A', 'X', 'House', 124.0, 124, 200, '../Music/one.flac', NULL, 'db', 1),
        (2, 'Two', 'B', 'Y', 'Techno', 130.5, 130, 61, '../Music/two.mp3', NULL, 'db', 2),
        (3, 'Three', 'C', 'Z', 'Pop', 0.0, 0, 90, '../tidal/three.mp4', 'TIDAL', 'db', 3);
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
                    position: 0
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
    assert_eq!(tracks[2].bpm, None);
    assert!(tracks[2].streaming && !tracks[0].streaming);

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
                    position: 0
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
                    position: 1
                })
                .await
        ),
        Code::InvalidArgument
    );

    let status = playback
        .play(PlayRequest {
            track_ids: vec![0],
            position: 0,
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
