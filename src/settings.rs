use iced::Element;
use iced::widget::{button, column, radio, text};

use crate::Message;
use crate::config::{ConfigStore, ThemeMode};

pub fn view<'a>(config: &'a ConfigStore, error: Option<&'a str>) -> Element<'a, Message> {
    let current = config.effective().theme;

    let mut page = column![
        text("Settings").size(30),
        text("Theme").size(20),
        radio(
            "Light",
            ThemeMode::Light,
            Some(current),
            Message::ThemeSelected
        ),
        radio(
            "Dark",
            ThemeMode::Dark,
            Some(current),
            Message::ThemeSelected
        ),
    ]
    .spacing(10);

    if config.is_layered() {
        page = page.push(text(format!(
            "The config in {} is read-only (declarative). Changes are saved as \
             overrides on top of it.",
            config.base_path().display()
        )));

        let underlying = match config.underlying().theme {
            ThemeMode::Light => "light",
            ThemeMode::Dark => "dark",
        };
        page = page.push(text(format!("Underlying theme: {underlying}")));

        let reset = button("Reset to underlying config");
        page = page
            .push(reset.on_press_maybe(config.has_overrides().then_some(Message::ResetOverrides)));
    }

    for warning in &config.warnings {
        page = page.push(text(warning.as_str()).style(text::danger));
    }
    if let Some(error) = error {
        page = page.push(text(format!("Could not save settings: {error}")).style(text::danger));
    }

    page.into()
}
