//! The waveform display of the track in the player, like a DJ deck's: a zoomed,
//! scrolling waveform around the playhead with the beatgrid, the loops and the cues drawn
//! over it, and the whole track below it (with them too) to see where you are and to jump to
//! somewhere else.
//!
//! How the waveform is colored is a setting (`WaveformColors`) that can also be changed
//! from a menu next to the display.

use std::cell::RefCell;
use std::ops::Range;
use std::time::{Duration, Instant};

use iced::advanced::graphics::geometry::Renderer as GeometryRenderer;
use iced::advanced::renderer::Quad;
use iced::advanced::widget::{Tree, Widget, tree};
use iced::advanced::{
    Clipboard, Layout, Renderer as AdvancedRenderer, Shell, layout, mouse, renderer,
};
use iced::widget::canvas::{Action, Frame, Geometry, Path, Text};
use iced::widget::{canvas, column, container, pick_list, row, text};
use iced::{
    Color, Element, Event, Fill, Length, Pixels, Point, Rectangle, Renderer, Size, Theme, Vector,
    window,
};

use crate::Message;
use crate::clock::Clock;
use crate::colors::{self, STACKED_LAYERS};
use crate::config::{Language, WaveformColors};
use crate::cues::{self, Cue};
use crate::i18n::fill;
use crate::loops::{self, Loop};
use crate::waveform::{Bands, Display, Serial, Waveform};

/// How much of the track the zoomed waveform shows.
const VISIBLE_SECONDS: f64 = 6.0;
const ZOOM_HEIGHT: f32 = 110.0;
const OVERVIEW_HEIGHT: f32 = 44.0;
/// How much of the track one cached piece of the zoomed waveform holds. A piece is drawn
/// (the outline built, its triangles made) once, and then only moved as the track plays.
const PIECE_SECONDS: f64 = 12.0;
/// Room around a piece for what is drawn a little over its edge (half a beat line).
const PIECE_MARGIN: f32 = 4.0;
/// How far (in pixels) the pointer has to move with the button down before the waveform is
/// taken to be dragged, so that a click that slips a little does not hold the track.
const DRAG_SLOP: f32 = 3.0;
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

/// What is marked on the track: its cues and saved loops.
#[derive(Clone, Copy)]
pub struct Marks<'a> {
    pub cues: &'a [Cue],
    pub loops: &'a [Loop],
}

pub fn view<'a>(
    display: Display<'a>,
    marks: Marks<'a>,
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
        Element::from(ScrollingWaveform {
            waveform,
            marks,
            clock,
            colors
        }),
        canvas(Overview {
            waveform,
            marks,
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

/// What the zoomed waveform keeps between frames: the pieces of the track it has drawn,
/// and which waveform and colors they are of.
#[derive(Default)]
struct ZoomState {
    of: RefCell<Option<(Serial, WaveformColors)>>,
    /// By piece: the geometry of `PIECE_SECONDS` of the track (outline and beatgrid),
    /// made when the piece first comes into view.
    pieces: RefCell<Vec<canvas::Cache>>,
    /// The button is down on the waveform: dragging it scratches the track.
    drag: Option<Drag>,
}

/// A drag of the zoomed waveform, in pixels from its left edge.
#[derive(Debug, Clone, Copy)]
struct Drag {
    /// Where the button went down.
    start: f32,
    /// Where the pointer was when the track was last moved.
    at: f32,
    /// Moved far enough to scratch.
    scratching: bool,
}

/// How many seconds of the track the waveform moves under the playhead when it is dragged
/// by `pixels` to the right: the track goes back, as a record turned by hand would.
fn scratched_seconds(pixels: f32, pixels_per_second: f64) -> f64 {
    -f64::from(pixels) / pixels_per_second
}

/// The scrolling waveform around the playhead. Dragging it with the mouse scratches the
/// track: the waveform follows the pointer and the track is heard going forwards and
/// backwards with it.
///
/// Drawing the outline of a few seconds of waveform means building and tessellating
/// thousands of points, which done anew for every frame took up a fifth of the time
/// between two frames and made frames late now and then. Instead the track is cut into
/// pieces that are drawn once each, kept as geometry, and moved to where the playhead is
/// with the renderer's translation, which costs nothing (the triangles stay on the GPU).
struct ScrollingWaveform<'a> {
    waveform: &'a Waveform,
    marks: Marks<'a>,
    clock: Clock,
    colors: WaveformColors,
}

impl<'a> Widget<Message, Theme, Renderer> for ScrollingWaveform<'a> {
    fn tag(&self) -> tree::Tag {
        tree::Tag::of::<ZoomState>()
    }

    fn state(&self) -> tree::State {
        tree::State::new(ZoomState::default())
    }

    fn size(&self) -> Size<Length> {
        Size::new(Length::Fill, Length::Fixed(ZOOM_HEIGHT))
    }

    fn layout(
        &mut self,
        _tree: &mut Tree,
        _renderer: &Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        layout::atomic(limits, Length::Fill, Length::Fixed(ZOOM_HEIGHT))
    }

    fn update(
        &mut self,
        tree: &mut Tree,
        event: &Event,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        _renderer: &Renderer,
        _clipboard: &mut dyn Clipboard,
        shell: &mut Shell<'_, Message>,
        _viewport: &Rectangle,
    ) {
        let bounds = layout.bounds();
        let pixels_per_second = f64::from(bounds.width) / VISIBLE_SECONDS;
        let state = tree.state.downcast_mut::<ZoomState>();
        match event {
            Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)) => {
                if let Some(point) = cursor.position_in(bounds) {
                    state.drag = Some(Drag {
                        start: point.x,
                        at: point.x,
                        scratching: false,
                    });
                    shell.capture_event();
                }
            }
            Event::Mouse(mouse::Event::CursorMoved { position }) => {
                if let Some(drag) = &mut state.drag {
                    // (Also when the pointer is off the waveform: the drag goes on.)
                    let x = position.x - bounds.x;
                    if !drag.scratching && (x - drag.start).abs() >= DRAG_SLOP {
                        drag.scratching = true;
                        shell.publish(Message::ScratchStart);
                    }
                    if drag.scratching {
                        let seconds = scratched_seconds(x - drag.at, pixels_per_second);
                        drag.at = x;
                        if seconds != 0.0 {
                            shell.publish(Message::ScratchBy(seconds));
                        }
                        shell.capture_event();
                    }
                }
            }
            Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)) => {
                if let Some(drag) = state.drag.take()
                    && drag.scratching
                {
                    shell.publish(Message::ScratchEnd);
                    shell.capture_event();
                }
            }
            _ => {}
        }

        // While playing, draw the next frame as soon as the last one is out.
        if self.clock.is_running()
            && matches!(event, Event::Window(window::Event::RedrawRequested(_)))
        {
            shell.request_redraw();
        }
    }

    fn mouse_interaction(
        &self,
        tree: &Tree,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        _viewport: &Rectangle,
        _renderer: &Renderer,
    ) -> mouse::Interaction {
        let state = tree.state.downcast_ref::<ZoomState>();
        if state.drag.is_some_and(|drag| drag.scratching) {
            mouse::Interaction::Grabbing
        } else if cursor.is_over(layout.bounds()) {
            mouse::Interaction::Grab
        } else {
            mouse::Interaction::default()
        }
    }

    fn draw(
        &self,
        tree: &Tree,
        renderer: &mut Renderer,
        _theme: &Theme,
        _style: &renderer::Style,
        layout: Layout<'_>,
        _cursor: mouse::Cursor,
        _viewport: &Rectangle,
    ) {
        let bounds = layout.bounds();
        if bounds.width < 1.0 || bounds.height < 1.0 {
            return;
        }
        let state = tree.state.downcast_ref::<ZoomState>();
        let waveform = self.waveform;

        // Another track, or other colors: nothing that was drawn is of any use.
        let of = (waveform.serial, self.colors);
        if state.of.borrow().is_none_or(|known| known != of) {
            state.pieces.borrow_mut().clear();
            *state.of.borrow_mut() = Some(of);
        }

        let now = self.clock.position(Instant::now(), waveform.duration);
        let pixels_per_second = f64::from(bounds.width) / VISIBLE_SECONDS;
        let start = now - VISIBLE_SECONDS / 2.0;
        let end = now + VISIBLE_SECONDS / 2.0;

        let piece_count = (waveform.duration / PIECE_SECONDS).ceil().max(1.0) as usize;
        let first = (start / PIECE_SECONDS).floor().max(0.0) as usize;
        let last = ((end / PIECE_SECONDS).floor().max(0.0) as usize).min(piece_count - 1);
        let piece_size = Size::new(
            (PIECE_SECONDS * pixels_per_second) as f32 + 2.0 * PIECE_MARGIN + 4.0,
            bounds.height,
        );

        // Under the pieces: the background.
        renderer.with_layer(bounds, |renderer| {
            renderer.fill_quad(
                Quad {
                    bounds,
                    ..Quad::default()
                },
                BACKGROUND,
            );
            for piece in first..=last {
                let geometry = {
                    let mut pieces = state.pieces.borrow_mut();
                    if pieces.len() <= piece {
                        pieces.resize_with(piece + 1, canvas::Cache::new);
                    }
                    pieces[piece].draw(renderer, piece_size, |frame| {
                        paint_piece(
                            frame,
                            waveform,
                            self.colors,
                            piece,
                            pixels_per_second,
                            bounds.height,
                        );
                    })
                };
                // Where the piece starts, which is further left the further the track has
                // played.
                let x = ((piece as f64 * PIECE_SECONDS - start) * pixels_per_second) as f32
                    - PIECE_MARGIN;
                renderer.with_translation(Vector::new(bounds.x + x, bounds.y), |renderer| {
                    renderer.draw_geometry(geometry);
                });
            }
        });

        // Over them: the cues (a few lines, so they are drawn anew every frame, which lets
        // them change without the pieces being made again), and the playhead, in the middle.
        renderer.with_layer(bounds, |renderer| {
            let mut frame = Frame::new(renderer, bounds.size());
            let mut any = false;
            for saved in self.marks.loops {
                let from = ((saved.start - start) * pixels_per_second) as f32;
                let to = ((saved.end - start) * pixels_per_second) as f32;
                // (The flag reaches to the right of the loop's start, like a cue's.)
                if to > 0.0 && from < bounds.width {
                    paint_loop(&mut frame, saved, from, to, bounds.size());
                    any = true;
                }
            }
            for cue in self.marks.cues {
                let x = ((cue.position - start) * pixels_per_second) as f32;
                // (A flag reaches to the right of its line, so a cue just off the left edge
                // can still show.)
                if x > -CUE_FLAG_REACH && x < bounds.width + 2.0 {
                    paint_cue(&mut frame, cue, x, bounds.height);
                    any = true;
                }
            }
            if any {
                renderer.with_translation(Vector::new(bounds.x, bounds.y), |renderer| {
                    renderer.draw_geometry(frame.into_geometry());
                });
            }

            renderer.fill_quad(
                Quad {
                    bounds: Rectangle::new(
                        Point::new(bounds.x + bounds.width / 2.0 - 1.0, bounds.y),
                        Size::new(2.0, bounds.height),
                    ),
                    ..Quad::default()
                },
                PLAYHEAD,
            );
        });
    }
}

impl<'a> From<ScrollingWaveform<'a>> for Element<'a, Message> {
    fn from(widget: ScrollingWaveform<'a>) -> Self {
        Element::new(widget)
    }
}

/// Draws one piece of the track: `PIECE_SECONDS` from `piece * PIECE_SECONDS` on, with
/// the first of it `PIECE_MARGIN` from the left. A piece ends with the column the next one
/// starts with, so that they join without a gap or an overlap.
fn paint_piece(
    frame: &mut Frame,
    waveform: &Waveform,
    colors: WaveformColors,
    piece: usize,
    pixels_per_second: f64,
    height: f32,
) {
    let origin = piece as f64 * PIECE_SECONDS;
    let columns_per_second = waveform.columns_per_second;
    let x_of = |time: f64| PIECE_MARGIN + ((time - origin) * pixels_per_second) as f32;
    let middle = height / 2.0;

    let per_piece = (PIECE_SECONDS * columns_per_second).round() as usize;
    let first = piece * per_piece;
    let last = ((piece + 1) * per_piece + 1).min(waveform.columns.len());
    paint_waveform(
        frame,
        colors,
        &waveform.columns,
        first..last,
        // The middle of a column's 10 ms.
        |index| x_of((index as f64 + 0.5) / columns_per_second),
        middle,
        middle - 4.0,
    );

    // The beatgrid, with the first beat of every bar stronger. (A beat belongs to the piece
    // it falls in.)
    if let Some(grid) = &waveform.grid {
        for beat in grid.beats(origin, origin + PIECE_SECONDS) {
            if beat.time < origin || beat.time >= origin + PIECE_SECONDS {
                continue;
            }
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

            // The bar and the beat in it ("17.1" is the first beat of bar 17). The first
            // beat of a bar is brighter and larger.
            if let Some((bar, in_bar)) = beat.bar_and_beat() {
                paint_beat_number(frame, x, format!("{bar}.{in_bar}"), beat.is_bar_start());
            }
        }
    }
}

/// How far to the right of its line a cue's flag can reach.
const CUE_FLAG_REACH: f32 = 200.0;

/// A cue on the zoomed waveform: a line, and at the bottom a flag with the pad's number
/// and the name (when the cue has one other than Engine's own "Cue N").
fn paint_cue(frame: &mut Frame, cue: &Cue, x: f32, height: f32) {
    let color = cue.color();
    frame.fill_rectangle(
        Point::new(x - 1.0, 0.0),
        Size::new(2.0, height),
        Color { a: 0.9, ..color },
    );

    let name = if cue.label == cues::default_label(cue.slot) {
        String::new()
    } else {
        cue.label.clone()
    };
    let label = if name.is_empty() {
        (cue.slot + 1).to_string()
    } else {
        format!("{} {name}", cue.slot + 1)
    };
    let size = 12.0;
    // About as wide as the text is, which is a bit over half its size per character.
    let width = (label.chars().count() as f32 * size * 0.6 + 8.0).min(CUE_FLAG_REACH);
    let origin = Point::new(x + 1.0, height - size - 8.0);
    frame.fill_rectangle(origin, Size::new(width, size + 6.0), color);
    frame.fill_text(Text {
        content: label,
        position: Point::new(origin.x + 4.0, origin.y + 3.0),
        color: cues::text_on(color),
        size: Pixels(size),
        ..Text::default()
    });
}

/// A loop on the zoomed waveform: the stretch it covers tinted in its color, a line at each
/// end, and above the cues' flags one with "L" and the pad's number and the name (left out
/// when it is Engine's default `Loop N`). `from` and `to` are the ends' horizontal positions,
/// which can be off to either side.
fn paint_loop(frame: &mut Frame, saved: &Loop, from: f32, to: f32, size: Size) {
    let color = saved.color();
    let left = from.max(0.0);
    let right = to.min(size.width);
    frame.fill_rectangle(
        Point::new(left, 0.0),
        Size::new((right - left).max(0.0), size.height),
        Color { a: 0.22, ..color },
    );
    for (x, visible) in [(from, from >= 0.0), (to, to <= size.width)] {
        if visible {
            frame.fill_rectangle(
                Point::new(x - 1.0, 0.0),
                Size::new(2.0, size.height),
                Color { a: 0.9, ..color },
            );
        }
    }

    let name = if saved.label == loops::default_label(saved.slot) {
        String::new()
    } else {
        saved.label.clone()
    };
    let label = if name.is_empty() {
        format!("L{}", saved.slot + 1)
    } else {
        format!("L{} {name}", saved.slot + 1)
    };
    let text_size = 12.0;
    let width = (label.chars().count() as f32 * text_size * 0.6 + 8.0).min(CUE_FLAG_REACH);
    let origin = Point::new(left + 1.0, size.height - 2.0 * (text_size + 8.0));
    frame.fill_rectangle(origin, Size::new(width, text_size + 6.0), color);
    frame.fill_text(Text {
        content: label,
        position: Point::new(origin.x + 4.0, origin.y + 3.0),
        color: cues::text_on(color),
        size: Pixels(text_size),
        ..Text::default()
    });
}

/// The whole track, with what has been played dimmed. Clicking jumps there.
struct Overview<'a> {
    waveform: &'a Waveform,
    marks: Marks<'a>,
    clock: Clock,
    colors: WaveformColors,
}

/// What the overview keeps between frames: the waveform it drew, and which one that was.
#[derive(Default)]
struct OverviewState {
    of: RefCell<Option<(Serial, WaveformColors, Size)>>,
    base: canvas::Cache,
}

impl canvas::Program<Message> for Overview<'_> {
    type State = OverviewState;

    fn update(
        &self,
        _state: &mut OverviewState,
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
        state: &OverviewState,
        renderer: &Renderer,
        _theme: &Theme,
        bounds: Rectangle,
        _cursor: mouse::Cursor,
    ) -> Vec<Geometry> {
        let waveform = self.waveform;
        let (width, height) = (bounds.width, bounds.height);

        // The whole track does not change while it plays: it is drawn once (again for another
        // track, other colors or another size), and only what is over it is drawn per frame.
        let of = (waveform.serial, self.colors, bounds.size());
        if state.of.borrow().is_none_or(|known| known != of) {
            state.base.clear();
            *state.of.borrow_mut() = Some(of);
        }
        let base = state.base.draw(renderer, bounds.size(), |frame| {
            frame.fill_rectangle(Point::ORIGIN, bounds.size(), BACKGROUND);
            let middle = height / 2.0;
            let bin_width = width / waveform.overview.len().max(1) as f32;
            paint_waveform(
                frame,
                self.colors,
                &waveform.overview,
                0..waveform.overview.len(),
                |index| (index as f32 + 0.5) * bin_width,
                middle,
                middle - 2.0,
            );
        });

        let mut frame = Frame::new(renderer, bounds.size());
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
        // The loops: the stretch tinted, with a bar along the bottom, in the pad's color.
        let duration = waveform.duration.max(f64::EPSILON);
        for saved in self.marks.loops {
            let from = ((saved.start / duration) as f32 * width).clamp(0.0, width);
            let to = ((saved.end / duration) as f32 * width).clamp(0.0, width);
            let color = saved.color();
            frame.fill_rectangle(
                Point::new(from, 0.0),
                Size::new((to - from).max(2.0), height),
                Color { a: 0.28, ..color },
            );
            frame.fill_rectangle(
                Point::new(from, height - 4.0),
                Size::new((to - from).max(2.0), 4.0),
                color,
            );
        }
        // The cues: a line with a small square at the top, in the pad's color.
        for cue in self.marks.cues {
            let x = (cue.position / waveform.duration.max(f64::EPSILON)) as f32 * width;
            let color = cue.color();
            frame.fill_rectangle(
                Point::new((x - 1.0).min(width - 2.0).max(0.0), 0.0),
                Size::new(2.0, height),
                Color { a: 0.85, ..color },
            );
            frame.fill_rectangle(
                Point::new((x - 3.0).min(width - 6.0).max(0.0), 0.0),
                Size::new(6.0, 6.0),
                color,
            );
        }
        frame.fill_rectangle(
            Point::new((played - 1.0).max(0.0), 0.0),
            Size::new(2.0, height),
            PLAYHEAD,
        );
        vec![base, frame.into_geometry()]
    }

    fn mouse_interaction(
        &self,
        _state: &OverviewState,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dragging_the_waveform_right_takes_the_track_back() {
        // 6 seconds across 600 pixels: 100 pixels a second.
        assert_eq!(scratched_seconds(50.0, 100.0), -0.5);
        assert_eq!(scratched_seconds(-100.0, 100.0), 1.0);
        assert_eq!(scratched_seconds(0.0, 100.0), 0.0);
    }
}
