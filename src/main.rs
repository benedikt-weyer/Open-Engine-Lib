#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod backend;
mod browser;
mod config;
mod i18n;
mod model;
mod navbar;
mod player_bar;
mod settings;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use backend::{Backend, Control};
use browser::{Browser, Source};
use config::{ConfigStore, Language, Locations, ThemeMode};
use i18n::fill;
use iced::widget::{Space, button, column, container, row, scrollable, text};
use iced::{Center, Element, Fill, Subscription, Task, Theme};
use model::Library;
use open_engine_api::v1::{PlaybackError, PlaybackErrorKind, PlaybackState, PlaybackStatus};

/// How often the playback position is fetched from the backend while playing.
const POLL_INTERVAL: Duration = Duration::from_millis(250);

fn main() -> iced::Result {
    iced::application(State::new, update, view)
        .title("Open Engine Lib")
        .theme(State::theme)
        .subscription(State::subscription)
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
    /// A library to open as soon as the backend is up (from the command line).
    open_when_ready: Option<PathBuf>,
    library: LibraryState,
    /// What the backend last said about playback.
    status: PlaybackStatus,
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

    BackendStarted(Result<Box<Backend>, String>),
    RetryBackend,

    ChooseLibrary,
    LibraryChosen(Option<PathBuf>),
    LibraryOpened(Result<Arc<Library>, String>),
    CloseLibrary,
    LibraryClosed(Result<(), String>),

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
    Status(Result<PlaybackStatus, String>),
}

impl State {
    fn new() -> (Self, Task<Message>) {
        let state = Self {
            page: Page::default(),
            config: ConfigStore::load(Locations::discover()),
            config_error: None,
            backend: BackendState::Starting,
            open_when_ready: std::env::args_os().nth(1).map(PathBuf::from),
            library: LibraryState::Closed { error: None },
            status: PlaybackStatus::default(),
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
        if self.status.state() == PlaybackState::Playing {
            iced::time::every(POLL_INTERVAL).map(|_| Message::Tick)
        } else {
            Subscription::none()
        }
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

    fn open(&mut self, path: PathBuf) -> Task<Message> {
        let Some(backend) = self.backend() else {
            // Still starting: remember it for when the backend is up.
            self.open_when_ready = Some(path);
            return Task::none();
        };
        self.reset_playback();
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
        })
    }

    /// The row of the shown list that is playing, if the shown list is the playing one.
    fn playing_row(&self, browser: &Browser) -> Option<usize> {
        if self.is_stopped() || self.play_source != Some(browser.source) {
            return None;
        }
        self.status.queue_position.map(|position| position as usize)
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

fn update(state: &mut State, message: Message) -> Task<Message> {
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
        Message::ResetOverrides => {
            state.config_error = state.config.reset_overrides().err().map(|e| e.to_string());
        }

        Message::BackendStarted(Ok(backend)) => {
            state.backend = BackendState::Ready(backend);
            if let Some(path) = state.open_when_ready.take() {
                return state.open(path);
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
        Message::LibraryChosen(Some(path)) => return state.open(path),
        Message::LibraryChosen(None) => {}
        Message::LibraryOpened(result) => {
            if matches!(state.library, LibraryState::Opening(_)) {
                state.library = match result {
                    Ok(library) => LibraryState::Open(Browser::new(library)),
                    Err(error) => LibraryState::Closed { error: Some(error) },
                };
            }
        }
        Message::CloseLibrary => {
            state.library = LibraryState::Closed { error: None };
            state.reset_playback();
            if let Some(backend) = state.backend() {
                return Task::perform(backend::close_library(backend), Message::LibraryClosed);
            }
        }
        Message::LibraryClosed(result) => state.backend_error = result.err(),

        Message::ShowCollection | Message::ShowPlaylist(_) => {
            if let LibraryState::Open(browser) = &mut state.library {
                browser.show(match message {
                    Message::ShowPlaylist(index) => Source::Playlist(index),
                    _ => Source::Collection,
                });
                return browser::scroll_to_top();
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
        Message::SeekReleased => {
            if let Some(seconds) = state.seeking.take() {
                return state.control(Control::Seek {
                    position_ms: (seconds * 1000.0) as u64,
                });
            }
        }
        Message::Status(Ok(status)) => {
            state.status = status;
            state.backend_error = None;
            if state.is_stopped() {
                state.play_source = None;
            }
        }
        Message::Status(Err(error)) => state.backend_error = Some(error),
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

    match (&state.backend, &state.library) {
        (BackendState::Starting, _) => {
            return centered(vec![text(&strings.backend_starting).into()]);
        }
        (BackendState::Failed(error), _) => {
            return centered(vec![
                text(fill(&strings.backend_failed, &[("error", error)]))
                    .style(text::danger)
                    .into(),
                button(text(&strings.retry))
                    .on_press(Message::RetryBackend)
                    .into(),
            ]);
        }
        (BackendState::Ready(_), LibraryState::Opening(path)) => {
            return centered(vec![
                text(fill(&strings.opening_library, &[("path", &path.display())])).into(),
            ]);
        }
        (BackendState::Ready(_), LibraryState::Closed { error }) => {
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
            return centered(lines);
        }
        (BackendState::Ready(_), LibraryState::Open(_)) => {}
    }

    let LibraryState::Open(browser) = &state.library else {
        unreachable!("handled above");
    };
    let library = &browser.library;

    let header = row![
        text(fill(
            &strings.library_summary,
            &[
                ("path", &library.path),
                ("tracks", &library.tracks.len()),
                ("playlists", &library.playlists.len()),
            ],
        ))
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
    let bar = player_bar::view(
        language,
        player_bar::Bar {
            now,
            can_start: !browser.tracks().is_empty(),
        },
    );

    let mut page = column![
        header,
        browser::view(browser, state.playing_row(browser), language),
        bar
    ]
    .spacing(10);

    if let Some(error) = &state.status.error {
        page = page.push(text(playback_error(language, error, library)).style(text::danger));
    }
    if let Some(error) = &state.backend_error {
        page = page.push(backend_error(language, error));
    }
    page.push(Space::new().height(0)).into()
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
