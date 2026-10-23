use iced::Element;
use iced::widget::{
    Column, button, checkbox, column, pick_list, radio, row, scrollable, slider, text, text_input,
};

use crate::Message;
use crate::config::{
    ConfigStore, Language, ThemeMode, WAVEFORM_DELAY_MAX_MS, WaveformBand, WaveformColors,
};
use open_engine_api::CONTROLLER_SEEK_SECONDS;
use open_engine_api::v1::{Control, ControllerAction};

use crate::controller::{self, Controller};
use crate::error_text;
use crate::i18n::{Strings, fill};

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

fn action_name(strings: &Strings, action: ControllerAction) -> String {
    match action {
        ControllerAction::PlayPause => strings.action_play_pause.clone(),
        ControllerAction::Stop => strings.action_stop.clone(),
        ControllerAction::Previous => strings.action_previous.clone(),
        ControllerAction::Next => strings.action_next.clone(),
        ControllerAction::SeekBack => fill(
            &strings.action_seek_back,
            &[("seconds", &CONTROLLER_SEEK_SECONDS)],
        ),
        ControllerAction::SeekForward => fill(
            &strings.action_seek_forward,
            &[("seconds", &CONTROLLER_SEEK_SECONDS)],
        ),
        ControllerAction::JogTouch => strings.action_jog_touch.clone(),
        ControllerAction::JogWheel => strings.action_jog_wheel.clone(),
        pad => fill(
            &strings.action_pad,
            &[(
                "number",
                &(controller::pad_of(pad).map_or(0, |slot| slot + 1)),
            )],
        ),
    }
}

fn control_name(strings: &Strings, control: &Control) -> String {
    let template = if controller::is_note(control) {
        &strings.control_note
    } else {
        &strings.control_cc
    };
    fill(
        template,
        &[("number", &control.number), ("channel", &control.channel)],
    )
}

/// The profile in use, with what changes the profiles: pick one, make one (empty or a copy
/// of the one in use), rename or delete the one in use. The name field is for the first
/// three; the built-in profiles can't be renamed or deleted.
fn profile_picker<'a>(
    page: Column<'a, Message>,
    strings: &'a Strings,
    controller: &'a Controller,
) -> Column<'a, Message> {
    let names: Vec<String> = controller
        .profiles()
        .iter()
        .map(|profile| profile.name.clone())
        .collect();
    let active = controller.active_profile().map(str::to_owned);
    let built_in = controller.active_is_built_in();
    let typed = controller.typed_name().is_some();
    let confirming = controller.confirm_delete.as_deref() == controller.active_profile();

    let delete = button(text(if confirming {
        &strings.controller_profile_delete_confirm
    } else {
        &strings.controller_profile_delete
    }))
    .on_press_maybe(
        (!built_in && controller.active_profile().is_some())
            .then_some(Message::ControllerProfileDelete),
    );
    let mut page = page
        .push(text(&strings.controller_profile_heading).size(16))
        .push(
            row![
                pick_list(names, active, Message::ControllerProfileSelected).width(300),
                delete,
            ]
            .spacing(10)
            .align_y(iced::Center),
        );
    if built_in {
        page = page.push(
            text(&strings.controller_profile_built_in)
                .size(14)
                .style(text::secondary),
        );
    }
    page = page.push(
        row![
            text_input(&strings.controller_profile_name, &controller.name_draft)
                .on_input(Message::ControllerProfileNameChanged)
                .on_submit_maybe(typed.then_some(Message::ControllerProfileCreate(false)))
                .width(300),
            button(text(&strings.controller_profile_new))
                .on_press_maybe(typed.then_some(Message::ControllerProfileCreate(false))),
            button(text(&strings.controller_profile_duplicate))
                .on_press_maybe(typed.then_some(Message::ControllerProfileCreate(true))),
            button(text(&strings.controller_profile_rename))
                .on_press_maybe((typed && !built_in).then_some(Message::ControllerProfileRename)),
        ]
        .spacing(10)
        .align_y(iced::Center),
    );
    if let Some(error) = &controller.profile_error {
        page = page.push(
            text(fill(
                &strings.controller_profile_failed,
                &[("error", error)],
            ))
            .style(text::danger),
        );
    }
    page
}

/// The controller: whether it is used, what is connected and what each action is bound to,
/// with a button to bind it by pressing a control. (The backend does the listening and the
/// learning; this shows what it says.)
fn controller_section<'a>(
    page: Column<'a, Message>,
    strings: &'a Strings,
    enabled: bool,
    controller: &'a Controller,
) -> Column<'a, Message> {
    let mut page = page.push(text(&strings.controller_heading).size(20)).push(
        checkbox(enabled)
            .label(&strings.controller_enable)
            .on_toggle(Message::ControllerEnabledToggled),
    );
    if !enabled {
        return page;
    }
    page = page.push(
        text(&strings.controller_hint)
            .size(14)
            .style(text::secondary),
    );
    page = page.push(text(if controller.devices().is_empty() {
        strings.controller_none.clone()
    } else {
        fill(
            &strings.controller_connected,
            &[("devices", &controller.devices().join(", "))],
        )
    }));
    if let Some((control, value)) = controller.last_input() {
        let message = format!("{}, {value}", control_name(strings, control));
        page = page.push(text(fill(&strings.controller_last, &[("message", &message)])).size(14));
    }
    page = profile_picker(page, strings, controller);
    // The bindings of a built-in profile are the program's: they are shown, not learned.
    let editable = !controller.active_is_built_in();
    for action in controller::ACTIONS {
        let bound = controller.control_for(action);
        let binding = match bound {
            Some(control) => text(control_name(strings, control)),
            None => text(&strings.controller_unassigned).style(text::secondary),
        };
        let learn = if controller.learning() == Some(action) {
            row![
                text(&strings.controller_waiting),
                button(text(&strings.controller_cancel)).on_press(Message::ControllerLearnCancel),
            ]
            .spacing(10)
            .align_y(iced::Center)
        } else {
            row![
                button(text(&strings.controller_learn))
                    .on_press_maybe(editable.then_some(Message::ControllerLearn(action))),
                button(text(&strings.controller_clear)).on_press_maybe(
                    bound
                        .filter(|_| editable)
                        .map(|_| Message::ControllerClear(action))
                ),
            ]
            .spacing(10)
        };
        page = page.push(
            row![
                text(action_name(strings, action)).width(150),
                binding.width(200),
                learn
            ]
            .spacing(10)
            .align_y(iced::Center),
        );
    }
    if let Some(error) = controller.problem() {
        page = page.push(
            text(fill(&strings.controller_unavailable, &[("error", &error)])).style(text::danger),
        );
    }
    page
}

/// `draft` is the gain slider that is being dragged, and `delay_draft` the delay slider:
/// they show where they are before it is saved.
pub fn view<'a>(
    config: &'a ConfigStore,
    error: Option<&'a str>,
    draft: Option<(WaveformBand, f32)>,
    delay_draft: Option<i32>,
    controller: &'a Controller,
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

    page = controller_section(page, strings, current.controller_enabled, controller);

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

    // The page is longer than a small window, so it scrolls; the padding keeps the scrollbar
    // off the controls.
    scrollable(
        page.padding(iced::Padding::ZERO.right(16))
            .width(iced::Fill),
    )
    .width(iced::Fill)
    .height(iced::Fill)
    .into()
}
