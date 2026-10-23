//! The row of cue pads under the waveform of the track in the player: one per pad of
//! Engine's eight. A pad that is set shows the cue in its color, and clicking it jumps to
//! the cue (right-clicking removes it); an empty pad sets a cue at the playhead.

use iced::widget::{Space, button, container, mouse_area, row, text, tooltip};
use iced::{Background, Border, Center, Element, Fill, Theme};

use crate::Message;
use crate::config::Language;
use crate::cues::{self, Cue, SLOTS};
use crate::i18n::fill;

const PAD_WIDTH: f32 = 104.0;

/// The pads of a track with these `cues`. While `busy` (a change is being saved) they can't
/// be used.
pub fn view<'a>(cues: &'a [Cue], busy: bool, language: Language) -> Element<'a, Message> {
    let strings = language.strings();

    let mut pads = row![].spacing(6).align_y(Center);
    for slot in 0..SLOTS {
        pads = pads.push(pad(
            slot,
            cues.iter().find(|cue| cue.slot == slot),
            busy,
            language,
        ));
    }

    row![
        text(&strings.cue_bar_title).size(13).style(text::secondary),
        pads,
        Space::new().width(Fill),
        button(text(&strings.cue_edit).size(13))
            .padding([2, 8])
            .style(button::secondary)
            .on_press_maybe((!busy).then_some(Message::CueEditorOpen)),
    ]
    .spacing(10)
    .align_y(Center)
    .into()
}

fn pad<'a>(
    slot: usize,
    cue: Option<&'a Cue>,
    busy: bool,
    language: Language,
) -> Element<'a, Message> {
    let strings = language.strings();
    let number = (slot + 1).to_string();

    let Some(cue) = cue else {
        let empty = button(text(number.clone()).size(13).style(text::secondary))
            .width(PAD_WIDTH)
            .padding([3, 8])
            .style(button::secondary)
            .on_press_maybe((!busy).then_some(Message::CueAdd(slot)));
        return tooltip(
            empty,
            text(fill(&strings.cue_pad_empty_tip, &[("number", &number)])).size(13),
            tooltip::Position::Top,
        )
        .style(container::rounded_box)
        .into();
    };

    let color = cue.color();
    let text_color = cues::text_on(color);
    let label = if cue.label.is_empty() {
        number
    } else {
        format!("{number} {}", cue.label)
    };
    let set = button(text(label).size(13).wrapping(text::Wrapping::None))
        .width(PAD_WIDTH)
        .padding([3, 8])
        .style(move |_: &Theme, status| {
            let shade = match status {
                button::Status::Hovered | button::Status::Pressed => 0.12,
                _ => 0.0,
            };
            button::Style {
                background: Some(Background::Color(iced::Color {
                    r: (color.r + shade).min(1.0),
                    g: (color.g + shade).min(1.0),
                    b: (color.b + shade).min(1.0),
                    a: if matches!(status, button::Status::Disabled) {
                        0.5
                    } else {
                        1.0
                    },
                })),
                text_color,
                border: Border::default().rounded(4),
                ..button::Style::default()
            }
        })
        .on_press_maybe((!busy).then_some(Message::CueJump(slot)));

    let tip = fill(
        &strings.cue_pad_set_tip,
        &[
            ("label", &cue.label),
            ("time", &cues::format_position(cue.position)),
        ],
    );
    let pad: Element<'a, Message> = mouse_area(set)
        .on_right_press(Message::CueRemove(slot))
        .into();
    tooltip(pad, text(tip).size(13), tooltip::Position::Top)
        .style(container::rounded_box)
        .into()
}
