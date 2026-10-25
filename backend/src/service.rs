//! The gRPC services (`openenginelib.v1`) that the UI talks to.

use std::path::PathBuf;
use std::sync::{Arc, Mutex, RwLock};

use open_engine_api::API_VERSION;
use open_engine_api::v1 as api;
use open_engine_api::v1::backend_service_server::BackendService;
use open_engine_api::v1::library_service_server::LibraryService;
use open_engine_api::v1::playback_service_server::PlaybackService;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status};

use crate::audio_client::Audio;
use crate::controller::Controller;
use crate::cues::Cue;
use crate::folder_import;
use crate::library::{self, EditError, Library, OpenError};
use crate::loops::Loop;
use crate::mirror::{self, MirrorError};
use crate::name_order::NameOrder;
use crate::progress::{Phase, Progress};
use crate::repair;
use crate::save::{self, SaveError};
use crate::slot::{self, Slot};
use crate::smartlist::Rules;
use crate::sync::{self, SyncError};
use crate::track_analysis::Found;

mod controller;

/// Tracks per message of `ListTracks`, to stay well below gRPC's message size limit.
const TRACKS_PER_BATCH: usize = 500;

struct Shared {
    /// The open Engine library and the open folder of music: one of each at most.
    engine: RwLock<Option<Arc<Library>>>,
    folder: RwLock<Option<Arc<Library>>>,
    /// The library the player was last given tracks of, which the DJ controller acts on.
    playing: Mutex<Option<Slot>>,
    /// The audio process, which plays and decodes for us, or why it is not there.
    audio: Result<Audio, String>,
    /// Where copies of the libraries' databases are kept.
    cache_root: PathBuf,
    /// How far the sync that is running has got.
    progress: Progress,
    /// The DJ controller, and the task that listens to it while it is turned on.
    controller: Controller,
    listener: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

#[derive(Clone)]
pub struct Service(Arc<Shared>);

impl Service {
    pub fn new(audio: Result<Audio, String>, cache_root: PathBuf, controller: Controller) -> Self {
        Self(Arc::new(Shared {
            engine: RwLock::new(None),
            folder: RwLock::new(None),
            playing: Mutex::new(None),
            audio,
            cache_root,
            progress: Progress::default(),
            controller,
            listener: Mutex::new(None),
        }))
    }

    /// The connection to the audio process.
    fn audio(&self) -> Result<crate::audio_client::Client, Status> {
        self.0
            .audio
            .as_ref()
            .map(Audio::client)
            .map_err(|error| Status::unavailable(format!("no audio process: {error}")))
    }

    /// Where the library is kept.
    fn slot(&self, slot: Slot) -> &RwLock<Option<Arc<Library>>> {
        match slot {
            Slot::Engine => &self.0.engine,
            Slot::Folder => &self.0.folder,
        }
    }

    /// The slot the request that is being handled is about: the one it names, else the Engine
    /// library's if there is one, else the folder's.
    fn requested(&self) -> Slot {
        slot::current().unwrap_or_else(|| {
            if self.0.engine.read().expect("library lock").is_some() {
                Slot::Engine
            } else {
                Slot::Folder
            }
        })
    }

    /// Replaces the Engine library that is open by one read again from the same working copy
    /// (which changed). Playback is not stopped.
    async fn reopen(&self, dir: PathBuf) -> Result<(), Status> {
        let cache_root = self.0.cache_root.clone();
        let library = tokio::task::spawn_blocking(move || Library::open(&dir, &cache_root))
            .await
            .map_err(|error| Status::internal(error.to_string()))?
            .map_err(|error| Status::internal(error.to_string()))?;
        *self.0.engine.write().expect("library lock") = Some(Arc::new(library));
        Ok(())
    }

    /// The library the request is about.
    fn library(&self) -> Result<Arc<Library>, Status> {
        self.slot(self.requested())
            .read()
            .expect("library lock")
            .clone()
            .ok_or_else(|| Status::failed_precondition("no library is open"))
    }

    /// The library the player plays from (what the DJ controller acts on), else the one a
    /// request would be about.
    fn playing_library(&self) -> Result<Arc<Library>, Status> {
        let playing = *self.0.playing.lock().expect("playing lock");
        match playing.and_then(|slot| self.slot(slot).read().expect("library lock").clone()) {
            Some(library) => Ok(library),
            None => self.library(),
        }
    }

    /// The open library, which must be an Engine library: what only those have (syncing,
    /// playlists to edit, ratings) is not there for a plain folder of music.
    fn engine_library(&self) -> Result<Arc<Library>, Status> {
        let library = self.library()?;
        if library.is_folder() {
            return Err(Status::failed_precondition(
                "the open library is a folder of music, not an Engine library",
            ));
        }
        Ok(library)
    }

    /// Takes tracks out of the working copy of the open Engine library (`remove`), and if any
    /// went, reads the working copy again and stops what plays: the tracks after the first one
    /// removed have other ids now, and the player's queue has the old ones.
    async fn remove_from_library(
        &self,
        remove: impl FnOnce(&Library) -> Result<library::TracksRemoved, library::EditError>
        + Send
        + 'static,
    ) -> Result<library::TracksRemoved, Status> {
        let library = self.engine_library()?;
        let dir = library.dir.clone();
        // Writes the database: not on the async threads.
        let removed = tokio::task::spawn_blocking(move || remove(&library))
            .await
            .map_err(|error| Status::internal(error.to_string()))?
            .map_err(edit_status)?;
        if removed.tracks > 0 {
            self.reopen(dir).await?;
            if let Ok(mut audio) = self.audio() {
                let _ = audio.stop(api::StopRequest {}).await;
            }
        }
        Ok(removed)
    }

    /// Stops playback, releases the audio device and forgets the library in `slot` (the
    /// queue may hold tracks of either library). The other library stays open.
    async fn close(&self, slot: Slot) {
        if let Ok(audio) = &self.0.audio {
            let _ = audio.client().close(api::CloseRequest {}).await;
        }
        *self.0.playing.lock().expect("playing lock") = None;
        *self.slot(slot).write().expect("library lock") = None;
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
        self.close(Slot::Engine).await;

        let library = tokio::task::spawn_blocking(move || Library::open(&path, &cache_root))
            .await
            .map_err(|error| Status::internal(error.to_string()))?
            .map_err(|error| match error {
                OpenError::NotALibrary(_) | OpenError::NotAFolder(..) => {
                    Status::not_found(error.to_string())
                }
                OpenError::Cache(_) | OpenError::Database(_) => Status::internal(error.to_string()),
            })?;

        let response = api::OpenLibraryResponse {
            path: library.dir.display().to_string(),
            cache_path: library.cache.display().to_string(),
            track_count: library.tracks.len() as u32,
            playlist_count: library.playlists().len() as u32,
        };
        *self.0.engine.write().expect("library lock") = Some(Arc::new(library));
        Ok(Response::new(response))
    }

    async fn open_local_folder(
        &self,
        request: Request<api::OpenLocalFolderRequest>,
    ) -> Result<Response<api::OpenLibraryResponse>, Status> {
        let path = PathBuf::from(request.into_inner().path);
        // Where an edit would be noted, which a folder of music never gets.
        let scratch = self.0.cache_root.join("local-folder");
        self.close(Slot::Folder).await;

        // Looking at every file takes a moment and must not block the async threads. The UI
        // asks `GetProgress` meanwhile.
        let progress = self.0.progress.clone();
        let library = tokio::task::spawn_blocking(move || {
            let _running = progress.begin();
            Library::open_folder(&path, scratch, &progress)
        })
        .await
        .map_err(|error| Status::internal(error.to_string()))?
        .map_err(|error| match error {
            OpenError::NotAFolder(..) | OpenError::NotALibrary(_) => {
                Status::not_found(error.to_string())
            }
            OpenError::Cache(_) | OpenError::Database(_) => Status::internal(error.to_string()),
        })?;

        let response = api::OpenLibraryResponse {
            path: library.dir.display().to_string(),
            cache_path: String::new(),
            track_count: library.tracks.len() as u32,
            playlist_count: library.playlists().len() as u32,
        };
        *self.0.folder.write().expect("library lock") = Some(Arc::new(library));
        Ok(Response::new(response))
    }

    async fn save_library_as(
        &self,
        request: Request<api::SaveLibraryAsRequest>,
    ) -> Result<Response<api::SaveLibraryAsResponse>, Status> {
        let library = self.engine_library()?;
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
        *self.0.engine.write().expect("library lock") = Some(Arc::new(saved));
        Ok(Response::new(response))
    }

    async fn get_sync_diff(
        &self,
        request: Request<api::GetSyncDiffRequest>,
    ) -> Result<Response<api::GetSyncDiffResponse>, Status> {
        let library = self.engine_library()?;
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
        let library = self.engine_library()?;
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
        let library = self.engine_library()?;
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
        let library = self.engine_library()?;
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
        let library = self.engine_library()?;
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
        let library = self.engine_library()?;
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
        let library = self.engine_library()?;
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
        let library = self.engine_library()?;
        let request = request.into_inner();
        playlists_edited(library, move |library| {
            library
                .rename_playlist(request.playlist_id as usize, &request.title)
                .map(|()| None)
        })
        .await
    }

    async fn move_track_in_playlist(
        &self,
        request: Request<api::MoveTrackInPlaylistRequest>,
    ) -> Result<Response<api::PlaylistsEdited>, Status> {
        let library = self.engine_library()?;
        let request = request.into_inner();
        playlists_edited(library, move |library| {
            library
                .move_track_in_playlist(
                    request.playlist_id as usize,
                    request.track_id as usize,
                    request.before_track_id.map(|id| id as usize),
                )
                .map(|()| None)
        })
        .await
    }

    async fn move_playlist(
        &self,
        request: Request<api::MovePlaylistRequest>,
    ) -> Result<Response<api::PlaylistsEdited>, Status> {
        let library = self.engine_library()?;
        let request = request.into_inner();
        playlists_edited(library, move |library| {
            library
                .move_playlist(
                    request.playlist_id as usize,
                    request.parent_id.map(|id| id as usize),
                    request.before_id.map(|id| id as usize),
                )
                .map(Some)
        })
        .await
    }

    async fn delete_playlist(
        &self,
        request: Request<api::DeletePlaylistRequest>,
    ) -> Result<Response<api::PlaylistsEdited>, Status> {
        let library = self.engine_library()?;
        let request = request.into_inner();
        playlists_edited(library, move |library| {
            library
                .delete_playlist(request.playlist_id as usize)
                .map(|()| None)
        })
        .await
    }

    async fn repair_library(
        &self,
        request: Request<api::RepairLibraryRequest>,
    ) -> Result<Response<api::RepairLibraryResponse>, Status> {
        let target = request.into_inner().target_path;
        // Moves and deletes files, maybe on a slow drive: not on the async threads.
        let done =
            tokio::task::spawn_blocking(move || repair::repair(std::path::Path::new(&target)))
                .await
                .map_err(|error| Status::internal(error.to_string()))?
                .map_err(|error| match error {
                    repair::RepairError::NotALibrary(_) => Status::not_found(error.to_string()),
                    repair::RepairError::Io(_) => Status::internal(error.to_string()),
                })?;
        Ok(Response::new(api::RepairLibraryResponse {
            target_path: done.library.display().to_string(),
            origins_moved: done.origins_moved,
            origins_conflict: done.origins_conflict,
            backup_removed: done.backup_removed,
        }))
    }

    async fn migrate_origins(
        &self,
        _: Request<api::MigrateOriginsRequest>,
    ) -> Result<Response<api::MigrateOriginsResponse>, Status> {
        let library = self.engine_library()?;
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
            Ok(api::MigrateOriginsResponse {
                saved: new as u32,
                kept: (locations.len() - new) as u32,
                skipped: (library.tracks.len() - locations.len()) as u32,
                origins: known_origins(&library),
            })
        })
        .await
        .map_err(|error| Status::internal(error.to_string()))?
        .map(Response::new)
    }

    async fn remove_streaming_tracks(
        &self,
        _: Request<api::RemoveStreamingTracksRequest>,
    ) -> Result<Response<api::RemoveStreamingTracksResponse>, Status> {
        let removed = self
            .remove_from_library(|library| library.remove_streaming_tracks())
            .await?;
        Ok(Response::new(api::RemoveStreamingTracksResponse {
            removed: removed.tracks as u32,
            playlist_entries: removed.playlist_entries as u32,
        }))
    }

    async fn remove_tracks(
        &self,
        request: Request<api::RemoveTracksRequest>,
    ) -> Result<Response<api::RemoveTracksResponse>, Status> {
        let indices: Vec<usize> = request
            .into_inner()
            .track_ids
            .into_iter()
            .map(|id| id as usize)
            .collect();
        let removed = self
            .remove_from_library(move |library| library.remove_tracks(&indices))
            .await?;
        Ok(Response::new(api::RemoveTracksResponse {
            removed: removed.tracks as u32,
            playlist_entries: removed.playlist_entries as u32,
        }))
    }

    async fn remove_track_from_playlist(
        &self,
        request: Request<api::RemoveTrackFromPlaylistRequest>,
    ) -> Result<Response<api::PlaylistsEdited>, Status> {
        let library = self.engine_library()?;
        let request = request.into_inner();
        playlists_edited(library, move |library| {
            library
                .remove_track_from_playlist(request.playlist_id as usize, request.track_id as usize)
                .map(|_| None)
        })
        .await
    }

    async fn get_origin_prefix(
        &self,
        _: Request<api::GetOriginPrefixRequest>,
    ) -> Result<Response<api::GetOriginPrefixResponse>, Status> {
        let library = self.engine_library()?;
        // Reads a file: not on the async threads.
        tokio::task::spawn_blocking(move || {
            let (prefix, count) = library.origin_prefix();
            api::GetOriginPrefixResponse {
                prefix,
                count: count as u32,
            }
        })
        .await
        .map(Response::new)
        .map_err(|error| Status::internal(error.to_string()))
    }

    async fn replace_origin_prefix(
        &self,
        request: Request<api::ReplaceOriginPrefixRequest>,
    ) -> Result<Response<api::ReplaceOriginPrefixResponse>, Status> {
        let library = self.engine_library()?;
        let request = request.into_inner();
        if request.from.is_empty() || request.to.is_empty() {
            return Err(Status::invalid_argument(
                "the folder to replace and the new one must not be empty",
            ));
        }
        // Reads and writes a file: not on the async threads.
        tokio::task::spawn_blocking(move || {
            let changed = library
                .replace_origin_prefix(&request.from, &request.to)
                .map_err(edit_status)?;
            Ok(api::ReplaceOriginPrefixResponse {
                changed: changed as u32,
                origins: known_origins(&library),
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
        let library = self.engine_library()?;
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
                    crate::track_sync::Options {
                        stars: request.sync_stars,
                        locations: request.sync_locations,
                        copy_tracks: request.copy_tracks,
                        copy_stems: request.copy_stems,
                        analysis: request.sync_analysis,
                    },
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
        let library = self.engine_library()?;
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
                    crate::track_sync::Options {
                        stars: request.sync_stars,
                        locations: request.sync_locations,
                        copy_tracks: request.copy_tracks,
                        copy_stems: request.copy_stems,
                        analysis: request.sync_analysis,
                    },
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

    async fn import_folder_as_playlist(
        &self,
        request: Request<api::ImportFolderRequest>,
    ) -> Result<Response<api::ImportFolderResponse>, Status> {
        let library = self.library()?;
        if !library.is_folder() {
            return Err(Status::failed_precondition(
                "the open library is not a folder of music",
            ));
        }
        let request = request.into_inner();
        let index = request.playlist_id as usize;
        let title = {
            let playlists = library.playlists();
            let playlist = playlists
                .get(index)
                .ok_or_else(|| Status::not_found(format!("unknown playlist id {index}")))?;
            if request.title.trim().is_empty() {
                playlist.title.clone()
            } else {
                request.title.clone()
            }
        };
        let progress = self.0.progress.clone();
        // Copies files and writes a database: not on the async threads.
        tokio::task::spawn_blocking(move || {
            let _running = progress.try_begin().ok_or_else(|| {
                Status::failed_precondition(
                    "another operation is running, try again when it is done",
                )
            })?;
            folder_import::import(
                &library,
                &folder_import::Request {
                    target: std::path::Path::new(&request.target_path),
                    playlist: index,
                    title: &title,
                    copy_files: request.copy_files,
                    name_order: NameOrder::from_api(request.name_order),
                },
                &progress,
            )
            .map_err(mirror_status)
        })
        .await
        .map_err(|error| Status::internal(error.to_string()))?
        .map(Response::new)
    }

    async fn preview_folder_import(
        &self,
        request: Request<api::PreviewFolderImportRequest>,
    ) -> Result<Response<api::PreviewFolderImportResponse>, Status> {
        let library = self.library()?;
        if !library.is_folder() {
            return Err(Status::failed_precondition(
                "the open library is not a folder of music",
            ));
        }
        let request = request.into_inner();
        // Sorting thousands of names is not for the async threads.
        let paths = tokio::task::spawn_blocking(move || {
            folder_import::preview(
                &library,
                request.playlist_id as usize,
                NameOrder::from_api(request.name_order),
            )
            .map_err(mirror_status)
        })
        .await
        .map_err(|error| Status::internal(error.to_string()))??;
        Ok(Response::new(api::PreviewFolderImportResponse { paths }))
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
                Phase::Scanning => api::ProgressPhase::Scanning,
                Phase::ReadingTags => api::ProgressPhase::ReadingTags,
                Phase::AnalysingDecoding => api::ProgressPhase::AnalysingDecoding,
                Phase::AnalysingBeats => api::ProgressPhase::AnalysingBeats,
                Phase::AnalysingKey => api::ProgressPhase::AnalysingKey,
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
        // The one the request names; without one, both.
        match slot::current() {
            Some(slot) => self.close(slot).await,
            None => {
                self.close(Slot::Engine).await;
                self.close(Slot::Folder).await;
            }
        }
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
            // (Read off the async threads; a library without origins just has none, nor
            // analyses that Open Engine Lib made.)
            let (origins, ours) = {
                let library = Arc::clone(&library);
                tokio::task::spawn_blocking(move || (library.origins(), library.analysed_by_us()))
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
                        let analysis = library.analysis(id);
                        track_message(
                            id as u32,
                            track,
                            Counts {
                                rating: library.rating(id),
                                cues: library.cue_count(id),
                                loops: library.loop_count(id),
                                bpm: library.analysed_bpm(id),
                                analysis,
                                analysis_note: ours.get(&id).cloned(),
                            },
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
        let track = library
            .tracks
            .get(id as usize)
            .ok_or_else(|| Status::invalid_argument(format!("unknown track id {id}")))?;
        if track.streaming {
            return Err(Status::failed_precondition(
                "a streaming track has no audio file to analyse",
            ));
        }
        let path = track.path.display().to_string();
        let mut audio = self.audio()?;

        // The saved origin is where else the file may be, and the beatgrid is read from
        // the database: not on the async threads.
        let (origin, grid) = tokio::task::spawn_blocking(move || {
            let origin = library.tracks[id as usize]
                .origin_key
                .as_ref()
                .and_then(|key| library.origins().remove(key));
            let grid = library.beat_grid(id as usize).unwrap_or_else(|error| {
                // The waveform is still worth showing without its grid.
                eprintln!("could not read the beatgrid of track {id}: {error}");
                None
            });
            (origin, grid)
        })
        .await
        .map_err(|error| Status::internal(error.to_string()))?;

        // The audio process decodes the file (and checks the settings): its errors say
        // what is wrong with the file or the request, so they are passed on.
        let peaks = audio
            .analyze_waveform(api::AnalyzeWaveformRequest {
                path,
                fallback_path: origin.unwrap_or_default(),
                low_mid_hz: request.low_mid_hz,
                mid_high_hz: request.mid_high_hz,
                gain_low: request.gain_low,
                gain_mid: request.gain_mid,
                gain_high: request.gain_high,
            })
            .await?
            .into_inner();

        Ok(Response::new(api::GetWaveformResponse {
            duration_seconds: peaks.duration_seconds,
            columns_per_second: peaks.columns_per_second,
            columns: peaks.columns,
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

    async fn analyze_track(
        &self,
        request: Request<api::AnalyzeTrackRequest>,
    ) -> Result<Response<api::AnalyzeTrackResponse>, Status> {
        let library = self.library()?;
        let request = request.into_inner();
        let id = request.track_id;
        let index = id as usize;
        let track = library
            .tracks
            .get(index)
            .ok_or_else(|| Status::invalid_argument(format!("unknown track id {id}")))?;
        if library.is_folder() {
            return Err(edit_status(EditError::NotEngine));
        }
        if track.streaming {
            return Err(edit_status(EditError::Streaming));
        }
        if library.analysis(index) != 0 && !request.overwrite {
            return Err(edit_status(EditError::AlreadyAnalysed));
        }
        let path = track.path.display().to_string();
        let title = track.title.clone();
        let mut audio = self.audio()?;
        // (Taken after the checks, which need no progress; ends when this call does.)
        let progress = self.0.progress.clone();
        let _running = progress.try_begin().ok_or_else(|| {
            Status::failed_precondition("another operation is running, try again when it is done")
        })?;
        progress.analysing(Phase::AnalysingDecoding, 0, &title);

        let origin = {
            let library = Arc::clone(&library);
            tokio::task::spawn_blocking(move || {
                library.tracks[index]
                    .origin_key
                    .as_ref()
                    .and_then(|key| library.origins().remove(key))
            })
            .await
            .map_err(|error| Status::internal(error.to_string()))?
        };

        // The audio process decodes the file and finds the beats and the key, and says how
        // far it is; its errors (no file, no models) say what is wrong and are passed on.
        let mut events = audio
            .analyze_track(api::AudioAnalyzeTrackRequest {
                path,
                fallback_path: origin.unwrap_or_default(),
            })
            .await?
            .into_inner();
        let found = loop {
            let event = events.message().await?.ok_or_else(|| {
                Status::internal("the audio process ended the analysis without a result")
            })?;
            match event.event {
                Some(api::audio_analyze_track_event::Event::Progress(step)) => {
                    let phase = match step.stage() {
                        api::AudioAnalysisStage::Beats => Phase::AnalysingBeats,
                        api::AudioAnalysisStage::Key => Phase::AnalysingKey,
                        _ => Phase::AnalysingDecoding,
                    };
                    progress.analysing(
                        phase,
                        (step.fraction.clamp(0.0, 1.0) * 1000.0) as u64,
                        &title,
                    );
                }
                Some(api::audio_analyze_track_event::Event::Found(found)) => break found,
                None => {}
            }
        };
        progress.analysing(Phase::AnalysingKey, 1000, &title);

        let found = Found {
            beats: found.beats,
            downbeats: found.downbeats,
            sample_rate: found.sample_rate,
            frames: found.frames,
            key: found.key,
            overview: found.overview,
        };
        let overwrite = request.overwrite;
        // Writes the database: not on the async threads.
        let applied =
            tokio::task::spawn_blocking(move || library.apply_analysis(index, &found, overwrite))
                .await
                .map_err(|error| Status::internal(error.to_string()))?
                .map_err(edit_status)?;
        Ok(Response::new(api::AnalyzeTrackResponse {
            analysis: applied.bits,
            analysis_origin: api::AnalysisOrigin::OpenEngineLib.into(),
            analysis_note: Some(applied.note),
            bpm: Some(applied.bpm),
            key: applied.key.map(str::to_owned),
            beats: applied.beats as u32,
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

/// The origin of every track of the library that has one saved.
fn known_origins(library: &Library) -> Vec<api::TrackOrigin> {
    let saved = library.origins();
    library
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
        .collect()
}

fn mirror_options(
    remove_missing: bool,
    tracks: crate::track_sync::Options,
    reverse: bool,
    create: bool,
) -> mirror::Options {
    mirror::Options {
        remove_missing,
        reverse,
        create,
        tracks,
    }
}

fn mirror_status(error: MirrorError) -> Status {
    match error {
        MirrorError::NotALibrary(_) => Status::not_found(error.to_string()),
        MirrorError::SameLibrary | MirrorError::NothingToImport => {
            Status::invalid_argument(error.to_string())
        }
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
        EditError::BadMove => Status::invalid_argument(error.to_string()),
        EditError::NoSmartlists
        | EditError::NotEngine
        | EditError::Streaming
        | EditError::AlreadyAnalysed
        | EditError::NoBeats
        | EditError::NoAnalysis
        | EditError::UnreadableCues
        | EditError::UnreadableLoops => Status::failed_precondition(error.to_string()),
        EditError::Cache(_) | EditError::Database(_) => Status::internal(error.to_string()),
    }
}

/// What a track has now, which may differ from what it was opened with.
struct Counts {
    rating: Option<u32>,
    cues: u32,
    loops: u32,
    /// The tempo of an analysis made since the library was opened.
    bpm: Option<f64>,
    /// The `ANALYSIS_*` bits.
    analysis: u32,
    /// What is saved about an analysis that Open Engine Lib made.
    analysis_note: Option<String>,
}

fn track_message(
    id: u32,
    track: &library::Track,
    now: Counts,
    origin: Option<String>,
) -> api::Track {
    let Counts {
        rating,
        cues: cue_count,
        loops: loop_count,
        bpm,
        analysis,
        analysis_note,
    } = now;
    let analysis_origin = match (analysis, &analysis_note) {
        (0, _) => api::AnalysisOrigin::Unspecified,
        (_, Some(_)) => api::AnalysisOrigin::OpenEngineLib,
        (_, None) => api::AnalysisOrigin::Engine,
    };
    api::Track {
        id,
        title: track.title.clone(),
        artist: track.artist.clone(),
        album: track.album.clone(),
        genre: track.genre.clone(),
        bpm: bpm.or(track.bpm),
        length_seconds: track.length,
        streaming: track.streaming,
        album_art_id: track.album_art,
        rating,
        soundswitch: track.lighting.state.into(),
        soundswitch_projects: track.lighting.projects.clone(),
        has_stems: track.has_stems,
        analysis,
        analysis_origin: analysis_origin.into(),
        analysis_note,
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
        *self.0.playing.lock().expect("playing lock") = Some(self.requested());
        let request = request.into_inner();
        let mut audio = self.audio()?;

        // The saved origins, for a file that is not at its location. (Read from the
        // database: not on the async threads.)
        let origins = {
            let library = Arc::clone(&library);
            tokio::task::spawn_blocking(move || library.origins())
                .await
                .map_err(|error| Status::internal(error.to_string()))?
        };
        let queue = request
            .track_ids
            .iter()
            .map(|&id| {
                let track = library
                    .tracks
                    .get(id as usize)
                    .ok_or_else(|| Status::invalid_argument(format!("unknown track id {id}")))?;
                Ok(api::QueueEntry {
                    track_id: id,
                    path: track.path.display().to_string(),
                    fallback_path: track
                        .origin_key
                        .as_ref()
                        .and_then(|key| origins.get(key))
                        .cloned()
                        .unwrap_or_default(),
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

        Ok(audio
            .play(api::AudioPlayRequest {
                queue,
                position: request.position,
                start_paused: request.start_paused,
                start_position_ms: request.start_position_ms,
            })
            .await?)
    }

    async fn toggle_pause(
        &self,
        _: Request<api::TogglePauseRequest>,
    ) -> Result<Response<api::PlaybackStatus>, Status> {
        Ok(self
            .audio()?
            .toggle_pause(api::TogglePauseRequest {})
            .await?)
    }

    async fn stop(
        &self,
        _: Request<api::StopRequest>,
    ) -> Result<Response<api::PlaybackStatus>, Status> {
        Ok(self.audio()?.stop(api::StopRequest {}).await?)
    }

    async fn next(
        &self,
        _: Request<api::NextRequest>,
    ) -> Result<Response<api::PlaybackStatus>, Status> {
        Ok(self.audio()?.next(api::NextRequest {}).await?)
    }

    async fn previous(
        &self,
        _: Request<api::PreviousRequest>,
    ) -> Result<Response<api::PlaybackStatus>, Status> {
        Ok(self.audio()?.previous(api::PreviousRequest {}).await?)
    }

    async fn seek(
        &self,
        request: Request<api::SeekRequest>,
    ) -> Result<Response<api::PlaybackStatus>, Status> {
        Ok(self.audio()?.seek(request.into_inner()).await?)
    }

    async fn begin_scratch(
        &self,
        request: Request<api::BeginScratchRequest>,
    ) -> Result<Response<api::PlaybackStatus>, Status> {
        Ok(self.audio()?.begin_scratch(request.into_inner()).await?)
    }

    async fn move_scratch(
        &self,
        request: Request<api::MoveScratchRequest>,
    ) -> Result<Response<api::PlaybackStatus>, Status> {
        Ok(self.audio()?.move_scratch(request.into_inner()).await?)
    }

    async fn end_scratch(
        &self,
        request: Request<api::EndScratchRequest>,
    ) -> Result<Response<api::PlaybackStatus>, Status> {
        Ok(self.audio()?.end_scratch(request.into_inner()).await?)
    }

    async fn get_status(
        &self,
        _: Request<api::GetStatusRequest>,
    ) -> Result<Response<api::PlaybackStatus>, Status> {
        Ok(self.audio()?.get_status(api::GetStatusRequest {}).await?)
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
