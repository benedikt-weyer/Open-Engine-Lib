//! The gRPC services (`openenginelib.v1`) that the UI talks to.

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use open_engine_api::API_VERSION;
use open_engine_api::v1 as api;
use open_engine_api::v1::backend_service_server::BackendService;
use open_engine_api::v1::library_service_server::LibraryService;
use open_engine_api::v1::playback_service_server::PlaybackService;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status};

use crate::engine::{Command, Engine, ErrorKind, QueueItem, State};
use crate::library::{self, Library, OpenError};
use crate::player::Start;
use crate::save::{self, SaveError};
use crate::waveform::{self, AnalyzeError, Gains, Peaks, Split};

/// Tracks per message of `ListTracks`, to stay well below gRPC's message size limit.
const TRACKS_PER_BATCH: usize = 500;

/// Waveforms are kept for the tracks used last, so skipping back to a track is instant.
const WAVEFORMS_KEPT: usize = 8;

#[derive(Default)]
struct WaveformCache {
    /// Paths, oldest first.
    order: VecDeque<WaveformKey>,
    waveforms: HashMap<WaveformKey, Arc<Peaks>>,
}

/// A waveform depends on where the bands were split, not just on the file.
type WaveformKey = (PathBuf, Split);

impl WaveformCache {
    fn insert(&mut self, key: WaveformKey, waveform: Arc<Peaks>) {
        if self.waveforms.insert(key.clone(), waveform).is_none() {
            self.order.push_back(key);
        }
        while self.order.len() > WAVEFORMS_KEPT {
            if let Some(oldest) = self.order.pop_front() {
                self.waveforms.remove(&oldest);
            }
        }
    }
}

struct Shared {
    library: RwLock<Option<Arc<Library>>>,
    waveforms: Mutex<WaveformCache>,
    engine: Engine,
    /// Where copies of the libraries' databases are kept.
    cache_root: PathBuf,
}

#[derive(Clone)]
pub struct Service(Arc<Shared>);

impl Service {
    pub fn new(engine: Engine, cache_root: PathBuf) -> Self {
        Self(Arc::new(Shared {
            library: RwLock::new(None),
            waveforms: Mutex::new(WaveformCache::default()),
            engine,
            cache_root,
        }))
    }

    fn library(&self) -> Result<Arc<Library>, Status> {
        self.0
            .library
            .read()
            .expect("library lock")
            .clone()
            .ok_or_else(|| Status::failed_precondition("no library is open"))
    }

    /// Stops playback, releases the audio device and forgets the library.
    async fn close(&self) {
        self.0.engine.send(Command::Close).await;
        *self.0.library.write().expect("library lock") = None;
    }
}

#[tonic::async_trait]
impl BackendService for Service {
    async fn get_info(
        &self,
        _: Request<api::GetInfoRequest>,
    ) -> Result<Response<api::GetInfoResponse>, Status> {
        Ok(Response::new(api::GetInfoResponse {
            backend_version: env!("CARGO_PKG_VERSION").to_owned(),
            supported_api_versions: vec![API_VERSION],
        }))
    }
}

#[tonic::async_trait]
impl LibraryService for Service {
    async fn open_library(
        &self,
        request: Request<api::OpenLibraryRequest>,
    ) -> Result<Response<api::OpenLibraryResponse>, Status> {
        let path = PathBuf::from(request.into_inner().path);
        let cache_root = self.0.cache_root.clone();
        self.close().await;

        let library = tokio::task::spawn_blocking(move || Library::open(&path, &cache_root))
            .await
            .map_err(|error| Status::internal(error.to_string()))?
            .map_err(|error| match error {
                OpenError::NotALibrary(_) => Status::not_found(error.to_string()),
                OpenError::Cache(_) | OpenError::Database(_) => Status::internal(error.to_string()),
            })?;

        let response = api::OpenLibraryResponse {
            path: library.dir.display().to_string(),
            cache_path: library.cache.display().to_string(),
            track_count: library.tracks.len() as u32,
            playlist_count: library.playlists.len() as u32,
        };
        *self.0.library.write().expect("library lock") = Some(Arc::new(library));
        Ok(Response::new(response))
    }

    async fn save_library_as(
        &self,
        request: Request<api::SaveLibraryAsRequest>,
    ) -> Result<Response<api::SaveLibraryAsResponse>, Status> {
        let library = self.library()?;
        let destination = PathBuf::from(request.into_inner().path);
        let cache_root = self.0.cache_root.clone();

        // Copying databases takes a moment and must not block the async threads.
        let saved = tokio::task::spawn_blocking(move || {
            let target = save::save_as(&library, &destination)?;
            // Only once everything is copied is the new place made the library's location,
            // by opening it. Were that to fail, the library would stay where it is.
            match Library::open(&target, &cache_root) {
                Ok(opened) => Ok(opened),
                Err(error) => {
                    save::discard(&target);
                    Err(SaveError::Failed(error.to_string()))
                }
            }
        })
        .await
        .map_err(|error| Status::internal(error.to_string()))?
        .map_err(|error| match error {
            SaveError::SameAsSource => Status::invalid_argument(error.to_string()),
            SaveError::AlreadyExists(_) => Status::already_exists(error.to_string()),
            SaveError::Failed(_) => Status::internal(error.to_string()),
        })?;

        let response = api::SaveLibraryAsResponse {
            path: saved.dir.display().to_string(),
            cache_path: saved.cache.display().to_string(),
        };
        // The tracks are the same files in the same order, so playback and every id carry on.
        *self.0.library.write().expect("library lock") = Some(Arc::new(saved));
        Ok(Response::new(response))
    }

    async fn close_library(
        &self,
        _: Request<api::CloseLibraryRequest>,
    ) -> Result<Response<api::CloseLibraryResponse>, Status> {
        self.close().await;
        Ok(Response::new(api::CloseLibraryResponse {}))
    }

    type ListTracksStream = ReceiverStream<Result<api::TrackBatch, Status>>;

    async fn list_tracks(
        &self,
        _: Request<api::ListTracksRequest>,
    ) -> Result<Response<Self::ListTracksStream>, Status> {
        let library = self.library()?;
        let (sender, receiver) = mpsc::channel(4);

        tokio::spawn(async move {
            for (batch, tracks) in library.tracks.chunks(TRACKS_PER_BATCH).enumerate() {
                let first = batch * TRACKS_PER_BATCH;
                let tracks = tracks
                    .iter()
                    .enumerate()
                    .map(|(offset, track)| track_message((first + offset) as u32, track))
                    .collect();
                if sender.send(Ok(api::TrackBatch { tracks })).await.is_err() {
                    break; // the UI stopped listening
                }
            }
        });

        Ok(Response::new(ReceiverStream::new(receiver)))
    }

    async fn list_playlists(
        &self,
        _: Request<api::ListPlaylistsRequest>,
    ) -> Result<Response<api::ListPlaylistsResponse>, Status> {
        let library = self.library()?;
        let playlists = library
            .playlists
            .iter()
            .enumerate()
            .map(|(index, playlist)| api::Playlist {
                id: index as u32,
                title: playlist.title.clone(),
                depth: playlist.depth as u32,
                parent_id: playlist.parent.map(|parent| parent as u32),
                has_children: playlist.has_children,
                track_ids: playlist.tracks.iter().map(|&track| track as u32).collect(),
            })
            .collect();
        Ok(Response::new(api::ListPlaylistsResponse { playlists }))
    }

    async fn get_album_art(
        &self,
        request: Request<api::GetAlbumArtRequest>,
    ) -> Result<Response<api::GetAlbumArtResponse>, Status> {
        let library = self.library()?;
        let id = request.into_inner().id;

        // Reads a file, so keep it off the async threads.
        let art = tokio::task::spawn_blocking(move || library.album_art(id))
            .await
            .map_err(|error| Status::internal(error.to_string()))?
            .map_err(|error| Status::internal(error.to_string()))?
            .ok_or_else(|| Status::not_found(format!("no image for cover {id}")))?;
        Ok(Response::new(api::GetAlbumArtResponse {
            data: art.data,
            mime_type: art.mime_type.to_owned(),
        }))
    }

    async fn get_waveform(
        &self,
        request: Request<api::GetWaveformRequest>,
    ) -> Result<Response<api::GetWaveformResponse>, Status> {
        let library = self.library()?;
        let request = request.into_inner();
        let id = request.track_id;
        let split = Split {
            low_mid_hz: request.low_mid_hz.unwrap_or(waveform::DEFAULT_LOW_MID_HZ),
            mid_high_hz: request.mid_high_hz.unwrap_or(waveform::DEFAULT_MID_HIGH_HZ),
        };
        split.validate().map_err(Status::invalid_argument)?;
        let defaults = Gains::default();
        let gains = Gains {
            low: request.gain_low.unwrap_or(defaults.low),
            mid: request.gain_mid.unwrap_or(defaults.mid),
            high: request.gain_high.unwrap_or(defaults.high),
        };
        gains.validate().map_err(Status::invalid_argument)?;
        let track = library
            .tracks
            .get(id as usize)
            .ok_or_else(|| Status::invalid_argument(format!("unknown track id {id}")))?;
        if track.streaming {
            return Err(Status::failed_precondition(
                "a streaming track has no audio file to analyse",
            ));
        }
        let path = track.path.clone();

        let shared = Arc::clone(&self.0);
        // Decoding a whole track takes a moment and must not block the async threads.
        let (peaks, grid) = tokio::task::spawn_blocking(move || {
            let cached = shared
                .waveforms
                .lock()
                .expect("waveform lock")
                .waveforms
                .get(&(path.clone(), split))
                .cloned();
            let waveform = match cached {
                Some(waveform) => waveform,
                None => {
                    let waveform = Arc::new(waveform::analyze_file(&path, split).map_err(
                        |error| match error {
                            AnalyzeError::NotFound => {
                                Status::not_found(format!("{} does not exist", path.display()))
                            }
                            AnalyzeError::Failed(error) => Status::internal(error),
                        },
                    )?);
                    shared
                        .waveforms
                        .lock()
                        .expect("waveform lock")
                        .insert((path, split), Arc::clone(&waveform));
                    waveform
                }
            };
            let grid = library.beat_grid(id as usize).unwrap_or_else(|error| {
                // The waveform is still worth showing without its grid.
                eprintln!("could not read the beatgrid of track {id}: {error}");
                None
            });
            Ok::<_, Status>((waveform, grid))
        })
        .await
        .map_err(|error| Status::internal(error.to_string()))??;

        Ok(Response::new(api::GetWaveformResponse {
            duration_seconds: peaks.duration_seconds,
            columns_per_second: waveform::COLUMNS_PER_SECOND,
            columns: peaks.scaled(gains).iter().flatten().copied().collect(),
            beat_grid: grid.map(|markers| api::BeatGrid {
                markers: markers
                    .into_iter()
                    .map(|marker| api::BeatMarker {
                        position_seconds: marker.position_seconds,
                        beat_number: marker.beat_number,
                    })
                    .collect(),
            }),
        }))
    }
}

fn track_message(id: u32, track: &library::Track) -> api::Track {
    api::Track {
        id,
        title: track.title.clone(),
        artist: track.artist.clone(),
        album: track.album.clone(),
        genre: track.genre.clone(),
        bpm: track.bpm,
        length_seconds: track.length,
        streaming: track.streaming,
        album_art_id: track.album_art,
        path: track.relative_path.clone(),
    }
}

#[tonic::async_trait]
impl PlaybackService for Service {
    async fn play(
        &self,
        request: Request<api::PlayRequest>,
    ) -> Result<Response<api::PlaybackStatus>, Status> {
        let library = self.library()?;
        let request = request.into_inner();

        let queue = request
            .track_ids
            .iter()
            .map(|&id| {
                let track = library
                    .tracks
                    .get(id as usize)
                    .ok_or_else(|| Status::invalid_argument(format!("unknown track id {id}")))?;
                Ok(QueueItem {
                    track_id: id,
                    path: track.path.clone(),
                    streaming: track.streaming,
                })
            })
            .collect::<Result<Vec<_>, Status>>()?;

        let position = request.position as usize;
        if position >= queue.len() {
            return Err(Status::invalid_argument(format!(
                "position {position} is outside the queue of {} tracks",
                queue.len()
            )));
        }

        let start = Start {
            paused: request.start_paused,
            at: Duration::from_millis(request.start_position_ms),
        };
        self.command(Command::Play {
            queue,
            position,
            start,
        })
        .await
    }

    async fn toggle_pause(
        &self,
        _: Request<api::TogglePauseRequest>,
    ) -> Result<Response<api::PlaybackStatus>, Status> {
        self.command(Command::TogglePause).await
    }

    async fn stop(
        &self,
        _: Request<api::StopRequest>,
    ) -> Result<Response<api::PlaybackStatus>, Status> {
        self.command(Command::Stop).await
    }

    async fn next(
        &self,
        _: Request<api::NextRequest>,
    ) -> Result<Response<api::PlaybackStatus>, Status> {
        self.command(Command::Next).await
    }

    async fn previous(
        &self,
        _: Request<api::PreviousRequest>,
    ) -> Result<Response<api::PlaybackStatus>, Status> {
        self.command(Command::Previous).await
    }

    async fn seek(
        &self,
        request: Request<api::SeekRequest>,
    ) -> Result<Response<api::PlaybackStatus>, Status> {
        let to = Duration::from_millis(request.into_inner().position_ms);
        self.command(Command::Seek(to)).await
    }

    async fn get_status(
        &self,
        _: Request<api::GetStatusRequest>,
    ) -> Result<Response<api::PlaybackStatus>, Status> {
        // Asked of the engine, not read from what it published last: that can be stale.
        self.command(Command::Poll).await
    }
}

impl Service {
    async fn command(&self, command: Command) -> Result<Response<api::PlaybackStatus>, Status> {
        Ok(Response::new(status_message(
            self.0.engine.send(command).await,
        )))
    }
}

fn status_message(status: crate::engine::Status) -> api::PlaybackStatus {
    api::PlaybackStatus {
        state: match status.state {
            State::Stopped => api::PlaybackState::Stopped,
            State::Playing => api::PlaybackState::Playing,
            State::Paused => api::PlaybackState::Paused,
        }
        .into(),
        queue_position: status.queue_position,
        track_id: status.track_id,
        position_ms: status.position.as_millis() as u64,
        error: status.error.map(|error| api::PlaybackError {
            kind: match error.kind {
                ErrorKind::NotFound => api::PlaybackErrorKind::NotFound,
                ErrorKind::Streaming => api::PlaybackErrorKind::Streaming,
                ErrorKind::Failed => api::PlaybackErrorKind::Failed,
            }
            .into(),
            track_id: error.track_id,
            detail: error.detail,
        }),
    }
}
