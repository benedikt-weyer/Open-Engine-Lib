#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod config;
mod i18n;
mod navbar;
mod settings;

use config::{ConfigStore, Language, Locations, ThemeMode};
use i18n::fill;
use iced::widget::{button, column, container, text};
use iced::{Center, Element, Fill, Theme};

fn main() -> iced::Result {
    iced::application(State::new, update, view)
        .title("Open Engine Lib")
        .theme(State::theme)
        .run()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Page {
    #[default]
    Home,
    Settings,
}

struct State {
    page: Page,
    value: i64,
    config: ConfigStore,
    /// Last error from saving or resetting the config, shown on the settings page.
    config_error: Option<String>,
}

#[derive(Debug, Clone, Copy)]
pub enum Message {
    Navigate(Page),
    Increment,
    Decrement,
    ThemeSelected(ThemeMode),
    LanguageSelected(Language),
    ResetOverrides,
}

impl State {
    fn new() -> Self {
        Self {
            page: Page::default(),
            value: 0,
            config: ConfigStore::load(Locations::discover()),
            config_error: None,
        }
    }

    fn theme(&self) -> Theme {
        match self.config.effective().theme {
            ThemeMode::Light => Theme::Light,
            ThemeMode::Dark => Theme::Dark,
        }
    }
}

fn update(state: &mut State, message: Message) {
    match message {
        Message::Navigate(page) => state.page = page,
        Message::Increment => state.value += 1,
        Message::Decrement => state.value -= 1,
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
    }
}

fn view(state: &State) -> Element<'_, Message> {
    let language = state.config.effective().language;
    let page = match state.page {
        Page::Home => home(state, language),
        Page::Settings => settings::view(&state.config, state.config_error.as_deref()),
    };

    column![
        navbar::view(state.page, language),
        container(page).padding(20).width(Fill)
    ]
    .width(Fill)
    .height(Fill)
    .into()
}

fn home(state: &State, language: Language) -> Element<'_, Message> {
    column![
        button(text(&language.strings().increment)).on_press(Message::Increment),
        text(state.value).size(50),
        text(fill(
            &language.strings().counter_status,
            &[("value", &state.value)],
        )),
        button(text(&language.strings().decrement)).on_press(Message::Decrement),
    ]
    .spacing(10)
    .align_x(Center)
    .width(Fill)
    .into()
}
