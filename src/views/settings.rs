use iced::Element;
use iced::widget::{button, checkbox, column, radio, row, slider, text};

use crate::Message;
use crate::config::{
    ConfigStore, Language, ThemeMode, WAVEFORM_DELAY_MAX_MS, WaveformBand, WaveformColors,
};
use crate::error_text;
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

/// The range of the delay slider, in milliseconds. (The same as the config file accepts.)
const DELAY_SLIDER: std::ops::RangeInclusive<i32> = -WAVEFORM_DELAY_MAX_MS..=WAVEFORM_DELAY_MAX_MS;

/// The range of the gain sliders. (The config file accepts more.)
const GAIN_SLIDER: std::ops::RangeInclusive<f32> = 0.25..=4.0;

fn band_label(language: Language, band: WaveformBand) -> &'static str {
    let strings = language.strings();
    match band {
        WaveformBand::Low => &strings.waveform_gain_low,
        WaveformBand::Mid => &strings.waveform_gain_mid,
        WaveformBand::High => &strings.waveform_gain_high,
    }
}

/// `draft` is the gain slider that is being dragged, and `delay_draft` the delay slider:
/// they show where they are before it is saved.
pub fn view<'a>(
    config: &'a ConfigStore,
    error: Option<&'a str>,
    draft: Option<(WaveformBand, f32)>,
    delay_draft: Option<i32>,
) -> Element<'a, Message> {
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

    page = page.push(text(&strings.waveform_gains_heading).size(20));
    page = page.push(
        text(&strings.waveform_gains_hint)
            .size(14)
            .style(text::secondary),
    );
    for band in WaveformBand::ALL {
        let gain = match draft {
            Some((dragged, gain)) if dragged == band => gain,
            _ => current.waveform_gain(band),
        };
        page = page.push(
            row![
                text(band_label(language, band)).width(150),
                slider(GAIN_SLIDER, gain, move |gain| Message::WaveformGainDragged(
                    band, gain
                ))
                .step(0.05f32)
                .on_release(Message::WaveformGainReleased)
                .width(250),
                text(format!("{gain:.2}×")),
            ]
            .spacing(10)
            .align_y(iced::Center),
        );
    }
    page = page
        .push(button(text(&strings.reset_waveform_gains)).on_press(Message::ResetWaveformGains));

    let delay = delay_draft.unwrap_or(current.waveform_delay_ms);
    page = page.push(text(&strings.waveform_delay_heading).size(20));
    page = page.push(
        text(&strings.waveform_delay_hint)
            .size(14)
            .style(text::secondary),
    );
    page = page.push(
        row![
            slider(DELAY_SLIDER, delay, Message::WaveformDelayDragged)
                .step(5)
                .on_release(Message::WaveformDelayReleased)
                .width(250),
            text(format!("{delay} ms")).width(70),
            button(text(&strings.reset_waveform_delay)).on_press(Message::ResetWaveformDelay),
        ]
        .spacing(10)
        .align_y(iced::Center),
    );

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
        page = page.push(error_text::view(warning.as_str(), language));
    }
    if let Some(error) = error {
        page = page.push(error_text::view(
            fill(&strings.save_failed, &[("error", &error)]),
            language,
        ));
    }

    page.into()
}
