//! The playback controls at the bottom of the library page.

use iced::widget::canvas::{Frame, Geometry, Path};
use iced::widget::image::Handle;
use iced::widget::{button, canvas, container, image, row, text, tooltip};
use iced::{Color, Element, Fill, Point, Rectangle, Renderer, Size, Theme, mouse};

use crate::Message;
use crate::browser::format_time;
use crate::config::Language;
use crate::model::Track;

/// Side of the cover next to the controls.
const COVER: f32 = 96.0;
/// Side of the icon on a control button.
const ICON: f32 = 22.0;

pub struct NowPlaying<'a> {
    pub track: &'a Track,
    /// Seconds into the track.
    pub position: f32,
    pub paused: bool,
}

/// The stems in the order of the channel pairs of Engine's stems files.
pub const STEM_COUNT: usize = 4;

/// Whether the tracks are played from their stems, and which of these are off.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct StemMix {
    pub on: bool,
    pub muted: [bool; STEM_COUNT],
}

impl StemMix {
    /// How loud each stem is: off or as it was separated.
    pub fn gains(&self) -> [f32; STEM_COUNT] {
        self.muted.map(|muted| if muted { 0.0 } else { 1.0 })
    }
}

/// The name of a stem, by its channel pair in the file: vocals, bass, drums, melody (found
/// by measuring five real stems files, see docs/engine-library-format.md).
fn stem_name(strings: &crate::i18n::Strings, stem: usize) -> &str {
    match stem {
        0 => &strings.stem_vocals,
        1 => &strings.stem_bass,
        2 => &strings.stem_drums,
        _ => &strings.stem_melody,
    }
}

/// What the player shows of the stems.
pub struct Stems<'a> {
    pub mix: StemMix,
    /// A key that fits Engine's stems is set.
    pub has_key: bool,
    /// The playing track is played from its stems, and if not although they were asked for,
    /// why.
    pub playing: bool,
    pub error: Option<&'a str>,
}

pub struct Bar<'a> {
    pub now: Option<NowPlaying<'a>>,
    /// The cover of the playing track, if it has one and it has been fetched.
    pub cover: Option<&'a Handle>,
    /// The play button also starts the selected track, so it is enabled whenever
    /// there is something to start.
    pub can_start: bool,
    pub stems: Stems<'a>,
}

pub fn view<'a>(language: Language, bar: Bar<'a>) -> Element<'a, Message> {
    let strings = language.strings();
    let playing = bar.now.is_some();
    let running = bar.now.as_ref().is_some_and(|now| !now.paused);

    // The icon says what the button does; the words are for pointing at it.
    let control = |icon: Icon, label: &'a str, message: Message, enabled: bool| {
        tooltip(
            button(canvas(Glyph { icon, enabled }).width(ICON).height(ICON))
                .padding([6, 12])
                .on_press_maybe(enabled.then_some(message)),
            text(label).size(13),
            tooltip::Position::Top,
        )
        .style(container::rounded_box)
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

    let cover: iced::Element<'a, Message> = match bar.cover {
        Some(handle) => image(handle.clone())
            .width(Fill)
            .height(Fill)
            .content_fit(iced::ContentFit::Contain)
            .into(),
        None => iced::widget::Space::new().into(),
    };

    row![
        container(cover)
            .width(COVER)
            .height(COVER)
            .style(container::bordered_box),
        control(
            Icon::Previous,
            &strings.previous,
            Message::Previous,
            playing
        ),
        control(
            if running { Icon::Pause } else { Icon::Play },
            if running {
                &strings.pause
            } else {
                &strings.play
            },
            Message::PlayPause,
            playing || bar.can_start,
        ),
        control(Icon::Stop, &strings.stop, Message::Stop, playing),
        control(Icon::Next, &strings.next, Message::Next, playing),
        stems_controls(strings, &bar),
        text(title).width(Fill),
        text(format!(
            "{} / {}",
            format_time(position as u32),
            format_time(length as u32)
        )),
    ]
    .spacing(10)
    .align_y(iced::Center)
    .into()
}

/// The stems button, and while the track plays from its stems a button for each of them,
/// lit while it is heard.
fn stems_controls<'a>(strings: &'a crate::i18n::Strings, bar: &Bar<'a>) -> Element<'a, Message> {
    let stems = &bar.stems;
    let has_stems = bar.now.as_ref().is_some_and(|now| now.track.has_stems);
    let hint = match (stems.mix.on, stems.error) {
        (true, Some(error)) => crate::i18n::fill(&strings.stems_failed, &[("error", &error)]),
        _ if !has_stems && bar.now.is_some() => strings.stems_missing_hint.clone(),
        _ if !stems.has_key => strings.stems_no_key_hint.clone(),
        _ => strings.stems_play_hint.clone(),
    };
    let toggle = button(text(&strings.stems_button).size(13))
        .padding([6, 10])
        .style(if stems.mix.on {
            button::primary
        } else {
            button::secondary
        })
        // Switching off always works; switching on needs the key.
        .on_press_maybe((stems.mix.on || stems.has_key).then_some(Message::StemsToggle));
    let mut controls = row![
        tooltip(toggle, text(hint).size(13), tooltip::Position::Top).style(container::rounded_box)
    ]
    .spacing(4)
    .align_y(iced::Center);

    if stems.mix.on && stems.playing {
        for (stem, muted) in stems.mix.muted.iter().enumerate() {
            let name = stem_name(strings, stem);
            let stem_button = button(text(name).size(13))
                .padding([6, 8])
                .style(if *muted {
                    button::secondary
                } else {
                    button::success
                })
                .on_press(Message::StemToggle(stem));
            controls = controls.push(
                tooltip(
                    stem_button,
                    text(crate::i18n::fill(
                        &strings.stem_toggle_hint,
                        &[("stem", &name)],
                    ))
                    .size(13),
                    tooltip::Position::Top,
                )
                .style(container::rounded_box),
            );
        }
    }
    controls.into()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Icon {
    Previous,
    Play,
    Pause,
    Stop,
    Next,
}

/// An icon, drawn in the text color of a button (faded if it can't be pressed). Drawn
/// rather than written as a character so that it looks the same whatever fonts there are.
struct Glyph {
    icon: Icon,
    enabled: bool,
}

impl Glyph {
    /// The shapes of the icon on a square of side 1, as (x, y) of the corners of polygons.
    fn shapes(&self) -> Vec<Vec<(f32, f32)>> {
        let bar =
            |left: f32, right: f32| vec![(left, 0.15), (right, 0.15), (right, 0.85), (left, 0.85)];
        match self.icon {
            Icon::Play => vec![vec![(0.25, 0.1), (0.9, 0.5), (0.25, 0.9)]],
            Icon::Pause => vec![bar(0.2, 0.4), bar(0.6, 0.8)],
            Icon::Stop => vec![vec![(0.2, 0.2), (0.8, 0.2), (0.8, 0.8), (0.2, 0.8)]],
            Icon::Previous => vec![bar(0.1, 0.24), vec![(0.9, 0.15), (0.9, 0.85), (0.3, 0.5)]],
            Icon::Next => vec![bar(0.76, 0.9), vec![(0.1, 0.15), (0.1, 0.85), (0.7, 0.5)]],
        }
    }
}

impl canvas::Program<Message> for Glyph {
    type State = ();

    fn draw(
        &self,
        _state: &(),
        renderer: &Renderer,
        theme: &Theme,
        bounds: Rectangle,
        _cursor: mouse::Cursor,
    ) -> Vec<Geometry> {
        let color = theme.extended_palette().primary.base.text;
        let color = Color {
            a: if self.enabled { color.a } else { color.a * 0.5 },
            ..color
        };
        let side = bounds.width.min(bounds.height);
        let to_point = |&(x, y): &(f32, f32)| Point::new(x * side, y * side);

        let mut frame = Frame::new(renderer, Size::new(side, side));
        for shape in self.shapes() {
            let outline = Path::new(|builder| {
                let mut corners = shape.iter().map(to_point);
                if let Some(first) = corners.next() {
                    builder.move_to(first);
                    corners.for_each(|corner| builder.line_to(corner));
                    builder.close();
                }
            });
            frame.fill(&outline, color);
        }
        vec![frame.into_geometry()]
    }
}
