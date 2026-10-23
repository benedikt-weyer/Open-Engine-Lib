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
    CloseLibraryRequest, GetAlbumArtRequest, GetInfoRequest, GetSyncDiffRequest,
    GetWaveformRequest, ListPlaylistsRequest, ListTracksRequest, OpenLibraryRequest, PlayRequest,
    PlaybackErrorKind, PlaybackState, SaveLibraryAsRequest, SetRatingRequest, SyncToSourceRequest,
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
        code(library.get_sync_diff(GetSyncDiffRequest {}).await),
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
        .get_sync_diff(GetSyncDiffRequest {})
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
        .get_sync_diff(GetSyncDiffRequest {})
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
        .get_sync_diff(GetSyncDiffRequest {})
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
        .get_sync_diff(GetSyncDiffRequest {})
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
        .get_sync_diff(GetSyncDiffRequest {})
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
        .get_sync_diff(GetSyncDiffRequest {})
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
        .get_sync_diff(GetSyncDiffRequest {})
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
