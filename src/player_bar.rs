//! The playback controls at the bottom of the library page.

use iced::widget::{button, row, slider, text};
use iced::{Element, Fill};

use crate::Message;
use crate::browser::format_time;
use crate::config::Language;
use crate::model::Track;

pub struct NowPlaying<'a> {
    pub track: &'a Track,
    /// Seconds into the track, or where the user is dragging the slider to.
    pub position: f32,
    pub paused: bool,
}

pub struct Bar<'a> {
    pub now: Option<NowPlaying<'a>>,
    /// The play button also starts the selected track, so it is enabled whenever
    /// there is something to start.
    pub can_start: bool,
}

pub fn view<'a>(language: Language, bar: Bar<'a>) -> Element<'a, Message> {
    let strings = language.strings();
    let playing = bar.now.is_some();
    let running = bar.now.as_ref().is_some_and(|now| !now.paused);

    let control = |label: &'a str, message: Message, enabled: bool| {
        button(text(label)).on_press_maybe(enabled.then_some(message))
    };

    let (title, position, length) = match &bar.now {
        Some(now) => (
            if now.track.artist.is_empty() {
                now.track.title.clone()
            } else {
                crate::i18n::fill(
                    &strings.now_playing,
                    &[("title", &now.track.title), ("artist", &now.track.artist)],
                )
            },
            now.position,
            now.track.length.unwrap_or(0) as f32,
        ),
        None => (strings.nothing_playing.clone(), 0.0, 0.0),
    };

    row![
        control(&strings.previous, Message::Previous, playing),
        control(
            if running {
                &strings.pause
            } else {
                &strings.play
            },
            Message::PlayPause,
            playing || bar.can_start,
        ),
        control(&strings.stop, Message::Stop, playing),
        control(&strings.next, Message::Next, playing),
        text(title).width(Fill),
        text(format_time(position as u32)),
        slider(
            0.0..=length.max(position).max(1.0),
            position,
            Message::SeekDragged
        )
        .on_release(Message::SeekReleased)
        .width(Fill),
        text(format_time(length as u32)),
    ]
    .spacing(10)
    .align_y(iced::Center)
    .into()
}
