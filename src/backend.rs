//! The backend process and the gRPC client the UI talks to it with.
//!
//! The UI starts the backend (`open-engine-backend`, found next to this program),
//! which listens on a loopback port and tells us the address on its first line of
//! output. Every request carries a random per-launch token, so other programs on
//! the machine can't use the port. The backend exits by itself when we exit,
//! because it watches the pipe on its stdin; dropping the last [`Backend`] also
//! kills it right away.

use std::collections::HashMap;
use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::Arc;
use std::time::Duration;

use open_engine_api::v1::backend_service_client::BackendServiceClient;
use open_engine_api::v1::library_service_client::LibraryServiceClient;
use open_engine_api::v1::playback_service_client::PlaybackServiceClient;
use open_engine_api::v1::{
    CloseLibraryRequest, CreatePlaylistRequest, CreateSmartPlaylistRequest, DeletePlaylistRequest,
    GetAlbumArtRequest, GetInfoRequest, GetLibrarySyncDiffRequest, GetLibrarySyncDiffResponse,
    GetProgressRequest, GetSyncDiffRequest, GetSyncDiffResponse, GetWaveformRequest,
    ListPlaylistsRequest, ListTracksRequest, MigrateOriginsRequest, MigrateOriginsResponse,
    NextRequest, OpenLibraryRequest, OperationProgress, PlayRequest, PlaybackStatus,
    PreviousRequest, PullFromSourceRequest, PullFromSourceResponse, RenamePlaylistRequest,
    SaveLibraryAsRequest, SeekRequest, SetRatingRequest, SmartRules, SoundSwitchState, StopRequest,
    SyncToLibraryRequest, SyncToLibraryResponse, SyncToSourceRequest, SyncToSourceResponse,
    TogglePauseRequest, UpdateSmartPlaylistRequest,
};
use open_engine_api::{API_VERSION, AUTHORIZATION, LISTENING_PREFIX, TOKEN_ENV};
use tonic::metadata::MetadataValue;
use tonic::service::Interceptor;
use tonic::service::interceptor::InterceptedService;
use tonic::transport::{Channel, Endpoint};
use tonic::{Request, Status};

use crate::model::{Library, Lighting, Playlist, Track};
use crate::waveform::{BeatGrid, Marker, Waveform};

/// Overrides where to find the backend executable.
const BACKEND_ENV: &str = "OPEN_ENGINE_BACKEND";
const BACKEND_NAME: &str = "open-engine-backend";
/// How long the backend gets to start and report its address.
const START_TIMEOUT: Duration = Duration::from_secs(10);
/// Replies like the playlist list can exceed gRPC's 4 MB default for big libraries.
const MAX_MESSAGE_SIZE: usize = 64 * 1024 * 1024;

/// Adds the launch token to every request.
#[derive(Clone)]
struct Token(MetadataValue<tonic::metadata::Ascii>);

impl Interceptor for Token {
    fn call(&mut self, mut request: Request<()>) -> Result<Request<()>, Status> {
        request.metadata_mut().insert(AUTHORIZATION, self.0.clone());
        Ok(request)
    }
}

type Connection = InterceptedService<Channel, Token>;

/// The running backend process. Dropping it stops the backend.
struct Process {
    child: Child,
    /// Held open on purpose: the backend exits when the other end closes.
    _stdin: ChildStdin,
}

impl Drop for Process {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// A connection to a running backend. Cheap to clone; the process stops when the
/// last clone is gone.
#[derive(Clone)]
pub struct Backend {
    _process: Arc<Process>,
    library: LibraryServiceClient<Connection>,
    playback: PlaybackServiceClient<Connection>,
}

impl std::fmt::Debug for Backend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Backend")
    }
}

fn backend_path() -> Result<PathBuf, String> {
    if let Some(path) = std::env::var_os(BACKEND_ENV) {
        return Ok(PathBuf::from(path));
    }
    let own = std::env::current_exe().map_err(|error| error.to_string())?;
    let path = own.with_file_name(format!("{BACKEND_NAME}{}", std::env::consts::EXE_SUFFIX));
    if path.is_file() {
        Ok(path)
    } else {
        Err(format!(
            "{} not found; build it with `cargo build -p {BACKEND_NAME}`",
            path.display()
        ))
    }
}

fn describe(status: Status) -> String {
    status.message().to_owned()
}

/// Starts the backend, connects to it and checks that it speaks our API version.
pub async fn start() -> Result<Backend, String> {
    let path = backend_path()?;

    let mut token_bytes = [0u8; 32];
    getrandom::fill(&mut token_bytes).map_err(|error| error.to_string())?;
    let token: String = token_bytes
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();

    let mut command = Command::new(&path);
    command
        .env(TOKEN_ENV, &token)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());
    #[cfg(windows)]
    {
        // The UI has no console, so don't let the backend open one.
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    let mut child = command
        .spawn()
        .map_err(|error| format!("could not start {}: {error}", path.display()))?;
    let stdin = child.stdin.take().expect("stdin is piped");
    let stdout = child.stdout.take().expect("stdout is piped");
    // From here on the child is killed if anything below fails or times out.
    let process = Arc::new(Process {
        child,
        _stdin: stdin,
    });

    // The first line of output says where the backend listens.
    let line = tokio::time::timeout(
        START_TIMEOUT,
        tokio::task::spawn_blocking(move || {
            let mut line = String::new();
            BufReader::new(stdout).read_line(&mut line).map(|_| line)
        }),
    )
    .await
    .map_err(|_| "the backend did not start in time".to_owned())?
    .map_err(|error| error.to_string())?
    .map_err(|error| error.to_string())?;
    let address = line
        .strip_prefix(LISTENING_PREFIX)
        .map(str::trim)
        .ok_or_else(|| "the backend exited before it was ready".to_owned())?;

    let channel = Endpoint::from_shared(format!("http://{address}"))
        .map_err(|error| error.to_string())?
        .connect_timeout(START_TIMEOUT)
        .connect()
        .await
        .map_err(|error| error.to_string())?;
    let token = Token(
        format!("Bearer {token}")
            .parse()
            .map_err(|_| "invalid token".to_owned())?,
    );

    let info = BackendServiceClient::with_interceptor(channel.clone(), token.clone())
        .get_info(GetInfoRequest {})
        .await
        .map_err(describe)?
        .into_inner();
    if !info.supported_api_versions.contains(&API_VERSION) {
        return Err(format!(
            "backend {} speaks API version(s) {:?}, but this app needs version {API_VERSION}",
            info.backend_version, info.supported_api_versions
        ));
    }

    Ok(Backend {
        _process: process,
        library: LibraryServiceClient::with_interceptor(channel.clone(), token.clone())
            .max_decoding_message_size(MAX_MESSAGE_SIZE),
        playback: PlaybackServiceClient::with_interceptor(channel, token)
            .max_decoding_message_size(MAX_MESSAGE_SIZE),
    })
}

/// Has the backend open the library at `path` and fetches its tracks and playlists.
pub async fn open_library(mut backend: Backend, path: PathBuf) -> Result<Arc<Library>, String> {
    let opened = backend
        .library
        .open_library(OpenLibraryRequest {
            path: path.display().to_string(),
        })
        .await
        .map_err(describe)?
        .into_inner();

    load_library(
        &mut backend,
        opened.path,
        opened.cache_path,
        opened.track_count,
    )
    .await
}

/// Saves the open library to a new place, which becomes its location, and fetches it
/// again: the tracks are the same but their paths now start from the new folder.
pub async fn save_library_as(mut backend: Backend, path: PathBuf) -> Result<Arc<Library>, String> {
    let saved = backend
        .library
        .save_library_as(SaveLibraryAsRequest {
            path: path.display().to_string(),
        })
        .await
        .map_err(describe)?
        .into_inner();
    load_library(&mut backend, saved.path, saved.cache_path, 0).await
}

/// Fetches the tracks and playlists of the library the backend has open.
async fn load_library(
    backend: &mut Backend,
    path: String,
    cache_path: String,
    track_count: u32,
) -> Result<Arc<Library>, String> {
    let mut tracks = Vec::with_capacity(track_count as usize);
    let mut stream = backend
        .library
        .list_tracks(ListTracksRequest {})
        .await
        .map_err(describe)?
        .into_inner();
    while let Some(batch) = stream.message().await.map_err(describe)? {
        tracks.extend(batch.tracks.into_iter().map(|track| {
            let lighting = match track.soundswitch() {
                SoundSwitchState::Rendered => Lighting::Rendered,
                SoundSwitchState::Analysed => Lighting::Analysed,
                SoundSwitchState::Unspecified => Lighting::None,
            };
            Track {
                id: track.id,
                title: track.title,
                artist: track.artist,
                album: track.album,
                genre: track.genre,
                bpm: track.bpm,
                length: track.length_seconds,
                streaming: track.streaming,
                album_art: track.album_art_id,
                location: if track.streaming {
                    String::new()
                } else {
                    crate::model::location(&path, &track.path)
                },
                rating: track.rating,
                lighting,
                lighting_projects: track.soundswitch_projects,
                origin: track.origin,
                path: track.path,
            }
        }));
    }

    let index_of: HashMap<u32, usize> = tracks
        .iter()
        .enumerate()
        .map(|(index, track)| (track.id, index))
        .collect();
    let playlists = backend
        .library
        .list_playlists(ListPlaylistsRequest {})
        .await
        .map_err(describe)?
        .into_inner()
        .playlists;
    let playlists = playlists_from(playlists, |id| index_of.get(&id).copied());

    Ok(Arc::new(Library::new(path, cache_path, tracks, playlists)))
}

/// The UI's playlists from the backend's, with the tracks as indices into the UI's list.
fn playlists_from(
    playlists: Vec<open_engine_api::v1::Playlist>,
    index_of: impl Fn(u32) -> Option<usize>,
) -> Vec<Playlist> {
    playlists
        .into_iter()
        .map(|playlist| Playlist {
            title: playlist.title,
            depth: playlist.depth as usize,
            has_children: playlist.has_children,
            is_smart: playlist.is_smart,
            rules: playlist.rules,
            tracks: playlist
                .track_ids
                .iter()
                .filter_map(|&id| index_of(id))
                .collect(),
        })
        .collect()
}

/// A smart playlist that was added or changed: all playlists as they are now, and which
/// one it is.
#[derive(Debug, Clone)]
pub struct SmartPlaylistEdited {
    pub playlists: Vec<Playlist>,
    pub playlist: usize,
}

/// Adds a smart playlist to the working copy. `library` is the open one, whose tracks the
/// playlists refer to.
pub async fn create_smart_playlist(
    mut backend: Backend,
    library: Arc<Library>,
    title: String,
    rules: SmartRules,
) -> Result<SmartPlaylistEdited, String> {
    let edited = backend
        .library
        .create_smart_playlist(CreateSmartPlaylistRequest {
            title,
            rules: Some(rules),
        })
        .await
        .map_err(describe)?
        .into_inner();
    Ok(edited_from(edited, &library))
}

/// Changes the title and rules of the smart playlist `playlist` (an index into the
/// library's playlists).
pub async fn update_smart_playlist(
    mut backend: Backend,
    library: Arc<Library>,
    playlist: usize,
    title: String,
    rules: SmartRules,
) -> Result<SmartPlaylistEdited, String> {
    let edited = backend
        .library
        .update_smart_playlist(UpdateSmartPlaylistRequest {
            playlist_id: playlist as u32,
            title,
            rules: Some(rules),
        })
        .await
        .map_err(describe)?
        .into_inner();
    Ok(edited_from(edited, &library))
}

fn edited_from(
    edited: open_engine_api::v1::SmartPlaylistEdited,
    library: &Library,
) -> SmartPlaylistEdited {
    SmartPlaylistEdited {
        playlist: edited.playlist_id as usize,
        playlists: playlists_from(edited.playlists, |id| library.track_index(id)),
    }
}

/// The playlists after a playlist was added, renamed or deleted: all of them, and the
/// one that was added, if one was.
#[derive(Debug, Clone)]
pub struct PlaylistsEdited {
    pub playlists: Vec<Playlist>,
    pub playlist: Option<usize>,
}

/// Adds an empty playlist to `parent` (an index into the library's playlists), or to
/// the top level.
pub async fn create_playlist(
    mut backend: Backend,
    library: Arc<Library>,
    title: String,
    parent: Option<usize>,
) -> Result<PlaylistsEdited, String> {
    let edited = backend
        .library
        .create_playlist(CreatePlaylistRequest {
            title,
            parent_id: parent.map(|parent| parent as u32),
        })
        .await
        .map_err(describe)?
        .into_inner();
    Ok(playlists_edited_from(edited, &library))
}

/// Gives the playlist at index `playlist` another title.
pub async fn rename_playlist(
    mut backend: Backend,
    library: Arc<Library>,
    playlist: usize,
    title: String,
) -> Result<PlaylistsEdited, String> {
    let edited = backend
        .library
        .rename_playlist(RenamePlaylistRequest {
            playlist_id: playlist as u32,
            title,
        })
        .await
        .map_err(describe)?
        .into_inner();
    Ok(playlists_edited_from(edited, &library))
}

/// Deletes the playlist at index `playlist` and what is in it.
pub async fn delete_playlist(
    mut backend: Backend,
    library: Arc<Library>,
    playlist: usize,
) -> Result<PlaylistsEdited, String> {
    let edited = backend
        .library
        .delete_playlist(DeletePlaylistRequest {
            playlist_id: playlist as u32,
        })
        .await
        .map_err(describe)?
        .into_inner();
    Ok(playlists_edited_from(edited, &library))
}

fn playlists_edited_from(
    edited: open_engine_api::v1::PlaylistsEdited,
    library: &Library,
) -> PlaylistsEdited {
    PlaylistsEdited {
        playlist: edited.playlist_id.map(|id| id as usize),
        playlists: playlists_from(edited.playlists, |id| library.track_index(id)),
    }
}

/// Fetches a cover image: `None` if the backend has no image for it.
pub async fn album_art(mut backend: Backend, id: u32) -> Result<Option<Vec<u8>>, String> {
    match backend
        .library
        .get_album_art(GetAlbumArtRequest { id })
        .await
    {
        Ok(response) => Ok(Some(response.into_inner().data)),
        Err(status) if status.code() == tonic::Code::NotFound => Ok(None),
        Err(status) => Err(describe(status)),
    }
}

/// Fetches the waveform and beatgrid of a track: `None` if there is none, because the
/// track is a streaming track or its file is missing or can't be decoded.
/// How the backend should compute a waveform: where the bands meet and how much each
/// is amplified.
#[derive(Debug, Clone, Copy)]
pub struct WaveformSettings {
    pub low_mid_hz: u32,
    pub mid_high_hz: u32,
    /// Low, mid and high.
    pub gains: [f32; 3],
}

pub async fn waveform(
    mut backend: Backend,
    id: u32,
    settings: WaveformSettings,
) -> Result<Option<Arc<Waveform>>, String> {
    let response = match backend
        .library
        .get_waveform(GetWaveformRequest {
            track_id: id,
            low_mid_hz: Some(settings.low_mid_hz),
            mid_high_hz: Some(settings.mid_high_hz),
            gain_low: Some(settings.gains[0]),
            gain_mid: Some(settings.gains[1]),
            gain_high: Some(settings.gains[2]),
        })
        .await
    {
        Ok(response) => response.into_inner(),
        Err(status)
            if matches!(
                status.code(),
                tonic::Code::NotFound | tonic::Code::FailedPrecondition | tonic::Code::Internal
            ) =>
        {
            return Ok(None);
        }
        Err(status) => return Err(describe(status)),
    };
    if response.columns_per_second == 0 {
        return Ok(None);
    }

    let columns = response.columns.as_chunks::<3>().0.to_vec();
    let grid = response.beat_grid.and_then(|grid| {
        BeatGrid::new(
            grid.markers
                .into_iter()
                .map(|marker| Marker {
                    position: marker.position_seconds,
                    beat_number: marker.beat_number,
                })
                .collect(),
        )
    });
    Ok(Some(Arc::new(Waveform::new(
        response.duration_seconds,
        f64::from(response.columns_per_second),
        columns,
        grid,
    ))))
}

/// Sets a track's rating (0 to 100) in the working copy.
pub async fn set_rating(mut backend: Backend, track_id: u32, rating: u32) -> Result<(), String> {
    backend
        .library
        .set_rating(SetRatingRequest { track_id, rating })
        .await
        .map(|_| ())
        .map_err(describe)
}

/// Saves the location of each track's file as its origin, for the tracks that have none.
pub async fn migrate_origins(mut backend: Backend) -> Result<MigrateOriginsResponse, String> {
    backend
        .library
        .migrate_origins(MigrateOriginsRequest {})
        .await
        .map(|response| response.into_inner())
        .map_err(describe)
}

/// What making the playlists of the library at `target` the same as the open one's would
/// change.
pub async fn library_sync_diff(
    mut backend: Backend,
    target: PathBuf,
    options: crate::library_sync_view::Options,
) -> Result<Arc<GetLibrarySyncDiffResponse>, String> {
    backend
        .library
        .get_library_sync_diff(GetLibrarySyncDiffRequest {
            target_path: target.display().to_string(),
            remove_missing: options.remove_missing,
            sync_stars: options.stars,
            sync_locations: options.locations,
            copy_tracks: options.copy_tracks,
            reverse: options.reverse,
        })
        .await
        .map(|response| Arc::new(response.into_inner()))
        .map_err(describe)
}

/// Makes the playlists of the library at `target` the same as the open one's.
pub async fn sync_to_library(
    mut backend: Backend,
    target: PathBuf,
    options: crate::library_sync_view::Options,
) -> Result<SyncToLibraryResponse, String> {
    backend
        .library
        .sync_to_library(SyncToLibraryRequest {
            target_path: target.display().to_string(),
            remove_missing: options.remove_missing,
            sync_stars: options.stars,
            sync_locations: options.locations,
            copy_tracks: options.copy_tracks,
            reverse: options.reverse,
        })
        .await
        .map(|response| response.into_inner())
        .map_err(describe)
}

/// How far the sync that is running has got.
pub async fn progress(mut backend: Backend) -> Result<OperationProgress, String> {
    backend
        .library
        .get_progress(GetProgressRequest {})
        .await
        .map(|response| response.into_inner())
        .map_err(describe)
}

/// What would change in the source if the working copy were written to it.
pub async fn sync_diff(
    mut backend: Backend,
    reverse: bool,
) -> Result<Arc<GetSyncDiffResponse>, String> {
    backend
        .library
        .get_sync_diff(GetSyncDiffRequest { reverse })
        .await
        .map(|response| Arc::new(response.into_inner()))
        .map_err(describe)
}

/// Replaces the working copy by the source's databases (edits that were not synced are
/// kept aside).
pub async fn pull_from_source(mut backend: Backend) -> Result<PullFromSourceResponse, String> {
    backend
        .library
        .pull_from_source(PullFromSourceRequest {})
        .await
        .map(|response| response.into_inner())
        .map_err(describe)
}

/// Fetches the library the backend has open again, after its working copy was replaced or
/// changed (the backend reads it again by itself).
pub async fn reload_library(
    mut backend: Backend,
    path: String,
    cache_path: String,
    track_count: u32,
) -> Result<Arc<Library>, String> {
    load_library(&mut backend, path, cache_path, track_count).await
}

/// Writes the working copy to the source.
pub async fn sync_to_source(
    mut backend: Backend,
    overwrite_outside_changes: bool,
) -> Result<SyncToSourceResponse, String> {
    backend
        .library
        .sync_to_source(SyncToSourceRequest {
            overwrite_outside_changes,
        })
        .await
        .map(|response| response.into_inner())
        .map_err(describe)
}

pub async fn close_library(mut backend: Backend) -> Result<(), String> {
    backend
        .library
        .close_library(CloseLibraryRequest {})
        .await
        .map(|_| ())
        .map_err(describe)
}

/// A playback request. Every one of them answers with the resulting status.
#[derive(Debug, Clone)]
pub enum Control {
    Play {
        track_ids: Vec<u32>,
        position: u32,
        /// Load the track without playing it.
        start_paused: bool,
        start_position_ms: u64,
    },
    TogglePause,
    Stop,
    Next,
    Previous,
    Seek {
        position_ms: u64,
    },
    Poll,
}

pub async fn control(mut backend: Backend, control: Control) -> Result<PlaybackStatus, String> {
    let playback = &mut backend.playback;
    let status = match control {
        Control::Play {
            track_ids,
            position,
            start_paused,
            start_position_ms,
        } => {
            playback
                .play(PlayRequest {
                    track_ids,
                    position,
                    start_paused,
                    start_position_ms,
                })
                .await
        }
        Control::TogglePause => playback.toggle_pause(TogglePauseRequest {}).await,
        Control::Stop => playback.stop(StopRequest {}).await,
        Control::Next => playback.next(NextRequest {}).await,
        Control::Previous => playback.previous(PreviousRequest {}).await,
        Control::Seek { position_ms } => playback.seek(SeekRequest { position_ms }).await,
        Control::Poll => {
            playback
                .get_status(open_engine_api::v1::GetStatusRequest {})
                .await
        }
    };
    status.map(|status| status.into_inner()).map_err(describe)
}
