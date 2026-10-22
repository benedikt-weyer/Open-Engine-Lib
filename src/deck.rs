//! The waveform display of the track in the player, like a DJ deck's: a zoomed,
//! scrolling waveform around the playhead with the beatgrid drawn over it, and the
//! whole track below it to see where you are and to jump to somewhere else.
//!
//! The waveform is drawn in three layers, the low frequencies red, the mids green and
//! the highs blue, the way Engine's tri-band waveform shows them.

use std::time::{Duration, Instant};

use iced::mouse;
use std::ops::Range;

use iced::widget::canvas::{Action, Frame, Geometry, Path};
use iced::widget::{canvas, column, container, text};
use iced::{Color, Element, Event, Fill, Point, Rectangle, Renderer, Size, Theme, window};

use crate::Message;
use crate::clock::Clock;
use crate::config::Language;
use crate::i18n::fill;
use crate::waveform::{Bands, Display, Waveform};

/// How much of the track the zoomed waveform shows.
const VISIBLE_SECONDS: f64 = 6.0;
const ZOOM_HEIGHT: f32 = 110.0;
const OVERVIEW_HEIGHT: f32 = 44.0;
/// The overview only needs to follow the playhead a few times a second.
const OVERVIEW_REDRAW: Duration = Duration::from_millis(100);

const BACKGROUND: Color = Color::from_rgb(0.06, 0.07, 0.09);
const LOW: Color = Color::from_rgb(0.90, 0.28, 0.24);
const MID: Color = Color::from_rgb(0.26, 0.76, 0.38);
const HIGH: Color = Color::from_rgb(0.38, 0.62, 1.0);
const PLAYHEAD: Color = Color::WHITE;

pub fn view<'a>(display: Display<'a>, clock: Clock, language: Language) -> Element<'a, Message> {
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

    column![
        canvas(Zoomed { waveform, clock })
            .width(Fill)
            .height(ZOOM_HEIGHT),
        canvas(Overview { waveform, clock })
            .width(Fill)
            .height(OVERVIEW_HEIGHT),
        text(grid).size(13).style(text::secondary),
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

    for (band, color) in [LOW, MID, HIGH].into_iter().enumerate() {
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

/// The scrolling waveform around the playhead.
struct Zoomed<'a> {
    waveform: &'a Waveform,
    clock: Clock,
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
                frame.fill_rectangle(
                    Point::new(x_of(beat.time) - line / 2.0, 0.0),
                    Size::new(line, height),
                    Color {
                        a: alpha,
                        ..Color::WHITE
                    },
                );
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
