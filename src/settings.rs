use iced::Element;
use iced::widget::{button, column, radio, text};

use crate::Message;
use crate::config::{ConfigStore, Language, ThemeMode};
use crate::i18n::fill;

fn theme_name(language: Language, theme: ThemeMode) -> &'static str {
    let strings = language.strings();
    match theme {
        ThemeMode::Light => &strings.theme_light,
        ThemeMode::Dark => &strings.theme_dark,
    }
}

pub fn view<'a>(config: &'a ConfigStore, error: Option<&'a str>) -> Element<'a, Message> {
    let current = config.effective();
    let language = current.language;
    let strings = language.strings();

    let mut page = column![
        text(&language.strings().settings_title).size(30),
        text(&language.strings().theme).size(20),
    ]
    .spacing(10);

    for theme in [ThemeMode::Light, ThemeMode::Dark] {
        page = page.push(radio(
            theme_name(language, theme),
            theme,
            Some(current.theme),
            Message::ThemeSelected,
        ));
    }

    page = page.push(text(&language.strings().language_heading).size(20));
    for option in Language::ALL {
        page = page.push(radio(
            &option.strings().native_name,
            option,
            Some(current.language),
            Message::LanguageSelected,
        ));
    }

    if config.is_layered() {
        let underlying = config.underlying();
        page = page
            .push(text(&language.strings().config_read_only_notice))
            .push(text(fill(
                &strings.config_file,
                &[("path", &config.base_path().display())],
            )))
            .push(text(fill(
                &strings.underlying_theme,
                &[("theme", &theme_name(language, underlying.theme))],
            )))
            .push(text(fill(
                &strings.underlying_language,
                &[("language", &underlying.language.strings().native_name)],
            )));

        let reset = button(text(&language.strings().reset_overrides));
        page = page
            .push(reset.on_press_maybe(config.has_overrides().then_some(Message::ResetOverrides)));
    }

    for warning in &config.warnings {
        page = page.push(text(warning.as_str()).style(text::danger));
    }
    if let Some(error) = error {
        page =
            page.push(text(fill(&strings.save_failed, &[("error", &error)])).style(text::danger));
    }

    page.into()
}
