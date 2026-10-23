//! The window for editing the cues of the track in the player: for each of the eight pads
//! the name, the position and the color, with buttons to take the position from the
//! playhead, to jump there, and to remove the cue. Nothing is written until "Save".

use iced::widget::{
    Space, button, center, column, container, opaque, row, scrollable, text, text_input, tooltip,
};
use iced::{Background, Border, Center, Color, Element, Fill, Theme};

use crate::Message;
use crate::config::Language;
use crate::cues::{self, Cue, PALETTE, SLOTS};
use crate::error_text;
use crate::i18n::fill;

const WIDTH: f32 = 700.0;
const SWATCH: f32 = 18.0;

/// A pad that has a cue, as it is being edited: the position is the text typed.
#[derive(Debug, Clone, PartialEq)]
pub struct Row {
    pub label: String,
    pub position: String,
    pub color: u32,
}

/// What the user did in the window. (Taking the position from the playhead and jumping
/// there need the player, so the app does those.)
#[derive(Debug, Clone)]
pub enum Event {
    Label(usize, String),
    Position(usize, String),
    Color(usize, u32),
    /// Give the cue the playhead's position.
    ToPlayhead(usize),
    /// Jump to the cue.
    Go(usize),
    Remove(usize),
    /// Set a cue on an empty pad, at the playhead.
    Add(usize),
}

/// A position that could not be read: the pad (0 to 7) and what was typed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BadPosition {
    pub slot: usize,
    pub text: String,
}

#[derive(Debug, Clone)]
pub struct Editor {
    /// The track whose cues these are (the backend's id).
    pub track: u32,
    pub title: String,
    rows: [Option<Row>; SLOTS],
    /// Why the last attempt failed (or what is wrong).
    pub error: Option<String>,
    /// Waiting for the backend to write it.
    pub busy: bool,
}

impl Editor {
    pub fn new(track: u32, title: &str, cues: &[Cue]) -> Self {
        let mut rows: [Option<Row>; SLOTS] = Default::default();
        for cue in cues {
            if let Some(row) = rows.get_mut(cue.slot) {
                *row = Some(Row {
                    label: cue.label.clone(),
                    position: cues::format_position(cue.position),
                    color: cue.color,
                });
            }
        }
        Self {
            track,
            title: title.to_string(),
            rows,
            error: None,
            busy: false,
        }
    }

    /// Applies something the user did; `playhead` is where the player is, in seconds.
    pub fn update(&mut self, event: Event, playhead: f64) {
        self.error = None;
        match event {
            Event::Label(slot, label) => {
                if let Some(Some(row)) = self.rows.get_mut(slot) {
                    row.label = label;
                }
            }
            Event::Position(slot, position) => {
                if let Some(Some(row)) = self.rows.get_mut(slot) {
                    row.position = position;
                }
            }
            Event::Color(slot, color) => {
                if let Some(Some(row)) = self.rows.get_mut(slot) {
                    row.color = color;
                }
            }
            Event::ToPlayhead(slot) => {
                if let Some(Some(row)) = self.rows.get_mut(slot) {
                    row.position = cues::format_position(playhead);
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
                    let cue = Cue::new(slot, playhead);
                    *row = Some(Row {
                        label: cue.label,
                        position: cues::format_position(cue.position),
                        color: cue.color,
                    });
                }
            }
        }
    }

    /// Where the cue of `slot` is as typed, if there is one and it can be read.
    pub fn position_of(&self, slot: usize) -> Option<f64> {
        let row = self.rows.get(slot)?.as_ref()?;
        cues::parse_position(&row.position)
    }

    /// The cues as edited, or the first position that can't be read.
    pub fn cues(&self) -> Result<Vec<Cue>, BadPosition> {
        let mut cues = Vec::new();
        for (slot, row) in self.rows.iter().enumerate() {
            let Some(row) = row else { continue };
            let position = cues::parse_position(&row.position).ok_or_else(|| BadPosition {
                slot,
                text: row.position.clone(),
            })?;
            cues.push(Cue {
                slot,
                label: row.label.clone(),
                position,
                color: row.color,
            });
        }
        Ok(cues)
    }
}

/// The window, centred over a dimmed view of the app that can't be clicked.
pub fn modal<'a>(editor: &'a Editor, language: Language) -> Element<'a, Message> {
    let strings = language.strings();

    let mut rows = column![].spacing(8);
    for (slot, row) in editor.rows.iter().enumerate() {
        rows = rows.push(match row {
            Some(row) => cue_row(slot, row, editor.busy, language),
            None => empty_row(slot, editor.busy, language),
        });
    }

    let mut body = column![
        text(fill(&strings.cue_editor_title, &[("title", &editor.title)])).size(24),
        scrollable(rows).height(iced::Shrink),
        text(&strings.cue_editor_note)
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
    .on_press_maybe((!editor.busy).then_some(Message::CueEditorSave));
    let cancel = button(text(&strings.smart_cancel))
        .style(button::secondary)
        .on_press_maybe((!editor.busy).then_some(Message::CueEditorCancel));
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
        button(text(&strings.cue_editor_add).size(13))
            .padding([2, 8])
            .style(button::secondary)
            .on_press_maybe((!busy).then_some(Message::CueEditor(Event::Add(slot)))),
    ]
    .spacing(8)
    .align_y(Center)
    .height(30)
    .into()
}

fn cue_row<'a>(slot: usize, cue: &'a Row, busy: bool, language: Language) -> Element<'a, Message> {
    let strings = language.strings();
    let edit = |event: fn(usize, String) -> Event| {
        move |value: String| Message::CueEditor(event(slot, value))
    };

    let mut swatches = row![].spacing(3).align_y(Center);
    for color in PALETTE {
        swatches = swatches.push(swatch(slot, color, cue.color == color, busy));
    }

    // A small button with a symbol on it, which says in words what it does when pointed at.
    let icon = |symbol: &'a str, tip: &'a str, event: Event| -> Element<'a, Message> {
        tooltip(
            button(text(symbol).size(14).center())
                .width(32)
                .padding([2, 0])
                .style(button::secondary)
                .on_press_maybe((!busy).then_some(Message::CueEditor(event))),
            text(tip).size(13),
            tooltip::Position::Top,
        )
        .style(container::rounded_box)
        .into()
    };

    row![
        number(slot),
        swatches,
        text_input(&strings.cue_editor_name, &cue.label)
            .on_input_maybe((!busy).then(|| edit(Event::Label)))
            .size(14)
            .padding([4, 6])
            .width(Fill),
        text_input(&strings.cue_editor_position, &cue.position)
            .on_input_maybe((!busy).then(|| edit(Event::Position)))
            .size(14)
            .padding([4, 6])
            .width(104),
        icon(
            "⌖",
            &strings.cue_editor_to_playhead,
            Event::ToPlayhead(slot)
        ),
        icon("▶", &strings.cue_editor_go, Event::Go(slot)),
        icon("✕", &strings.cue_editor_remove, Event::Remove(slot)),
    ]
    .spacing(8)
    .align_y(Center)
    .height(30)
    .into()
}

/// One color to choose from, with a ring around the one the cue has.
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
        .on_press_maybe((!busy).then_some(Message::CueEditor(Event::Color(slot, color))))
        .into()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cue(slot: usize, label: &str, position: f64) -> Cue {
        Cue {
            label: label.into(),
            ..Cue::new(slot, position)
        }
    }

    fn cues_of(editor: &Editor) -> Vec<(usize, String, f64)> {
        editor
            .cues()
            .unwrap()
            .into_iter()
            .map(|cue| (cue.slot, cue.label, cue.position))
            .collect()
    }

    #[test]
    fn shows_the_cues_the_track_has_and_gives_them_back() {
        let original = [cue(1, "Intro", 12.5), cue(6, "Drop", 90.0)];
        let editor = Editor::new(4, "Song", &original);
        assert_eq!(editor.cues().unwrap(), original);
        assert_eq!(editor.position_of(1), Some(12.5));
        assert_eq!(editor.position_of(0), None, "an empty pad has no position");
    }

    #[test]
    fn changes_are_made_to_the_pad_they_are_for() {
        let mut editor = Editor::new(4, "Song", &[cue(1, "Intro", 12.5)]);
        editor.update(Event::Label(1, "Start".into()), 0.0);
        editor.update(Event::Position(1, "0:20".into()), 0.0);
        editor.update(Event::Color(1, 0xff00_ff00), 0.0);
        // Changing an empty pad does nothing.
        editor.update(Event::Label(3, "x".into()), 0.0);
        let cues = editor.cues().unwrap();
        assert_eq!(cues.len(), 1);
        assert_eq!(
            (cues[0].label.as_str(), cues[0].position, cues[0].color),
            ("Start", 20.0, 0xff00_ff00)
        );

        editor.update(Event::ToPlayhead(1), 61.25);
        assert_eq!(cues_of(&editor)[0].2, 61.25);
    }

    #[test]
    fn a_cue_is_added_at_the_playhead_and_removed() {
        let mut editor = Editor::new(4, "Song", &[cue(1, "Intro", 12.5)]);
        editor.update(Event::Add(4), 30.0);
        let added = &editor.cues().unwrap()[1];
        assert_eq!(
            (
                added.slot,
                added.label.as_str(),
                added.position,
                added.color
            ),
            (4, "Cue 5", 30.0, PALETTE[4])
        );

        // Adding to a pad that has a cue changes nothing.
        editor.update(Event::Add(1), 99.0);
        assert_eq!(editor.position_of(1), Some(12.5));

        editor.update(Event::Remove(1), 0.0);
        assert_eq!(cues_of(&editor).len(), 1);
        editor.update(Event::Remove(1), 0.0);
        editor.update(Event::Remove(7), 0.0);
        assert_eq!(cues_of(&editor).len(), 1);
    }

    #[test]
    fn a_position_that_can_not_be_read_stops_saving() {
        let mut editor = Editor::new(4, "Song", &[cue(0, "a", 1.0), cue(2, "b", 2.0)]);
        editor.update(Event::Position(2, "soon".into()), 0.0);
        assert_eq!(
            editor.cues(),
            Err(BadPosition {
                slot: 2,
                text: "soon".into()
            })
        );
        assert_eq!(editor.position_of(2), None);
    }

    #[test]
    fn any_change_clears_the_last_error() {
        let mut editor = Editor::new(4, "Song", &[]);
        editor.error = Some("oops".into());
        editor.update(Event::Add(0), 1.0);
        assert_eq!(editor.error, None);
    }
}
