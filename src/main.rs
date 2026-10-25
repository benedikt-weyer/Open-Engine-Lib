#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod data;
mod handlers;
mod views;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use backend::{Backend, Control, WaveformSettings};
use browser::{Browser, Change, SortKey, Source};
use clock::Clock;
use columns::{Column, ColumnStore};
use config::{ConfigStore, Language, Locations, ThemeMode, WaveformBand, WaveformColors};
use controller::Controller;
use covers::Covers;
use cues::{Cue, TrackCues};
use data::{
    analysis_queue, backend, clock, colors, columns, config, controller, covers, cues, drives,
    i18n, local_folders, loops, model, mpris, reachable, recent, session, waveform,
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
use repair_view::Stage as RepairStage;
use session::{Session, SessionStore};
use sync_view::Stage as SyncStage;
use views::{
    browser, cue_bar, cue_editor, deck, error_text, folder_menu, import_view, library_sync_view,
    loop_bar, loop_editor, menubar, navbar, origin_prefix_dialog, player_bar, playlist_dialog,
    progress_view, repair_view, right_click, settings, sidebar, smart_editor, sync_view, toast,
    track_menu,
};
use waveform::{Waveform, Waveforms};

/// How often the playback position is fetched from the backend while playing.
const POLL_INTERVAL: Duration = Duration::from_millis(250);
/// How often the player is asked where it is while the jog wheel scratches.
const SCRATCH_POLL_INTERVAL: Duration = Duration::from_millis(25);
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
    /// Why the last drive could not be unmounted.
    drive_error: Option<String>,
    /// What the backend last said about playback, and when.
    status: PlaybackStatus,
    /// The playhead's position, running on between the reports in `status`.
    clock: Clock,
    /// Where in the track the zoomed waveform is being dragged to (scratching with the
    /// mouse), in seconds. The clock follows it at once, the backend's reports wait.
    scratch: Option<f64>,
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
    /// The window for importing a folder into an Engine library, while it is open.
    import: Option<import_view::Import>,
    /// The menu of a folder of the playlist tree, while it is open.
    folder_menu: Option<folder_menu::Menu>,
    /// The tracks that wait to be analysed.
    analysis_queue: Option<analysis_queue::Queue>,
    /// What to do when the library that was imported into has been opened.
    after_open: Option<analysis_queue::AfterOpen>,
    /// The window for repairing a library, while it is open.
    repair: Option<RepairStage>,
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
    /// The button that removes the streaming songs was pressed once and waits for a second.
    streaming_confirm: bool,
    /// The context menu of a track of the list, while it is open.
    track_menu: Option<track_menu::Menu>,
    /// The toast in the corner: the analysis of a track that runs, or how it ended.
    toast: Option<toast::Toast>,
    reload_result: Option<(String, bool)>,
    /// How wide the user made the track list's columns.
    columns: ColumnStore,
    /// The column whose width is being dragged.
    resize: Option<Resize>,
    /// The menu to show and hide columns is open.
    columns_menu: bool,
    /// The DJ controller: what its controls do, what is connected, what it said last.
    controller: Controller,
    /// How to tell the desktop what plays (MPRIS), once the service is up, and what it was
    /// last told, to tell it again only when that changed.
    mpris: Option<mpris::Handle>,
    mpris_told: Option<handlers::mpris::Told>,
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
    /// Use the controller profile of that name.
    ControllerProfileSelected(String),
    ControllerProfileNameChanged(String),
    /// A profile with the typed name, empty (`false`) or a copy of the one in use (`true`).
    ControllerProfileCreate(bool),
    /// Gives the profile in use the typed name.
    ControllerProfileRename,
    /// Deletes the profile in use, once asked twice.
    ControllerProfileDelete,
    /// The answer to a change of the profiles: the state it leaves, or why it was refused.
    /// The flag says whether the typed name was used up.
    ControllerProfileChanged(bool, Result<ControllerState, String>),
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
    /// The "Columns" button over the track list: open or close the menu of columns.
    ColumnsMenuToggle,
    /// A column was ticked (shown) or unticked (hidden) in that menu.
    ColumnShown(Column, bool),

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

    /// The folder menu of the playlist tree (right click on a folder of a folder of music):
    /// opened on a folder at a place in the window, closed, and "Import as playlist…".
    FolderMenuOpen {
        playlist: usize,
        at: iced::Point,
    },
    FolderMenuClose,
    FolderMenuImport,
    /// The import window: a folder was chosen to add a library by (`None`: the dialog was
    /// closed), the library was chosen, the playlist's title was typed, a tick was set, the
    /// buttons, and the end of the work.
    ImportAdd,
    ImportAdded(Option<PathBuf>),
    ImportTarget(PathBuf),
    ImportTitle(String),
    ImportOption(import_view::Setting, bool),
    /// How the files are put in order by name was chosen, and the preview of the order
    /// for that choice (or why there is none) arrived.
    ImportOrder(open_engine_api::v1::NameOrder),
    ImportPreview(open_engine_api::v1::NameOrder, Result<Vec<String>, String>),
    ImportStart,
    ImportFinished(Result<open_engine_api::v1::ImportFolderResponse, String>),
    ImportBack,
    /// "Open library" in the window that says it is done.
    ImportOpenLibrary,
    ImportClose,

    /// The button that opens the window for repairing a library, and what it does.
    RepairOpen,
    RepairAdd,
    RepairAdded(Option<PathBuf>),
    RepairTarget(PathBuf),
    RepairFinished {
        target: PathBuf,
        result: Result<open_engine_api::v1::RepairLibraryResponse, String>,
    },
    RepairBack,
    RepairClose,

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
    /// The track list's context menu (right click): opened on a row at a place in the window,
    /// closed, and what it offers. Deleting from the library takes a second click.
    TrackMenuOpen {
        row: usize,
        at: iced::Point,
    },
    TrackMenuClose,
    /// "Analyse track" (or "Analyse again") in the context menu.
    TrackMenuAnalyse,
    /// "Analyse the N unanalysed tracks of this list" in the context menu.
    TrackMenuAnalyseAll,
    /// The toast's button that leaves out the tracks of the queue that wait.
    AnalysisQueueStop,
    /// The analysis of a track (in the library at `library`) ended.
    AnalysisFinished {
        library: String,
        track: u32,
        title: String,
        result: Result<open_engine_api::v1::AnalyzeTrackResponse, String>,
    },
    /// The close button of the toast.
    ToastClose,
    /// Time to look whether a finished toast has been shown long enough.
    ToastTick,
    TrackMenuRemoveFromPlaylist,
    TrackMenuRemoveFromLibrary,
    TrackRemovedFromPlaylist(Result<backend::PlaylistsEdited, String>),
    TracksRemoved(Result<open_engine_api::v1::RemoveTracksResponse, String>),
    /// The button that removes the streaming songs: the first press asks to be pressed again.
    RemoveStreaming,
    RemoveStreamingCancel,
    StreamingRemoved(Result<open_engine_api::v1::RemoveStreamingTracksResponse, String>),
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
    /// The button that unmounts a drive: its block device and name.
    UnmountDrive(PathBuf, String),
    DriveUnmounted {
        name: String,
        result: Result<(), String>,
    },
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
    /// A header of the track list was clicked: sort the list by it.
    SortBy(SortKey),
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
    /// The zoomed waveform was dragged: the track is held (scratching begins), moved by so
    /// many seconds (negative: back), and let go of.
    ScratchStart,
    ScratchBy(f64),
    ScratchEnd,
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
    /// The desktop's media interface is up, or asked for something.
    Mpris(mpris::Event),
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
            drive_error: None,
            status: PlaybackStatus::default(),
            clock: Clock::new(Instant::now()),
            scratch: None,
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
            import: None,
            folder_menu: None,
            analysis_queue: None,
            after_open: None,
            repair: None,
            smart: None,
            playlist_dialog: None,
            origin_prefix: None,
            cue_editor: None,
            loop_editor: None,
            saving: None,
            save_error: None,
            reloading: false,
            streaming_confirm: false,
            track_menu: None,
            toast: None,
            reload_result: None,
            columns: ColumnStore::discover(),
            resize: None,
            columns_menu: false,
            controller: Controller::default(),
            mpris: None,
            mpris_told: None,
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
            // The jog wheel moves the waveform, which has to keep up with it.
            let interval = if self.scratched_by_controller() {
                SCRATCH_POLL_INTERVAL
            } else {
                POLL_INTERVAL
            };
            iced::time::every(interval).map(|_| Message::Tick)
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
        // A finished toast goes away after a while.
        let toast_clock = if matches!(self.toast, Some(toast::Toast::Done { .. })) {
            iced::time::every(toast::CHECK_EVERY).map(|_| Message::ToastTick)
        } else {
            Subscription::none()
        };
        // While a sync is writing, a library is opening or a track is analysed, ask how far it is.
        let progress = if self.reporting_progress() {
            iced::time::every(progress_view::POLL_INTERVAL).map(|_| Message::ProgressTick)
        } else {
            Subscription::none()
        };
        Subscription::batch([
            mpris::subscription().map(Message::Mpris),
            poll,
            resizing,
            progress,
            toast_clock,
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
        self.saving.is_some() || self.reloading || self.analysing()
    }

    /// Whether a track is being analysed, which writes to the working copy when it is done.
    fn analysing(&self) -> bool {
        self.toast.as_ref().is_some_and(toast::Toast::running)
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
            || matches!(
                &self.import,
                Some(import_view::Import {
                    stage: import_view::Stage::Importing { .. },
                    ..
                })
            )
    }

    /// Whether the backend is doing something it reports progress on: a sync writing, or a
    /// library being opened.
    fn reporting_progress(&self) -> bool {
        self.syncing() || self.analysing() || matches!(self.library, LibraryState::Opening(_))
    }

    /// Whether one of the windows that lie over the app is open. Only one is at a time.
    fn window_open(&self) -> bool {
        self.sync.is_some()
            || self.library_sync.is_some()
            || self.import.is_some()
            || self.repair.is_some()
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
        self.streaming_confirm = false;
        self.track_menu = None;
        self.folder_menu = None;
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
        let position = self.status.queue_position? as usize;
        // The list may have been sorted since the queue was made from it: the playing track
        // is where it is in the list now.
        let Some(id) = self.status.track_id else {
            return Some(position);
        };
        let list = browser.tracks();
        let is_playing = |row: usize| {
            list.get(row)
                .is_some_and(|&track| browser.library.tracks[track].id == id)
        };
        if is_playing(position) {
            Some(position)
        } else {
            (0..list.len()).find(|&row| is_playing(row))
        }
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

    /// The DJ controller's jog wheel is scratching the track.
    fn scratched_by_controller(&self) -> bool {
        self.status.scratching && self.scratch.is_none()
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
    let status = matches!(message, Message::Status(Ok(_)));
    let task = handle(state, message);
    state.save_session(false);
    state.tell_desktop(status);
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
                | Message::RemoveStreaming
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
        | Message::ColumnShown(..)
        | Message::ColumnsMenuToggle
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
        | Message::ControllerProfileChanged(..)
        | Message::ControllerProfileCreate(..)
        | Message::ControllerProfileDelete
        | Message::ControllerProfileNameChanged(..)
        | Message::ControllerProfileRename
        | Message::ControllerProfileSelected(..)
        | Message::ControllerState(..) => handlers::controller::handle(state, message),
        Message::OriginPrefixApplied(..)
        | Message::OriginPrefixApply
        | Message::OriginPrefixBrowse
        | Message::OriginPrefixCancel
        | Message::OriginPrefixDialog(..)
        | Message::OriginPrefixLoaded(..)
        | Message::OriginPrefixOpen
        | Message::OriginPrefixPicked(..) => handlers::origin_prefix::handle(state, message),
        Message::FolderMenuClose
        | Message::FolderMenuImport
        | Message::FolderMenuOpen { .. }
        | Message::ImportAdd
        | Message::ImportAdded(..)
        | Message::ImportBack
        | Message::ImportClose
        | Message::ImportFinished(..)
        | Message::ImportOpenLibrary
        | Message::ImportOption(..)
        | Message::ImportOrder(..)
        | Message::ImportPreview(..)
        | Message::ImportStart
        | Message::ImportTarget(..)
        | Message::ImportTitle(..) => handlers::import::handle(state, message),
        Message::TrackMenuClose
        | Message::TrackMenuOpen { .. }
        | Message::TrackMenuAnalyse
        | Message::TrackMenuAnalyseAll
        | Message::AnalysisQueueStop
        | Message::AnalysisFinished { .. }
        | Message::ToastClose
        | Message::ToastTick
        | Message::TrackMenuRemoveFromLibrary
        | Message::TrackMenuRemoveFromPlaylist
        | Message::TrackRemovedFromPlaylist(..)
        | Message::TracksRemoved(..) => handlers::track_menu::handle(state, message),
        Message::MigrateOrigins
        | Message::OriginsMigrated(..)
        | Message::RemoveStreaming
        | Message::RemoveStreamingCancel
        | Message::StreamingRemoved(..)
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
        Message::RepairAdd
        | Message::RepairAdded(..)
        | Message::RepairBack
        | Message::RepairClose
        | Message::RepairFinished { .. }
        | Message::RepairOpen
        | Message::RepairTarget(..) => handlers::repair::handle(state, message),
        Message::AddLocalFolder
        | Message::ChooseLibrary
        | Message::CloseLibrary
        | Message::DriveUnmounted { .. }
        | Message::DrivesFound(..)
        | Message::ForgetRecent(..)
        | Message::LibraryChosen(..)
        | Message::LibraryClosed(..)
        | Message::LibraryOpened(..)
        | Message::LibrarySaved(..)
        | Message::LocalFolderChosen(..)
        | Message::MenuSelected(..)
        | Message::OpenDrive(..)
        | Message::UnmountDrive(..)
        | Message::OpenLocalFolder(..)
        | Message::OpenRecent(..)
        | Message::Reloaded(..)
        | Message::RemoveLocalFolder(..)
        | Message::SaveAsChosen(..)
        | Message::ScanDrives => handlers::library::handle(state, message),
        Message::Mpris(..) => handlers::mpris::handle(state, message),
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
        | Message::SortBy(..)
        | Message::ShowCollection
        | Message::ShowPlaylist(..)
        | Message::ScratchBy(..)
        | Message::ScratchEnd
        | Message::ScratchStart
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
    let content = view_page(state);
    match &state.toast {
        Some(toast) => iced::widget::stack![
            content,
            toast::layer(
                toast,
                state.progress.as_ref(),
                state.config.effective().language
            )
        ]
        .into(),
        None => content,
    }
}

fn view_page(state: &State) -> Element<'_, Message> {
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
    with_windows(state, window, language)
}

/// The page with the window that is open over it, if one is. The page stays the first of a
/// stack whether a window is open or not, so that it keeps its state (how far the track
/// list is scrolled) when one opens: a different widget in its place would be built anew,
/// and the scrollable would tell that it has "scrolled", which closes the track menu.
fn with_windows<'a>(
    state: &'a State,
    window: Element<'a, Message>,
    language: Language,
) -> Element<'a, Message> {
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
    if let Some(import) = &state.import {
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
            import_view::modal(import, sources, state.progress.as_ref(), language)
        ]
        .into();
    }
    if let Some(stage) = &state.repair {
        let sources = repair_view::Sources {
            recents: &state.recents,
            drives: &state.drives,
        };
        return iced::widget::stack![window, repair_view::modal(stage, sources, language)].into();
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
    if let (Some(menu), LibraryState::Open(_)) = (&state.track_menu, &state.library) {
        return iced::widget::stack![window, track_menu::layer(menu, state.analysing(), language)]
            .into();
    }
    if let (Some(menu), LibraryState::Open(_)) = (&state.folder_menu, &state.library) {
        return iced::widget::stack![
            window,
            folder_menu::layer(menu, !state.locked() && !state.syncing(), language)
        ]
        .into();
    }
    match (&state.playlist_dialog, &state.library) {
        (Some(dialog), LibraryState::Open(browser)) => iced::widget::stack![
            window,
            playlist_dialog::modal(dialog, &browser.library, language)
        ]
        .into(),
        _ => iced::widget::stack![window].into(),
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
                drive_error: state.drive_error.as_deref(),
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
        // Takes the streaming songs out (they have no file here); asks twice.
        let streaming = library
            .tracks
            .iter()
            .filter(|track| track.streaming)
            .count();
        if streaming > 0 {
            let armed = state.streaming_confirm;
            header = header.push(
                tooltip(
                    button(text(fill(
                        if armed {
                            &strings.streaming_remove_confirm
                        } else {
                            &strings.streaming_remove
                        },
                        &[("count", &streaming)],
                    )))
                    .style(if armed {
                        button::danger
                    } else {
                        button::secondary
                    })
                    .on_press_maybe((!state.locked()).then_some(Message::RemoveStreaming)),
                    text(&strings.streaming_remove_tip).size(13),
                    tooltip::Position::Bottom,
                )
                .style(container::rounded_box),
            );
            if armed {
                header = header.push(
                    button(text(&strings.streaming_remove_cancel))
                        .on_press(Message::RemoveStreamingCancel),
                );
            }
        }
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
            // Trailing the player by the set delay, to line up with what is heard (a scratch
            // is heard as it is moved).
            if state.scratch.is_some() || state.status.scratching {
                state.clock
            } else {
                state
                    .clock
                    .delayed(f64::from(state.config.effective().waveform_delay_ms) / 1000.0)
            },
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
            browser::Columns {
                widths: &state.columns.widths,
                hidden: &state.columns.hidden,
                picking: state.columns_menu,
            },
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
            analysis: 0,
            analysis_origin: model::AnalysisOrigin::None,
            analysis_note: None,
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

    /// Runs the events through the UI of the open library, without a window, and gives the
    /// messages they make.
    fn press(
        state: &State,
        cache: iced_runtime::user_interface::Cache,
        events: &[iced::Event],
        at: iced::Point,
    ) -> (Vec<Message>, iced_runtime::user_interface::Cache) {
        let LibraryState::Open(browser) = &state.library else {
            panic!("a library is open");
        };
        press_on(
            with_windows(
                state,
                open_library(state, browser, Language::English),
                Language::English,
            ),
            cache,
            events,
            at,
        )
    }

    /// The same for any element.
    fn press_on<'a>(
        element: Element<'a, Message>,
        cache: iced_runtime::user_interface::Cache,
        events: &[iced::Event],
        at: iced::Point,
    ) -> (Vec<Message>, iced_runtime::user_interface::Cache) {
        use iced::advanced::renderer::Headless;
        let mut renderer = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
            .block_on(<iced::Renderer as Headless>::new(
                iced::Font::default(),
                iced::Pixels(16.0),
                Some("tiny-skia"),
            ))
            .expect("a renderer that needs no graphics card");
        let mut interface = iced_runtime::user_interface::UserInterface::build(
            element,
            iced::Size::new(1400.0, 700.0),
            cache,
            &mut renderer,
        );
        let mut messages = Vec::new();
        interface.update(
            events,
            iced::mouse::Cursor::Available(at),
            &mut renderer,
            &mut iced::advanced::clipboard::Null,
            &mut messages,
        );
        (messages, interface.into_cache())
    }

    /// An app whose track list is long enough to scroll.
    fn state_with_a_long_list() -> State {
        let mut state = state();
        state.library = LibraryState::Open(Browser::new(Arc::new(Library::new(
            "/media/DRIVE/Engine Library".into(),
            "/cache".into(),
            (0..3000)
                .map(|i| Track {
                    path: format!("../Music/{i}.flac"),
                    ..track(i % 3)
                })
                .collect(),
            vec![playlist("House", 0, true, &[])],
        ))));
        state
    }

    #[test]
    fn the_context_menu_stays_open_in_a_list_with_a_scrollbar() {
        use iced::mouse::{Button, Event};
        let mut state = state_with_a_long_list();
        let at = iced::Point::new(600.0, 300.0);
        let moved = iced::Event::Mouse(Event::CursorMoved { position: at });
        let right = iced::Event::Mouse(Event::ButtonPressed(Button::Right));
        let redraw = || iced::Event::Window(iced::window::Event::RedrawRequested(Instant::now()));

        let (messages, mut cache) = press(&state, Default::default(), &[redraw()], at);
        for message in messages {
            let _ = handle(&mut state, message);
        }
        let (messages, next) = press(&state, cache, &[moved, right], at);
        cache = next;
        for message in messages {
            let _ = handle(&mut state, message);
        }
        assert!(state.track_menu.is_some(), "a right click opens the menu");

        // The menu is on screen now, and the window is drawn again.
        for _ in 0..3 {
            let (messages, next) = press(&state, cache, &[redraw()], at);
            cache = next;
            for message in messages {
                let _ = handle(&mut state, message);
            }
        }
        assert!(
            state.track_menu.is_some(),
            "drawing the list again is not scrolling it"
        );
    }

    #[test]
    fn the_context_menu_opens_where_the_right_click_was_even_in_a_scrolled_list() {
        use iced::mouse::{Button, Event, ScrollDelta};
        let mut state = state_with_a_long_list();
        let at = iced::Point::new(600.0, 300.0);
        let moved = iced::Event::Mouse(Event::CursorMoved { position: at });
        let mut cache = iced_runtime::user_interface::Cache::default();

        // Scrolled down by ten screens.
        for _ in 0..5 {
            let wheel = iced::Event::Mouse(Event::WheelScrolled {
                delta: ScrollDelta::Pixels { x: 0.0, y: -600.0 },
            });
            let (messages, next) = press(&state, cache, &[moved.clone(), wheel], at);
            cache = next;
            for message in messages {
                let _ = handle(&mut state, message);
            }
        }

        let right = iced::Event::Mouse(Event::ButtonPressed(Button::Right));
        let (messages, _) = press(&state, cache, &[moved, right], at);
        let opened: Vec<_> = messages
            .into_iter()
            .filter_map(|message| match message {
                Message::TrackMenuOpen { row, at } => Some((row, at)),
                _ => None,
            })
            .collect();
        assert_eq!(opened.len(), 1, "a right click on a track opens its menu");
        let (row, opened_at) = opened[0];
        assert!(row > 0, "it is a track far down the list: {row}");
        assert_eq!(
            opened_at, at,
            "the menu opens at the pointer, not further down"
        );
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
            drive_error: None,
            status: PlaybackStatus::default(),
            clock: Clock::new(Instant::now()),
            scratch: None,
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
            import: None,
            folder_menu: None,
            analysis_queue: None,
            after_open: None,
            repair: None,
            smart: None,
            playlist_dialog: None,
            origin_prefix: None,
            cue_editor: None,
            loop_editor: None,
            saving: None,
            save_error: None,
            reloading: false,
            streaming_confirm: false,
            track_menu: None,
            toast: None,
            reload_result: None,
            columns: ColumnStore::load(temp.join("track-columns.toml")),
            resize: None,
            columns_menu: false,
            controller: Controller::default(),
            mpris: None,
            mpris_told: None,
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

    fn sort_by(state: &mut State, key: SortKey) {
        let _ = handle(state, Message::SortBy(key));
    }

    #[test]
    fn a_header_click_sorts_the_list_then_reverses_it_then_puts_it_back() {
        let mut state = state();
        let title = SortKey::Column(Column::Title);
        // Deep holds b and c, in that order.
        let _ = handle(&mut state, Message::ShowPlaylist(1));
        assert_eq!(browser(&state).tracks(), [1, 2]);
        assert!(browser(&state).numbered());

        sort_by(&mut state, title);
        assert_eq!(browser(&state).tracks(), [1, 2]);
        sort_by(&mut state, title);
        assert_eq!(browser(&state).tracks(), [2, 1], "descending");
        assert_eq!(
            browser(&state).position(0),
            1,
            "c is second in the playlist"
        );
        sort_by(&mut state, title);
        assert_eq!(browser(&state).sort(), None);
        assert_eq!(browser(&state).tracks(), [1, 2]);
    }

    #[test]
    fn sorting_keeps_the_selected_track_and_the_playing_marker_on_their_track() {
        let mut state = state();
        let _ = handle(&mut state, Message::ShowPlaylist(1));
        let _ = handle(&mut state, Message::SelectTrack(0));
        state.play_source = Some(Source::Playlist(1));
        state.status = PlaybackStatus {
            state: PlaybackState::Playing.into(),
            queue_position: Some(0),
            track_id: Some(1),
            position_ms: 0,
            error: None,
            scratching: false,
        };

        let title = SortKey::Column(Column::Title);
        sort_by(&mut state, title);
        sort_by(&mut state, title);
        let browser = browser(&state);
        assert_eq!(browser.tracks(), [2, 1]);
        assert_eq!(browser.selected, Some(1), "b moved down");
        assert_eq!(state.playing_row(browser), Some(1));
    }

    #[test]
    fn what_is_missing_sorts_last_whichever_way_round() {
        let mut state = state();
        if let LibraryState::Open(browser) = &mut state.library {
            let mut library = library_at("/media/DRIVE/Engine Library");
            library.tracks[1].bpm = Some(120.0);
            library.tracks[2].bpm = Some(90.0);
            *browser = Browser::new(Arc::new(library));
        }
        let bpm = SortKey::Column(Column::Bpm);
        sort_by(&mut state, bpm);
        assert_eq!(browser(&state).tracks(), [2, 1, 0]);
        sort_by(&mut state, bpm);
        assert_eq!(browser(&state).tracks(), [1, 2, 0]);
    }

    #[test]
    fn only_ordinary_playlists_are_numbered() {
        let mut state = state();
        assert!(!browser(&state).numbered(), "all tracks");
        let _ = handle(&mut state, Message::ShowPlaylist(2));
        assert!(browser(&state).numbered());
    }

    #[test]
    fn the_number_header_puts_a_playlist_back_to_front() {
        let mut state = state();
        let _ = handle(&mut state, Message::ShowPlaylist(1));
        sort_by(&mut state, SortKey::Number);
        assert_eq!(browser(&state).tracks(), [2, 1]);
        sort_by(&mut state, SortKey::Number);
        assert_eq!(browser(&state).tracks(), [1, 2]);
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
            scratching: false,
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
            scratching: false,
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

    #[test]
    fn columns_are_hidden_and_shown_from_the_columns_menu_and_remembered() {
        let mut state = state();
        assert!(!state.columns_menu);
        let _ = handle(&mut state, Message::ColumnsMenuToggle);
        assert!(state.columns_menu);

        let _ = handle(&mut state, Message::ColumnShown(Column::Genre, false));
        let _ = handle(&mut state, Message::ColumnShown(Column::Analysis, false));
        let _ = handle(&mut state, Message::ColumnShown(Column::Genre, true));
        assert!(!state.columns.hidden.contains(Column::Genre));
        assert!(state.columns.hidden.contains(Column::Analysis));
        assert!(state.columns_menu, "ticking a column leaves the menu open");

        let temp = temp_dir();
        let saved = ColumnStore::load(temp.join("track-columns.toml"));
        assert!(saved.hidden.contains(Column::Analysis));
        assert!(!saved.hidden.contains(Column::Genre));
        let _ = std::fs::remove_dir_all(temp);

        let _ = handle(&mut state, Message::ColumnsMenuToggle);
        assert!(!state.columns_menu);
    }

    #[test]
    fn the_analysis_of_a_track_is_counted_without_cues_and_loops() {
        let mut track = track(0);
        assert_eq!(track.analysis_count(), 0);
        track.analysis = model::ANALYSIS_TRACK_DATA | model::ANALYSIS_BEATGRID;
        assert_eq!(track.analysis_count(), 2);
        track.analysis |= model::ANALYSIS_WAVEFORM;
        assert_eq!(track.analysis_count(), model::ANALYSIS_KINDS);
        // (Bits the UI doesn't know are a newer backend's: they don't count.)
        track.analysis = 8;
        assert_eq!(track.analysis_count(), 0);
    }

    fn analysis_response(bpm: f64, key: Option<&str>) -> open_engine_api::v1::AnalyzeTrackResponse {
        open_engine_api::v1::AnalyzeTrackResponse {
            analysis: model::ANALYSIS_TRACK_DATA | model::ANALYSIS_BEATGRID,
            analysis_origin: open_engine_api::v1::AnalysisOrigin::OpenEngineLib.into(),
            analysis_note: Some("a tool, today".into()),
            bpm: Some(bpm),
            key: key.map(str::to_owned),
            beats: 400,
        }
    }

    fn analysing(state: &mut State) {
        state.toast = Some(toast::Toast::Analysing {
            library: "/media/DRIVE/Engine Library".into(),
            title: "Track 1".into(),
            position: None,
            stoppable: false,
        });
    }

    #[test]
    fn the_context_menu_offers_analysing_and_says_whether_it_replaces_an_analysis() {
        let mut state = state();
        let at = iced::Point::new(10.0, 10.0);
        let _ = handle(&mut state, Message::TrackMenuOpen { row: 1, at });
        assert_eq!(
            (menu(&state).can_analyse, menu(&state).analysed),
            (true, false)
        );

        // Once the track has an analysis, analysing it again replaces it.
        let _ = handle(&mut state, Message::TrackMenuClose);
        if let LibraryState::Open(browser) = &mut state.library {
            browser.set_analysed(
                1,
                browser::Analysed {
                    bits: model::ANALYSIS_BEATGRID,
                    origin: model::AnalysisOrigin::Engine,
                    note: None,
                    bpm: None,
                },
            );
        }
        let _ = handle(&mut state, Message::TrackMenuOpen { row: 1, at });
        assert_eq!(
            (menu(&state).can_analyse, menu(&state).analysed),
            (true, true)
        );
    }

    #[test]
    fn while_a_track_is_analysed_the_menu_opens_but_changes_nothing() {
        let mut state = state();
        analysing(&mut state);
        let at = iced::Point::new(10.0, 10.0);
        let _ = handle(&mut state, Message::TrackMenuOpen { row: 1, at });
        assert!(state.track_menu.is_some(), "it can be looked at");

        // Taking a track out of the library waits (it would not even ask for the second click).
        let _ = handle(&mut state, Message::TrackMenuRemoveFromLibrary);
        assert!(!menu(&state).confirm_library);
        // And a second analysis does not start.
        let _ = handle(&mut state, Message::TrackMenuAnalyse);
        assert!(state.track_menu.is_none() || state.analysing());
        assert!(matches!(
            state.toast,
            Some(toast::Toast::Analysing { ref title, .. }) if title == "Track 1"
        ));
    }

    #[test]
    fn a_track_that_was_analysed_shows_its_new_analysis_tempo_and_origin() {
        let mut state = state();
        analysing(&mut state);
        assert!(
            state.analysing() && state.locked(),
            "nothing else writes meanwhile"
        );
        assert!(state.reporting_progress(), "the progress is asked for");

        let _ = handle(
            &mut state,
            Message::AnalysisFinished {
                library: "/media/DRIVE/Engine Library".into(),
                track: 1,
                title: "Track 1".into(),
                result: Ok(analysis_response(126.04, Some("A♭ minor"))),
            },
        );
        assert!(!state.analysing() && !state.locked());
        match &state.toast {
            Some(toast::Toast::Done { message, .. }) => {
                assert!(
                    message.contains("Track 1")
                        && message.contains("126.0")
                        && message.contains("A♭ minor"),
                    "{message}"
                );
            }
            other => panic!("{other:?}"),
        }
        let browser = browser(&state);
        let track = browser.library.track_by_id(1).unwrap();
        assert_eq!(browser.analysis(track), 5);
        assert_eq!(
            browser.analysis_origin(track),
            model::AnalysisOrigin::OpenEngineLib
        );
        assert_eq!(browser.analysis_note(track), Some("a tool, today"));
        assert_eq!(browser.bpm(track), Some(126.04));
        // The other tracks are as they were.
        let other = browser.library.track_by_id(0).unwrap();
        assert_eq!(browser.analysis(other), 0);
        assert_eq!(browser.analysis_origin(other), model::AnalysisOrigin::None);

        // A key that was not heard is said.
        analysing(&mut state);
        let _ = handle(
            &mut state,
            Message::AnalysisFinished {
                library: "/media/DRIVE/Engine Library".into(),
                track: 1,
                title: "Track 1".into(),
                result: Ok(analysis_response(90.0, None)),
            },
        );
        match &state.toast {
            Some(toast::Toast::Done { message, .. }) => assert!(message.contains("no key")),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_failed_analysis_is_shown_until_it_is_closed_and_a_late_answer_is_dropped() {
        let mut state = state();
        analysing(&mut state);
        // A running toast can't be closed.
        let _ = handle(&mut state, Message::ToastClose);
        assert!(state.analysing());

        // The answer for another library than the one open is not for this toast.
        let _ = handle(
            &mut state,
            Message::AnalysisFinished {
                library: "/elsewhere".into(),
                track: 1,
                title: "Track 1".into(),
                result: Ok(analysis_response(100.0, None)),
            },
        );
        assert!(state.analysing());
        assert_eq!(
            browser(&state).bpm(browser(&state).library.track_by_id(1).unwrap()),
            None
        );

        let _ = handle(
            &mut state,
            Message::AnalysisFinished {
                library: "/media/DRIVE/Engine Library".into(),
                track: 1,
                title: "Track 1".into(),
                result: Err("the models were not found".into()),
            },
        );
        match &state.toast {
            Some(toast::Toast::Failed { message }) => {
                assert!(
                    message.contains("Track 1") && message.contains("models"),
                    "{message}"
                );
            }
            other => panic!("{other:?}"),
        }
        // The track keeps what it had, and the list can be used again.
        assert!(!state.locked());
        let browser = browser(&state);
        assert_eq!(browser.analysis(browser.library.track_by_id(1).unwrap()), 0);

        let _ = handle(&mut state, Message::ToastTick);
        assert!(state.toast.is_some(), "a failure stays");
        let _ = handle(&mut state, Message::ToastClose);
        assert!(state.toast.is_none());
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

    /// An app whose library has a streaming song (its second track).
    fn state_with_a_streaming_song() -> State {
        let mut state = state();
        let mut streaming = track(1);
        streaming.streaming = true;
        let library = Library::new(
            "/media/DRIVE/Engine Library".into(),
            "/cache".into(),
            vec![track(0), streaming, track(2)],
            vec![playlist("House", 0, false, &[0, 1, 2])],
        );
        state.library = LibraryState::Open(Browser::new(Arc::new(library)));
        state
    }

    #[test]
    fn the_streaming_songs_are_removed_on_the_second_press_and_the_library_is_read_again() {
        use open_engine_api::v1::RemoveStreamingTracksResponse;
        let mut state = state_with_a_streaming_song();

        // The first press only asks; cancelling takes it back.
        let _ = handle(&mut state, Message::RemoveStreaming);
        assert!(state.streaming_confirm && !state.reloading);
        let _ = handle(&mut state, Message::RemoveStreamingCancel);
        assert!(!state.streaming_confirm);

        // A library that is being read again is not pressed on.
        state.reloading = true;
        let _ = handle(&mut state, Message::RemoveStreaming);
        assert!(!state.streaming_confirm);

        // The answer is told, and the library is fetched again (there is no backend here to
        // do it).
        let _ = handle(
            &mut state,
            Message::StreamingRemoved(Ok(RemoveStreamingTracksResponse {
                removed: 1,
                playlist_entries: 2,
            })),
        );
        assert_eq!(
            state.reload_result,
            Some((
                "Removed 1 streaming songs and 2 playlist entries.".to_owned(),
                false
            ))
        );

        // Nothing removed (it was already gone) just unlocks, and a failure is shown.
        state.reloading = true;
        let _ = handle(
            &mut state,
            Message::StreamingRemoved(Ok(RemoveStreamingTracksResponse::default())),
        );
        assert!(!state.reloading);
        state.reloading = true;
        let _ = handle(
            &mut state,
            Message::StreamingRemoved(Err("could not write".into())),
        );
        assert!(!state.reloading);
        assert_eq!(state.backend_error.as_deref(), Some("could not write"));
    }

    #[test]
    fn there_is_nothing_to_ask_about_without_streaming_songs_or_in_a_folder_of_music() {
        let mut state = state();
        let _ = handle(&mut state, Message::RemoveStreaming);
        assert!(!state.streaming_confirm, "no streaming songs here");

        // (Nor in a folder of music, which has nothing to remove them from.)
        let mut folder = state_with_a_streaming_song();
        if let LibraryState::Open(browser) = &folder.library {
            let library = Library::new(
                "/home/me/Music".into(),
                String::new(),
                browser.library.tracks.clone(),
                Vec::new(),
            )
            .of_kind(model::Kind::Folder);
            folder.library = LibraryState::Open(Browser::new(Arc::new(library)));
        }
        assert!(
            browser(&folder)
                .library
                .tracks
                .iter()
                .any(|track| track.streaming),
            "the folder has a streaming song to be left alone"
        );
        let _ = handle(&mut folder, Message::RemoveStreaming);
        assert!(!folder.streaming_confirm);
    }

    fn menu(state: &State) -> &track_menu::Menu {
        state.track_menu.as_ref().expect("the menu is open")
    }

    #[test]
    fn right_clicking_a_row_opens_a_menu_for_its_track_with_what_the_list_allows() {
        let mut state = state();
        let at = iced::Point::new(120.0, 80.0);

        // In "All tracks" a track can only be deleted from the library.
        let _ = handle(&mut state, Message::TrackMenuOpen { row: 1, at });
        assert_eq!(
            menu(&state),
            &track_menu::Menu {
                row: 1,
                track: 1,
                title: "Track 1".into(),
                playlist: None,
                at,
                confirm_library: false,
                can_analyse: true,
                analysed: false,
                analyse_all: 3,
            }
        );
        assert_eq!(browser(&state).selected, Some(1), "the row is selected");

        // In a playlist, also from that playlist. (Deep, the second playlist, has tracks 1, 2.)
        let _ = handle(&mut state, Message::TrackMenuClose);
        assert!(state.track_menu.is_none());
        let _ = handle(&mut state, Message::ShowPlaylist(1));
        let _ = handle(&mut state, Message::TrackMenuOpen { row: 1, at });
        assert_eq!((menu(&state).playlist, menu(&state).track), (Some(1), 2));

        // A row that is not there, and a library that is being read again, open nothing.
        let _ = handle(&mut state, Message::TrackMenuClose);
        let _ = handle(&mut state, Message::TrackMenuOpen { row: 9, at });
        assert!(state.track_menu.is_none());
        state.reloading = true;
        let _ = handle(&mut state, Message::TrackMenuOpen { row: 0, at });
        assert!(state.track_menu.is_none());
    }

    #[test]
    fn a_smart_playlist_and_a_folder_of_music_have_nothing_to_take_a_track_out_of() {
        let at = iced::Point::ORIGIN;
        let mut state = state();
        let mut smart = playlist("Smart", 0, false, &[0, 1]);
        smart.is_smart = true;
        if let LibraryState::Open(browser) = &mut state.library {
            let library = browser.library.with_playlists(vec![smart]);
            *browser = Browser::new(Arc::new(library));
            browser.show(Source::Playlist(0));
        }
        let _ = handle(&mut state, Message::TrackMenuOpen { row: 0, at });
        assert_eq!(menu(&state).playlist, None);

        let mut folder = state_in_folder();
        let _ = handle(&mut folder, Message::TrackMenuOpen { row: 0, at });
        assert!(folder.track_menu.is_none(), "nothing to change in a folder");
    }

    #[test]
    fn deleting_from_the_library_takes_a_second_click_and_a_new_menu_starts_unasked() {
        let mut state = state();
        let at = iced::Point::ORIGIN;
        let _ = handle(&mut state, Message::TrackMenuOpen { row: 0, at });
        let _ = handle(&mut state, Message::TrackMenuRemoveFromLibrary);
        assert!(menu(&state).confirm_library, "the first click only asks");
        assert!(!state.reloading);

        // (There is no backend here to do the second one.) A new menu starts out unasked.
        let _ = handle(&mut state, Message::TrackMenuClose);
        let _ = handle(&mut state, Message::TrackMenuOpen { row: 0, at });
        assert!(!menu(&state).confirm_library);
    }

    #[test]
    fn a_track_taken_out_of_the_playlist_shown_is_gone_from_it_and_a_deleted_one_is_told() {
        use open_engine_api::v1::RemoveTracksResponse;
        let mut state = state();
        let _ = handle(&mut state, Message::ShowPlaylist(1));
        let _ = handle(&mut state, Message::SelectTrack(0));
        assert_eq!(browser(&state).tracks(), [1, 2]);

        let _ = handle(
            &mut state,
            Message::TrackRemovedFromPlaylist(Ok(backend::PlaylistsEdited {
                playlists: vec![
                    playlist("House", 0, true, &[]),
                    playlist("Deep", 1, false, &[2]),
                    playlist("Pop", 0, false, &[0]),
                ],
                playlist: None,
            })),
        );
        assert_eq!(browser(&state).source, Source::Playlist(1));
        assert_eq!(browser(&state).tracks(), [2]);
        assert_eq!(browser(&state).selected, None);
        assert_eq!(
            browser(&state).library.tracks.len(),
            3,
            "still in the library"
        );

        let _ = handle(
            &mut state,
            Message::TrackRemovedFromPlaylist(Err("not there".into())),
        );
        assert_eq!(state.backend_error.as_deref(), Some("not there"));

        // A deletion from the library is told, and the library is fetched again.
        state.reloading = true;
        let _ = handle(
            &mut state,
            Message::TracksRemoved(Ok(RemoveTracksResponse {
                removed: 1,
                playlist_entries: 3,
            })),
        );
        assert_eq!(
            state.reload_result,
            Some((
                "Deleted the track from the library, and 3 playlist entries.".to_owned(),
                false
            ))
        );
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
    fn a_drive_that_cannot_be_unmounted_says_why_until_the_next_try() {
        let mut state = state();
        let _ = handle(
            &mut state,
            Message::DriveUnmounted {
                name: "STICK".into(),
                result: Err("target is busy".into()),
            },
        );
        let shown = state.drive_error.clone().unwrap();
        assert!(
            shown.contains("STICK") && shown.contains("target is busy"),
            "{shown}"
        );

        // Trying again clears it; a drive that went is looked for at once.
        let _ = handle(
            &mut state,
            Message::UnmountDrive("/dev/sdc1".into(), "STICK".into()),
        );
        assert_eq!(state.drive_error, None);
        state.scanning_drives = false;
        let _ = handle(
            &mut state,
            Message::DriveUnmounted {
                name: "STICK".into(),
                result: Ok(()),
            },
        );
        assert!(state.scanning_drives);
        assert_eq!(state.drive_error, None);
    }

    #[test]
    fn a_library_is_repaired_in_steps() {
        let mut state = state();
        let target = PathBuf::from("/media/STICK/Engine Library");
        let done = |moved| open_engine_api::v1::RepairLibraryResponse {
            target_path: target.display().to_string(),
            origins_moved: moved,
            ..Default::default()
        };

        // The window opens on the choice, and no second window opens over it.
        let _ = handle(&mut state, Message::RepairOpen);
        assert!(matches!(state.repair, Some(RepairStage::Pick)));
        let _ = handle(&mut state, Message::LibrarySyncOpen);
        assert!(state.library_sync.is_none());

        // An answer nobody waits for is not wanted.
        let _ = handle(
            &mut state,
            Message::RepairFinished {
                target: target.clone(),
                result: Ok(done(true)),
            },
        );
        assert!(matches!(state.repair, Some(RepairStage::Pick)));

        // While it works the window can't be closed or sent back; the answer for another
        // library is not its answer.
        state.repair = Some(RepairStage::Working {
            target: target.clone(),
        });
        let _ = handle(&mut state, Message::RepairClose);
        let _ = handle(&mut state, Message::RepairBack);
        assert!(matches!(state.repair, Some(RepairStage::Working { .. })));
        let _ = handle(
            &mut state,
            Message::RepairFinished {
                target: "/elsewhere".into(),
                result: Ok(done(true)),
            },
        );
        assert!(matches!(state.repair, Some(RepairStage::Working { .. })));

        // Its answer is shown, a failure with the reason, and the window goes back or closes.
        let _ = handle(
            &mut state,
            Message::RepairFinished {
                target: target.clone(),
                result: Ok(done(true)),
            },
        );
        assert!(matches!(&state.repair, Some(RepairStage::Done(d)) if d.origins_moved));
        let _ = handle(&mut state, Message::RepairBack);
        assert!(matches!(state.repair, Some(RepairStage::Pick)));
        state.repair = Some(RepairStage::Working {
            target: target.clone(),
        });
        let _ = handle(
            &mut state,
            Message::RepairFinished {
                target: target.clone(),
                result: Err("denied".into()),
            },
        );
        assert!(
            matches!(&state.repair, Some(RepairStage::Failed { error, .. }) if error == "denied")
        );
        let _ = handle(&mut state, Message::RepairClose);
        assert!(state.repair.is_none());
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
                    remove_missing: false,
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
                device: None,
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
            scratching: false,
        };
        assert!(state.cues.claim(1));
        let generation = state.cues.generation();
        let _ = handle(&mut state, Message::CuesLoaded(generation, 1, Ok(cues)));
        state
    }

    fn status_at(seconds: u64, state: PlaybackState, scratching: bool) -> PlaybackStatus {
        PlaybackStatus {
            state: state.into(),
            queue_position: Some(0),
            track_id: Some(1),
            position_ms: seconds * 1000,
            error: None,
            scratching,
        }
    }

    #[test]
    fn dragging_the_waveform_moves_the_playhead_by_hand_and_the_backends_reports_wait() {
        let mut state = state_playing_with_cues(vec![]);
        assert!(state.waveforms.claim(1));
        let generation = state.waveforms.generation();
        state.waveforms.set(
            generation,
            1,
            Some(Arc::new(Waveform::new(100.0, 100.0, vec![], None))),
        );
        let _ = handle(
            &mut state,
            Message::Status(Ok(status_at(10, PlaybackState::Paused, false))),
        );
        let at = |state: &State| state.clock.position(Instant::now(), 100.0);
        assert!((at(&state) - 10.0).abs() < 1e-6);

        let _ = handle(&mut state, Message::ScratchStart);
        assert_eq!(state.scratch, Some(10.0));
        let _ = handle(&mut state, Message::ScratchBy(2.5));
        assert_eq!(state.scratch, Some(12.5));
        assert!((at(&state) - 12.5).abs() < 1e-6);

        // A report that left before the last moves is not believed over the hand.
        let _ = handle(
            &mut state,
            Message::Status(Ok(status_at(10, PlaybackState::Paused, true))),
        );
        assert!((at(&state) - 12.5).abs() < 1e-6);
        assert!(state.status.scratching);

        // The track has its ends.
        let _ = handle(&mut state, Message::ScratchBy(-50.0));
        assert_eq!(state.scratch, Some(0.0));
        let _ = handle(&mut state, Message::ScratchBy(500.0));
        assert_eq!(state.scratch, Some(100.0));

        // Let go: the backend says where the track went on from, and that is believed.
        let _ = handle(&mut state, Message::ScratchEnd);
        assert_eq!(state.scratch, None);
        let _ = handle(
            &mut state,
            Message::Status(Ok(status_at(40, PlaybackState::Playing, false))),
        );
        assert!((at(&state) - 40.0).abs() < 1e-6);
        assert!(state.clock.is_running());
    }

    #[test]
    fn a_drag_without_a_track_or_its_waveform_scratches_nothing() {
        // (No waveform has been loaded: the track's length is not known.)
        let mut state = state_playing_with_cues(vec![]);
        let _ = handle(&mut state, Message::ScratchStart);
        assert_eq!(state.scratch, None);
        let _ = handle(&mut state, Message::ScratchBy(1.0));
        let _ = handle(&mut state, Message::ScratchEnd);
        assert_eq!(state.scratch, None);

        let mut stopped = super::tests::state();
        let _ = handle(&mut stopped, Message::ScratchStart);
        assert_eq!(stopped.scratch, None);
    }

    #[test]
    fn only_the_controllers_scratch_speeds_up_the_polling() {
        let mut state = state_playing_with_cues(vec![]);
        state.status = status_at(10, PlaybackState::Paused, true);
        assert!(state.scratched_by_controller());
        // The mouse's scratch is followed locally.
        state.scratch = Some(10.0);
        assert!(!state.scratched_by_controller());
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
    fn controller_profiles_are_deleted_on_the_second_press_and_refusals_are_shown() {
        use open_engine_api::v1::ControllerProfile;

        let mut state = state_playing_with_cues(vec![]);
        let report = |active: &str| ControllerState {
            profiles: vec![
                ControllerProfile {
                    name: "SC LIVE 4".into(),
                    built_in: true,
                },
                ControllerProfile {
                    name: "Booth".into(),
                    built_in: false,
                },
            ],
            active_profile: active.into(),
            ..ControllerState::default()
        };
        let _ = handle(&mut state, Message::ControllerState(Ok(report("Booth"))));

        // The first press on Delete only asks (there is no backend here to answer anyway).
        let _ = handle(&mut state, Message::ControllerProfileDelete);
        assert_eq!(state.controller.confirm_delete.as_deref(), Some("Booth"));
        // Choosing a profile, or leaving the page, takes the question back.
        let _ = handle(
            &mut state,
            Message::ControllerProfileSelected("SC LIVE 4".into()),
        );
        assert_eq!(state.controller.confirm_delete, None);
        let _ = handle(&mut state, Message::ControllerProfileDelete);
        let _ = handle(&mut state, Message::Navigate(Page::Library));
        assert_eq!(state.controller.confirm_delete, None);

        // Nothing is made without a name.
        let _ = handle(&mut state, Message::ControllerProfileCreate(false));
        let _ = handle(&mut state, Message::ControllerProfileRename);
        assert_eq!(state.controller.profile_error, None);

        // A refusal is shown as one, not as the controller being out of reach, and the typed
        // name stays to be corrected.
        let _ = handle(
            &mut state,
            Message::ControllerProfileNameChanged("Booth".into()),
        );
        let _ = handle(
            &mut state,
            Message::ControllerProfileChanged(true, Err("another profile has that name".into())),
        );
        assert_eq!(
            state.controller.profile_error.as_deref(),
            Some("another profile has that name")
        );
        assert_eq!(state.controller.problem(), None);
        assert_eq!(state.controller.name_draft, "Booth");

        // A change that was made takes the state, the error and, if it used it, the name.
        let _ = handle(
            &mut state,
            Message::ControllerProfileChanged(false, Ok(report("Booth"))),
        );
        assert_eq!(state.controller.profile_error, None);
        assert_eq!(state.controller.name_draft, "Booth");
        let _ = handle(
            &mut state,
            Message::ControllerProfileChanged(true, Ok(report("Booth"))),
        );
        assert_eq!(state.controller.name_draft, "");
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
            scratching: false,
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
            scratching: false,
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

    /// An app with a folder of music open (see `library_at`): the folders House (no tracks
    /// of its own), Deep (b, c) and Pop (a).
    fn state_with_a_folder() -> State {
        let mut state = state();
        state.library = LibraryState::Open(Browser::new(Arc::new(
            library_at("/home/me/Music").of_kind(model::Kind::Folder),
        )));
        state
    }

    #[test]
    fn right_clicking_a_folder_of_a_folder_of_music_opens_its_menu() {
        let folder = state_with_a_folder();
        let LibraryState::Open(browser) = &folder.library else {
            unreachable!()
        };
        // "Pop" is the third line of the tree, under "All tracks" and "House" (which is closed,
        // so "Deep" in it is not shown).
        let at = iced::Point::new(100.0, 90.0);
        let events = [
            iced::Event::Mouse(iced::mouse::Event::CursorMoved { position: at }),
            iced::Event::Mouse(iced::mouse::Event::ButtonPressed(
                iced::mouse::Button::Right,
            )),
        ];
        let (messages, _) = press_on(
            browser::playlists(browser, Language::English),
            Default::default(),
            &events,
            at,
        );
        let opened: Vec<_> = messages
            .iter()
            .filter_map(|message| match message {
                Message::FolderMenuOpen { playlist, .. } => Some(*playlist),
                _ => None,
            })
            .collect();
        assert_eq!(opened, [2], "the folder under the pointer");

        // In an Engine library the tree has no such menu.
        let engine = state();
        let LibraryState::Open(browser) = &engine.library else {
            unreachable!()
        };
        let (messages, _) = press_on(
            browser::playlists(browser, Language::English),
            Default::default(),
            &events,
            at,
        );
        assert!(
            !messages
                .iter()
                .any(|message| matches!(message, Message::FolderMenuOpen { .. }))
        );
    }

    #[test]
    fn the_folder_menu_opens_only_for_a_folder_of_music_and_leads_to_the_import_window() {
        let at = iced::Point::new(10.0, 10.0);
        let mut engine = state();
        let _ = handle(&mut engine, Message::FolderMenuOpen { playlist: 1, at });
        assert!(
            engine.folder_menu.is_none(),
            "an Engine library has no such menu"
        );

        let mut state = state_with_a_folder();
        let _ = handle(&mut state, Message::FolderMenuOpen { playlist: 1, at });
        assert_eq!(
            state.folder_menu,
            Some(folder_menu::Menu {
                playlist: 1,
                title: "Deep".into(),
                at
            })
        );
        let _ = handle(&mut state, Message::FolderMenuImport);
        assert!(state.folder_menu.is_none());
        let import = state.import.as_ref().expect("the window is open");
        assert_eq!((import.name.as_str(), import.tracks), ("Deep", 2));
        assert!(matches!(import.stage, import_view::Stage::Pick));
        assert!(state.window_open(), "nothing else opens meanwhile");

        // A folder with no music of its own below it has nothing to import.
        let mut empty = state_with_a_folder();
        let _ = handle(&mut empty, Message::FolderMenuOpen { playlist: 0, at });
        let _ = handle(&mut empty, Message::FolderMenuImport);
        assert!(empty.import.is_none());
    }

    fn import_done(added: &[&str]) -> open_engine_api::v1::ImportFolderResponse {
        open_engine_api::v1::ImportFolderResponse {
            target_path: "/media/DRIVE/Engine Library".into(),
            playlist_title: "Pop".into(),
            tracks_added: added.len() as u32,
            added_paths: added.iter().map(|path| (*path).to_owned()).collect(),
            ..Default::default()
        }
    }

    #[test]
    fn the_import_window_goes_from_the_library_to_the_options_to_the_end() {
        use import_view::{Setting, Stage};
        use open_engine_api::v1::NameOrder;
        let mut state = state_with_a_folder();
        let at = iced::Point::new(10.0, 10.0);
        let _ = handle(&mut state, Message::FolderMenuOpen { playlist: 1, at });
        let _ = handle(&mut state, Message::FolderMenuImport);

        let target = PathBuf::from("/media/DRIVE/Engine Library");
        let _ = handle(&mut state, Message::ImportTarget(target.clone()));
        let stage = |state: &State| state.import.as_ref().unwrap().stage.clone();
        match stage(&state) {
            Stage::Options { options, .. } => {
                // The folder's name is the playlist's, the files are copied and the tracks
                // analysed unless that is taken back.
                assert_eq!(options.title, "Deep");
                assert!(options.copy_files && options.analyse);
                // Digits are characters unless that is changed, as the folder is shown.
                assert_eq!(options.name_order, NameOrder::Strict);
            }
            other => panic!("{other:?}"),
        }
        // The files are listed in the order that is chosen; an answer for the order that
        // was left is not wanted.
        let preview = |order, names: &[&str]| {
            Message::ImportPreview(order, Ok(names.iter().map(|n| (*n).to_owned()).collect()))
        };
        let _ = handle(&mut state, preview(NameOrder::Strict, &["1", "10", "2"]));
        assert_eq!(
            state.import.as_ref().unwrap().preview,
            Some(Ok(vec!["1".to_owned(), "10".to_owned(), "2".to_owned()]))
        );
        let _ = handle(&mut state, Message::ImportOrder(NameOrder::Natural));
        assert!(state.import.as_ref().unwrap().preview.is_none());
        let _ = handle(&mut state, preview(NameOrder::Strict, &["late"]));
        assert!(state.import.as_ref().unwrap().preview.is_none());
        let _ = handle(&mut state, preview(NameOrder::Natural, &["1", "2", "10"]));
        assert_eq!(
            state
                .import
                .as_ref()
                .unwrap()
                .preview
                .as_ref()
                .unwrap()
                .as_ref()
                .unwrap()[2],
            "10"
        );
        let _ = handle(&mut state, Message::ImportOrder(NameOrder::Strict));
        let _ = handle(&mut state, Message::ImportTitle("Warm-up".into()));
        let _ = handle(&mut state, Message::ImportOption(Setting::Analyse, false));
        match stage(&state) {
            Stage::Options { options, .. } => {
                assert_eq!(options.title, "Warm-up");
                assert!(options.copy_files && !options.analyse);
            }
            other => panic!("{other:?}"),
        }
        // Back to the choice of the library, and on again.
        let _ = handle(&mut state, Message::ImportBack);
        assert!(matches!(stage(&state), Stage::Pick));
        let _ = handle(&mut state, Message::ImportTarget(target.clone()));

        // It fails: the options are back, with why.
        let options = import_view::Options {
            title: "Deep".into(),
            copy_files: true,
            analyse: true,
            name_order: NameOrder::Strict,
        };
        state.import.as_mut().unwrap().stage = Stage::Importing {
            target: target.clone(),
            options: options.clone(),
        };
        assert!(state.syncing(), "how far it is gets asked");
        assert!(
            !state.import.as_ref().unwrap().stage.can_close(),
            "it can't be closed meanwhile"
        );
        let _ = handle(&mut state, Message::ImportClose);
        assert!(state.import.is_some());
        let _ = handle(
            &mut state,
            Message::ImportFinished(Err("no library".into())),
        );
        match stage(&state) {
            Stage::Options { error, .. } => assert_eq!(error.as_deref(), Some("no library")),
            other => panic!("{other:?}"),
        }

        // It works: the end says so, and the library is remembered.
        state.import.as_mut().unwrap().stage = Stage::Importing { target, options };
        let _ = handle(
            &mut state,
            Message::ImportFinished(Ok(import_done(&["../a.flac"]))),
        );
        assert!(matches!(stage(&state), Stage::Done { analyse: true, .. }));
        assert_eq!(
            state.recents.paths().first().map(PathBuf::as_path),
            Some(Path::new("/media/DRIVE/Engine Library"))
        );

        // Opening the library closes the window and remembers what to do when it is open.
        let _ = handle(&mut state, Message::ImportOpenLibrary);
        assert!(state.import.is_none());
        assert_eq!(
            state.after_open,
            Some(analysis_queue::AfterOpen {
                library: "/media/DRIVE/Engine Library".into(),
                playlist: "Pop".into(),
                added: vec!["../a.flac".into()],
                analyse: true,
            })
        );
    }

    #[test]
    fn the_imported_playlist_is_shown_when_its_library_is_open() {
        let mut state = state_with_a_folder();
        state.after_open = Some(analysis_queue::AfterOpen {
            library: "/media/DRIVE/Engine Library".into(),
            playlist: "Pop".into(),
            added: Vec::new(),
            analyse: false,
        });
        state.library = LibraryState::Opening("/media/DRIVE/Engine Library".into());
        let library = Arc::new(library_at("/media/DRIVE/Engine Library"));
        let _ = handle(&mut state, Message::LibraryOpened(Ok(library)));
        assert!(state.after_open.is_none());
        assert_eq!(
            browser(&state).source,
            Source::Playlist(2),
            "Pop is the third"
        );
        assert!(state.analysis_queue.is_none() && !state.analysing());

        // Another library than the one imported into is left alone.
        let mut other = state_with_a_folder();
        other.after_open = Some(analysis_queue::AfterOpen {
            library: "/media/OTHER/Engine Library".into(),
            playlist: "Pop".into(),
            added: Vec::new(),
            analyse: true,
        });
        other.library = LibraryState::Opening("/media/DRIVE/Engine Library".into());
        let library = Arc::new(library_at("/media/DRIVE/Engine Library"));
        let _ = handle(&mut other, Message::LibraryOpened(Ok(library)));
        assert_eq!(browser(&other).source, Source::Collection);
        assert!(!other.analysing());
    }

    #[test]
    fn a_list_of_tracks_is_analysed_one_after_the_other_and_the_end_says_how_it_went() {
        let library = "/media/DRIVE/Engine Library".to_owned();
        let queue = |tracks: u32| {
            let mut queue = analysis_queue::Queue::new(
                library.clone(),
                (1..=tracks).map(|id| (id, format!("Track {id}"))).collect(),
                false,
            );
            queue.next();
            queue
        };
        let finished = |state: &mut State, track: u32, result: Result<_, String>| {
            let _ = handle(
                state,
                Message::AnalysisFinished {
                    library: library.clone(),
                    track,
                    title: format!("Track {track}"),
                    result,
                },
            );
        };
        let running = |state: &mut State| {
            state.toast = Some(toast::Toast::Analysing {
                library: library.clone(),
                title: "Track 1".into(),
                position: Some((1, 2)),
                stoppable: true,
            });
        };

        // The first of two is done and the second can't be started (there is no backend
        // here): that ends the queue, with how many are done.
        let mut state = state();
        state.analysis_queue = Some(queue(2));
        running(&mut state);
        finished(&mut state, 1, Ok(analysis_response(120.0, Some("C major"))));
        assert!(state.analysis_queue.is_none());
        assert!(!state.analysing(), "nothing is left to wait for");
        match &state.toast {
            Some(toast::Toast::Done { message, .. }) => {
                assert!(message.contains("1 of 2"), "{message}")
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(
            browser(&state).analysis(browser(&state).library.track_by_id(1).unwrap()),
            5,
            "the track that was done shows it"
        );

        // A failure is told with the track and why, and stays.
        let mut state = self::state();
        state.analysis_queue = Some(queue(2));
        running(&mut state);
        finished(&mut state, 1, Err("no models".into()));
        match &state.toast {
            Some(toast::Toast::Failed { message }) => {
                assert!(
                    message.contains("1 of 2")
                        && message.contains("Track 1")
                        && message.contains("no models"),
                    "{message}"
                )
            }
            other => panic!("{other:?}"),
        }

        // A queue of one is told as before: with the track's tempo.
        let mut state = self::state();
        state.analysis_queue = Some(queue(1));
        state.toast = Some(toast::Toast::Analysing {
            library: library.clone(),
            title: "Track 1".into(),
            position: None,
            stoppable: false,
        });
        finished(
            &mut state,
            1,
            Ok(analysis_response(126.04, Some("A♭ minor"))),
        );
        match &state.toast {
            Some(toast::Toast::Done { message, .. }) => {
                assert!(
                    message.contains("Track 1") && message.contains("126.0"),
                    "{message}"
                )
            }
            other => panic!("{other:?}"),
        }

        // Stopping drops the tracks that wait and takes the button away.
        let mut state = self::state();
        state.analysis_queue = Some(queue(3));
        running(&mut state);
        let _ = handle(&mut state, Message::AnalysisQueueStop);
        assert!(!state.analysis_queue.as_ref().unwrap().has_next());
        assert!(matches!(
            state.toast,
            Some(toast::Toast::Analysing {
                stoppable: false,
                ..
            })
        ));
    }

    #[test]
    fn the_context_menu_offers_to_analyse_every_track_of_the_list_that_has_no_analysis() {
        let mut state = state();
        let at = iced::Point::new(10.0, 10.0);
        let _ = handle(&mut state, Message::TrackMenuOpen { row: 1, at });
        assert_eq!(menu(&state).analyse_all, 3);

        // Tracks that have an analysis are not counted.
        let _ = handle(&mut state, Message::TrackMenuClose);
        if let LibraryState::Open(browser) = &mut state.library {
            browser.set_analysed(
                0,
                browser::Analysed {
                    bits: model::ANALYSIS_BEATGRID,
                    origin: model::AnalysisOrigin::Engine,
                    note: None,
                    bpm: None,
                },
            );
        }
        let _ = handle(&mut state, Message::TrackMenuOpen { row: 1, at });
        assert_eq!(menu(&state).analyse_all, 2);
    }
}
