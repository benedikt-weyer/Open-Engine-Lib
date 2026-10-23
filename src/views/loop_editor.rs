//! The window for editing the saved loops of the track in the player: for each of the eight
//! pads the name, the start, the end and the color, with buttons to take the start or the
//! end from the playhead, to jump to the start, and to remove the loop. Nothing is written
//! until "Save".

use iced::widget::{
    Space, button, center, column, container, opaque, row, scrollable, text, text_input, tooltip,
};
use iced::{Background, Border, Center, Color, Element, Fill, Theme};

use crate::Message;
use crate::config::Language;
use crate::cues::{self, PALETTE, SLOTS};
use crate::error_text;
use crate::i18n::fill;
use crate::loops::Loop;

const WIDTH: f32 = 860.0;
const SWATCH: f32 = 16.0;

/// A pad that has a loop, as it is being edited: the positions are the text typed.
#[derive(Debug, Clone, PartialEq)]
pub struct Row {
    pub label: String,
    pub start: String,
    pub end: String,
    pub color: u32,
}

/// What the user did in the window. (Taking a position from the playhead and jumping there
/// need the player, so the app does those.)
#[derive(Debug, Clone)]
pub enum Event {
    Label(usize, String),
    Start(usize, String),
    End(usize, String),
    Color(usize, u32),
    /// Give the loop the playhead's position as its start.
    StartToPlayhead(usize),
    /// Give the loop the playhead's position as its end.
    EndToPlayhead(usize),
    /// Jump to the start of the loop.
    Go(usize),
    Remove(usize),
    /// Set a loop on an empty pad, at the playhead, `length` seconds long.
    Add(usize),
}

/// What is wrong with the loops as typed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Problem {
    /// A position that could not be read: the pad (0 to 7) and what was typed.
    BadPosition { slot: usize, text: String },
    /// A loop that does not end after it starts.
    BadRange { slot: usize },
}

#[derive(Debug, Clone)]
pub struct Editor {
    /// The track whose loops these are (the backend's id).
    pub track: u32,
    pub title: String,
    rows: [Option<Row>; SLOTS],
    /// How long a loop added from an empty pad is, in seconds.
    length: f64,
    /// Why the last attempt failed (or what is wrong).
    pub error: Option<String>,
    /// Waiting for the backend to write it.
    pub busy: bool,
}

impl Editor {
    /// `length` is how long a loop added in the window is, in seconds.
    pub fn new(track: u32, title: &str, saved: &[Loop], length: f64) -> Self {
        let mut rows: [Option<Row>; SLOTS] = Default::default();
        for one in saved {
            if let Some(row) = rows.get_mut(one.slot) {
                *row = Some(Row {
                    label: one.label.clone(),
                    start: cues::format_position(one.start),
                    end: cues::format_position(one.end),
                    color: one.color,
                });
            }
        }
        Self {
            track,
            title: title.to_string(),
            rows,
            length,
            error: None,
            busy: false,
        }
    }

    /// Applies something the user did; `playhead` is where the player is, in seconds.
    pub fn update(&mut self, event: Event, playhead: f64) {
        self.error = None;
        let row = |rows: &mut [Option<Row>; SLOTS], slot: usize| -> Option<usize> {
            rows.get(slot).and_then(Option::as_ref).map(|_| slot)
        };
        match event {
            Event::Label(slot, label) => {
                if let Some(slot) = row(&mut self.rows, slot) {
                    self.rows[slot].as_mut().expect("checked").label = label;
                }
            }
            Event::Start(slot, start) => {
                if let Some(slot) = row(&mut self.rows, slot) {
                    self.rows[slot].as_mut().expect("checked").start = start;
                }
            }
            Event::End(slot, end) => {
                if let Some(slot) = row(&mut self.rows, slot) {
                    self.rows[slot].as_mut().expect("checked").end = end;
                }
            }
            Event::Color(slot, color) => {
                if let Some(slot) = row(&mut self.rows, slot) {
                    self.rows[slot].as_mut().expect("checked").color = color;
                }
            }
            Event::StartToPlayhead(slot) => {
                if let Some(slot) = row(&mut self.rows, slot) {
                    self.rows[slot].as_mut().expect("checked").start =
                        cues::format_position(playhead);
                }
            }
            Event::EndToPlayhead(slot) => {
                if let Some(slot) = row(&mut self.rows, slot) {
                    self.rows[slot].as_mut().expect("checked").end =
                        cues::format_position(playhead);
                }
            }
            // (Jumping changes nothing in the window.)
            Event::Go(_) => {}
            Event::Remove(slot) => {
                if let Some(row) = self.rows.get_mut(slot) {
                    *row = None;
                }
            }
            Event::Add(slot) => {
                if let Some(row @ None) = self.rows.get_mut(slot) {
                    let one = Loop::new(slot, playhead, playhead + self.length);
                    *row = Some(Row {
                        label: one.label,
                        start: cues::format_position(one.start),
                        end: cues::format_position(one.end),
                        color: one.color,
                    });
                }
            }
        }
    }

    /// Where the loop of `slot` starts as typed, if there is one and it can be read.
    pub fn start_of(&self, slot: usize) -> Option<f64> {
        let row = self.rows.get(slot)?.as_ref()?;
        cues::parse_position(&row.start)
    }

    /// The loops as edited, or the first thing that is wrong with them.
    pub fn loops(&self) -> Result<Vec<Loop>, Problem> {
        let mut saved = Vec::new();
        for (slot, row) in self.rows.iter().enumerate() {
            let Some(row) = row else { continue };
            let position = |text: &str| {
                cues::parse_position(text).ok_or_else(|| Problem::BadPosition {
                    slot,
                    text: text.to_string(),
                })
            };
            let (start, end) = (position(&row.start)?, position(&row.end)?);
            if end <= start {
                return Err(Problem::BadRange { slot });
            }
            saved.push(Loop {
                slot,
                label: row.label.clone(),
                start,
                end,
                color: row.color,
            });
        }
        Ok(saved)
    }
}

/// The window, centred over a dimmed view of the app that can't be clicked.
pub fn modal<'a>(editor: &'a Editor, language: Language) -> Element<'a, Message> {
    let strings = language.strings();

    let mut rows = column![].spacing(8);
    for (slot, row) in editor.rows.iter().enumerate() {
        rows = rows.push(match row {
            Some(row) => loop_row(slot, row, editor.busy, language),
            None => empty_row(slot, editor.busy, language),
        });
    }

    let mut body = column![
        text(fill(
            &strings.loop_editor_title,
            &[("title", &editor.title)]
        ))
        .size(24),
        scrollable(rows).height(iced::Shrink),
        text(&strings.loop_editor_note)
            .size(13)
            .style(text::secondary),
    ]
    .spacing(14);
    if let Some(error) = &editor.error {
        body = body.push(error_text::view(error.as_str(), language));
    }

    let save = button(text(if editor.busy {
        &strings.smart_saving
    } else {
        &strings.cue_editor_save
    }))
    .style(button::primary)
    .on_press_maybe((!editor.busy).then_some(Message::LoopEditorSave));
    let cancel = button(text(&strings.smart_cancel))
        .style(button::secondary)
        .on_press_maybe((!editor.busy).then_some(Message::LoopEditorCancel));
    body = body.push(row![Space::new().width(Fill), cancel, save].spacing(10));

    let window = container(body)
        .padding(20)
        .width(WIDTH)
        .style(container::rounded_box);

    // The dimmed backdrop swallows clicks, so nothing behind the window can be used.
    opaque(center(opaque(window)).style(|_: &Theme| {
        container::Style {
            background: Some(
                Color {
                    a: 0.6,
                    ..Color::BLACK
                }
                .into(),
            ),
            ..container::Style::default()
        }
    }))
}

/// The number of a pad, in a cell of a fixed width so the rows line up.
fn number<'a>(slot: usize) -> Element<'a, Message> {
    text((slot + 1).to_string())
        .size(14)
        .width(18)
        .style(text::secondary)
        .into()
}

fn empty_row<'a>(slot: usize, busy: bool, language: Language) -> Element<'a, Message> {
    let strings = language.strings();
    row![
        number(slot),
        text(&strings.cue_editor_empty)
            .style(text::secondary)
            .width(Fill),
        button(text(&strings.loop_editor_add).size(13))
            .padding([2, 8])
            .style(button::secondary)
            .on_press_maybe((!busy).then_some(Message::LoopEditor(Event::Add(slot)))),
    ]
    .spacing(8)
    .align_y(Center)
    .height(30)
    .into()
}

fn loop_row<'a>(
    slot: usize,
    saved: &'a Row,
    busy: bool,
    language: Language,
) -> Element<'a, Message> {
    let strings = language.strings();
    let edit = |event: fn(usize, String) -> Event| {
        move |value: String| Message::LoopEditor(event(slot, value))
    };

    let mut swatches = row![].spacing(2).align_y(Center);
    for color in PALETTE {
        swatches = swatches.push(swatch(slot, color, saved.color == color, busy));
    }

    // A small button with a symbol on it, which says in words what it does when pointed at.
    let icon = |symbol: &'a str, tip: &'a str, event: Event| -> Element<'a, Message> {
        tooltip(
            button(text(symbol).size(14).center())
                .width(32)
                .padding([2, 0])
                .style(button::secondary)
                .on_press_maybe((!busy).then_some(Message::LoopEditor(event))),
            text(tip).size(13),
            tooltip::Position::Top,
        )
        .style(container::rounded_box)
        .into()
    };
    let position = |placeholder: &'a str, value: &'a str, event: fn(usize, String) -> Event| {
        text_input(placeholder, value)
            .on_input_maybe((!busy).then(|| edit(event)))
            .size(14)
            .padding([4, 6])
            .width(96)
    };

    row![
        number(slot),
        swatches,
        text_input(&strings.cue_editor_name, &saved.label)
            .on_input_maybe((!busy).then(|| edit(Event::Label)))
            .size(14)
            .padding([4, 6])
            .width(Fill),
        position(&strings.loop_editor_start, &saved.start, Event::Start),
        icon(
            "⌖",
            &strings.loop_editor_start_to_playhead,
            Event::StartToPlayhead(slot)
        ),
        position(&strings.loop_editor_end, &saved.end, Event::End),
        icon(
            "⌖",
            &strings.loop_editor_end_to_playhead,
            Event::EndToPlayhead(slot)
        ),
        icon("▶", &strings.loop_editor_go, Event::Go(slot)),
        icon("✕", &strings.cue_editor_remove, Event::Remove(slot)),
    ]
    .spacing(8)
    .align_y(Center)
    .height(30)
    .into()
}

/// One color to choose from, with a ring around the one the loop has.
fn swatch<'a>(slot: usize, color: u32, chosen: bool, busy: bool) -> Element<'a, Message> {
    let fill_color = cues::argb(color);
    button(Space::new())
        .width(SWATCH)
        .height(SWATCH)
        .padding(0)
        .style(move |theme: &Theme, _| button::Style {
            background: Some(Background::Color(fill_color)),
            border: Border {
                color: if chosen {
                    theme.extended_palette().background.base.text
                } else {
                    Color::TRANSPARENT
                },
                width: 2.0,
                radius: 4.0.into(),
            },
            ..button::Style::default()
        })
        .on_press_maybe((!busy).then_some(Message::LoopEditor(Event::Color(slot, color))))
        .into()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn saved(slot: usize, label: &str, start: f64, end: f64) -> Loop {
        Loop {
            label: label.into(),
            ..Loop::new(slot, start, end)
        }
    }

    #[test]
    fn shows_the_loops_the_track_has_and_gives_them_back() {
        let original = [saved(1, "Intro", 12.5, 16.5), saved(6, "Drop", 90.0, 98.0)];
        let editor = Editor::new(4, "Song", &original, 2.0);
        assert_eq!(editor.loops().unwrap(), original);
        assert_eq!(editor.start_of(1), Some(12.5));
        assert_eq!(editor.start_of(0), None, "an empty pad has no start");
    }

    #[test]
    fn changes_are_made_to_the_pad_they_are_for() {
        let mut editor = Editor::new(4, "Song", &[saved(1, "Intro", 12.5, 16.5)], 2.0);
        editor.update(Event::Label(1, "Start".into()), 0.0);
        editor.update(Event::Start(1, "0:20".into()), 0.0);
        editor.update(Event::End(1, "0:24.5".into()), 0.0);
        editor.update(Event::Color(1, 0xff00_ff00), 0.0);
        // Changing an empty pad does nothing.
        editor.update(Event::Label(3, "x".into()), 0.0);
        editor.update(Event::StartToPlayhead(3), 9.0);
        let loops = editor.loops().unwrap();
        assert_eq!(loops.len(), 1);
        assert_eq!(
            (
                loops[0].label.as_str(),
                loops[0].start,
                loops[0].end,
                loops[0].color
            ),
            ("Start", 20.0, 24.5, 0xff00_ff00)
        );

        editor.update(Event::EndToPlayhead(1), 61.25);
        editor.update(Event::StartToPlayhead(1), 60.0);
        let loops = editor.loops().unwrap();
        assert_eq!((loops[0].start, loops[0].end), (60.0, 61.25));
    }

    #[test]
    fn a_loop_is_added_at_the_playhead_with_the_length_given_and_removed() {
        let mut editor = Editor::new(4, "Song", &[saved(1, "Intro", 12.5, 16.5)], 1.875);
        editor.update(Event::Add(4), 30.0);
        let added = &editor.loops().unwrap()[1];
        assert_eq!(
            (
                added.slot,
                added.label.as_str(),
                added.start,
                added.end,
                added.color
            ),
            (4, "Loop 5", 30.0, 31.875, PALETTE[4])
        );

        // Adding to a pad that has a loop changes nothing.
        editor.update(Event::Add(1), 99.0);
        assert_eq!(editor.start_of(1), Some(12.5));

        editor.update(Event::Remove(1), 0.0);
        assert_eq!(editor.loops().unwrap().len(), 1);
        editor.update(Event::Remove(1), 0.0);
        editor.update(Event::Remove(7), 0.0);
        assert_eq!(editor.loops().unwrap().len(), 1);
    }

    #[test]
    fn what_can_not_be_saved_is_said_before_anything_is_sent() {
        let mut editor = Editor::new(
            4,
            "Song",
            &[saved(0, "a", 1.0, 2.0), saved(2, "b", 3.0, 4.0)],
            2.0,
        );
        editor.update(Event::End(2, "soon".into()), 0.0);
        assert_eq!(
            editor.loops(),
            Err(Problem::BadPosition {
                slot: 2,
                text: "soon".into()
            })
        );
        editor.update(Event::End(2, "0:02".into()), 0.0);
        assert_eq!(editor.loops(), Err(Problem::BadRange { slot: 2 }));
        editor.update(Event::End(2, "0:03".into()), 0.0);
        assert_eq!(
            editor.loops(),
            Err(Problem::BadRange { slot: 2 }),
            "equal is empty"
        );
        editor.update(Event::End(2, "0:03.001".into()), 0.0);
        assert!(editor.loops().is_ok());
    }

    #[test]
    fn any_change_clears_the_last_error() {
        let mut editor = Editor::new(4, "Song", &[], 2.0);
        editor.error = Some("oops".into());
        editor.update(Event::Add(0), 1.0);
        assert_eq!(editor.error, None);
    }

    #[test]
    fn a_default_label_is_the_pads_own() {
        assert_eq!(crate::loops::default_label(0), "Loop 1");
    }
}
