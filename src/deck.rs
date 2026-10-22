//! The waveform display of the track in the player, like a DJ deck's: a zoomed,
//! scrolling waveform around the playhead with the beatgrid drawn over it, and the
//! whole track below it to see where you are and to jump to somewhere else.
//!
//! The waveform is drawn in three layers, the low frequencies red, the mids green and
//! the highs blue, the way Engine's tri-band waveform shows them.

use std::time::{Duration, Instant};

use iced::mouse;
use iced::widget::canvas::{Action, Frame, Geometry};
use iced::widget::{canvas, column, container, text};
use iced::{Color, Element, Event, Fill, Point, Rectangle, Renderer, Size, Theme, window};

use crate::Message;
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

/// Where the player was, and when we heard it: between the backend's updates (a few a
/// second) the position is worked out from the time that has passed, so the waveform
/// scrolls smoothly.
#[derive(Debug, Clone, Copy)]
pub struct Anchor {
    /// Seconds into the track.
    pub position: f64,
    pub at: Instant,
    pub playing: bool,
}

impl Anchor {
    fn now(&self, duration: f64) -> f64 {
        let moved = if self.playing {
            self.at.elapsed().as_secs_f64()
        } else {
            0.0
        };
        (self.position + moved).clamp(0.0, duration)
    }
}

pub fn view<'a>(display: Display<'a>, anchor: Anchor, language: Language) -> Element<'a, Message> {
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
        canvas(Zoomed { waveform, anchor })
            .width(Fill)
            .height(ZOOM_HEIGHT),
        canvas(Overview { waveform, anchor })
            .width(Fill)
            .height(OVERVIEW_HEIGHT),
        text(grid).size(13).style(text::secondary),
    ]
    .spacing(4)
    .into()
}

/// Draws the bands of one column as three centred bars over each other, the loudest
/// usually the low one at the back.
fn paint_bands(frame: &mut Frame, x: f32, width: f32, middle: f32, half_height: f32, bands: Bands) {
    for (level, color) in bands.into_iter().zip([LOW, MID, HIGH]) {
        let half = f32::from(level) / 255.0 * half_height;
        if half >= 0.5 {
            frame.fill_rectangle(
                Point::new(x, middle - half),
                Size::new(width, half * 2.0),
                color,
            );
        }
    }
}

/// The scrolling waveform around the playhead.
struct Zoomed<'a> {
    waveform: &'a Waveform,
    anchor: Anchor,
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
        (self.anchor.playing && matches!(event, Event::Window(window::Event::RedrawRequested(_))))
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

        let now = self.anchor.now(waveform.duration);
        let pixels_per_second = f64::from(width) / VISIBLE_SECONDS;
        let start = now - VISIBLE_SECONDS / 2.0;
        let end = now + VISIBLE_SECONDS / 2.0;
        let x_of = |time: f64| ((time - start) * pixels_per_second) as f32;

        // The waveform. A little extra width keeps rounding from leaving gaps.
        let columns_per_second = waveform.columns_per_second;
        let column_width = (pixels_per_second / columns_per_second) as f32 + 0.6;
        let first = (start * columns_per_second).floor().max(0.0) as usize;
        let last =
            ((end * columns_per_second).ceil().max(0.0) as usize).min(waveform.columns.len());
        for index in first..last {
            let x = x_of(index as f64 / columns_per_second);
            paint_bands(
                &mut frame,
                x,
                column_width,
                middle,
                half_height,
                waveform.columns[index],
            );
        }

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
    anchor: Anchor,
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
            Event::Window(window::Event::RedrawRequested(now)) if self.anchor.playing => {
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

        for (index, bands) in waveform.overview.iter().enumerate() {
            paint_bands(
                &mut frame,
                index as f32 * bin_width,
                bin_width + 0.6,
                middle,
                middle - 2.0,
                *bands,
            );
        }

        let progress =
            (self.anchor.now(waveform.duration) / waveform.duration.max(f64::EPSILON)) as f32;
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
