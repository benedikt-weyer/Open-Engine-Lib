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

use crate::cues::Cue;
use crate::engine::{Command, Engine, ErrorKind, QueueItem, State};
use crate::library::{self, EditError, Library, OpenError};
use crate::loops::Loop;
use crate::mirror::{self, MirrorError};
use crate::player::Start;
use crate::progress::{Phase, Progress};
use crate::save::{self, SaveError};
use crate::smartlist::Rules;
use crate::sync::{self, SyncError};
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
    /// How far the sync that is running has got.
    progress: Progress,
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
            progress: Progress::default(),
        }))
    }

    /// Replaces the library that is open by one read again from the same working copy
    /// (which changed). Playback is not stopped.
    async fn reopen(&self, dir: PathBuf) -> Result<(), Status> {
        let cache_root = self.0.cache_root.clone();
        let library = tokio::task::spawn_blocking(move || Library::open(&dir, &cache_root))
            .await
            .map_err(|error| Status::internal(error.to_string()))?
            .map_err(|error| Status::internal(error.to_string()))?;
        *self.0.library.write().expect("library lock") = Some(Arc::new(library));
        Ok(())
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
            playlist_count: library.playlists().len() as u32,
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

    async fn get_sync_diff(
        &self,
        request: Request<api::GetSyncDiffRequest>,
    ) -> Result<Response<api::GetSyncDiffResponse>, Status> {
        let library = self.library()?;
        let reverse = request.into_inner().reverse;
        // Compares files, possibly on a slow drive: not on the async threads.
        let diff = tokio::task::spawn_blocking(move || sync::diff(&library, reverse))
            .await
            .map_err(|error| Status::internal(error.to_string()))?
            .map_err(sync_status)?;
        Ok(Response::new(diff))
    }

    async fn sync_to_source(
        &self,
        request: Request<api::SyncToSourceRequest>,
    ) -> Result<Response<api::SyncToSourceResponse>, Status> {
        let library = self.library()?;
        let overwrite = request.into_inner().overwrite_outside_changes;
        let progress = self.0.progress.clone();
        let done = tokio::task::spawn_blocking(move || {
            let _running = progress.begin();
            sync::push(&library, overwrite, &progress)
        })
        .await
        .map_err(|error| Status::internal(error.to_string()))?
        .map_err(sync_status)?;
        Ok(Response::new(done))
    }

    async fn pull_from_source(
        &self,
        _: Request<api::PullFromSourceRequest>,
    ) -> Result<Response<api::PullFromSourceResponse>, Status> {
        let library = self.library()?;
        let dir = library.dir.clone();
        let progress = self.0.progress.clone();
        let done = tokio::task::spawn_blocking(move || {
            let _running = progress.begin();
            sync::pull(&library, &progress)
        })
        .await
        .map_err(|error| Status::internal(error.to_string()))?
        .map_err(sync_status)?;
        // The library that is open reads files that are gone.
        self.reopen(dir).await?;
        Ok(Response::new(done))
    }

    async fn set_rating(
        &self,
        request: Request<api::SetRatingRequest>,
    ) -> Result<Response<api::SetRatingResponse>, Status> {
        let library = self.library()?;
        let request = request.into_inner();
        if request.rating > 100 {
            return Err(Status::invalid_argument(format!(
                "a rating is 0 to 100, not {}",
                request.rating
            )));
        }
        let index = request.track_id as usize;
        // Writes a file, possibly after waiting for a read: not on the async threads.
        tokio::task::spawn_blocking(move || library.set_rating(index, request.rating))
            .await
            .map_err(|error| Status::internal(error.to_string()))?
            .map_err(|error| match error {
                EditError::NoSuchTrack => Status::not_found(error.to_string()),
                _ => Status::internal(error.to_string()),
            })?;
        Ok(Response::new(api::SetRatingResponse {}))
    }

    async fn create_smart_playlist(
        &self,
        request: Request<api::CreateSmartPlaylistRequest>,
    ) -> Result<Response<api::SmartPlaylistEdited>, Status> {
        let library = self.library()?;
        let request = request.into_inner();
        let rules = rules_from(request.rules)?;
        edited(library, move |library| {
            library.create_smart_playlist(&request.title, &rules)
        })
        .await
    }

    async fn update_smart_playlist(
        &self,
        request: Request<api::UpdateSmartPlaylistRequest>,
    ) -> Result<Response<api::SmartPlaylistEdited>, Status> {
        let library = self.library()?;
        let request = request.into_inner();
        let rules = rules_from(request.rules)?;
        edited(library, move |library| {
            library.update_smart_playlist(request.playlist_id as usize, &request.title, &rules)
        })
        .await
    }

    async fn create_playlist(
        &self,
        request: Request<api::CreatePlaylistRequest>,
    ) -> Result<Response<api::PlaylistsEdited>, Status> {
        let library = self.library()?;
        let request = request.into_inner();
        playlists_edited(library, move |library| {
            library
                .create_playlist(&request.title, request.parent_id.map(|id| id as usize))
                .map(Some)
        })
        .await
    }

    async fn rename_playlist(
        &self,
        request: Request<api::RenamePlaylistRequest>,
    ) -> Result<Response<api::PlaylistsEdited>, Status> {
        let library = self.library()?;
        let request = request.into_inner();
        playlists_edited(library, move |library| {
            library
                .rename_playlist(request.playlist_id as usize, &request.title)
                .map(|()| None)
        })
        .await
    }

    async fn delete_playlist(
        &self,
        request: Request<api::DeletePlaylistRequest>,
    ) -> Result<Response<api::PlaylistsEdited>, Status> {
        let library = self.library()?;
        let request = request.into_inner();
        playlists_edited(library, move |library| {
            library
                .delete_playlist(request.playlist_id as usize)
                .map(|()| None)
        })
        .await
    }

    async fn migrate_origins(
        &self,
        _: Request<api::MigrateOriginsRequest>,
    ) -> Result<Response<api::MigrateOriginsResponse>, Status> {
        let library = self.library()?;
        // Reads and writes a file: not on the async threads.
        tokio::task::spawn_blocking(move || {
            let locations = library.locations();
            let new = library
                .save_origins(
                    &locations
                        .iter()
                        .map(|(_, key, location)| (key.clone(), location.clone()))
                        .collect::<Vec<_>>(),
                    false,
                )
                .map_err(edit_status)?;
            let saved = library.origins();
            let known: Vec<api::TrackOrigin> = library
                .tracks
                .iter()
                .enumerate()
                .filter_map(|(index, track)| {
                    let origin = saved.get(track.origin_key.as_ref()?)?;
                    Some(api::TrackOrigin {
                        track_id: index as u32,
                        origin: origin.clone(),
                    })
                })
                .collect();
            Ok(api::MigrateOriginsResponse {
                saved: new as u32,
                kept: (locations.len() - new) as u32,
                skipped: (library.tracks.len() - locations.len()) as u32,
                origins: known,
            })
        })
        .await
        .map_err(|error| Status::internal(error.to_string()))?
        .map(Response::new)
    }

    async fn get_library_sync_diff(
        &self,
        request: Request<api::GetLibrarySyncDiffRequest>,
    ) -> Result<Response<api::GetLibrarySyncDiffResponse>, Status> {
        let library = self.library()?;
        let request = request.into_inner();
        let scratch = self.0.cache_root.clone();
        // Copies a database and reads it: not on the async threads.
        tokio::task::spawn_blocking(move || {
            mirror::diff(
                &library,
                std::path::Path::new(&request.target_path),
                &scratch,
                mirror_options(
                    request.remove_missing,
                    request.sync_stars,
                    request.sync_locations,
                    request.copy_tracks,
                    request.reverse,
                    request.create_if_missing,
                ),
            )
        })
        .await
        .map_err(|error| Status::internal(error.to_string()))?
        .map(Response::new)
        .map_err(mirror_status)
    }

    async fn sync_to_library(
        &self,
        request: Request<api::SyncToLibraryRequest>,
    ) -> Result<Response<api::SyncToLibraryResponse>, Status> {
        let library = self.library()?;
        let request = request.into_inner();
        let scratch = self.0.cache_root.clone();
        let progress = self.0.progress.clone();
        let reverse = request.reverse;
        let dir = library.dir.clone();
        let done = tokio::task::spawn_blocking(move || {
            let _running = progress.begin();
            mirror::push(
                &library,
                std::path::Path::new(&request.target_path),
                &scratch,
                mirror_options(
                    request.remove_missing,
                    request.sync_stars,
                    request.sync_locations,
                    request.copy_tracks,
                    request.reverse,
                    request.create_if_missing,
                ),
                &progress,
            )
        })
        .await
        .map_err(|error| Status::internal(error.to_string()))?
        .map_err(mirror_status)?;
        // The other way round the working copy was changed, and the library that is open
        // still reads the one that was replaced.
        if reverse && !done.backup_path.is_empty() {
            self.reopen(dir).await?;
        }
        Ok(Response::new(done))
    }

    async fn get_progress(
        &self,
        _: Request<api::GetProgressRequest>,
    ) -> Result<Response<api::OperationProgress>, Status> {
        let snapshot = self.0.progress.snapshot();
        Ok(Response::new(api::OperationProgress {
            running: snapshot.running,
            phase: match snapshot.phase {
                Phase::Idle => api::ProgressPhase::Unspecified,
                Phase::Preparing => api::ProgressPhase::Preparing,
                Phase::CopyingTracks => api::ProgressPhase::CopyingTracks,
                Phase::Updating => api::ProgressPhase::Updating,
                Phase::Writing => api::ProgressPhase::Writing,
                Phase::Pulling => api::ProgressPhase::Pulling,
            }
            .into(),
            done_bytes: snapshot.done_bytes,
            total_bytes: snapshot.total_bytes,
            done_files: snapshot.done_files,
            total_files: snapshot.total_files,
            current: snapshot.current,
        }))
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
            // (Read off the async threads; a library without origins just has none.)
            let origins = {
                let library = Arc::clone(&library);
                tokio::task::spawn_blocking(move || library.origins())
                    .await
                    .unwrap_or_default()
            };
            for (batch, tracks) in library.tracks.chunks(TRACKS_PER_BATCH).enumerate() {
                let first = batch * TRACKS_PER_BATCH;
                let tracks = tracks
                    .iter()
                    .enumerate()
                    .map(|(offset, track)| {
                        let id = first + offset;
                        let origin = track
                            .origin_key
                            .as_ref()
                            .and_then(|key| origins.get(key))
                            .cloned();
                        track_message(
                            id as u32,
                            track,
                            library.rating(id),
                            library.cue_count(id),
                            library.loop_count(id),
                            origin,
                        )
                    })
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
        Ok(Response::new(api::ListPlaylistsResponse {
            playlists: playlist_messages(&library),
        }))
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

    async fn get_cues(
        &self,
        request: Request<api::GetCuesRequest>,
    ) -> Result<Response<api::CuesResponse>, Status> {
        let library = self.library()?;
        let id = request.into_inner().track_id;
        if library.tracks.get(id as usize).is_none() {
            return Err(Status::not_found(format!("unknown track id {id}")));
        }
        let cues = tokio::task::spawn_blocking(move || library.cues(id as usize))
            .await
            .map_err(|error| Status::internal(error.to_string()))?
            .map_err(|error| Status::internal(error.to_string()))?;
        Ok(Response::new(cues_message(cues)))
    }

    async fn set_cues(
        &self,
        request: Request<api::SetCuesRequest>,
    ) -> Result<Response<api::CuesResponse>, Status> {
        let library = self.library()?;
        let request = request.into_inner();
        let id = request.track_id;
        let mut cues = Vec::with_capacity(request.cues.len());
        for cue in request.cues {
            cues.push(Cue {
                slot: cue.slot as usize,
                label: cue.label,
                position_seconds: cue.position_seconds,
                color: cue.color_argb,
            });
        }
        // Reads and writes the database: not on the async threads.
        let cues = tokio::task::spawn_blocking(move || library.set_cues(id as usize, &cues))
            .await
            .map_err(|error| Status::internal(error.to_string()))?
            .map_err(edit_status)?;
        Ok(Response::new(cues_message(cues)))
    }

    async fn get_loops(
        &self,
        request: Request<api::GetLoopsRequest>,
    ) -> Result<Response<api::LoopsResponse>, Status> {
        let library = self.library()?;
        let id = request.into_inner().track_id;
        if library.tracks.get(id as usize).is_none() {
            return Err(Status::not_found(format!("unknown track id {id}")));
        }
        let loops = tokio::task::spawn_blocking(move || library.loops(id as usize))
            .await
            .map_err(|error| Status::internal(error.to_string()))?
            .map_err(|error| Status::internal(error.to_string()))?;
        Ok(Response::new(loops_message(loops)))
    }

    async fn set_loops(
        &self,
        request: Request<api::SetLoopsRequest>,
    ) -> Result<Response<api::LoopsResponse>, Status> {
        let library = self.library()?;
        let request = request.into_inner();
        let id = request.track_id;
        let loops = request
            .loops
            .into_iter()
            .map(|saved| Loop {
                slot: saved.slot as usize,
                label: saved.label,
                start_seconds: saved.start_seconds,
                end_seconds: saved.end_seconds,
                color: saved.color_argb,
            })
            .collect::<Vec<_>>();
        // Reads and writes the database: not on the async threads.
        let loops = tokio::task::spawn_blocking(move || library.set_loops(id as usize, &loops))
            .await
            .map_err(|error| Status::internal(error.to_string()))?
            .map_err(edit_status)?;
        Ok(Response::new(loops_message(loops)))
    }
}

fn loops_message(loops: Vec<Loop>) -> api::LoopsResponse {
    api::LoopsResponse {
        loops: loops
            .into_iter()
            .map(|saved| api::Loop {
                slot: saved.slot as u32,
                label: saved.label,
                start_seconds: saved.start_seconds,
                end_seconds: saved.end_seconds,
                color_argb: saved.color,
            })
            .collect(),
    }
}

fn cues_message(cues: Vec<Cue>) -> api::CuesResponse {
    api::CuesResponse {
        cues: cues
            .into_iter()
            .map(|cue| api::Cue {
                slot: cue.slot as u32,
                label: cue.label,
                position_seconds: cue.position_seconds,
                color_argb: cue.color,
            })
            .collect(),
    }
}

fn playlist_messages(library: &Library) -> Vec<api::Playlist> {
    library
        .playlists()
        .iter()
        .enumerate()
        .map(|(index, playlist)| api::Playlist {
            id: index as u32,
            title: playlist.title.clone(),
            depth: playlist.depth as u32,
            parent_id: playlist.parent.map(|parent| parent as u32),
            has_children: playlist.has_children,
            is_smart: playlist.smart,
            rules: playlist.rules.as_ref().map(|rules| api::SmartRules {
                match_all: rules.match_all(),
                rules: rules
                    .parts()
                    .map(|(column, condition, param)| api::SmartRule {
                        column: column.to_string(),
                        condition: condition.to_string(),
                        param,
                    })
                    .collect(),
            }),
            track_ids: playlist.tracks.iter().map(|&track| track as u32).collect(),
        })
        .collect()
}

fn rules_from(rules: Option<api::SmartRules>) -> Result<Rules, Status> {
    let rules = rules.ok_or_else(|| Status::invalid_argument("a smart playlist needs rules"))?;
    Rules::from_parts(
        rules.match_all,
        rules
            .rules
            .into_iter()
            .map(|rule| (rule.column, rule.condition, rule.param))
            .collect(),
    )
    .map_err(Status::invalid_argument)
}

/// Runs a change of the smart playlists off the async threads (it writes a file,
/// possibly after waiting for a read) and answers with the playlists as they are then.
async fn edited(
    library: Arc<Library>,
    edit: impl FnOnce(&Library) -> Result<usize, EditError> + Send + 'static,
) -> Result<Response<api::SmartPlaylistEdited>, Status> {
    tokio::task::spawn_blocking(move || {
        edit(&library).map(|index| api::SmartPlaylistEdited {
            playlist_id: index as u32,
            playlists: playlist_messages(&library),
        })
    })
    .await
    .map_err(|error| Status::internal(error.to_string()))?
    .map(Response::new)
    .map_err(edit_status)
}

/// Like `edited`, for the changes of playlists in general: the closure gives the index of
/// the playlist that was added, if one was.
async fn playlists_edited(
    library: Arc<Library>,
    edit: impl FnOnce(&Library) -> Result<Option<usize>, EditError> + Send + 'static,
) -> Result<Response<api::PlaylistsEdited>, Status> {
    tokio::task::spawn_blocking(move || {
        edit(&library).map(|index| api::PlaylistsEdited {
            playlist_id: index.map(|index| index as u32),
            playlists: playlist_messages(&library),
        })
    })
    .await
    .map_err(|error| Status::internal(error.to_string()))?
    .map(Response::new)
    .map_err(edit_status)
}

fn mirror_options(
    remove_missing: bool,
    stars: bool,
    locations: bool,
    copy_tracks: bool,
    reverse: bool,
    create: bool,
) -> mirror::Options {
    mirror::Options {
        remove_missing,
        reverse,
        create,
        tracks: crate::track_sync::Options {
            stars,
            locations,
            copy_tracks,
        },
    }
}

fn mirror_status(error: MirrorError) -> Status {
    match error {
        MirrorError::NotALibrary(_) => Status::not_found(error.to_string()),
        MirrorError::SameLibrary => Status::invalid_argument(error.to_string()),
        MirrorError::Busy => Status::failed_precondition(error.to_string()),
        MirrorError::Edit(error) => edit_status(error),
        MirrorError::Io(_) | MirrorError::Open(_) | MirrorError::Database(_) => {
            Status::internal(error.to_string())
        }
    }
}

fn edit_status(error: EditError) -> Status {
    match error {
        EditError::NoSuchTrack | EditError::NoSuchPlaylist => Status::not_found(error.to_string()),
        EditError::NotSmart
        | EditError::IsSmart
        | EditError::EmptyTitle
        | EditError::BadCue(_)
        | EditError::BadLoop(_) => Status::invalid_argument(error.to_string()),
        EditError::TitleTaken => Status::already_exists(error.to_string()),
        EditError::NoSmartlists
        | EditError::NoAnalysis
        | EditError::UnreadableCues
        | EditError::UnreadableLoops => Status::failed_precondition(error.to_string()),
        EditError::Cache(_) | EditError::Database(_) => Status::internal(error.to_string()),
    }
}

fn track_message(
    id: u32,
    track: &library::Track,
    rating: Option<u32>,
    cue_count: u32,
    loop_count: u32,
    origin: Option<String>,
) -> api::Track {
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
        rating,
        soundswitch: track.lighting.state.into(),
        soundswitch_projects: track.lighting.projects.clone(),
        path: track.relative_path.clone(),
        origin,
        cue_count,
        loop_count,
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

/// A sync problem the caller can do something about is a failed precondition; the rest is
/// an error of the backend.
fn sync_status(error: SyncError) -> Status {
    match error {
        SyncError::Unavailable(_) | SyncError::Busy | SyncError::ChangedOutside => {
            Status::failed_precondition(error.to_string())
        }
        SyncError::Failed(_) => Status::internal(error.to_string()),
    }
}
