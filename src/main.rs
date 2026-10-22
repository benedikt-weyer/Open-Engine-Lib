#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod backend;
mod browser;
mod config;
mod covers;
mod deck;
mod i18n;
mod model;
mod navbar;
mod player_bar;
mod recent;
mod session;
mod settings;
mod sidebar;
mod waveform;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use backend::{Backend, Control};
use browser::{Browser, Source};
use config::{ConfigStore, Language, Locations, ThemeMode};
use covers::Covers;
use i18n::fill;
use iced::widget::{button, column, container, row, scrollable, text};
use iced::{Center, Element, Fill, Subscription, Task, Theme};
use model::Library;
use open_engine_api::v1::{PlaybackError, PlaybackErrorKind, PlaybackState, PlaybackStatus};
use recent::Recents;
use session::{Session, SessionStore};
use waveform::{Waveform, Waveforms};

/// How often the playback position is fetched from the backend while playing.
const POLL_INTERVAL: Duration = Duration::from_millis(250);
/// How long after a library opens to wait with scrolling its track list to the right place.
const REVEAL_DELAY: Duration = Duration::from_millis(150);

fn main() -> iced::Result {
    iced::application(State::new, update, view)
        .title("Open Engine Lib")
        .theme(State::theme)
        .subscription(State::subscription)
        // Closing the window saves the session first, see `Message::CloseRequested`.
        .exit_on_close_request(false)
        .run()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Page {
    #[default]
    Library,
    Settings,
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
    /// Cover images fetched for the tracks in view and the one playing.
    covers: Covers,
    /// Why the recent libraries could not be saved.
    recents_error: Option<String>,
    /// What the backend last said about playback, and when.
    status: PlaybackStatus,
    status_at: Instant,
    /// The waveform and beatgrid of the playing track.
    waveforms: Waveforms,
    /// The list the queue was made from, so its playing row can be marked.
    play_source: Option<Source>,
    /// Where the user is dragging the seek slider to, in seconds.
    seeking: Option<f32>,
    /// A failed request to the backend.
    backend_error: Option<String>,
}

#[derive(Debug, Clone)]
pub enum Message {
    Navigate(Page),
    ThemeSelected(ThemeMode),
    LanguageSelected(Language),
    ResetOverrides,
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
    ForgetRecent(PathBuf),
    /// Scroll the track list to a row, once the list is on screen.
    RevealRow(usize),

    ShowCollection,
    ShowPlaylist(usize),
    ToggleFolder(usize),
    SelectTrack(usize),
    PlayTrack(usize),
    TrackListScrolled(scrollable::Viewport),

    PlayPause,
    Stop,
    Next,
    Previous,
    Tick,
    SeekDragged(f32),
    SeekReleased,
    /// Jump to a point of the track, in seconds (from clicking the waveform overview).
    SeekTo(f32),
    Status(Result<PlaybackStatus, String>),
    /// A fetched cover: the covers' generation it was asked in, its id and its image.
    CoverLoaded(u32, u32, Result<Option<Vec<u8>>, String>),
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
            covers: Covers::default(),
            recents_error: None,
            status: PlaybackStatus::default(),
            status_at: Instant::now(),
            waveforms: Waveforms::default(),
            play_source: None,
            seeking: None,
            backend_error: None,
        };
        (state, start_backend())
    }

    fn theme(&self) -> Theme {
        match self.config.effective().theme {
            ThemeMode::Light => Theme::Light,
            ThemeMode::Dark => Theme::Dark,
        }
    }

    fn subscription(&self) -> Subscription<Message> {
        let poll = if self.status.state() == PlaybackState::Playing {
            iced::time::every(POLL_INTERVAL).map(|_| Message::Tick)
        } else {
            Subscription::none()
        };
        Subscription::batch([
            poll,
            iced::window::close_requests().map(Message::CloseRequested),
        ])
    }

    fn backend(&self) -> Option<Backend> {
        match &self.backend {
            BackendState::Ready(backend) => Some(Backend::clone(backend)),
            _ => None,
        }
    }

    /// Sends a playback request to the backend; its answer arrives as `Message::Status`.
    fn control(&self, control: Control) -> Task<Message> {
        match self.backend() {
            Some(backend) => Task::perform(backend::control(backend, control), Message::Status),
            None => Task::none(),
        }
    }

    /// Opens the library at `path`; `restore` says what to go back to in it, if anything.
    fn open(&mut self, path: PathBuf, restore: Option<Session>) -> Task<Message> {
        let Some(backend) = self.backend() else {
            // Still starting: remember it for when the backend is up.
            self.open_when_ready = Some((path, restore));
            return Task::none();
        };
        self.restore = restore;
        self.reset_playback();
        self.covers.clear();
        self.waveforms.clear();
        self.library = LibraryState::Opening(path.clone());
        Task::perform(backend::open_library(backend, path), Message::LibraryOpened)
    }

    fn reset_playback(&mut self) {
        self.status = PlaybackStatus::default();
        self.play_source = None;
        self.seeking = None;
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
        Task::perform(backend::waveform(backend, id), move |result| {
            Message::WaveformLoaded(generation, id, result)
        })
    }

    fn is_stopped(&self) -> bool {
        !matches!(
            self.status.state(),
            PlaybackState::Playing | PlaybackState::Paused
        )
    }
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
    Task::batch([task, state.fetch_covers(), state.fetch_waveform()])
}

fn handle(state: &mut State, message: Message) -> Task<Message> {
    match message {
        Message::Navigate(page) => state.page = page,
        Message::ThemeSelected(theme) => {
            state.config_error = state.config.set_theme(theme).err().map(|e| e.to_string());
        }
        Message::LanguageSelected(language) => {
            state.config_error = state
                .config
                .set_language(language)
                .err()
                .map(|e| e.to_string());
        }
        Message::ReopenLibraryToggled(reopen) => {
            state.config_error = state
                .config
                .set_reopen_library(reopen)
                .err()
                .map(|e| e.to_string());
        }
        Message::RestorePositionToggled(restore) => {
            state.config_error = state
                .config
                .set_restore_position(restore)
                .err()
                .map(|e| e.to_string());
        }
        Message::CloseRequested(_) => {
            state.save_session(true);
            return iced::exit();
        }
        Message::ResetOverrides => {
            state.config_error = state.config.reset_overrides().err().map(|e| e.to_string());
        }

        Message::BackendStarted(Ok(backend)) => {
            state.backend = BackendState::Ready(backend);
            if let Some((path, restore)) = state.open_when_ready.take() {
                return state.open(path, restore);
            }
        }
        Message::BackendStarted(Err(error)) => state.backend = BackendState::Failed(error),
        Message::RetryBackend => {
            state.backend = BackendState::Starting;
            state.backend_error = None;
            return start_backend();
        }

        Message::ChooseLibrary => {
            let title = state
                .config
                .effective()
                .language
                .strings()
                .choose_library
                .clone();
            return Task::perform(
                async move {
                    rfd::AsyncFileDialog::new()
                        .set_title(title)
                        .pick_folder()
                        .await
                        .map(|folder| folder.path().to_path_buf())
                },
                Message::LibraryChosen,
            );
        }
        Message::LibraryChosen(Some(path)) => return state.open(path, None),
        Message::LibraryChosen(None) => {}
        Message::LibraryOpened(result) => {
            if matches!(state.library, LibraryState::Opening(_)) {
                state.library = match result {
                    Ok(library) => {
                        state.recents_error = state
                            .recents
                            .add(PathBuf::from(&library.path))
                            .err()
                            .map(|error| error.to_string());
                        LibraryState::Open(Browser::new(library))
                    }
                    Err(error) => {
                        state.restore = None;
                        LibraryState::Closed { error: Some(error) }
                    }
                };
                if let Some(session) = state.restore.take() {
                    return state.restore_session(session);
                }
            }
        }
        Message::CloseLibrary => {
            state.library = LibraryState::Closed { error: None };
            state.reset_playback();
            state.covers.clear();
            state.waveforms.clear();
            // Closing a library on purpose means the next start doesn't reopen it.
            state.save(&Session::default(), true);
            if let Some(backend) = state.backend() {
                return Task::perform(backend::close_library(backend), Message::LibraryClosed);
            }
        }
        Message::LibraryClosed(result) => state.backend_error = result.err(),
        Message::OpenRecent(path) => {
            // One library at a time: the backend would end up with the other one.
            if !matches!(state.library, LibraryState::Opening(_)) {
                return state.open(path, None);
            }
        }
        Message::ForgetRecent(path) => {
            state.recents_error = state
                .recents
                .forget(&path)
                .err()
                .map(|error| error.to_string());
        }

        Message::ShowCollection | Message::ShowPlaylist(_) => {
            if let LibraryState::Open(browser) = &mut state.library {
                browser.show(match message {
                    Message::ShowPlaylist(index) => Source::Playlist(index),
                    _ => Source::Collection,
                });
                return browser::scroll_to_top();
            }
        }
        Message::RevealRow(row) => {
            if let LibraryState::Open(browser) = &mut state.library {
                return browser.reveal(row);
            }
        }
        Message::ToggleFolder(index) => {
            if let LibraryState::Open(browser) = &mut state.library {
                browser.toggle(index);
            }
        }
        Message::SelectTrack(row) => {
            if let LibraryState::Open(browser) = &mut state.library {
                browser.selected = Some(row);
            }
        }
        Message::PlayTrack(row) => {
            if let LibraryState::Open(browser) = &mut state.library {
                browser.selected = Some(row);
            }
            return state.play_row(row);
        }
        Message::TrackListScrolled(viewport) => {
            if let LibraryState::Open(browser) = &mut state.library {
                browser.scrolled(viewport);
            }
        }

        Message::PlayPause => {
            if state.is_stopped() {
                let LibraryState::Open(browser) = &state.library else {
                    return Task::none();
                };
                let row = browser.selected.unwrap_or(0);
                return state.play_row(row);
            }
            return state.control(Control::TogglePause);
        }
        Message::Stop => return state.control(Control::Stop),
        Message::Next => return state.control(Control::Next),
        Message::Previous => return state.control(Control::Previous),
        Message::Tick => return state.control(Control::Poll),
        Message::SeekDragged(seconds) => state.seeking = Some(seconds),
        Message::SeekTo(seconds) => {
            return state.control(Control::Seek {
                position_ms: (seconds.max(0.0) * 1000.0) as u64,
            });
        }
        Message::SeekReleased => {
            if let Some(seconds) = state.seeking.take() {
                return state.control(Control::Seek {
                    position_ms: (seconds * 1000.0) as u64,
                });
            }
        }
        Message::Status(Ok(status)) => {
            state.status = status;
            state.status_at = Instant::now();
            state.backend_error = None;
            if state.is_stopped() {
                state.play_source = None;
            }
        }
        Message::Status(Err(error)) => state.backend_error = Some(error),
        Message::WaveformLoaded(generation, id, result) => match result {
            Ok(waveform) => state.waveforms.set(generation, id, waveform),
            Err(error) => {
                state.waveforms.set(generation, id, None);
                state.backend_error = Some(error);
            }
        },
        Message::CoverLoaded(generation, id, result) => match result {
            Ok(image) => state.covers.set(generation, id, image),
            Err(error) => {
                state.covers.set(generation, id, None);
                state.backend_error = Some(error);
            }
        },
    }
    Task::none()
}

fn view(state: &State) -> Element<'_, Message> {
    let language = state.config.effective().language;
    let page = match state.page {
        Page::Library => library_page(state, language),
        Page::Settings => settings::view(&state.config, state.config_error.as_deref()),
    };

    column![
        navbar::view(state.page, language),
        container(page).padding(20).width(Fill).height(Fill)
    ]
    .width(Fill)
    .height(Fill)
    .into()
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
                text(fill(&strings.backend_failed, &[("error", error)]))
                    .style(text::danger)
                    .into(),
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
            centered(vec![
                text(fill(&strings.opening_library, &[("path", &path.display())])).into(),
            ]),
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
                lines.push(
                    text(fill(&strings.open_failed, &[("error", error)]))
                        .style(text::danger)
                        .into(),
                );
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
            &state.recents,
            state.recents_error.as_deref(),
            browser.map(|browser| browser.library.path.as_str()),
            !matches!(state.library, LibraryState::Opening(_)),
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

    let header = row![
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
        .spacing(2)
        .width(Fill),
        button(text(&strings.close_library)).on_press(Message::CloseLibrary),
    ]
    .spacing(10)
    .align_y(Center);

    let now = (!state.is_stopped())
        .then_some(state.status.track_id)
        .flatten()
        .and_then(|id| library.track_by_id(id))
        .map(|track| player_bar::NowPlaying {
            track,
            position: state
                .seeking
                .unwrap_or(state.status.position_ms as f32 / 1000.0),
            paused: state.status.state() == PlaybackState::Paused,
        });
    // The waveform of the track in the player, with the playhead where the player is.
    let deck = now.as_ref().map(|now| {
        deck::view(
            state.waveforms.display(now.track.id),
            deck::Anchor {
                position: state.status.position_ms as f64 / 1000.0,
                at: state.status_at,
                playing: state.status.state() == PlaybackState::Playing,
            },
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
        browser::tracks(browser, state.playing_row(browser), &state.covers, language),
    ]
    .spacing(10)
    .width(Fill)
    .height(Fill);
    if let Some(deck) = deck {
        page = page.push(deck);
    }
    page = page.push(bar);

    if let Some(error) = &state.status.error {
        page = page.push(text(playback_error(language, error, library)).style(text::danger));
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
    text(fill(
        &language.strings().backend_error,
        &[("error", &error)],
    ))
    .style(text::danger)
    .into()
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
        }
    }

    fn playlist(title: &str, depth: usize, has_children: bool, tracks: &[usize]) -> Playlist {
        Playlist {
            title: title.into(),
            depth,
            has_children,
            tracks: tracks.to_vec(),
        }
    }

    /// An app with a library open: tracks a, b, c; playlists House/Deep (b, c) and Pop (a).
    fn state() -> State {
        let temp =
            std::env::temp_dir().join(format!("open-engine-lib-main-test-{}", std::process::id()));
        let library = Library::new(
            "/media/DRIVE/Engine Library".into(),
            "/cache".into(),
            vec![track(0), track(1), track(2)],
            vec![
                playlist("House", 0, true, &[]),
                playlist("Deep", 1, false, &[1, 2]),
                playlist("Pop", 0, false, &[0]),
            ],
        );
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
            covers: Covers::default(),
            recents_error: None,
            status: PlaybackStatus::default(),
            status_at: Instant::now(),
            waveforms: Waveforms::default(),
            play_source: None,
            seeking: None,
            backend_error: None,
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

    #[test]
    fn remembers_nothing_while_no_library_is_open() {
        let mut state = state();
        state.library = LibraryState::Closed { error: None };
        assert_eq!(state.snapshot(), None);
    }
}
