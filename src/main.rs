#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod data;
mod handlers;
mod views;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use backend::{Backend, Control, WaveformSettings};
use browser::{Browser, Change, Source};
use clock::Clock;
use columns::{Column, ColumnStore};
use config::{ConfigStore, Language, Locations, ThemeMode, WaveformBand, WaveformColors};
use controller::Controller;
use covers::Covers;
use cues::{Cue, TrackCues};
use data::{
    backend, clock, colors, columns, config, controller, covers, cues, drives, i18n, local_folders,
    loops, model, reachable, recent, session, waveform,
};
use drives::Drive;
use i18n::fill;
use iced::widget::{button, column, container, row, scrollable, text, tooltip};
use iced::{Center, Element, Fill, Subscription, Task, Theme};
use library_sync_view::Stage as LibrarySyncStage;
use local_folders::LocalFolders;
use loops::{Loop, TrackLoops};
use menubar::MenuItem;
use model::Library;
use open_engine_api::v1::{
    ControllerAction, ControllerState, PlaybackError, PlaybackErrorKind, PlaybackState,
    PlaybackStatus,
};
use reachable::Reachability;
use recent::Recents;
use session::{Session, SessionStore};
use sync_view::Stage as SyncStage;
use views::{
    browser, cue_bar, cue_editor, deck, error_text, library_sync_view, loop_bar, loop_editor,
    menubar, navbar, origin_prefix_dialog, player_bar, playlist_dialog, progress_view, settings,
    sidebar, smart_editor, sync_view,
};
use waveform::{Waveform, Waveforms};

/// How often the playback position is fetched from the backend while playing.
const POLL_INTERVAL: Duration = Duration::from_millis(250);
/// How long after a library opens to wait with scrolling its track list to the right place.
const REVEAL_DELAY: Duration = Duration::from_millis(150);
/// How often to look for USB drives that were plugged in or removed.
const DRIVE_SCAN_INTERVAL: Duration = Duration::from_secs(3);

fn main() -> iced::Result {
    iced::application(State::new, update, view)
        .title("Open Engine Lib")
        .theme(State::theme)
        .subscription(State::subscription)
        // Closing the window saves the session first, see `Message::CloseRequested`.
        .exit_on_close_request(false)
        // The waveform is drawn as filled shapes, whose edges need smoothing.
        .antialiasing(true)
        .run()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Page {
    #[default]
    Library,
    Settings,
}

/// Dragging the border of a column header.
#[derive(Debug, Clone, Copy)]
struct Resize {
    column: Column,
    /// The column's width when the drag began.
    width: f32,
    /// Where the pointer was at the first move, which the drag is measured from.
    grab: Option<f32>,
}

/// The backend is a separate process that reads libraries and plays audio.
enum BackendState {
    Starting,
    Ready(Box<Backend>),
    Failed(String),
}

enum LibraryState {
    Closed {
        /// Why the last attempt to open a library failed.
        error: Option<String>,
    },
    Opening(PathBuf),
    Open(Browser),
}

struct State {
    page: Page,
    config: ConfigStore,
    /// Last error from saving or resetting the config, shown on the settings page.
    config_error: Option<String>,
    backend: BackendState,
    /// A library to open as soon as the backend is up, and the session to go back to
    /// in it: the command line's library, or the one open last time.
    open_when_ready: Option<(PathBuf, Option<Session>)>,
    /// What to go back to once the library being opened is open.
    restore: Option<Session>,
    session: SessionStore,
    library: LibraryState,
    recents: Recents,
    /// The folders of music that were added to the "Local music" section.
    local_folders: LocalFolders,
    /// The USB drives that are connected, as of the last scan.
    drives: Vec<Drive>,
    /// Whether a scan is still running, so a slow drive can't pile them up.
    scanning_drives: bool,
    /// Cover images fetched for the tracks in view and the one playing.
    covers: Covers,
    /// Whether the locations and origins in view can be reached.
    reachable: Reachability,
    /// Why the recent libraries could not be saved.
    recents_error: Option<String>,
    /// What the backend last said about playback, and when.
    status: PlaybackStatus,
    /// The playhead's position, running on between the reports in `status`.
    clock: Clock,
    /// The waveform and beatgrid of the playing track.
    waveforms: Waveforms,
    /// The cues of the tracks played.
    cues: TrackCues,
    /// A change to cues is being written, so no other can be started: it would be made
    /// from cues that are about to be out of date.
    cues_busy: bool,
    /// The saved loops of the tracks played, and whether a change to them is being written.
    loops: TrackLoops,
    loops_busy: bool,
    /// The list the queue was made from, so its playing row can be marked.
    play_source: Option<Source>,
    /// A failed request to the backend.
    backend_error: Option<String>,
    /// The waveform gain slider being dragged and where it is.
    gain_draft: Option<(WaveformBand, f32)>,
    /// The waveform delay slider being dragged and where it is, in milliseconds.
    delay_draft: Option<i32>,
    /// The window comparing the working copy with the source, while it is open.
    sync: Option<SyncStage>,
    /// The sync window is about replacing the working copy by the source (the reverse).
    sync_reverse: bool,
    /// How far the sync that is writing has got, as the backend last said.
    progress: Option<open_engine_api::v1::OperationProgress>,
    /// The window for syncing the playlists to another library, while it is open.
    library_sync: Option<LibrarySyncStage>,
    /// The window for adding or changing a smart playlist, while it is open.
    smart: Option<smart_editor::Editor>,
    /// The window for adding, renaming or deleting a playlist, while it is open.
    playlist_dialog: Option<playlist_dialog::Dialog>,
    /// The window for changing the folder the saved origins start with, while it is open.
    origin_prefix: Option<origin_prefix_dialog::Dialog>,
    /// The window for editing the cues of the playing track, while it is open.
    cue_editor: Option<cue_editor::Editor>,
    /// The window for editing the saved loops of the playing track, while it is open.
    loop_editor: Option<loop_editor::Editor>,
    /// Where the library is being saved to, and why the last save failed.
    saving: Option<PathBuf>,
    save_error: Option<String>,
    /// Whether the library is being read from the disk again, and how the last time went
    /// (what to say, and whether it failed).
    reloading: bool,
    reload_result: Option<(String, bool)>,
    /// How wide the user made the track list's columns.
    columns: ColumnStore,
    /// The column whose width is being dragged.
    resize: Option<Resize>,
    /// The DJ controller: what its controls do, what is connected, what it said last.
    controller: Controller,
}

#[derive(Debug, Clone)]
pub enum Message {
    Navigate(Page),
    /// What the backend reports of the DJ controller (or why it can't): it listens to the
    /// controller, learns what its controls do and does it. This is only for showing it.
    ControllerState(Result<ControllerState, String>),
    /// The next control pressed on the controller is to be bound to this action.
    ControllerLearn(ControllerAction),
    ControllerLearnCancel,
    ControllerClear(ControllerAction),
    ControllerEnabledToggled(bool),
    ThemeSelected(ThemeMode),
    LanguageSelected(Language),
    ResetOverrides,
    /// Puts an error message on the clipboard.
    CopyText(String),
    WaveformColorsSelected(WaveformColors),
    /// A waveform gain slider is being dragged to a value: not saved until it is let go.
    WaveformGainDragged(WaveformBand, f32),
    WaveformGainReleased,
    ResetWaveformGains,
    /// The waveform delay slider is being dragged to a value (ms): not saved until it is let go.
    WaveformDelayDragged(i32),
    WaveformDelayReleased,
    ResetWaveformDelay,
    /// The border after a column header was pressed.
    ResizeStarted(Column),
    /// The pointer moved, to this x, while a column was being resized.
    ResizeMoved(f32),
    ResizeEnded,

    /// The Sync button: open the window and compare (also "Compare again").
    SyncOpen,
    SyncDiffLoaded(Result<Arc<open_engine_api::v1::GetSyncDiffResponse>, String>),
    /// The tick that allows overwriting changes made outside this app.
    SyncAcknowledged(bool),
    SyncStart,
    /// Which way the sync window works: `true` replaces the working copy by the source.
    SyncDirection(bool),
    /// The working copy was replaced by the source.
    SyncPulled(Result<open_engine_api::v1::PullFromSourceResponse, String>),
    /// The library was read again, because its working copy was replaced or changed.
    LibraryReloaded(Result<Arc<Library>, String>),
    SyncFinished(Result<open_engine_api::v1::SyncToSourceResponse, String>),
    SyncClose,

    /// The button for syncing the playlists to another library.
    LibrarySyncOpen,
    /// The button for adding a library by its folder.
    LibrarySyncAdd,
    /// A folder was chosen to add (`None`: the dialog was closed).
    LibrarySyncAdded(Option<PathBuf>),
    /// The library to sync to was chosen, from the lists or by its folder.
    LibrarySyncTarget(PathBuf),
    LibrarySyncDiffLoaded {
        target: PathBuf,
        options: library_sync_view::Options,
        initial: bool,
        result: Result<Arc<open_engine_api::v1::GetLibrarySyncDiffResponse>, String>,
    },
    /// A tick of what to sync (and so what the comparison is about) was set or cleared.
    LibrarySyncOption(library_sync_view::Setting, bool),
    LibrarySyncStart,
    LibrarySyncFinished(Result<open_engine_api::v1::SyncToLibraryResponse, String>),
    /// Back to choosing the library.
    LibrarySyncBack,
    LibrarySyncClose,

    /// The button for a new smart playlist.
    SmartNew,
    /// The edit button of the smart playlist at this index.
    SmartEdit(usize),
    SmartEditor(smart_editor::Event),
    SmartSave,
    SmartCancel,
    SmartSaved(Result<backend::SmartPlaylistEdited, String>),

    /// The button that saves the location of every track as its origin.
    MigrateOrigins,
    OriginsMigrated(Result<open_engine_api::v1::MigrateOriginsResponse, String>),
    /// The button that opens the window for changing the folder the origins start with.
    OriginPrefixOpen,
    OriginPrefixLoaded(Result<open_engine_api::v1::GetOriginPrefixResponse, String>),
    OriginPrefixDialog(origin_prefix_dialog::Event),
    /// The button that picks the new folder in a file dialog, and what it picked.
    OriginPrefixBrowse,
    OriginPrefixPicked(Option<PathBuf>),
    OriginPrefixApply,
    OriginPrefixApplied(Result<open_engine_api::v1::ReplaceOriginPrefixResponse, String>),
    OriginPrefixCancel,

    /// The button for a new playlist.
    PlaylistNew,
    /// The buttons next to the selected playlist (an index into the library's playlists).
    PlaylistRename(usize),
    PlaylistDelete(usize),
    PlaylistDialog(playlist_dialog::Event),
    PlaylistConfirm,
    PlaylistCancel,
    PlaylistsEdited(Result<backend::PlaylistsEdited, String>),

    /// A File menu entry was picked.
    MenuSelected(MenuItem),
    /// The working copy was replaced by the library on the disk (or it could not be).
    Reloaded(Result<open_engine_api::v1::PullFromSourceResponse, String>),
    SaveAsChosen(Option<PathBuf>),
    LibrarySaved(Result<Arc<Library>, String>),
    ReopenLibraryToggled(bool),
    RestorePositionToggled(bool),
    CloseRequested(iced::window::Id),

    BackendStarted(Result<Box<Backend>, String>),
    RetryBackend,

    ChooseLibrary,
    LibraryChosen(Option<PathBuf>),
    LibraryOpened(Result<Arc<Library>, String>),
    CloseLibrary,
    LibraryClosed(Result<(), String>),
    OpenRecent(PathBuf),
    ScanDrives,
    DrivesFound(Vec<Drive>),
    /// Open the Engine library on a drive (the path is its `Engine Library` folder).
    OpenDrive(PathBuf),
    /// "Add local music folder…": ask for a folder.
    AddLocalFolder,
    /// The folder that was picked, if one was.
    LocalFolderChosen(Option<PathBuf>),
    /// A folder of the "Local music" section was clicked: open it as the library.
    OpenLocalFolder(PathBuf),
    /// The × of a folder of the "Local music" section: take it out of the list.
    RemoveLocalFolder(PathBuf),
    ForgetRecent(PathBuf),
    /// Scroll the track list to a row, once the list is on screen.
    RevealRow(usize),

    ShowCollection,
    ShowPlaylist(usize),
    ToggleFolder(usize),
    SelectTrack(usize),
    PlayTrack(usize),
    /// A star in the track list was clicked: give the track (by id) this many stars.
    SetStars {
        track: u32,
        stars: u8,
    },
    /// The backend answered; `previous` is what to go back to if it could not do it.
    RatingSet {
        track: u32,
        previous: Option<u32>,
        result: Result<(), String>,
    },
    TrackListScrolled(scrollable::Viewport),

    PlayPause,
    Stop,
    Next,
    Previous,
    Tick,
    /// Jump to a point of the track, in seconds (from clicking the waveform overview).
    SeekTo(f32),
    /// A pad of the playing track was clicked: jump to its cue.
    CueJump(usize),
    /// An empty pad of the playing track was clicked: set a cue at the playhead.
    CueAdd(usize),
    /// A pad of the playing track was right-clicked: remove its cue.
    CueRemove(usize),
    CueEditorOpen,
    CueEditor(cue_editor::Event),
    CueEditorSave,
    CueEditorCancel,
    /// A pad of the playing track was clicked: jump to the start of its loop.
    LoopJump(usize),
    /// An empty pad of the playing track was clicked: set a loop at the playhead.
    LoopAdd(usize),
    /// A pad of the playing track was right-clicked: remove its loop.
    LoopRemove(usize),
    LoopEditorOpen,
    LoopEditor(loop_editor::Event),
    LoopEditorSave,
    LoopEditorCancel,
    /// A fetched list of loops: the loops' generation it was asked in, the track id and the loops.
    LoopsLoaded(u32, u32, Result<Vec<Loop>, String>),
    /// The backend answered a change of a track's loops with the loops it has now.
    LoopsEdited {
        generation: u32,
        track: u32,
        result: Result<Vec<Loop>, String>,
    },
    /// A fetched list of cues: the cues' generation it was asked in, the track id and the cues.
    CuesLoaded(u32, u32, Result<Vec<Cue>, String>),
    /// The backend answered a change of a track's cues with the cues it has now.
    CuesEdited {
        generation: u32,
        track: u32,
        result: Result<Vec<Cue>, String>,
    },
    Status(Result<PlaybackStatus, String>),
    /// A fetched cover: the covers' generation it was asked in, its id and its image.
    CoverLoaded(u32, u32, Result<Option<Vec<u8>>, String>),
    /// The sync that is running has got this far, as the backend last said.
    ProgressUpdated(Result<open_engine_api::v1::OperationProgress, String>),
    /// Time to ask the backend how far the sync is.
    ProgressTick,
    /// Which of the paths in view can be reached, in the generation they were asked in.
    PathsChecked(u32, Vec<(String, bool)>),
    /// A fetched waveform: the waveforms' generation it was asked in, the track id and the waveform.
    WaveformLoaded(u32, u32, Result<Option<Arc<Waveform>>, String>),
}

impl State {
    fn new() -> (Self, Task<Message>) {
        let config = ConfigStore::load(Locations::discover());
        let session = SessionStore::discover();

        // A library on the command line is what was asked for. Otherwise go back to the
        // last one, if the settings say so.
        let settings = config.effective();
        let open_when_ready = match std::env::args_os().nth(1) {
            Some(path) => Some((PathBuf::from(path), None)),
            None if settings.reopen_library => session.session().library.clone().map(|path| {
                let restore = settings.restore_position.then(|| session.session().clone());
                (path, restore)
            }),
            None => None,
        };

        let state = Self {
            page: Page::default(),
            config,
            config_error: None,
            backend: BackendState::Starting,
            open_when_ready,
            restore: None,
            session,
            library: LibraryState::Closed { error: None },
            recents: Recents::discover(),
            local_folders: LocalFolders::discover(),
            drives: Vec::new(),
            scanning_drives: true,
            covers: Covers::default(),
            reachable: Reachability::default(),
            recents_error: None,
            status: PlaybackStatus::default(),
            clock: Clock::new(Instant::now()),
            waveforms: Waveforms::default(),
            cues: TrackCues::default(),
            cues_busy: false,
            loops: TrackLoops::default(),
            loops_busy: false,
            play_source: None,
            backend_error: None,
            gain_draft: None,
            delay_draft: None,
            sync: None,
            sync_reverse: false,
            progress: None,
            library_sync: None,
            smart: None,
            playlist_dialog: None,
            origin_prefix: None,
            cue_editor: None,
            loop_editor: None,
            saving: None,
            save_error: None,
            reloading: false,
            reload_result: None,
            columns: ColumnStore::discover(),
            resize: None,
            controller: Controller::default(),
        };
        (state, Task::batch([start_backend(), scan_drives()]))
    }

    fn theme(&self) -> Theme {
        match self.config.effective().theme {
            ThemeMode::Light => Theme::Light,
            ThemeMode::Dark => Theme::Dark,
        }
    }

    fn subscription(&self) -> Subscription<Message> {
        // A paused song is also watched when a DJ controller is on: it can resume the song
        // (or move within it) without the UI having asked for it.
        let watched = match self.status.state() {
            PlaybackState::Playing => true,
            PlaybackState::Paused => self.config.effective().controller_enabled,
            _ => false,
        };
        let poll = if watched {
            iced::time::every(POLL_INTERVAL).map(|_| Message::Tick)
        } else {
            Subscription::none()
        };
        // While a column is being resized, follow the pointer wherever it goes (it leaves
        // the thin handle at once) and notice when the button is let go.
        let resizing = if self.resize.is_some() {
            iced::event::listen_with(|event, _status, _window| match event {
                iced::Event::Mouse(iced::mouse::Event::CursorMoved { position }) => {
                    Some(Message::ResizeMoved(position.x))
                }
                iced::Event::Mouse(iced::mouse::Event::ButtonReleased(
                    iced::mouse::Button::Left,
                )) => Some(Message::ResizeEnded),
                _ => None,
            })
        } else {
            Subscription::none()
        };
        // While a sync is writing or a library is opening, ask how far it is.
        let progress = if self.reporting_progress() {
            iced::time::every(progress_view::POLL_INTERVAL).map(|_| Message::ProgressTick)
        } else {
            Subscription::none()
        };
        Subscription::batch([
            poll,
            resizing,
            progress,
            iced::time::every(DRIVE_SCAN_INTERVAL).map(|_| Message::ScanDrives),
            iced::window::close_requests().map(Message::CloseRequested),
        ])
    }

    fn backend(&self) -> Option<Backend> {
        match &self.backend {
            BackendState::Ready(backend) => Some(Backend::clone(backend)),
            _ => None,
        }
    }

    /// Takes the playlists as they are after one was added, changed or deleted. The
    /// list the queue was made from stays marked, unless that playlist is gone.
    fn playlists_changed(
        &mut self,
        playlists: Vec<model::Playlist>,
        change: Change,
        show: Option<usize>,
    ) {
        let LibraryState::Open(browser) = &mut self.library else {
            return;
        };
        browser.replace_playlists(playlists, &change, show);
        if let Some(Source::Playlist(index)) = self.play_source {
            self.play_source = change.map(index).map(Source::Playlist);
        }
    }

    /// Fetches the open library again, after the backend read its working copy again.
    fn reload_library(&mut self) -> Task<Message> {
        let (Some(backend), LibraryState::Open(browser)) = (self.backend(), &self.library) else {
            return Task::none();
        };
        let library = &browser.library;
        Task::perform(
            backend::reload_library(
                backend,
                library.path.clone(),
                library.cache_path.clone(),
                library.tracks.len() as u32,
            ),
            Message::LibraryReloaded,
        )
    }

    /// Whether the library is being saved or read again, so that nothing may replace or
    /// change it meanwhile.
    fn locked(&self) -> bool {
        self.saving.is_some() || self.reloading
    }

    /// Reads the open library from the disk again, replacing the working copy. The edits
    /// that were not synced are kept aside by the backend. A folder of music is scanned
    /// again.
    fn reload_from_disk(&mut self) -> Task<Message> {
        let LibraryState::Open(browser) = &self.library else {
            return Task::none();
        };
        if self.locked() || self.window_open() || self.syncing() {
            return Task::none();
        }
        if !browser.library.is_engine() {
            let path = PathBuf::from(&browser.library.path);
            // What was open stays open, but nothing is started.
            let session = self.snapshot().map(|session| Session {
                loaded: false,
                ..session
            });
            return self.open_as(path, model::Kind::Folder, session);
        }
        let Some(backend) = self.backend() else {
            return Task::none();
        };
        self.reloading = true;
        self.reload_result = None;
        Task::perform(backend::pull_from_source(backend), Message::Reloaded)
    }

    /// Whether a sync is writing, so that how far it is gets asked.
    fn syncing(&self) -> bool {
        matches!(self.sync, Some(SyncStage::Syncing { .. }))
            || matches!(self.library_sync, Some(LibrarySyncStage::Syncing { .. }))
    }

    /// Whether the backend is doing something it reports progress on: a sync writing, or a
    /// library being opened.
    fn reporting_progress(&self) -> bool {
        self.syncing() || matches!(self.library, LibraryState::Opening(_))
    }

    /// Whether one of the windows that lie over the app is open. Only one is at a time.
    fn window_open(&self) -> bool {
        self.sync.is_some()
            || self.library_sync.is_some()
            || self.smart.is_some()
            || self.playlist_dialog.is_some()
            || self.origin_prefix.is_some()
            || self.cue_editor.is_some()
            || self.loop_editor.is_some()
    }

    /// Starts comparing the playlists of the open library with those of the library at
    /// `target`, for the window that syncs them.
    fn compare_libraries(
        &mut self,
        target: PathBuf,
        options: library_sync_view::Options,
        initial: bool,
    ) -> Task<Message> {
        let Some(backend) = self.backend() else {
            return Task::none();
        };
        if !matches!(
            self.library_sync,
            Some(
                LibrarySyncStage::Pick
                    | LibrarySyncStage::Ready { .. }
                    | LibrarySyncStage::Loading { .. }
            )
        ) {
            return Task::none();
        }
        self.library_sync = Some(LibrarySyncStage::Loading {
            target: target.clone(),
            options,
        });
        let answered = target.clone();
        Task::perform(
            backend::library_sync_diff(backend, target, options),
            move |result| Message::LibrarySyncDiffLoaded {
                target: answered.clone(),
                options,
                initial,
                result,
            },
        )
    }

    /// Sends a playback request to the backend; its answer arrives as `Message::Status`.
    fn control(&self, control: Control) -> Task<Message> {
        match self.backend() {
            Some(backend) => Task::perform(backend::control(backend, control), Message::Status),
            None => Task::none(),
        }
    }

    /// Opens the library at `path`; `restore` says what to go back to in it, if anything.
    /// (A session that was in a folder of music goes back to that folder.)
    fn open(&mut self, path: PathBuf, restore: Option<Session>) -> Task<Message> {
        let kind = match &restore {
            Some(session) if session.folder => model::Kind::Folder,
            _ => model::Kind::Engine,
        };
        self.open_as(path, kind, restore)
    }

    /// Opens the folder of music at `path` as the library.
    fn open_folder(&mut self, path: PathBuf) -> Task<Message> {
        self.open_as(path, model::Kind::Folder, None)
    }

    fn open_as(
        &mut self,
        path: PathBuf,
        kind: model::Kind,
        restore: Option<Session>,
    ) -> Task<Message> {
        let Some(backend) = self.backend() else {
            // Still starting: remember it for when the backend is up.
            self.open_when_ready = Some((path, restore));
            return Task::none();
        };
        self.restore = restore;
        self.reset_playback();
        self.covers.clear();
        self.reachable.clear();
        self.waveforms.clear();
        self.cues.clear();
        self.loops.clear();
        self.sync = None;
        self.reload_result = None;
        self.progress = None;
        self.library = LibraryState::Opening(path.clone());
        match kind {
            model::Kind::Engine => {
                Task::perform(backend::open_library(backend, path), Message::LibraryOpened)
            }
            model::Kind::Folder => Task::perform(
                backend::open_local_folder(backend, path),
                Message::LibraryOpened,
            ),
        }
    }

    /// Whether the open library is a folder of music, which has nothing of an Engine library
    /// to edit, show or sync.
    fn is_folder(&self) -> bool {
        matches!(&self.library, LibraryState::Open(browser) if !browser.library.is_engine())
    }

    fn reset_playback(&mut self) {
        self.status = PlaybackStatus::default();
        self.play_source = None;
    }

    /// Starts the shown list at `row`.
    fn play_row(&mut self, row: usize) -> Task<Message> {
        self.play_row_at(row, false, 0)
    }

    /// Loads the shown list with `row` playing, or paused if `paused`, from `at_ms`.
    fn play_row_at(&mut self, row: usize, paused: bool, at_ms: u64) -> Task<Message> {
        let LibraryState::Open(browser) = &self.library else {
            return Task::none();
        };
        let tracks = browser.tracks();
        if row >= tracks.len() {
            return Task::none();
        }
        let track_ids = tracks
            .iter()
            .map(|&index| browser.library.tracks[index].id)
            .collect();
        self.play_source = Some(browser.source);
        self.control(Control::Play {
            track_ids,
            position: row as u32,
            start_paused: paused,
            start_position_ms: at_ms,
        })
    }

    /// Goes back to what `session` describes in the library that was just opened: the
    /// playlist, the song (selected and in view) and, if it was loaded, the song
    /// itself, paused where it was left.
    fn restore_session(&mut self, session: Session) -> Task<Message> {
        let LibraryState::Open(browser) = &mut self.library else {
            return Task::none();
        };
        let library = Arc::clone(&browser.library);

        // A playlist that is gone just means the list of all tracks.
        let source = match library.find_playlist(&session.playlist) {
            Some(index) => {
                browser.expand(&library.playlist_chain(index));
                Source::Playlist(index)
            }
            None => Source::Collection,
        };
        browser.show(source);

        let row = session.track.as_deref().and_then(|path| {
            browser
                .tracks()
                .iter()
                .position(|&track| library.tracks[track].path == path)
        });
        let Some(row) = row else {
            return Task::none();
        };
        browser.selected = Some(row);

        let play = if session.loaded {
            self.play_row_at(row, true, session.position_ms)
        } else {
            Task::none()
        };
        // The list is not on screen yet, and a scroll needs it to be.
        // (The timer is made inside the future: update does not run in the runtime.)
        let reveal = Task::perform(
            async { tokio::time::sleep(REVEAL_DELAY).await },
            move |()| Message::RevealRow(row),
        );
        Task::batch([play, reveal])
    }

    /// What to remember of the current state: `None` while no library is open, when
    /// there is nothing to remember (and what was remembered must be left alone).
    fn snapshot(&self) -> Option<Session> {
        let LibraryState::Open(browser) = &self.library else {
            return None;
        };
        let library = &browser.library;
        let playlist_of = |source: Source| match source {
            Source::Collection => Vec::new(),
            Source::Playlist(index) => library.playlist_path(index),
        };

        // A song in the player wins over the one that is only selected, and brings its
        // own playlist: the one it is playing from.
        let playing = if self.is_stopped() {
            None
        } else {
            self.status.track_id.and_then(|id| library.track_by_id(id))
        };
        let (playlist, track, loaded, position_ms) = match playing {
            Some(track) => (
                playlist_of(self.play_source.unwrap_or(browser.source)),
                Some(track.path.clone()),
                true,
                self.status.position_ms,
            ),
            None => (
                playlist_of(browser.source),
                browser
                    .selected
                    .and_then(|row| browser.tracks().get(row))
                    .map(|&track| library.tracks[track].path.clone()),
                false,
                0,
            ),
        };

        Some(Session {
            library: Some(PathBuf::from(&library.path)),
            folder: !library.is_engine(),
            playlist,
            track,
            loaded,
            position_ms,
        })
    }

    /// Writes the session down if it changed. A playing song's position is only
    /// written every few seconds, unless `force`d (when the app closes).
    fn save_session(&mut self, force: bool) {
        if let Some(session) = self.snapshot() {
            self.save(&session, force);
        }
    }

    fn save(&mut self, session: &Session, force: bool) {
        // Not being able to remember is not worth bothering the user about.
        if let Err(error) = self.session.save(session, force) {
            eprintln!("could not save the session: {error}");
        }
    }

    /// The row of the shown list that is playing, if the shown list is the playing one.
    fn playing_row(&self, browser: &Browser) -> Option<usize> {
        if self.is_stopped() || self.play_source != Some(browser.source) {
            return None;
        }
        self.status.queue_position.map(|position| position as usize)
    }

    /// Asks the backend for the covers of the rows in view and of the playing track
    /// that have not been asked for yet.
    fn fetch_covers(&mut self) -> Task<Message> {
        let (Some(backend), LibraryState::Open(browser)) = (self.backend(), &self.library) else {
            return Task::none();
        };
        let playing = self
            .status
            .track_id
            .and_then(|id| browser.library.track_by_id(id))
            .and_then(|track| track.album_art);
        let wanted = browser.visible_covers().chain(playing);

        let generation = self.covers.generation();
        Task::batch(self.covers.claim_unrequested(wanted).into_iter().map(|id| {
            Task::perform(backend::album_art(backend.clone(), id), move |result| {
                Message::CoverLoaded(generation, id, result)
            })
        }))
    }

    /// Checks the locations and origins of the rows in view that have not been checked.
    fn check_paths(&mut self) -> Task<Message> {
        let LibraryState::Open(browser) = &self.library else {
            return Task::none();
        };
        let paths = self.reachable.claim_unrequested(browser.visible_paths());
        if paths.is_empty() {
            return Task::none();
        }
        let generation = self.reachable.generation();
        Task::perform(reachable::check(paths), move |answers| {
            Message::PathsChecked(generation, answers)
        })
    }

    /// Asks the backend for the waveform of the track in the player, if it has not been
    /// asked for yet.
    fn fetch_waveform(&mut self) -> Task<Message> {
        let (Some(backend), false, Some(id)) =
            (self.backend(), self.is_stopped(), self.status.track_id)
        else {
            return Task::none();
        };
        if !self.waveforms.claim(id) {
            return Task::none();
        }
        let generation = self.waveforms.generation();
        // Where the bands meet and how much each is amplified are settings.
        let config = self.config.effective();
        let settings = WaveformSettings {
            low_mid_hz: config.waveform_low_mid_hz,
            mid_high_hz: config.waveform_mid_high_hz,
            gains: WaveformBand::ALL.map(|band| config.waveform_gain(band)),
        };
        Task::perform(backend::waveform(backend, id, settings), move |result| {
            Message::WaveformLoaded(generation, id, result)
        })
    }

    /// The id of the track in the player.
    fn playing_id(&self) -> Option<u32> {
        if self.is_stopped() {
            return None;
        }
        self.status.track_id
    }

    /// Where the playhead of the waveform is, in seconds into the playing track: what is
    /// heard now (the waveform trails the player by the set delay, to line up with it).
    fn playhead(&self) -> f64 {
        let duration = self.playing_duration().unwrap_or(f64::INFINITY);
        let delay = f64::from(self.config.effective().waveform_delay_ms) / 1000.0;
        self.clock.delayed(delay).position(Instant::now(), duration)
    }

    /// How long the playing track is, in seconds, once its waveform is known.
    fn playing_duration(&self) -> Option<f64> {
        match self.playing_id().map(|id| self.waveforms.display(id)) {
            Some(waveform::Display::Ready(waveform)) => Some(waveform.duration),
            _ => None,
        }
    }

    /// How long a loop set from a pad of the playing track is: four beats at the tempo of
    /// its beatgrid, else of the library.
    fn new_loop_length(&self) -> f64 {
        let id = self.playing_id();
        let grid = match id.map(|id| self.waveforms.display(id)) {
            Some(waveform::Display::Ready(waveform)) => waveform.grid.as_ref().map(|g| g.bpm()),
            _ => None,
        };
        let library = || match (&self.library, id) {
            (LibraryState::Open(browser), Some(id)) => {
                browser.library.track_by_id(id).and_then(|track| track.bpm)
            }
            _ => None,
        };
        loops::default_length(grid.or_else(library))
    }

    /// Asks the backend for the saved loops of the track in the player, if it has not been
    /// asked for yet.
    fn fetch_loops(&mut self) -> Task<Message> {
        let (Some(backend), Some(id)) = (self.backend(), self.playing_id()) else {
            return Task::none();
        };
        if !matches!(&self.library, LibraryState::Open(browser) if browser.library.is_engine())
            || !self.loops.claim(id)
        {
            return Task::none();
        }
        let generation = self.loops.generation();
        Task::perform(backend::loops(backend, id), move |result| {
            Message::LoopsLoaded(generation, id, result)
        })
    }

    /// Asks the backend to make `loops` the saved loops of `track`.
    fn save_loops(&mut self, track: u32, loops: Vec<Loop>) -> Task<Message> {
        let Some(backend) = self.backend() else {
            return Task::none();
        };
        self.loops_busy = true;
        let generation = self.loops.generation();
        Task::perform(backend::set_loops(backend, track, loops), move |result| {
            Message::LoopsEdited {
                generation,
                track,
                result,
            }
        })
    }

    /// Asks the backend for the cues of the track in the player, if it has not been asked
    /// for yet.
    fn fetch_cues(&mut self) -> Task<Message> {
        let (Some(backend), Some(id)) = (self.backend(), self.playing_id()) else {
            return Task::none();
        };
        if !matches!(&self.library, LibraryState::Open(browser) if browser.library.is_engine())
            || !self.cues.claim(id)
        {
            return Task::none();
        }
        let generation = self.cues.generation();
        Task::perform(backend::cues(backend, id), move |result| {
            Message::CuesLoaded(generation, id, result)
        })
    }

    /// Asks the backend to make `cues` the cues of `track`.
    fn save_cues(&mut self, track: u32, cues: Vec<Cue>) -> Task<Message> {
        let Some(backend) = self.backend() else {
            return Task::none();
        };
        self.cues_busy = true;
        let generation = self.cues.generation();
        Task::perform(backend::set_cues(backend, track, cues), move |result| {
            Message::CuesEdited {
                generation,
                track,
                result,
            }
        })
    }

    /// Remembers what was open, then quits.
    fn exit(&mut self) -> Task<Message> {
        self.save_session(true);
        iced::exit()
    }

    fn is_stopped(&self) -> bool {
        !matches!(
            self.status.state(),
            PlaybackState::Playing | PlaybackState::Paused
        )
    }
}

/// Looks for USB drives off the UI thread, since reading a drive can take a while.
fn scan_drives() -> Task<Message> {
    Task::perform(
        async {
            tokio::task::spawn_blocking(drives::detect)
                .await
                .unwrap_or_default()
        },
        Message::DrivesFound,
    )
}

fn start_backend() -> Task<Message> {
    Task::perform(
        async { backend::start().await.map(Box::new) },
        Message::BackendStarted,
    )
}

/// Handles a message, then fetches whatever covers the result has put on screen.
fn update(state: &mut State, message: Message) -> Task<Message> {
    let task = handle(state, message);
    state.save_session(false);
    Task::batch([
        task,
        state.fetch_covers(),
        state.fetch_waveform(),
        state.fetch_cues(),
        state.fetch_loops(),
        state.check_paths(),
    ])
}

fn handle(state: &mut State, message: Message) -> Task<Message> {
    // A folder of music has nothing to sync, no playlists or ratings to change and no
    // origins to save (the buttons for these are not shown either).
    if state.is_folder()
        && matches!(
            message,
            Message::SyncOpen
                | Message::LibrarySyncOpen
                | Message::PlaylistNew
                | Message::PlaylistRename(_)
                | Message::PlaylistDelete(_)
                | Message::SmartNew
                | Message::SmartEdit(_)
                | Message::MigrateOrigins
                | Message::OriginPrefixOpen
                | Message::SetStars { .. }
                | Message::MenuSelected(MenuItem::SaveAs)
        )
    {
        return Task::none();
    }
    match message {
        Message::CopyText(text) => iced::clipboard::write(text),
        Message::BackendStarted(..)
        | Message::CloseRequested(..)
        | Message::LanguageSelected(..)
        | Message::Navigate(..)
        | Message::ReopenLibraryToggled(..)
        | Message::ResetOverrides
        | Message::ResetWaveformDelay
        | Message::ResetWaveformGains
        | Message::ResizeEnded
        | Message::ResizeMoved(..)
        | Message::ResizeStarted(..)
        | Message::RestorePositionToggled(..)
        | Message::RetryBackend
        | Message::ThemeSelected(..)
        | Message::WaveformColorsSelected(..)
        | Message::WaveformDelayDragged(..)
        | Message::WaveformDelayReleased
        | Message::WaveformGainDragged(..)
        | Message::WaveformGainReleased => handlers::settings::handle(state, message),
        Message::LibraryReloaded(..)
        | Message::SyncAcknowledged(..)
        | Message::SyncClose
        | Message::SyncDiffLoaded(..)
        | Message::SyncDirection(..)
        | Message::SyncFinished(..)
        | Message::SyncOpen
        | Message::SyncPulled(..)
        | Message::SyncStart => handlers::sync::handle(state, message),
        Message::ControllerClear(..)
        | Message::ControllerEnabledToggled(..)
        | Message::ControllerLearn(..)
        | Message::ControllerLearnCancel
        | Message::ControllerState(..) => handlers::controller::handle(state, message),
        Message::OriginPrefixApplied(..)
        | Message::OriginPrefixApply
        | Message::OriginPrefixBrowse
        | Message::OriginPrefixCancel
        | Message::OriginPrefixDialog(..)
        | Message::OriginPrefixLoaded(..)
        | Message::OriginPrefixOpen
        | Message::OriginPrefixPicked(..) => handlers::origin_prefix::handle(state, message),
        Message::MigrateOrigins
        | Message::OriginsMigrated(..)
        | Message::PlaylistCancel
        | Message::PlaylistConfirm
        | Message::PlaylistDelete(..)
        | Message::PlaylistDialog(..)
        | Message::PlaylistNew
        | Message::PlaylistRename(..)
        | Message::PlaylistsEdited(..)
        | Message::SmartCancel
        | Message::SmartEdit(..)
        | Message::SmartEditor(..)
        | Message::SmartNew
        | Message::SmartSave
        | Message::SmartSaved(..) => handlers::playlists::handle(state, message),
        Message::LibrarySyncAdd
        | Message::LibrarySyncAdded(..)
        | Message::LibrarySyncBack
        | Message::LibrarySyncClose
        | Message::LibrarySyncDiffLoaded { .. }
        | Message::LibrarySyncFinished(..)
        | Message::LibrarySyncOpen
        | Message::LibrarySyncOption(..)
        | Message::LibrarySyncStart
        | Message::LibrarySyncTarget(..) => handlers::library_sync::handle(state, message),
        Message::AddLocalFolder
        | Message::ChooseLibrary
        | Message::CloseLibrary
        | Message::DrivesFound(..)
        | Message::ForgetRecent(..)
        | Message::LibraryChosen(..)
        | Message::LibraryClosed(..)
        | Message::LibraryOpened(..)
        | Message::LibrarySaved(..)
        | Message::LocalFolderChosen(..)
        | Message::MenuSelected(..)
        | Message::OpenDrive(..)
        | Message::OpenLocalFolder(..)
        | Message::OpenRecent(..)
        | Message::Reloaded(..)
        | Message::RemoveLocalFolder(..)
        | Message::SaveAsChosen(..)
        | Message::ScanDrives => handlers::library::handle(state, message),
        Message::CoverLoaded(..)
        | Message::Next
        | Message::PathsChecked(..)
        | Message::PlayPause
        | Message::PlayTrack(..)
        | Message::Previous
        | Message::ProgressTick
        | Message::ProgressUpdated(..)
        | Message::RatingSet { .. }
        | Message::RevealRow(..)
        | Message::SeekTo(..)
        | Message::SelectTrack(..)
        | Message::SetStars { .. }
        | Message::ShowCollection
        | Message::ShowPlaylist(..)
        | Message::Status(..)
        | Message::Stop
        | Message::Tick
        | Message::ToggleFolder(..)
        | Message::TrackListScrolled(..)
        | Message::WaveformLoaded(..) => handlers::playback::handle(state, message),
        Message::CueAdd(..)
        | Message::CueEditor(..)
        | Message::CueEditorCancel
        | Message::CueEditorOpen
        | Message::CueEditorSave
        | Message::CueJump(..)
        | Message::CueRemove(..)
        | Message::CuesEdited { .. }
        | Message::CuesLoaded(..) => handlers::cues::handle(state, message),
        Message::LoopAdd(..)
        | Message::LoopEditor(..)
        | Message::LoopEditorCancel
        | Message::LoopEditorOpen
        | Message::LoopEditorSave
        | Message::LoopJump(..)
        | Message::LoopRemove(..)
        | Message::LoopsEdited { .. }
        | Message::LoopsLoaded(..) => handlers::loops::handle(state, message),
    }
}

fn view(state: &State) -> Element<'_, Message> {
    let language = state.config.effective().language;
    let page = match state.page {
        Page::Library => library_page(state, language),
        Page::Settings => settings::view(
            &state.config,
            state.config_error.as_deref(),
            state.gain_draft,
            state.delay_draft,
            &state.controller,
        ),
    };

    let window = column![
        menubar::view(
            language,
            matches!(state.library, LibraryState::Open(_)),
            menubar::Status {
                saving: state.saving.as_deref(),
                error: state.save_error.as_deref(),
                reloading: state.reloading,
                reload: state
                    .reload_result
                    .as_ref()
                    .map(|(message, failed)| (message.as_str(), *failed)),
            },
        ),
        navbar::view(state.page, language),
        container(page).padding(20).width(Fill).height(Fill)
    ]
    .width(Fill)
    .height(Fill)
    .into();
    // The sync window lies over the whole app, which is dimmed and can't be used meanwhile.
    // So do the windows for playlists (only one is open at a time).
    if let Some(stage) = &state.sync {
        return iced::widget::stack![
            window,
            sync_view::modal(stage, state.progress.as_ref(), state.sync_reverse, language)
        ]
        .into();
    }
    if let Some(stage) = &state.library_sync {
        let current = match &state.library {
            LibraryState::Open(browser) => browser.library.path.as_str(),
            _ => "",
        };
        let sources = library_sync_view::Sources {
            recents: &state.recents,
            drives: &state.drives,
            current,
        };
        return iced::widget::stack![
            window,
            library_sync_view::modal(stage, sources, state.progress.as_ref(), language)
        ]
        .into();
    }
    if let Some(editor) = &state.smart {
        return iced::widget::stack![window, smart_editor::modal(editor, language)].into();
    }
    if let Some(editor) = &state.cue_editor {
        return iced::widget::stack![window, cue_editor::modal(editor, language)].into();
    }
    if let Some(editor) = &state.loop_editor {
        return iced::widget::stack![window, loop_editor::modal(editor, language)].into();
    }
    if let Some(dialog) = &state.origin_prefix {
        return iced::widget::stack![window, origin_prefix_dialog::modal(dialog, language)].into();
    }
    match (&state.playlist_dialog, &state.library) {
        (Some(dialog), LibraryState::Open(browser)) => iced::widget::stack![
            window,
            playlist_dialog::modal(dialog, &browser.library, language)
        ]
        .into(),
        _ => window,
    }
}

fn library_page(state: &State, language: Language) -> Element<'_, Message> {
    let strings = language.strings();

    // Until the backend is up there is nothing to open, so no sidebar either.
    match &state.backend {
        BackendState::Starting => {
            return centered(vec![text(&strings.backend_starting).into()]);
        }
        BackendState::Failed(error) => {
            return centered(vec![
                error_text::view(fill(&strings.backend_failed, &[("error", error)]), language),
                button(text(&strings.retry))
                    .on_press(Message::RetryBackend)
                    .into(),
            ]);
        }
        BackendState::Ready(_) => {}
    }

    let (browser, content) = match &state.library {
        LibraryState::Opening(path) => (
            None,
            progress_view::opening(path, state.progress.as_ref(), language),
        ),
        LibraryState::Closed { error } => {
            let mut lines: Vec<Element<'_, Message>> = vec![
                text(&strings.library_closed_title).size(30).into(),
                text(&strings.library_closed_hint).into(),
                button(text(&strings.open_library))
                    .on_press(Message::ChooseLibrary)
                    .into(),
            ];
            if let Some(error) = error {
                lines.push(error_text::view(
                    fill(&strings.open_failed, &[("error", error)]),
                    language,
                ));
            }
            if let Some(error) = &state.backend_error {
                lines.push(backend_error(language, error));
            }
            (None, centered(lines))
        }
        LibraryState::Open(browser) => (Some(browser), open_library(state, browser, language)),
    };

    row![
        sidebar::view(
            browser,
            sidebar::Sources {
                recents: &state.recents,
                recents_error: state.recents_error.as_deref(),
                drives: &state.drives,
                local: &state.local_folders,
            },
            browser.map(|browser| browser.library.path.as_str()),
            !matches!(state.library, LibraryState::Opening(_)) && !state.locked(),
            !state.locked(),
            language,
        ),
        content
    ]
    .spacing(10)
    .height(Fill)
    .into()
}

/// The open library: its summary, the shown list, the player and any errors.
fn open_library<'a>(
    state: &'a State,
    browser: &'a Browser,
    language: Language,
) -> Element<'a, Message> {
    let strings = language.strings();
    let library = &browser.library;

    let mut summary = if library.is_engine() {
        column![
            text(fill(
                &strings.library_summary,
                &[
                    ("path", &library.path),
                    ("tracks", &library.tracks.len()),
                    ("playlists", &library.playlists.len()),
                ],
            )),
            text(fill(
                &strings.library_cache,
                &[("path", &library.cache_path)],
            ))
            .size(14)
            .style(text::secondary),
        ]
    } else {
        // A folder of music: its folders are the playlists, and there is no cache.
        column![
            text(fill(
                &strings.local_summary,
                &[
                    ("path", &library.path),
                    ("tracks", &library.tracks.len()),
                    ("folders", &library.playlists.len()),
                ],
            )),
            text(&strings.local_hint).size(14).style(text::secondary),
        ]
    }
    .spacing(2)
    .width(Fill);
    if let Some(notice) = browser.origin_notice() {
        summary = summary.push(text(notice).size(14).style(text::success));
    }

    let mut header = row![summary].spacing(10).align_y(Center);
    if library.is_engine() {
        // Saves where each track's file is as its origin (only the origin database changes).
        header = header.push(
            tooltip(
                button(text(&strings.origin_migrate)).on_press(Message::MigrateOrigins),
                text(&strings.origin_migrate_tip).size(13),
                tooltip::Position::Bottom,
            )
            .style(container::rounded_box),
        );
        // Changes the folder the saved origins start with (the music moved).
        header = header.push(
            tooltip(
                button(text(&strings.origin_prefix_button))
                    .on_press_maybe((!state.locked()).then_some(Message::OriginPrefixOpen)),
                text(&strings.origin_prefix_tip).size(13),
                tooltip::Position::Bottom,
            )
            .style(container::rounded_box),
        );
    }
    // Not while the library is being saved: it is in the middle of moving.
    header = header.push(
        button(text(if library.is_engine() {
            &strings.close_library
        } else {
            &strings.close_folder
        }))
        .on_press_maybe((!state.locked()).then_some(Message::CloseLibrary)),
    );

    let now = (!state.is_stopped())
        .then_some(state.status.track_id)
        .flatten()
        .and_then(|id| library.track_by_id(id))
        .map(|track| player_bar::NowPlaying {
            track,
            position: state.status.position_ms as f32 / 1000.0,
            paused: state.status.state() == PlaybackState::Paused,
        });
    // The waveform of the track in the player, with the playhead where the player is.
    let colors = state.config.effective().waveform_colors;
    let known_cues = now.as_ref().and_then(|now| state.cues.get(now.track.id));
    let cues_known = known_cues.is_some();
    let track_cues = known_cues.unwrap_or_default();
    let known_loops = now.as_ref().and_then(|now| state.loops.get(now.track.id));
    let loops_known = known_loops.is_some();
    let track_loops = known_loops.unwrap_or_default();
    let deck = now.as_ref().map(|now| {
        deck::view(
            state.waveforms.display(now.track.id),
            deck::Marks {
                cues: track_cues,
                loops: track_loops,
            },
            // Trailing the player by the set delay, to line up with what is heard.
            state
                .clock
                .delayed(f64::from(state.config.effective().waveform_delay_ms) / 1000.0),
            colors,
            language,
        )
    });
    let bar = player_bar::view(
        language,
        player_bar::Bar {
            cover: now
                .as_ref()
                .and_then(|now| now.track.album_art)
                .and_then(|id| state.covers.get(id)),
            now,
            can_start: !browser.tracks().is_empty(),
        },
    );

    let mut page = column![
        header,
        browser::tracks(
            browser,
            state.playing_row(browser),
            &state.covers,
            &state.reachable,
            &state.columns.widths,
            language,
        ),
    ]
    .spacing(10)
    .width(Fill)
    .height(Fill);
    if let Some(deck) = deck {
        page = page.push(deck);
        // (Once the backend has said what the track's cues are.)
        if cues_known {
            page = page.push(cue_bar::view(track_cues, state.cues_busy, language));
        }
        if loops_known {
            page = page.push(loop_bar::view(track_loops, state.loops_busy, language));
        }
    }
    page = page.push(bar);

    if let Some(error) = &state.status.error {
        page = page.push(error_text::view(
            playback_error(language, error, library),
            language,
        ));
    }
    if let Some(error) = &state.backend_error {
        page = page.push(backend_error(language, error));
    }
    page.into()
}

/// Messages shown in the middle of the page while there is no library to show.
fn centered<'a>(lines: Vec<Element<'a, Message>>) -> Element<'a, Message> {
    container(column(lines).spacing(12).align_x(Center))
        .center(Fill)
        .into()
}

fn backend_error<'a>(language: Language, error: &str) -> Element<'a, Message> {
    error_text::view(
        fill(&language.strings().backend_error, &[("error", &error)]),
        language,
    )
}

fn playback_error(language: Language, error: &PlaybackError, library: &Library) -> String {
    let strings = language.strings();
    let title = library
        .track_by_id(error.track_id)
        .map(|track| track.title.as_str())
        .unwrap_or_default();
    match error.kind() {
        PlaybackErrorKind::NotFound => fill(
            &strings.file_missing,
            &[("title", &title), ("path", &error.detail)],
        ),
        PlaybackErrorKind::Streaming => fill(&strings.streaming_not_playable, &[("title", &title)]),
        _ => fill(
            &strings.play_failed,
            &[("title", &title), ("error", &error.detail)],
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use model::{Playlist, Track};

    fn track(id: u32) -> Track {
        Track {
            id,
            title: format!("Track {id}"),
            artist: String::new(),
            album: String::new(),
            genre: String::new(),
            bpm: None,
            length: None,
            streaming: false,
            album_art: None,
            path: format!("../Music/{}.flac", ["a", "b", "c"][id as usize]),
            location: String::new(),
            rating: None,
            lighting: model::Lighting::None,
            lighting_projects: Vec::new(),
            has_stems: false,
            cue_count: 0,
            loop_count: 0,
            origin: None,
        }
    }

    fn playlist(title: &str, depth: usize, has_children: bool, tracks: &[usize]) -> Playlist {
        Playlist {
            title: title.into(),
            depth,
            has_children,
            is_smart: false,
            rules: None,
            tracks: tracks.to_vec(),
        }
    }

    /// A library at `path`: tracks a, b, c; playlists House/Deep (b, c) and Pop (a).
    fn library_at(path: &str) -> Library {
        Library::new(
            path.into(),
            "/cache".into(),
            vec![track(0), track(1), track(2)],
            vec![
                playlist("House", 0, true, &[]),
                playlist("Deep", 1, false, &[1, 2]),
                playlist("Pop", 0, false, &[0]),
            ],
        )
    }

    /// An app with a library open (see `library_at`).
    /// A folder of its own for the files of the `State` of the running test (the tests run
    /// side by side on threads of their own, and what one saves must not show up in
    /// another).
    fn temp_dir() -> PathBuf {
        static COUNT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        thread_local! {
            static DIR: PathBuf = std::env::temp_dir().join(format!(
                "open-engine-lib-main-test-{}-{}",
                std::process::id(),
                COUNT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            ));
        }
        DIR.with(PathBuf::clone)
    }

    fn state() -> State {
        let temp = temp_dir();
        let library = library_at("/media/DRIVE/Engine Library");
        State {
            page: Page::Library,
            config: ConfigStore::load(Locations {
                base_candidates: Vec::new(),
                user_config: temp.join("config.toml"),
                overrides: temp.join("overrides.toml"),
            }),
            config_error: None,
            backend: BackendState::Starting,
            open_when_ready: None,
            restore: None,
            session: SessionStore::load(temp.join("session.toml")),
            library: LibraryState::Open(Browser::new(Arc::new(library))),
            recents: Recents::load(temp.join("recent.toml")),
            local_folders: LocalFolders::load(temp.join("local-folders.toml")),
            drives: Vec::new(),
            scanning_drives: false,
            covers: Covers::default(),
            reachable: Reachability::default(),
            recents_error: None,
            status: PlaybackStatus::default(),
            clock: Clock::new(Instant::now()),
            waveforms: Waveforms::default(),
            cues: TrackCues::default(),
            cues_busy: false,
            loops: TrackLoops::default(),
            loops_busy: false,
            play_source: None,
            backend_error: None,
            gain_draft: None,
            delay_draft: None,
            sync: None,
            sync_reverse: false,
            progress: None,
            library_sync: None,
            smart: None,
            playlist_dialog: None,
            origin_prefix: None,
            cue_editor: None,
            loop_editor: None,
            saving: None,
            save_error: None,
            reloading: false,
            reload_result: None,
            columns: ColumnStore::load(temp.join("track-columns.toml")),
            resize: None,
            controller: Controller::default(),
        }
    }

    fn browser(state: &State) -> &Browser {
        match &state.library {
            LibraryState::Open(browser) => browser,
            _ => panic!("no library is open"),
        }
    }

    fn session(playlist: &[&str], track: Option<&str>, loaded: bool, position_ms: u64) -> Session {
        Session {
            library: Some("/media/DRIVE/Engine Library".into()),
            folder: false,
            playlist: playlist.iter().map(|title| title.to_string()).collect(),
            track: track.map(str::to_owned),
            loaded,
            position_ms,
        }
    }

    #[test]
    fn restores_the_playlist_the_song_in_it_and_loads_it() {
        let mut state = state();
        let _ = state.restore_session(session(
            &["House", "Deep"],
            Some("../Music/c.flac"),
            true,
            95_000,
        ));

        let browser = browser(&state);
        assert_eq!(browser.source, Source::Playlist(1));
        assert_eq!(browser.selected, Some(1), "c is the second track of Deep");
        assert!(browser.is_expanded(0), "the folder holding it is opened up");
        assert_eq!(
            state.play_source,
            Some(Source::Playlist(1)),
            "loaded from that list"
        );
    }

    #[test]
    fn a_song_that_was_only_selected_is_selected_but_not_loaded() {
        let mut state = state();
        let _ = state.restore_session(session(&["Pop"], Some("../Music/a.flac"), false, 0));

        assert_eq!(browser(&state).source, Source::Playlist(2));
        assert_eq!(browser(&state).selected, Some(0));
        assert_eq!(state.play_source, None);
    }

    #[test]
    fn falls_back_when_the_playlist_or_song_is_gone() {
        let mut state = state();
        let _ = state.restore_session(session(&["Gone"], Some("../Music/b.flac"), true, 1_000));
        assert_eq!(browser(&state).source, Source::Collection);
        assert_eq!(
            browser(&state).selected,
            Some(1),
            "b is the second of all tracks"
        );

        let mut state = state_with_nothing();
        let _ = state.restore_session(session(&["Pop"], Some("../Music/zzz.flac"), true, 1_000));
        assert_eq!(browser(&state).source, Source::Playlist(2));
        assert_eq!(browser(&state).selected, None);
        assert_eq!(state.play_source, None, "nothing to load");
    }

    fn state_with_nothing() -> State {
        state()
    }

    #[test]
    fn remembers_what_is_selected() {
        let mut state = state();
        assert_eq!(state.snapshot().unwrap().track, None);

        if let LibraryState::Open(browser) = &mut state.library {
            browser.show(Source::Playlist(1));
            browser.selected = Some(0);
        }
        assert_eq!(
            state.snapshot().unwrap(),
            session(&["House", "Deep"], Some("../Music/b.flac"), false, 0)
        );
    }

    #[test]
    fn remembers_the_playing_song_and_the_list_it_plays_from() {
        let mut state = state();
        // The user browses all tracks and has selected something else meanwhile.
        if let LibraryState::Open(browser) = &mut state.library {
            browser.selected = Some(0);
        }
        state.play_source = Some(Source::Playlist(1));
        state.status = PlaybackStatus {
            state: PlaybackState::Paused.into(),
            queue_position: Some(1),
            track_id: Some(2),
            position_ms: 61_250,
            error: None,
        };

        assert_eq!(
            state.snapshot().unwrap(),
            session(&["House", "Deep"], Some("../Music/c.flac"), true, 61_250)
        );
    }

    fn sync_diff(changed: &[&str]) -> Arc<open_engine_api::v1::GetSyncDiffResponse> {
        Arc::new(open_engine_api::v1::GetSyncDiffResponse {
            source_available: true,
            changed_databases: changed.iter().map(|name| name.to_string()).collect(),
            ..Default::default()
        })
    }

    #[test]
    fn the_sync_window_shows_the_differences_once_they_are_known() {
        let mut state = state();
        // (Without a backend there is nothing to compare with, so the window stays closed.)
        let _ = handle(&mut state, Message::SyncOpen);
        assert!(state.sync.is_none());

        state.sync = Some(SyncStage::Loading);
        let _ = handle(
            &mut state,
            Message::SyncDiffLoaded(Ok(sync_diff(&["m.db"]))),
        );
        assert!(matches!(
            state.sync,
            Some(SyncStage::Ready {
                acknowledged: false,
                error: None,
                ..
            })
        ));

        // The tick that allows overwriting changes made outside is the window's own.
        let _ = handle(&mut state, Message::SyncAcknowledged(true));
        assert!(matches!(
            state.sync,
            Some(SyncStage::Ready {
                acknowledged: true,
                ..
            })
        ));

        // Starting needs the backend; without it nothing is written or changed.
        let _ = handle(&mut state, Message::SyncStart);
        assert!(matches!(state.sync, Some(SyncStage::Ready { .. })));
    }

    #[test]
    fn a_failed_comparison_is_shown_and_a_late_answer_for_a_closed_window_is_dropped() {
        let mut state = state();
        state.sync = Some(SyncStage::Loading);
        let _ = handle(&mut state, Message::SyncDiffLoaded(Err("no luck".into())));
        assert!(matches!(&state.sync, Some(SyncStage::Failed(error)) if error == "no luck"));

        let _ = handle(&mut state, Message::SyncClose);
        assert!(state.sync.is_none());
        let _ = handle(
            &mut state,
            Message::SyncDiffLoaded(Ok(sync_diff(&["m.db"]))),
        );
        assert!(state.sync.is_none(), "nobody is waiting for it any more");
    }

    #[test]
    fn a_sync_ends_in_a_report_or_goes_back_with_the_reason_and_cannot_be_closed_meanwhile() {
        let writing = |diff| SyncStage::Syncing {
            diff,
            acknowledged: true,
        };
        let mut state = state();
        state.sync = Some(writing(sync_diff(&["m.db"])));

        // The window stays until the writing is done.
        let _ = handle(&mut state, Message::SyncClose);
        assert!(matches!(state.sync, Some(SyncStage::Syncing { .. })));

        // It failed: back to the differences, still ticked, with what went wrong.
        let _ = handle(&mut state, Message::SyncFinished(Err("disk full".into())));
        match &state.sync {
            Some(SyncStage::Ready {
                acknowledged,
                error,
                ..
            }) => {
                assert!(*acknowledged);
                assert_eq!(error.as_deref(), Some("disk full"));
            }
            other => panic!("expected the differences again, got {other:?}"),
        }

        // It worked: a report, and then the window can go.
        state.sync = Some(writing(sync_diff(&["m.db"])));
        let done = open_engine_api::v1::SyncToSourceResponse {
            files_written: 2,
            backup_path: "/media/DRIVE/Engine Library/Database2/open-engine-lib-backup".into(),
        };
        let _ = handle(&mut state, Message::SyncFinished(Ok(done)));
        assert!(matches!(&state.sync, Some(SyncStage::Done(done)) if done.files_written == 2));
        let _ = handle(&mut state, Message::SyncClose);
        assert!(state.sync.is_none());
    }

    #[test]
    fn closing_the_library_closes_the_sync_window() {
        let mut state = state();
        state.sync = Some(SyncStage::Ready {
            diff: sync_diff(&["m.db"]),
            acknowledged: false,
            error: None,
        });
        let _ = handle(&mut state, Message::CloseLibrary);
        assert!(state.sync.is_none());
    }

    #[test]
    fn a_saved_library_is_where_the_app_is_now_and_nothing_else_changes() {
        let mut state = state();
        if let LibraryState::Open(browser) = &mut state.library {
            browser.show(Source::Playlist(1));
            browser.selected = Some(1);
        }
        state.play_source = Some(Source::Playlist(1));
        state.status = PlaybackStatus {
            state: PlaybackState::Playing.into(),
            queue_position: Some(1),
            track_id: Some(2),
            position_ms: 42_000,
            error: None,
        };
        state.saving = Some("/media/NEW".into());

        let _ = handle(
            &mut state,
            Message::LibrarySaved(Ok(Arc::new(library_at("/media/NEW/Engine Library")))),
        );

        assert!(!state.locked() && state.save_error.is_none());
        let browser = browser(&state);
        assert_eq!(
            browser.library.path, "/media/NEW/Engine Library",
            "the new location"
        );
        // The list shown, the selection, and the song playing are all as they were.
        assert_eq!(
            (browser.source, browser.selected),
            (Source::Playlist(1), Some(1))
        );
        assert_eq!(state.status.position_ms, 42_000);
        assert_eq!(state.play_source, Some(Source::Playlist(1)));
        // The session and the recent libraries refer to the new place.
        assert_eq!(
            state.recents.paths().first().map(|p| p.to_str().unwrap()),
            Some("/media/NEW/Engine Library")
        );
        assert_eq!(
            state
                .snapshot()
                .unwrap()
                .library
                .as_deref()
                .and_then(|p| p.to_str()),
            Some("/media/NEW/Engine Library")
        );
    }

    #[test]
    fn a_failed_reload_unlocks_the_library_and_says_why() {
        let mut state = state();
        state.reloading = true;

        let _ = handle(
            &mut state,
            Message::Reloaded(Err("the source can't be reached".into())),
        );

        assert!(!state.locked());
        let (message, failed) = state.reload_result.clone().unwrap();
        assert!(failed && message.contains("the source can't be reached"));
        assert_eq!(browser(&state).library.path, "/media/DRIVE/Engine Library");
    }

    #[test]
    fn a_reload_is_done_when_the_library_is_fetched_again() {
        let mut state = state();
        state.reloading = true;

        let _ = handle(
            &mut state,
            Message::Reloaded(Ok(open_engine_api::v1::PullFromSourceResponse {
                files_written: 2,
                kept_edits_path: "/cache/unsynced-edits".into(),
            })),
        );
        // The backend has the new working copy, but this has not fetched it yet.
        assert!(state.locked());
        let (message, failed) = state.reload_result.clone().unwrap();
        assert!(!failed && message.contains("/cache/unsynced-edits"));

        let _ = handle(
            &mut state,
            Message::LibraryReloaded(Ok(Arc::new(library_at("/media/DRIVE/Engine Library")))),
        );
        assert!(!state.locked());
        assert_eq!(browser(&state).library.path, "/media/DRIVE/Engine Library");

        // Without edits that were kept there is nothing to point at.
        state.reloading = true;
        let _ = handle(
            &mut state,
            Message::Reloaded(Ok(open_engine_api::v1::PullFromSourceResponse {
                files_written: 2,
                kept_edits_path: String::new(),
            })),
        );
        let (message, _) = state.reload_result.clone().unwrap();
        assert!(!message.contains('/'), "{message}");
    }

    #[test]
    fn no_other_library_is_opened_while_one_is_reloaded() {
        let mut state = state();
        state.reloading = true;

        let _ = handle(&mut state, Message::OpenRecent("/media/OTHER".into()));
        let _ = handle(&mut state, Message::SyncOpen);

        assert!(matches!(state.library, LibraryState::Open(_)));
        assert_eq!(browser(&state).library.path, "/media/DRIVE/Engine Library");
        assert!(state.sync.is_none());
    }

    #[test]
    fn a_failed_save_keeps_the_library_where_it_was_and_says_why() {
        let mut state = state();
        state.saving = Some("/media/NEW".into());

        let _ = handle(
            &mut state,
            Message::LibrarySaved(Err("there already is a library there".into())),
        );

        assert!(!state.locked());
        assert_eq!(
            state.save_error.as_deref(),
            Some("there already is a library there")
        );
        assert_eq!(browser(&state).library.path, "/media/DRIVE/Engine Library");
        // Trying again clears the old message.
        let _ = handle(&mut state, Message::MenuSelected(MenuItem::SaveAs));
        assert!(state.save_error.is_none());
    }

    #[test]
    fn the_library_cannot_be_closed_or_replaced_while_it_is_being_saved() {
        let mut state = state();
        state.saving = Some("/media/NEW".into());

        for message in [
            Message::CloseLibrary,
            Message::ChooseLibrary,
            Message::OpenRecent("/media/OTHER/Engine Library".into()),
            Message::LibraryChosen(Some("/media/OTHER".into())),
            Message::MenuSelected(MenuItem::Open),
        ] {
            let _ = handle(&mut state, message);
            assert!(matches!(state.library, LibraryState::Open(_)), "still open");
        }
        assert_eq!(browser(&state).library.path, "/media/DRIVE/Engine Library");
        // And a second save can't start on top of the first.
        let _ = handle(
            &mut state,
            Message::SaveAsChosen(Some("/media/ELSE".into())),
        );
        assert_eq!(
            state.saving.as_deref(),
            Some(std::path::Path::new("/media/NEW"))
        );
    }

    #[test]
    fn dragging_a_column_border_changes_that_columns_width() {
        let mut state = state();
        let before = state.columns.widths.get(Column::Artist);

        let _ = handle(&mut state, Message::ResizeStarted(Column::Artist));
        // The drag is measured from where the pointer was at its first move.
        let _ = handle(&mut state, Message::ResizeMoved(500.0));
        assert_eq!(state.columns.widths.get(Column::Artist), before);
        let _ = handle(&mut state, Message::ResizeMoved(560.0));
        assert_eq!(state.columns.widths.get(Column::Artist), before + 60.0);
        let _ = handle(&mut state, Message::ResizeMoved(480.0));
        assert_eq!(
            state.columns.widths.get(Column::Artist),
            before - 20.0,
            "and back"
        );
        // Other columns are not touched, and a column can't be dragged away.
        assert_eq!(
            state.columns.widths.get(Column::Album),
            columns::ColumnWidths::default().get(Column::Album)
        );
        let _ = handle(&mut state, Message::ResizeMoved(-5000.0));
        assert_eq!(state.columns.widths.get(Column::Artist), columns::MIN_WIDTH);

        // Letting go ends the drag and saves the width.
        let _ = handle(&mut state, Message::ResizeEnded);
        assert!(state.resize.is_none());
        let temp = temp_dir();
        let saved = ColumnStore::load(temp.join("track-columns.toml"));
        assert_eq!(saved.widths.get(Column::Artist), columns::MIN_WIDTH);
        let _ = std::fs::remove_dir_all(temp);
    }

    #[test]
    fn pointer_moves_without_a_drag_change_nothing() {
        let mut state = state();
        let before = state.columns.widths;
        let _ = handle(&mut state, Message::ResizeMoved(300.0));
        let _ = handle(&mut state, Message::ResizeMoved(900.0));
        let _ = handle(&mut state, Message::ResizeEnded);
        assert_eq!(state.columns.widths, before);
    }

    fn smart(title: &str, tracks: &[usize]) -> Playlist {
        Playlist {
            is_smart: true,
            ..playlist(title, 0, false, tracks)
        }
    }

    #[test]
    fn a_new_smart_playlist_is_shown_and_the_playlists_after_it_are_found_again() {
        let mut state = state();
        let _ = handle(&mut state, Message::SmartNew);
        assert!(
            state
                .smart
                .as_ref()
                .is_some_and(|editor| editor.target.is_none())
        );

        // Without a name nothing is sent.
        let _ = handle(&mut state, Message::SmartSave);
        let editor = state.smart.as_ref().unwrap();
        assert!(!editor.saving);
        assert_eq!(
            editor.error.as_deref(),
            Some(Language::English.strings().smart_name_missing.as_str())
        );

        // The backend refusing it keeps the window, with the reason.
        let _ = handle(&mut state, Message::SmartSaved(Err("taken".into())));
        let editor = state.smart.as_ref().unwrap();
        assert!(!editor.saving);
        assert!(
            editor
                .error
                .as_deref()
                .is_some_and(|error| error.contains("taken"))
        );

        // Pop is playing and House is open; the new list lands between them.
        let LibraryState::Open(open) = &mut state.library else {
            unreachable!()
        };
        open.expand(&[0, 1]);
        state.play_source = Some(Source::Playlist(2));
        let _ = handle(
            &mut state,
            Message::SmartSaved(Ok(backend::SmartPlaylistEdited {
                playlists: vec![
                    playlist("House", 0, true, &[]),
                    playlist("Deep", 1, false, &[1, 2]),
                    smart("Mine", &[0, 1]),
                    playlist("Pop", 0, false, &[0]),
                ],
                playlist: 2,
            })),
        );
        assert!(state.smart.is_none());
        assert_eq!(browser(&state).source, Source::Playlist(2));
        assert_eq!(browser(&state).tracks(), [0, 1]);
        assert!(browser(&state).is_expanded(0), "House is still open");
        assert_eq!(state.play_source, Some(Source::Playlist(3)), "still Pop");
    }

    #[test]
    fn a_playlist_is_added_renamed_and_deleted_through_one_window() {
        let mut state = state();
        // House (0) with Deep (1), and Pop (2). Deep is shown and Pop is playing.
        let LibraryState::Open(open) = &mut state.library else {
            unreachable!()
        };
        open.expand(&[0, 1]);
        open.show(Source::Playlist(1));
        state.play_source = Some(Source::Playlist(2));

        // Only ordinary playlists have these windows, and only one window is open.
        let _ = handle(&mut state, Message::PlaylistRename(9));
        let _ = handle(&mut state, Message::PlaylistDelete(9));
        assert!(state.playlist_dialog.is_none());
        let _ = handle(&mut state, Message::PlaylistNew);
        let _ = handle(&mut state, Message::PlaylistDelete(0));
        assert!(matches!(
            state.playlist_dialog.as_ref().map(|dialog| dialog.kind),
            Some(playlist_dialog::Kind::Create { parent: None })
        ));
        let _ = handle(&mut state, Message::SmartNew);
        assert!(state.smart.is_none());

        // Nothing is sent without a name.
        let _ = handle(&mut state, Message::PlaylistConfirm);
        let dialog = state.playlist_dialog.as_ref().unwrap();
        assert!(!dialog.busy && dialog.error.is_some());

        // A new playlist between House and Pop is shown; the ones after it move down.
        let _ = handle(
            &mut state,
            Message::PlaylistsEdited(Ok(backend::PlaylistsEdited {
                playlists: vec![
                    playlist("House", 0, true, &[]),
                    playlist("Deep", 1, false, &[1, 2]),
                    playlist("Mix", 0, false, &[]),
                    playlist("Pop", 0, false, &[0]),
                ],
                playlist: Some(2),
            })),
        );
        assert!(state.playlist_dialog.is_none());
        assert_eq!(browser(&state).source, Source::Playlist(2));
        assert_eq!(state.play_source, Some(Source::Playlist(3)));
        assert!(browser(&state).is_expanded(0));

        // Renaming changes no numbers and keeps what is shown.
        let _ = handle(&mut state, Message::PlaylistRename(2));
        assert_eq!(state.playlist_dialog.as_ref().unwrap().name, "Mix");
        let _ = handle(
            &mut state,
            Message::PlaylistsEdited(Err("there is already a playlist with that title".into())),
        );
        let dialog = state.playlist_dialog.as_ref().unwrap();
        assert!(!dialog.busy);
        assert!(
            dialog
                .error
                .as_deref()
                .is_some_and(|error| error.contains("already"))
        );
        let _ = handle(
            &mut state,
            Message::PlaylistsEdited(Ok(backend::PlaylistsEdited {
                playlists: vec![
                    playlist("House", 0, true, &[]),
                    playlist("Deep", 1, false, &[1, 2]),
                    playlist("Mixes", 0, false, &[]),
                    playlist("Pop", 0, false, &[0]),
                ],
                playlist: None,
            })),
        );
        assert!(state.playlist_dialog.is_none());
        assert_eq!(browser(&state).source, Source::Playlist(2));
        assert_eq!(browser(&state).library.playlists[2].title, "Mixes");

        // Deleting House takes Deep with it: Mixes was shown and stays, Pop keeps playing.
        let _ = handle(&mut state, Message::PlaylistDelete(0));
        let _ = handle(
            &mut state,
            Message::PlaylistsEdited(Ok(backend::PlaylistsEdited {
                playlists: vec![
                    playlist("Mixes", 0, false, &[]),
                    playlist("Pop", 0, false, &[0]),
                ],
                playlist: None,
            })),
        );
        assert_eq!(browser(&state).source, Source::Playlist(0));
        assert_eq!(state.play_source, Some(Source::Playlist(1)));

        // Deleting the one that is shown goes back to all tracks, and the queue's list is gone
        // when that is the one deleted.
        let _ = handle(&mut state, Message::PlaylistDelete(1));
        let _ = handle(
            &mut state,
            Message::PlaylistsEdited(Ok(backend::PlaylistsEdited {
                playlists: vec![playlist("Mixes", 0, false, &[])],
                playlist: None,
            })),
        );
        assert_eq!(state.play_source, None);
        let _ = handle(&mut state, Message::PlaylistDelete(0));
        let _ = handle(
            &mut state,
            Message::PlaylistsEdited(Ok(backend::PlaylistsEdited {
                playlists: Vec::new(),
                playlist: None,
            })),
        );
        assert_eq!(browser(&state).source, Source::Collection);
    }

    #[test]
    fn saved_origins_are_shown_and_the_result_is_told() {
        let mut state = state();
        assert_eq!(
            browser(&state).origin(&browser(&state).library.tracks[0]),
            None
        );

        let _ = handle(
            &mut state,
            Message::OriginsMigrated(Ok(open_engine_api::v1::MigrateOriginsResponse {
                saved: 2,
                kept: 0,
                skipped: 1,
                origins: vec![open_engine_api::v1::TrackOrigin {
                    track_id: 1,
                    origin: "/music/b.flac".into(),
                }],
            })),
        );
        let open = browser(&state);
        assert_eq!(open.origin(&open.library.tracks[1]), Some("/music/b.flac"));
        assert_eq!(open.origin(&open.library.tracks[0]), None);
        assert_eq!(
            open.origin_notice(),
            Some("Origins saved: 2 new, 0 already known, 1 without a file here.")
        );

        let _ = handle(
            &mut state,
            Message::OriginsMigrated(Err("no database".into())),
        );
        assert_eq!(state.backend_error.as_deref(), Some("no database"));
    }

    #[test]
    fn the_origin_folder_is_changed_in_a_window_of_its_own() {
        use open_engine_api::v1::{
            GetOriginPrefixResponse, ReplaceOriginPrefixResponse, TrackOrigin,
        };
        let mut state = state();

        // No window opens over another, and the window itself blocks the others.
        state.origin_prefix = Some(origin_prefix_dialog::Dialog::new());
        let _ = handle(&mut state, Message::PlaylistNew);
        assert!(state.playlist_dialog.is_none());
        let _ = handle(&mut state, Message::SyncOpen);
        assert!(state.sync.is_none());

        // What the backend found is shown, and the new folder starts as it.
        let _ = handle(
            &mut state,
            Message::OriginPrefixLoaded(Ok(GetOriginPrefixResponse {
                prefix: "/old".into(),
                count: 2,
            })),
        );
        let dialog = state.origin_prefix.as_ref().unwrap();
        assert_eq!(dialog.found, Some(("/old".to_string(), 2)));
        assert_eq!(dialog.new, "/old");
        assert!(dialog.change().is_none());

        // Typing or picking a folder sets it.
        let _ = handle(
            &mut state,
            Message::OriginPrefixDialog(origin_prefix_dialog::Event::New("/typed".into())),
        );
        assert_eq!(state.origin_prefix.as_ref().unwrap().new, "/typed");
        let _ = handle(
            &mut state,
            Message::OriginPrefixPicked(Some(PathBuf::from("/picked"))),
        );
        assert_eq!(state.origin_prefix.as_ref().unwrap().new, "/picked");
        let _ = handle(&mut state, Message::OriginPrefixPicked(None));
        assert_eq!(state.origin_prefix.as_ref().unwrap().new, "/picked");
        assert_eq!(
            state.origin_prefix.as_ref().unwrap().change(),
            Some(("/old", "/picked"))
        );

        // A failure stays in the window, which can be tried again.
        state.origin_prefix.as_mut().unwrap().busy = true;
        let _ = handle(
            &mut state,
            Message::OriginPrefixApplied(Err("locked".into())),
        );
        let dialog = state.origin_prefix.as_ref().unwrap();
        assert!(!dialog.busy);
        assert_eq!(
            dialog.error.as_deref(),
            Some("Could not change the origins: locked")
        );

        // What changed is shown in the track list and the window closes.
        let _ = handle(
            &mut state,
            Message::OriginPrefixApplied(Ok(ReplaceOriginPrefixResponse {
                changed: 2,
                origins: vec![TrackOrigin {
                    track_id: 1,
                    origin: "/picked/b.flac".into(),
                }],
            })),
        );
        assert!(state.origin_prefix.is_none());
        let open = browser(&state);
        assert_eq!(open.origin(&open.library.tracks[1]), Some("/picked/b.flac"));
        assert_eq!(open.origin_notice(), Some("Origins changed: 2."));

        // Cancelling closes it, but not while it is writing.
        state.origin_prefix = Some(origin_prefix_dialog::Dialog::new());
        state.origin_prefix.as_mut().unwrap().busy = true;
        let _ = handle(&mut state, Message::OriginPrefixCancel);
        assert!(state.origin_prefix.is_some());
        state.origin_prefix.as_mut().unwrap().busy = false;
        let _ = handle(&mut state, Message::OriginPrefixCancel);
        assert!(state.origin_prefix.is_none());
    }

    fn library_diff(count: u32) -> Arc<open_engine_api::v1::GetLibrarySyncDiffResponse> {
        Arc::new(open_engine_api::v1::GetLibrarySyncDiffResponse {
            target_path: "/media/STICK/Engine Library".into(),
            playlists_changed: count,
            ..Default::default()
        })
    }

    #[test]
    fn playlists_are_synced_to_another_library_in_steps() {
        let mut state = state();
        let target = PathBuf::from("/media/STICK/Engine Library");

        // The window opens on the choice, and no second window opens over it.
        let _ = handle(&mut state, Message::LibrarySyncOpen);
        assert!(matches!(state.library_sync, Some(LibrarySyncStage::Pick)));
        let _ = handle(&mut state, Message::PlaylistNew);
        assert!(state.playlist_dialog.is_none());
        let _ = handle(&mut state, Message::SyncOpen);
        assert!(state.sync.is_none());

        // An answer for a comparison that is not under way is not wanted.
        let _ = handle(
            &mut state,
            Message::LibrarySyncDiffLoaded {
                target: target.clone(),
                options: library_sync_view::Options::default(),
                initial: false,
                result: Ok(library_diff(2)),
            },
        );
        assert!(matches!(state.library_sync, Some(LibrarySyncStage::Pick)));

        // A comparison that is under way takes its own answer, and not another's.
        state.library_sync = Some(LibrarySyncStage::Loading {
            target: target.clone(),
            options: library_sync_view::Options::default(),
        });
        let _ = handle(
            &mut state,
            Message::LibrarySyncDiffLoaded {
                target: target.clone(),
                initial: false,
                options: library_sync_view::Options {
                    remove_missing: true,
                    ..Default::default()
                },
                result: Ok(library_diff(9)),
            },
        );
        assert!(matches!(
            state.library_sync,
            Some(LibrarySyncStage::Loading { .. })
        ));
        let _ = handle(
            &mut state,
            Message::LibrarySyncDiffLoaded {
                target: target.clone(),
                options: library_sync_view::Options::default(),
                initial: false,
                result: Ok(library_diff(2)),
            },
        );
        assert!(matches!(
            &state.library_sync,
            Some(LibrarySyncStage::Ready { diff, .. }) if diff.playlists_changed == 2
        ));
        assert_eq!(
            state.recents.paths().first(),
            Some(&PathBuf::from("/media/STICK/Engine Library")),
            "a library that could be compared is remembered"
        );

        // Writing can't be closed; when it fails the differences come back with the reason.
        state.library_sync = Some(LibrarySyncStage::Syncing {
            target: target.clone(),
            options: library_sync_view::Options::default(),
            diff: library_diff(2),
        });
        let _ = handle(&mut state, Message::LibrarySyncClose);
        let _ = handle(&mut state, Message::LibrarySyncBack);
        assert!(matches!(
            state.library_sync,
            Some(LibrarySyncStage::Syncing { .. })
        ));
        let _ = handle(
            &mut state,
            Message::LibrarySyncFinished(Err("the stick is read-only".into())),
        );
        assert!(matches!(
            &state.library_sync,
            Some(LibrarySyncStage::Ready { error: Some(error), .. }) if error.contains("read-only")
        ));

        // A comparison that failed leads back to the choice.
        state.library_sync = Some(LibrarySyncStage::Loading {
            target: target.clone(),
            options: library_sync_view::Options::default(),
        });
        let _ = handle(
            &mut state,
            Message::LibrarySyncDiffLoaded {
                target,
                initial: false,
                options: library_sync_view::Options::default(),
                result: Err("not a library".into()),
            },
        );
        assert!(matches!(
            state.library_sync,
            Some(LibrarySyncStage::Failed { .. })
        ));
        let _ = handle(&mut state, Message::LibrarySyncBack);
        assert!(matches!(state.library_sync, Some(LibrarySyncStage::Pick)));
        let _ = handle(&mut state, Message::LibrarySyncClose);
        assert!(state.library_sync.is_none());
    }

    #[test]
    fn a_drive_coming_or_going_makes_the_paths_be_checked_again() {
        let mut state = state();
        let generation = state.reachable.generation();
        state.reachable.claim_unrequested(["/media/STICK/a.flac"]);
        let _ = handle(
            &mut state,
            Message::PathsChecked(generation, vec![("/media/STICK/a.flac".into(), false)]),
        );
        assert_eq!(state.reachable.get("/media/STICK/a.flac"), Some(false));

        // The same drives again change nothing.
        let _ = handle(&mut state, Message::DrivesFound(Vec::new()));
        assert_eq!(state.reachable.get("/media/STICK/a.flac"), Some(false));

        // A drive that appears may bring the file back.
        let _ = handle(
            &mut state,
            Message::DrivesFound(vec![drives::Drive {
                name: "STICK".into(),
                mount: "/media/STICK".into(),
                library: None,
            }]),
        );
        assert_eq!(state.reachable.get("/media/STICK/a.flac"), None);
        assert_eq!(
            state.reachable.claim_unrequested(["/media/STICK/a.flac"]),
            ["/media/STICK/a.flac"]
        );
    }

    #[test]
    fn how_far_a_sync_is_gets_asked_only_while_it_writes() {
        let mut state = state();
        let progress = |done: u64| open_engine_api::v1::OperationProgress {
            running: true,
            total_bytes: 100,
            done_bytes: done,
            ..Default::default()
        };

        // Not writing: an answer is not wanted, and nothing is asked.
        assert!(!state.syncing());
        let _ = handle(&mut state, Message::ProgressUpdated(Ok(progress(10))));
        assert!(state.progress.is_none());

        state.library_sync = Some(LibrarySyncStage::Syncing {
            target: PathBuf::from("/media/STICK/Engine Library"),
            options: library_sync_view::Options::default(),
            diff: library_diff(1),
        });
        assert!(state.syncing());
        let _ = handle(&mut state, Message::ProgressUpdated(Ok(progress(40))));
        assert_eq!(state.progress.as_ref().map(|p| p.done_bytes), Some(40));
        // A failed question just leaves the bar to say it is working.
        let _ = handle(&mut state, Message::ProgressUpdated(Err("busy".into())));
        assert!(state.progress.is_none());

        // The sync with the source writes too.
        state.library_sync = None;
        state.sync = Some(SyncStage::Syncing {
            diff: Arc::new(open_engine_api::v1::GetSyncDiffResponse::default()),
            acknowledged: false,
        });
        assert!(state.syncing());
    }

    #[test]
    fn how_far_a_folder_is_gets_asked_only_while_it_opens_and_is_forgotten_after() {
        let mut state = state();
        let progress = open_engine_api::v1::OperationProgress {
            running: true,
            phase: open_engine_api::v1::ProgressPhase::ReadingTags.into(),
            total_files: 10,
            done_files: 4,
            ..Default::default()
        };

        assert!(!state.reporting_progress());
        let _ = handle(&mut state, Message::ProgressUpdated(Ok(progress.clone())));
        assert!(state.progress.is_none());

        state.library = LibraryState::Opening("/home/me/Music".into());
        assert!(state.reporting_progress());
        let _ = handle(&mut state, Message::ProgressUpdated(Ok(progress)));
        assert_eq!(state.progress.as_ref().map(|p| p.done_files), Some(4));

        let folder = Arc::new(library_at("/home/me/Music").of_kind(model::Kind::Folder));
        let _ = handle(&mut state, Message::LibraryOpened(Ok(folder)));
        assert!(
            state.progress.is_none(),
            "the bar of the next sync starts empty"
        );
        assert!(!state.reporting_progress());
    }

    #[test]
    fn the_sync_window_can_replace_the_working_copy_by_the_source() {
        let mut state = state();
        let diff = Arc::new(open_engine_api::v1::GetSyncDiffResponse {
            source_available: true,
            changed_databases: vec!["m.db".into()],
            ..Default::default()
        });
        state.sync = Some(SyncStage::Ready {
            diff: Arc::clone(&diff),
            acknowledged: false,
            error: None,
        });

        // The direction is chosen in the window; opening it again starts the usual way.
        let _ = handle(&mut state, Message::SyncDirection(true));
        assert!(state.sync_reverse);
        state.sync = None;
        let _ = handle(&mut state, Message::SyncOpen);
        assert!(!state.sync_reverse);
        assert!(sync_view::can_pull(&diff));
        assert!(!sync_view::can_pull(
            &open_engine_api::v1::GetSyncDiffResponse {
                source_busy: true,
                ..(*diff).clone()
            }
        ));

        // A pull that fails leaves the differences with the reason; one that works ends
        // in the report (and the library is fetched again, which needs a backend).
        state.sync = Some(SyncStage::Syncing {
            diff: Arc::clone(&diff),
            acknowledged: false,
        });
        let _ = handle(&mut state, Message::SyncPulled(Err("disk full".into())));
        assert!(matches!(
            &state.sync,
            Some(SyncStage::Ready { error: Some(error), .. }) if error == "disk full"
        ));
        state.sync = Some(SyncStage::Syncing {
            diff,
            acknowledged: false,
        });
        let _ = handle(
            &mut state,
            Message::SyncPulled(Ok(open_engine_api::v1::PullFromSourceResponse {
                files_written: 2,
                kept_edits_path: "/cache/unsynced-edits".into(),
            })),
        );
        assert!(matches!(&state.sync, Some(SyncStage::Pulled(done)) if done.files_written == 2));
    }

    #[test]
    fn a_library_that_is_read_again_keeps_the_list_and_the_song_but_starts_nothing() {
        let mut state = state();
        let LibraryState::Open(open) = &mut state.library else {
            unreachable!()
        };
        open.show(Source::Playlist(1));
        open.selected = Some(1);

        let _ = handle(
            &mut state,
            Message::LibraryReloaded(Ok(Arc::new(library_at("/media/DRIVE/Engine Library")))),
        );
        assert_eq!(browser(&state).source, Source::Playlist(1));
        assert_eq!(browser(&state).selected, Some(1));
        assert!(state.is_stopped(), "nothing was started");

        let _ = handle(&mut state, Message::LibraryReloaded(Err("gone".into())));
        assert_eq!(state.backend_error.as_deref(), Some("gone"));
    }

    #[test]
    fn the_other_way_round_the_open_library_is_read_again_only_when_it_changed() {
        let mut state = state();
        let target = PathBuf::from("/media/STICK/Engine Library");
        let syncing = |options| LibrarySyncStage::Syncing {
            target: target.clone(),
            options,
            diff: library_diff(1),
        };
        let reverse = library_sync_view::Options {
            reverse: true,
            ..Default::default()
        };

        // Nothing was written: the stage is the report of that, and the way is remembered.
        state.library_sync = Some(syncing(reverse));
        let _ = handle(
            &mut state,
            Message::LibrarySyncFinished(Ok(open_engine_api::v1::SyncToLibraryResponse::default())),
        );
        assert!(matches!(
            state.library_sync,
            Some(LibrarySyncStage::Done(_, true))
        ));

        state.library_sync = Some(syncing(library_sync_view::Options::default()));
        let _ = handle(
            &mut state,
            Message::LibrarySyncFinished(Ok(open_engine_api::v1::SyncToLibraryResponse::default())),
        );
        assert!(matches!(
            state.library_sync,
            Some(LibrarySyncStage::Done(_, false))
        ));
    }

    #[test]
    fn a_library_that_is_not_there_yet_is_remembered_once_it_is_made() {
        let mut state = state();
        let target = PathBuf::from("/media/NEW");
        let new_diff = Arc::new(open_engine_api::v1::GetLibrarySyncDiffResponse {
            target_path: "/media/NEW/Engine Library".into(),
            target_is_new: true,
            tracks_added: 3,
            ..Default::default()
        });
        state.library_sync = Some(LibrarySyncStage::Loading {
            target: target.clone(),
            options: library_sync_view::Options {
                copy_tracks: true,
                ..Default::default()
            },
        });
        let _ = handle(
            &mut state,
            Message::LibrarySyncDiffLoaded {
                target: target.clone(),
                options: library_sync_view::Options {
                    copy_tracks: true,
                    ..Default::default()
                },
                initial: false,
                result: Ok(Arc::clone(&new_diff)),
            },
        );
        assert!(matches!(
            &state.library_sync,
            Some(LibrarySyncStage::Ready { diff, .. }) if diff.target_is_new
        ));
        assert!(
            library_sync_view::has_changes(&new_diff),
            "making it is a change"
        );
        assert!(
            state.recents.paths().is_empty(),
            "there is no library to remember yet"
        );

        // Made: now it is one.
        state.library_sync = Some(LibrarySyncStage::Syncing {
            target,
            options: library_sync_view::Options::default(),
            diff: new_diff,
        });
        let _ = handle(
            &mut state,
            Message::LibrarySyncFinished(Ok(open_engine_api::v1::SyncToLibraryResponse {
                target_path: "/media/NEW/Engine Library".into(),
                created: true,
                ..Default::default()
            })),
        );
        assert_eq!(
            state.recents.paths(),
            [PathBuf::from("/media/NEW/Engine Library")]
        );
        assert!(matches!(
            state.library_sync,
            Some(LibrarySyncStage::Done(_, false))
        ));
    }

    #[test]
    fn a_smart_playlist_is_edited_from_its_own_rules() {
        let mut state = state();
        let LibraryState::Open(open) = &mut state.library else {
            unreachable!()
        };
        let mut playlists = vec![smart("Mine", &[0])];
        playlists[0].rules = Some(open_engine_api::v1::SmartRules {
            match_all: false,
            rules: vec![open_engine_api::v1::SmartRule {
                column: "genre".into(),
                condition: "LIKE".into(),
                param: "%House%".into(),
            }],
        });
        open.replace_playlists(playlists, &Change::Same, Some(0));

        // Only smart playlists can be edited.
        let _ = handle(&mut state, Message::SmartEdit(5));
        assert!(state.smart.is_none());
        let _ = handle(&mut state, Message::SmartEdit(0));
        let editor = state.smart.as_ref().unwrap();
        assert_eq!(editor.target, Some(0));
        assert_eq!(editor.name, "Mine");
        assert!(!editor.match_all);

        // No second window on top of it.
        let _ = handle(&mut state, Message::SmartNew);
        assert_eq!(state.smart.as_ref().unwrap().target, Some(0));
        let _ = handle(&mut state, Message::SmartCancel);
        assert!(state.smart.is_none());
    }

    #[test]
    fn a_rating_the_backend_refused_is_taken_back() {
        let mut state = state();
        let LibraryState::Open(open) = &mut state.library else {
            panic!("no library is open");
        };
        open.set_rating(1, Some(60));
        // The click showed three stars at once; the backend said no to the fourth.
        open.set_rating(1, Some(80));

        let _ = handle(
            &mut state,
            Message::RatingSet {
                track: 1,
                previous: Some(60),
                result: Err("the database is read-only".into()),
            },
        );
        assert_eq!(browser(&state).rating(1), Some(60));
        assert_eq!(
            state.backend_error.as_deref(),
            Some("the database is read-only")
        );

        // And one it did is kept, and clears the error.
        let LibraryState::Open(open) = &mut state.library else {
            unreachable!()
        };
        open.set_rating(1, Some(100));
        let _ = handle(
            &mut state,
            Message::RatingSet {
                track: 1,
                previous: Some(60),
                result: Ok(()),
            },
        );
        assert_eq!(browser(&state).rating(1), Some(100));
        assert_eq!(state.backend_error, None);
        assert_eq!(
            browser(&state).rating(0),
            None,
            "other tracks are as they were"
        );
    }

    /// A state that is playing track 1, whose cues have been fetched.
    fn state_playing_with_cues(cues: Vec<Cue>) -> State {
        let mut state = state();
        state.status = PlaybackStatus {
            state: PlaybackState::Playing.into(),
            queue_position: Some(0),
            track_id: Some(1),
            position_ms: 10_000,
            error: None,
        };
        assert!(state.cues.claim(1));
        let generation = state.cues.generation();
        let _ = handle(&mut state, Message::CuesLoaded(generation, 1, Ok(cues)));
        state
    }

    #[test]
    fn the_cues_of_the_playing_track_are_kept_for_the_library_they_were_asked_in() {
        let mut state = state_playing_with_cues(vec![Cue::new(2, 5.0)]);
        assert_eq!(state.cues.get(1).map(<[Cue]>::len), Some(1));
        assert_eq!(
            browser(&state).cue_count(1),
            0,
            "the list has what the library said"
        );

        // An answer from before another library was opened is dropped.
        let old = state.cues.generation();
        state.cues.clear();
        let _ = handle(
            &mut state,
            Message::CuesLoaded(old, 1, Ok(vec![Cue::new(0, 1.0)])),
        );
        assert_eq!(state.cues.get(1), None);

        // A failure is shown, and the track gets no pads.
        let generation = state.cues.generation();
        let _ = handle(
            &mut state,
            Message::CuesLoaded(generation, 1, Err("no".into())),
        );
        assert_eq!(state.backend_error.as_deref(), Some("no"));
        assert_eq!(state.cues.get(1), None);
    }

    #[test]
    fn what_the_backend_reports_of_the_controller_is_kept_and_a_change_of_cues_refetches_them() {
        let mut state = state_playing_with_cues(vec![Cue::new(2, 5.0)]);
        let report = |revision, track| {
            Message::ControllerState(Ok(ControllerState {
                enabled: true,
                devices: vec!["SC LIVE 4".into()],
                cues_revision: revision,
                cues_track_id: track,
                ..ControllerState::default()
            }))
        };
        let _ = handle(&mut state, report(0, 0));
        assert_eq!(state.controller.devices(), ["SC LIVE 4"]);
        assert!(
            !state.cues.claim(1),
            "the cues were fetched, not to be again"
        );

        // A pad on the controller set a cue of another track: nothing to fetch for this one.
        let _ = handle(&mut state, report(1, 7));
        assert!(!state.cues.claim(1));
        // One of the playing track: its cues are to be asked for again.
        let _ = handle(&mut state, report(2, 1));
        assert!(state.cues.claim(1), "released to be fetched again");

        // The connection to the backend failing is shown.
        let _ = handle(&mut state, Message::ControllerState(Err("gone".into())));
        assert_eq!(state.controller.problem(), Some("gone"));
        let _ = handle(&mut state, report(2, 1));
        assert_eq!(state.controller.problem(), None);
    }

    #[test]
    fn the_cue_editor_opens_for_the_playing_track_and_closes_without_changes() {
        let mut state = state_playing_with_cues(vec![Cue::new(2, 5.0)]);
        let _ = handle(&mut state, Message::CueEditorOpen);
        let editor = state.cue_editor.as_ref().expect("the editor is open");
        assert_eq!(editor.track, 1);
        assert_eq!(editor.cues().unwrap(), [Cue::new(2, 5.0)]);

        // Edits go to the window's copy, nothing is written until it is saved.
        let _ = handle(
            &mut state,
            Message::CueEditor(cue_editor::Event::Label(2, "Drop".into())),
        );
        let _ = handle(&mut state, Message::CueEditorCancel);
        assert!(state.cue_editor.is_none());
        assert_eq!(state.cues.get(1).unwrap()[0].label, "Cue 3");

        // Not while another window is open, nor for a track whose cues are not known.
        state.smart = Some(smart_editor::Editor::new());
        let _ = handle(&mut state, Message::CueEditorOpen);
        assert!(state.cue_editor.is_none());
        state.smart = None;
        state.status.track_id = Some(2);
        let _ = handle(&mut state, Message::CueEditorOpen);
        assert!(state.cue_editor.is_none());
    }

    #[test]
    fn a_position_that_is_not_one_is_said_so_and_nothing_is_sent() {
        let mut state = state_playing_with_cues(vec![Cue::new(2, 5.0)]);
        let _ = handle(&mut state, Message::CueEditorOpen);
        let _ = handle(
            &mut state,
            Message::CueEditor(cue_editor::Event::Position(2, "soon".into())),
        );
        let _ = handle(&mut state, Message::CueEditorSave);
        let editor = state.cue_editor.as_ref().unwrap();
        assert!(!editor.busy && !state.cues_busy);
        let error = editor.error.as_deref().unwrap();
        assert!(error.contains('3') && error.contains("soon"), "{error}");
    }

    #[test]
    fn saved_cues_replace_the_old_ones_and_update_the_count() {
        let mut state = state_playing_with_cues(vec![Cue::new(2, 5.0)]);
        let _ = handle(&mut state, Message::CueEditorOpen);
        state.cues_busy = true;
        state.cue_editor.as_mut().unwrap().busy = true;

        let generation = state.cues.generation();
        let saved = vec![Cue::new(0, 1.0), Cue::new(2, 6.5)];
        let _ = handle(
            &mut state,
            Message::CuesEdited {
                generation,
                track: 1,
                result: Ok(saved.clone()),
            },
        );
        assert!(!state.cues_busy && state.cue_editor.is_none());
        assert_eq!(state.cues.get(1), Some(saved.as_slice()));
        assert_eq!(browser(&state).cue_count(1), 2);
        assert_eq!(
            browser(&state).cue_count(0),
            0,
            "other tracks are as they were"
        );
        assert_eq!(state.backend_error, None);

        // An answer from before another library was opened is not for this one.
        state.cues.clear();
        let _ = handle(
            &mut state,
            Message::CuesEdited {
                generation,
                track: 1,
                result: Ok(vec![]),
            },
        );
        assert_eq!(state.cues.get(1), None);
        assert_eq!(browser(&state).cue_count(1), 2);
    }

    #[test]
    fn cues_the_backend_refused_leave_the_old_ones_and_say_why() {
        let mut state = state_playing_with_cues(vec![Cue::new(2, 5.0)]);
        let _ = handle(&mut state, Message::CueEditorOpen);
        state.cues_busy = true;
        state.cue_editor.as_mut().unwrap().busy = true;
        let generation = state.cues.generation();

        let _ = handle(
            &mut state,
            Message::CuesEdited {
                generation,
                track: 1,
                result: Err("the track has not been analysed".into()),
            },
        );
        // The window stays, so the edits are not lost, and can be tried again.
        let editor = state.cue_editor.as_ref().unwrap();
        assert!(!editor.busy && !state.cues_busy);
        assert!(
            editor
                .error
                .as_deref()
                .unwrap()
                .contains("not been analysed")
        );
        assert_eq!(state.cues.get(1).unwrap().len(), 1);

        // Without the window (a pad was clicked) it is shown on the page.
        state.cue_editor = None;
        state.cues_busy = true;
        let _ = handle(
            &mut state,
            Message::CuesEdited {
                generation,
                track: 1,
                result: Err("full".into()),
            },
        );
        assert_eq!(state.backend_error.as_deref(), Some("full"));
        assert!(!state.cues_busy);
    }

    #[test]
    fn the_cue_editor_is_not_closed_while_it_is_saving() {
        let mut state = state_playing_with_cues(vec![]);
        let _ = handle(&mut state, Message::CueEditorOpen);
        state.cue_editor.as_mut().unwrap().busy = true;
        let _ = handle(&mut state, Message::CueEditorCancel);
        assert!(state.cue_editor.is_some());
        let _ = handle(&mut state, Message::CueEditor(cue_editor::Event::Add(0)));
        assert_eq!(
            state.cue_editor.as_ref().unwrap().cues().unwrap(),
            [],
            "no edits while it is being written"
        );
    }

    /// A state that is playing track 1, whose saved loops have been fetched.
    fn state_playing_with_loops(loops: Vec<Loop>) -> State {
        let mut state = state();
        state.status = PlaybackStatus {
            state: PlaybackState::Playing.into(),
            queue_position: Some(0),
            track_id: Some(1),
            position_ms: 10_000,
            error: None,
        };
        assert!(state.loops.claim(1));
        let generation = state.loops.generation();
        let _ = handle(&mut state, Message::LoopsLoaded(generation, 1, Ok(loops)));
        state
    }

    #[test]
    fn the_loops_of_the_playing_track_are_kept_for_the_library_they_were_asked_in() {
        let mut state = state_playing_with_loops(vec![Loop::new(2, 5.0, 7.0)]);
        assert_eq!(state.loops.get(1).map(<[Loop]>::len), Some(1));
        assert_eq!(
            browser(&state).loop_count(1),
            0,
            "the list has what the library said"
        );

        let old = state.loops.generation();
        state.loops.clear();
        let _ = handle(
            &mut state,
            Message::LoopsLoaded(old, 1, Ok(vec![Loop::new(0, 1.0, 2.0)])),
        );
        assert_eq!(state.loops.get(1), None);

        let generation = state.loops.generation();
        let _ = handle(
            &mut state,
            Message::LoopsLoaded(generation, 1, Err("no".into())),
        );
        assert_eq!(state.backend_error.as_deref(), Some("no"));
        assert_eq!(state.loops.get(1), None);
    }

    #[test]
    fn a_loop_from_a_pad_is_four_beats_of_the_tempo_there_is() {
        let state = state_playing_with_loops(vec![]);
        // (The sample library has no tempo for the track and there is no waveform.)
        assert_eq!(state.new_loop_length(), loops::FALLBACK_SECONDS);
        assert_eq!(state.playing_duration(), None);
    }

    #[test]
    fn the_loop_editor_opens_for_the_playing_track_and_closes_without_changes() {
        let mut state = state_playing_with_loops(vec![Loop::new(2, 5.0, 7.0)]);
        let _ = handle(&mut state, Message::LoopEditorOpen);
        let editor = state.loop_editor.as_ref().expect("the editor is open");
        assert_eq!(editor.track, 1);
        assert_eq!(editor.loops().unwrap(), [Loop::new(2, 5.0, 7.0)]);

        let _ = handle(
            &mut state,
            Message::LoopEditor(loop_editor::Event::Label(2, "Drop".into())),
        );
        let _ = handle(&mut state, Message::LoopEditorCancel);
        assert!(state.loop_editor.is_none());
        assert_eq!(state.loops.get(1).unwrap()[0].label, "Loop 3");

        // Not while another window is open, nor for a track whose loops are not known.
        state.smart = Some(smart_editor::Editor::new());
        let _ = handle(&mut state, Message::LoopEditorOpen);
        assert!(state.loop_editor.is_none());
        state.smart = None;
        state.cue_editor = Some(cue_editor::Editor::new(1, "x", &[]));
        let _ = handle(&mut state, Message::LoopEditorOpen);
        assert!(state.loop_editor.is_none());
        state.cue_editor = None;
        state.status.track_id = Some(2);
        let _ = handle(&mut state, Message::LoopEditorOpen);
        assert!(state.loop_editor.is_none());
    }

    #[test]
    fn a_loop_that_can_not_be_saved_is_said_so_and_nothing_is_sent() {
        let mut state = state_playing_with_loops(vec![Loop::new(2, 5.0, 7.0)]);
        let _ = handle(&mut state, Message::LoopEditorOpen);
        let _ = handle(
            &mut state,
            Message::LoopEditor(loop_editor::Event::End(2, "soon".into())),
        );
        let _ = handle(&mut state, Message::LoopEditorSave);
        let editor = state.loop_editor.as_ref().unwrap();
        assert!(!editor.busy && !state.loops_busy);
        let error = editor.error.as_deref().unwrap();
        assert!(error.contains('3') && error.contains("soon"), "{error}");

        let _ = handle(
            &mut state,
            Message::LoopEditor(loop_editor::Event::End(2, "0:04".into())),
        );
        let _ = handle(&mut state, Message::LoopEditorSave);
        let editor = state.loop_editor.as_ref().unwrap();
        assert!(!editor.busy && !state.loops_busy);
        assert!(editor.error.as_deref().unwrap().contains("end after"));
    }

    #[test]
    fn saved_loops_replace_the_old_ones_and_update_the_count() {
        let mut state = state_playing_with_loops(vec![Loop::new(2, 5.0, 7.0)]);
        let _ = handle(&mut state, Message::LoopEditorOpen);
        state.loops_busy = true;
        state.loop_editor.as_mut().unwrap().busy = true;

        let generation = state.loops.generation();
        let saved = vec![Loop::new(0, 1.0, 2.0), Loop::new(2, 6.5, 8.0)];
        let _ = handle(
            &mut state,
            Message::LoopsEdited {
                generation,
                track: 1,
                result: Ok(saved.clone()),
            },
        );
        assert!(!state.loops_busy && state.loop_editor.is_none());
        assert_eq!(state.loops.get(1), Some(saved.as_slice()));
        assert_eq!(browser(&state).loop_count(1), 2);
        assert_eq!(
            browser(&state).loop_count(0),
            0,
            "other tracks are as they were"
        );
        assert_eq!(browser(&state).cue_count(1), 0, "the cues are not loops");
        assert_eq!(state.backend_error, None);

        // An answer from before another library was opened is not for this one.
        state.loops.clear();
        let _ = handle(
            &mut state,
            Message::LoopsEdited {
                generation,
                track: 1,
                result: Ok(vec![]),
            },
        );
        assert_eq!(state.loops.get(1), None);
        assert_eq!(browser(&state).loop_count(1), 2);
    }

    #[test]
    fn loops_the_backend_refused_leave_the_old_ones_and_say_why() {
        let mut state = state_playing_with_loops(vec![Loop::new(2, 5.0, 7.0)]);
        let _ = handle(&mut state, Message::LoopEditorOpen);
        state.loops_busy = true;
        state.loop_editor.as_mut().unwrap().busy = true;
        let generation = state.loops.generation();

        let _ = handle(
            &mut state,
            Message::LoopsEdited {
                generation,
                track: 1,
                result: Err("the track has not been analysed".into()),
            },
        );
        // The window stays, so the edits are not lost, and can be tried again.
        let editor = state.loop_editor.as_ref().unwrap();
        assert!(!editor.busy && !state.loops_busy);
        assert!(
            editor
                .error
                .as_deref()
                .unwrap()
                .contains("not been analysed")
        );
        assert_eq!(state.loops.get(1).unwrap().len(), 1);

        // Without the window (a pad was clicked) it is shown on the page.
        state.loop_editor = None;
        state.loops_busy = true;
        let _ = handle(
            &mut state,
            Message::LoopsEdited {
                generation,
                track: 1,
                result: Err("full".into()),
            },
        );
        assert_eq!(state.backend_error.as_deref(), Some("full"));
        assert!(!state.loops_busy);
    }

    #[test]
    fn the_loop_editor_is_not_closed_while_it_is_saving() {
        let mut state = state_playing_with_loops(vec![]);
        let _ = handle(&mut state, Message::LoopEditorOpen);
        state.loop_editor.as_mut().unwrap().busy = true;
        let _ = handle(&mut state, Message::LoopEditorCancel);
        assert!(state.loop_editor.is_some());
        let _ = handle(&mut state, Message::LoopEditor(loop_editor::Event::Add(0)));
        assert_eq!(
            state.loop_editor.as_ref().unwrap().loops().unwrap(),
            [],
            "no edits while it is being written"
        );
    }

    /// An app with a folder of music open.
    fn state_in_folder() -> State {
        let mut state = state();
        state.library = LibraryState::Open(Browser::new(Arc::new(
            library_at("/home/me/Music").of_kind(model::Kind::Folder),
        )));
        state
    }

    #[test]
    fn a_folder_of_music_that_was_opened_is_listed_under_local_music_not_recents() {
        let mut state = state();
        state.library = LibraryState::Opening("/home/me/Music".into());
        let before = state.recents.paths().to_vec();

        let folder = Arc::new(library_at("/home/me/Music").of_kind(model::Kind::Folder));
        let _ = handle(&mut state, Message::LibraryOpened(Ok(folder)));
        assert!(!browser(&state).library.is_engine());
        assert!(state.local_folders.contains(Path::new("/home/me/Music")));
        assert_eq!(
            state.recents.paths(),
            before,
            "recents are Engine libraries"
        );

        // And an Engine library the other way round.
        state.library = LibraryState::Opening("/media/NEW/Engine Library".into());
        let _ = handle(
            &mut state,
            Message::LibraryOpened(Ok(Arc::new(library_at("/media/NEW/Engine Library")))),
        );
        assert_eq!(
            state.recents.paths().first(),
            Some(&PathBuf::from("/media/NEW/Engine Library"))
        );
        assert!(
            !state
                .local_folders
                .contains(Path::new("/media/NEW/Engine Library"))
        );
    }

    #[test]
    fn an_added_folder_is_remembered_and_opened() {
        let mut state = state();
        let _ = handle(
            &mut state,
            Message::LocalFolderChosen(Some("/home/me/Music".into())),
        );
        assert!(state.local_folders.contains(Path::new("/home/me/Music")));
        // (No backend is running in the test, so it waits for it to be up.)
        assert_eq!(
            state
                .open_when_ready
                .as_ref()
                .map(|(path, _)| path.as_path()),
            Some(Path::new("/home/me/Music"))
        );

        let before = state.local_folders.paths().len();
        let _ = handle(&mut state, Message::LocalFolderChosen(None));
        assert_eq!(state.local_folders.paths().len(), before);
    }

    #[test]
    fn what_only_an_engine_library_has_does_nothing_in_a_folder_of_music() {
        let mut state = state_in_folder();
        for message in [
            Message::SyncOpen,
            Message::LibrarySyncOpen,
            Message::PlaylistNew,
            Message::PlaylistRename(0),
            Message::PlaylistDelete(0),
            Message::SmartNew,
            Message::SmartEdit(0),
            Message::MenuSelected(MenuItem::SaveAs),
        ] {
            let _ = handle(&mut state, message);
        }
        assert!(state.sync.is_none() && state.library_sync.is_none());
        assert!(state.playlist_dialog.is_none() && state.smart.is_none());
        assert!(!state.locked());

        let _ = handle(&mut state, Message::SetStars { track: 1, stars: 3 });
        assert_eq!(browser(&state).rating(1), None);

        // The same messages work in an Engine library.
        let mut engine = self::state();
        let _ = handle(&mut engine, Message::PlaylistNew);
        assert!(engine.playlist_dialog.is_some());
    }

    #[test]
    fn no_cues_or_loops_are_asked_for_in_a_folder_of_music() {
        let mut state = state_in_folder();
        state.status = PlaybackStatus {
            state: PlaybackState::Playing.into(),
            queue_position: Some(0),
            track_id: Some(1),
            position_ms: 1_000,
            error: None,
        };
        let _ = state.fetch_cues();
        let _ = state.fetch_loops();
        // (Not claimed: nothing would have been asked, and it must not wait for an answer.)
        assert!(state.cues.claim(1) && state.loops.claim(1));
    }

    #[test]
    fn closing_a_folder_forgets_it_on_the_next_start_but_keeps_it_listed() {
        let mut state = state_in_folder();
        let _ = state.local_folders.add("/home/me/Music".into());
        let _ = handle(&mut state, Message::CloseLibrary);
        assert!(matches!(
            state.library,
            LibraryState::Closed { error: None }
        ));
        assert!(state.local_folders.contains(Path::new("/home/me/Music")));
        assert_eq!(state.session.session().library, None);
    }

    #[test]
    fn removing_a_folder_from_the_list_closes_it_only_if_it_is_open() {
        let mut state = state_in_folder();
        let _ = state.local_folders.add("/home/me/Music".into());
        let _ = state.local_folders.add("/home/me/Other".into());

        let _ = handle(
            &mut state,
            Message::RemoveLocalFolder("/home/me/Other".into()),
        );
        assert!(!state.local_folders.contains(Path::new("/home/me/Other")));
        assert!(
            matches!(state.library, LibraryState::Open(_)),
            "another folder: still open"
        );

        let _ = handle(
            &mut state,
            Message::RemoveLocalFolder("/home/me/Music".into()),
        );
        assert!(!state.local_folders.contains(Path::new("/home/me/Music")));
        assert!(matches!(state.library, LibraryState::Closed { .. }));

        // An Engine library at the same path is not a folder of music: it stays.
        let mut engine = self::state();
        let _ = engine
            .local_folders
            .add("/media/DRIVE/Engine Library".into());
        let _ = handle(
            &mut engine,
            Message::RemoveLocalFolder("/media/DRIVE/Engine Library".into()),
        );
        assert!(matches!(engine.library, LibraryState::Open(_)));
    }

    #[test]
    fn a_session_in_a_folder_of_music_goes_back_to_that_folder() {
        let mut state = state_in_folder();
        let snapshot = state.snapshot().unwrap();
        assert!(snapshot.folder);
        assert_eq!(snapshot.library, Some(PathBuf::from("/home/me/Music")));
        assert!(!self::state().snapshot().unwrap().folder);

        // (Without a running backend, opening only records what is to be opened.)
        let _ = state.open(PathBuf::from("/home/me/Music"), Some(snapshot.clone()));
        assert_eq!(
            state
                .open_when_ready
                .map(|(path, restore)| (path, restore.map(|s| s.folder))),
            Some((PathBuf::from("/home/me/Music"), Some(true)))
        );
    }

    #[test]
    fn remembers_nothing_while_no_library_is_open() {
        let mut state = state();
        state.library = LibraryState::Closed { error: None };
        assert_eq!(state.snapshot(), None);
    }
}
