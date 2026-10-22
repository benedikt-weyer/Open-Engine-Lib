use iced::Element;
use iced::widget::{button, checkbox, column, radio, text};

use crate::Message;
use crate::config::{ConfigStore, Language, ThemeMode, WaveformColors};
use crate::i18n::fill;

fn theme_name(language: Language, theme: ThemeMode) -> &'static str {
    let strings = language.strings();
    match theme {
        ThemeMode::Light => &strings.theme_light,
        ThemeMode::Dark => &strings.theme_dark,
    }
}

fn waveform_colors_name(language: Language, colors: WaveformColors) -> &'static str {
    let strings = language.strings();
    match colors {
        WaveformColors::Mono => &strings.waveform_colors_mono,
        WaveformColors::Rgb => &strings.waveform_colors_rgb,
        WaveformColors::Stacked => &strings.waveform_colors_stacked,
        WaveformColors::Spectrum => &strings.waveform_colors_spectrum,
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

    page = page.push(text(&strings.waveform_colors_heading).size(20));
    for colors in WaveformColors::ALL {
        page = page.push(radio(
            waveform_colors_name(language, colors),
            colors,
            Some(current.waveform_colors),
            Message::WaveformColorsSelected,
        ));
    }

    page = page.push(text(&strings.startup_heading).size(20));
    page = page.push(
        checkbox(current.reopen_library)
            .label(&strings.reopen_library)
            .on_toggle(Message::ReopenLibraryToggled),
    );
    // Going back to the song only makes sense in a library that is reopened.
    page = page.push(
        checkbox(current.restore_position)
            .label(&strings.restore_position)
            .on_toggle_maybe(
                current
                    .reopen_library
                    .then_some(Message::RestorePositionToggled),
            ),
    );

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
