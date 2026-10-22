//! The waveform display of the track in the player, like a DJ deck's: a zoomed,
//! scrolling waveform around the playhead with the beatgrid drawn over it, and the
//! whole track below it to see where you are and to jump to somewhere else.
//!
//! How the waveform is colored is a setting (`WaveformColors`) that can also be changed
//! from a menu next to the display.

use std::time::{Duration, Instant};

use iced::mouse;
use std::ops::Range;

use iced::widget::canvas::{Action, Frame, Geometry, Path, Text};
use iced::widget::{canvas, column, container, pick_list, row, text};
use iced::{Color, Element, Event, Fill, Pixels, Point, Rectangle, Renderer, Size, Theme, window};

use crate::Message;
use crate::clock::Clock;
use crate::colors::{self, STACKED_LAYERS};
use crate::config::{Language, WaveformColors};
use crate::i18n::fill;
use crate::waveform::{Bands, Display, Waveform};

/// How much of the track the zoomed waveform shows.
const VISIBLE_SECONDS: f64 = 6.0;
const ZOOM_HEIGHT: f32 = 110.0;
const OVERVIEW_HEIGHT: f32 = 44.0;
/// The overview only needs to follow the playhead a few times a second.
const OVERVIEW_REDRAW: Duration = Duration::from_millis(100);

const BACKGROUND: Color = Color::from_rgb(0.06, 0.07, 0.09);
const PLAYHEAD: Color = Color::WHITE;

/// An entry of the color scheme menu: the scheme and its name in the UI language.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SchemeChoice {
    colors: WaveformColors,
    name: &'static str,
}

impl std::fmt::Display for SchemeChoice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name)
    }
}

fn scheme_choice(language: Language, colors: WaveformColors) -> SchemeChoice {
    let strings = language.strings();
    SchemeChoice {
        colors,
        name: match colors {
            WaveformColors::Mono => &strings.waveform_colors_short_mono,
            WaveformColors::Rgb => &strings.waveform_colors_short_rgb,
            WaveformColors::Stacked => &strings.waveform_colors_short_stacked,
            WaveformColors::Spectrum => &strings.waveform_colors_short_spectrum,
        },
    }
}

pub fn view<'a>(
    display: Display<'a>,
    clock: Clock,
    colors: WaveformColors,
    language: Language,
) -> Element<'a, Message> {
    let strings = language.strings();

    let waveform = match display {
        Display::Ready(waveform) => waveform,
        Display::Loading | Display::Unavailable => {
            let message = match display {
                Display::Loading => &strings.waveform_loading,
                _ => &strings.waveform_unavailable,
            };
            return container(text(message).style(text::secondary))
                .center_x(Fill)
                .center_y(ZOOM_HEIGHT + OVERVIEW_HEIGHT)
                .style(container::bordered_box)
                .into();
        }
    };

    let grid = match &waveform.grid {
        Some(grid) => fill(
            &strings.beatgrid_bpm,
            &[("bpm", &format!("{:.1}", grid.bpm()))],
        ),
        None => strings.beatgrid_none.clone(),
    };

    // A quick way to change the colors without going to the settings.
    let choices = WaveformColors::ALL.map(|colors| scheme_choice(language, colors));
    let scheme_menu = pick_list(
        choices.to_vec(),
        Some(scheme_choice(language, colors)),
        |choice: SchemeChoice| Message::WaveformColorsSelected(choice.colors),
    )
    .text_size(13)
    .padding([2, 8]);

    column![
        canvas(Zoomed {
            waveform,
            clock,
            colors
        })
        .width(Fill)
        .height(ZOOM_HEIGHT),
        canvas(Overview {
            waveform,
            clock,
            colors
        })
        .width(Fill)
        .height(OVERVIEW_HEIGHT),
        row![
            text(grid).size(13).style(text::secondary).width(Fill),
            scheme_menu
        ]
        .align_y(iced::Center),
    ]
    .spacing(4)
    .into()
}

/// One band's loudness at `index`, 0 to 1, lightly smoothed with its neighbours so the
/// outline doesn't jitter from one 10 ms column to the next.
fn smoothed(columns: &[Bands], index: usize, band: usize) -> f32 {
    let at = |index: usize| f32::from(columns[index.min(columns.len() - 1)][band]);
    (at(index.saturating_sub(1)) * 0.25 + at(index) * 0.5 + at(index + 1) * 0.25) / 255.0
}

/// Draws the waveform of `columns[range]` as three smooth shapes over each other (the
/// loudest usually the low band at the back), mirrored around `middle`. `x_of` gives
/// the horizontal position of a column. Each band is one filled outline rather than a
/// bar per column, so it is drawn anti-aliased and scrolls without shimmering.
fn paint_waveform(
    frame: &mut Frame,
    colors: WaveformColors,
    columns: &[Bands],
    range: Range<usize>,
    x_of: impl Fn(usize) -> f32,
    middle: f32,
    half_height: f32,
) {
    if range.len() < 2 {
        return;
    }
    let xs: Vec<f32> = range.clone().map(x_of).collect();

    if colors == WaveformColors::Stacked {
        paint_stacked(frame, columns, &range, &xs, middle, half_height);
    } else {
        paint_colored(frame, colors, columns, &range, &xs, middle, half_height);
    }
}

/// Three layers over each other, one shape per band: the back one (low) is usually the
/// tallest and the front one (high) the smallest.
fn paint_stacked(
    frame: &mut Frame,
    columns: &[Bands],
    range: &Range<usize>,
    xs: &[f32],
    middle: f32,
    half_height: f32,
) {
    for (band, color) in STACKED_LAYERS.into_iter().enumerate() {
        let halves: Vec<f32> = range
            .clone()
            .map(|index| smoothed(columns, index, band) * half_height)
            .collect();
        if halves.iter().all(|half| *half < 0.25) {
            continue;
        }

        // Along the top from left to right, back along the bottom.
        let outline = Path::new(|builder| {
            builder.move_to(Point::new(xs[0], middle - halves[0]));
            for (x, half) in xs.iter().zip(&halves).skip(1) {
                builder.line_to(Point::new(*x, middle - half));
            }
            for (x, half) in xs.iter().zip(&halves).rev() {
                builder.line_to(Point::new(*x, middle + half));
            }
            builder.close();
        });
        frame.fill(&outline, color);
    }
}

/// One shape as tall as the loudest band, whose color changes along it: each stretch
/// between two columns is filled with the color of the sound there.
fn paint_colored(
    frame: &mut Frame,
    colors: WaveformColors,
    columns: &[Bands],
    range: &Range<usize>,
    xs: &[f32],
    middle: f32,
    half_height: f32,
) {
    let levels: Vec<[f32; 3]> = range
        .clone()
        .map(|index| [0, 1, 2].map(|band| smoothed(columns, index, band)))
        .collect();
    let halves: Vec<f32> = levels
        .iter()
        .map(|levels| colors::amplitude(*levels) * half_height)
        .collect();

    for index in 0..xs.len() - 1 {
        if halves[index] < 0.25 && halves[index + 1] < 0.25 {
            continue;
        }
        // A hair of overlap, so that no gap shows between neighbouring stretches.
        let (left, right) = (xs[index], xs[index + 1] + 0.4);
        let stretch = Path::new(|builder| {
            builder.move_to(Point::new(left, middle - halves[index]));
            builder.line_to(Point::new(right, middle - halves[index + 1]));
            builder.line_to(Point::new(right, middle + halves[index + 1]));
            builder.line_to(Point::new(left, middle + halves[index]));
            builder.close();
        });
        frame.fill(&stretch, colors::column(colors, levels[index]));
    }
}

/// Writes a beat's number at the top, just right of its line, on a dim backing so it
/// can be read over the waveform.
fn paint_beat_number(frame: &mut Frame, line_x: f32, label: String, bar_start: bool) {
    let (size, alpha) = if bar_start {
        (13.0, 0.95)
    } else {
        (11.0, 0.65)
    };
    // About as wide as the text is, which is a bit over half its size per character.
    let width = label.chars().count() as f32 * size * 0.6 + 6.0;
    let origin = Point::new(line_x + 3.0, 2.0);

    frame.fill_rectangle(
        origin,
        Size::new(width, size + 4.0),
        Color {
            a: 0.7,
            ..BACKGROUND
        },
    );
    frame.fill_text(Text {
        content: label,
        position: Point::new(origin.x + 3.0, origin.y + 2.0),
        color: Color {
            a: alpha,
            ..Color::WHITE
        },
        size: Pixels(size),
        ..Text::default()
    });
}

/// The scrolling waveform around the playhead.
struct Zoomed<'a> {
    waveform: &'a Waveform,
    clock: Clock,
    colors: WaveformColors,
}

impl canvas::Program<Message> for Zoomed<'_> {
    type State = ();

    fn update(
        &self,
        _state: &mut (),
        event: &Event,
        _bounds: Rectangle,
        _cursor: mouse::Cursor,
    ) -> Option<Action<Message>> {
        // While playing, draw the next frame as soon as the last one is out.
        (self.clock.is_running()
            && matches!(event, Event::Window(window::Event::RedrawRequested(_))))
        .then(Action::request_redraw)
    }

    fn draw(
        &self,
        _state: &(),
        renderer: &Renderer,
        _theme: &Theme,
        bounds: Rectangle,
        _cursor: mouse::Cursor,
    ) -> Vec<Geometry> {
        let mut frame = Frame::new(renderer, bounds.size());
        frame.fill_rectangle(Point::ORIGIN, bounds.size(), BACKGROUND);

        let waveform = self.waveform;
        let (width, height) = (bounds.width, bounds.height);
        let middle = height / 2.0;
        let half_height = middle - 4.0;

        let now = self.clock.position(Instant::now(), waveform.duration);
        let pixels_per_second = f64::from(width) / VISIBLE_SECONDS;
        let start = now - VISIBLE_SECONDS / 2.0;
        let end = now + VISIBLE_SECONDS / 2.0;
        let x_of = |time: f64| ((time - start) * pixels_per_second) as f32;

        // The waveform, from a column before the window to one after it, so its outline
        // runs off both edges.
        let columns_per_second = waveform.columns_per_second;
        let first = ((start * columns_per_second).floor().max(1.0) as usize).saturating_sub(1);
        let last =
            ((end * columns_per_second).ceil().max(0.0) as usize + 1).min(waveform.columns.len());
        paint_waveform(
            &mut frame,
            self.colors,
            &waveform.columns,
            first..last,
            // The middle of a column's 10 ms.
            |index| x_of((index as f64 + 0.5) / columns_per_second),
            middle,
            half_height,
        );

        // The beatgrid, with the first beat of every bar stronger.
        if let Some(grid) = &waveform.grid {
            for beat in grid.beats(start, end) {
                let (alpha, line) = if beat.is_bar_start() {
                    (0.9, 2.0)
                } else {
                    (0.4, 1.0)
                };
                let x = x_of(beat.time);
                frame.fill_rectangle(
                    Point::new(x - line / 2.0, 0.0),
                    Size::new(line, height),
                    Color {
                        a: alpha,
                        ..Color::WHITE
                    },
                );

                // The bar and the beat in it ("17.1" is the first beat of bar 17). The
                // first beat of a bar is brighter and larger.
                if let Some((bar, in_bar)) = beat.bar_and_beat() {
                    paint_beat_number(
                        &mut frame,
                        x,
                        format!("{bar}.{in_bar}"),
                        beat.is_bar_start(),
                    );
                }
            }
        }

        frame.fill_rectangle(
            Point::new(width / 2.0 - 1.0, 0.0),
            Size::new(2.0, height),
            PLAYHEAD,
        );
        vec![frame.into_geometry()]
    }
}

/// The whole track, with what has been played dimmed. Clicking jumps there.
struct Overview<'a> {
    waveform: &'a Waveform,
    clock: Clock,
    colors: WaveformColors,
}

impl canvas::Program<Message> for Overview<'_> {
    type State = ();

    fn update(
        &self,
        _state: &mut (),
        event: &Event,
        bounds: Rectangle,
        cursor: mouse::Cursor,
    ) -> Option<Action<Message>> {
        match event {
            Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)) => {
                let point = cursor.position_in(bounds)?;
                let seconds = f64::from(point.x / bounds.width) * self.waveform.duration;
                Some(Action::publish(Message::SeekTo(seconds as f32)).and_capture())
            }
            Event::Window(window::Event::RedrawRequested(now)) if self.clock.is_running() => {
                Some(Action::request_redraw_at(*now + OVERVIEW_REDRAW))
            }
            _ => None,
        }
    }

    fn draw(
        &self,
        _state: &(),
        renderer: &Renderer,
        _theme: &Theme,
        bounds: Rectangle,
        _cursor: mouse::Cursor,
    ) -> Vec<Geometry> {
        let mut frame = Frame::new(renderer, bounds.size());
        frame.fill_rectangle(Point::ORIGIN, bounds.size(), BACKGROUND);

        let waveform = self.waveform;
        let (width, height) = (bounds.width, bounds.height);
        let middle = height / 2.0;
        let bin_width = width / waveform.overview.len().max(1) as f32;
        paint_waveform(
            &mut frame,
            self.colors,
            &waveform.overview,
            0..waveform.overview.len(),
            |index| (index as f32 + 0.5) * bin_width,
            middle,
            middle - 2.0,
        );

        let progress = (self.clock.position(Instant::now(), waveform.duration)
            / waveform.duration.max(f64::EPSILON)) as f32;
        let played = (progress * width).clamp(0.0, width);
        frame.fill_rectangle(
            Point::ORIGIN,
            Size::new(played, height),
            Color {
                a: 0.5,
                ..BACKGROUND
            },
        );
        frame.fill_rectangle(
            Point::new((played - 1.0).max(0.0), 0.0),
            Size::new(2.0, height),
            PLAYHEAD,
        );
        vec![frame.into_geometry()]
    }

    fn mouse_interaction(
        &self,
        _state: &(),
        bounds: Rectangle,
        cursor: mouse::Cursor,
    ) -> mouse::Interaction {
        if cursor.is_over(bounds) {
            mouse::Interaction::Pointer
        } else {
            mouse::Interaction::default()
        }
    }
}
