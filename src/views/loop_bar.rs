//! The row of loop pads under the cue pads: one per loop pad of Engine's eight. A pad that
//! is set shows the loop in its color, and clicking it jumps to the start of the loop
//! (right-clicking removes it); an empty pad sets a loop of four beats at the playhead.

use iced::widget::{Space, button, container, mouse_area, row, text, tooltip};
use iced::{Background, Border, Center, Element, Fill, Theme};

use crate::Message;
use crate::config::Language;
use crate::cues::{self, SLOTS};
use crate::i18n::fill;
use crate::loops::Loop;

const PAD_WIDTH: f32 = 104.0;

/// The pads of a track with these `loops`. While `busy` (a change is being saved) they can't
/// be used.
pub fn view<'a>(loops: &'a [Loop], busy: bool, language: Language) -> Element<'a, Message> {
    let strings = language.strings();

    let mut pads = row![].spacing(6).align_y(Center);
    for slot in 0..SLOTS {
        pads = pads.push(pad(
            slot,
            loops.iter().find(|saved| saved.slot == slot),
            busy,
            language,
        ));
    }

    row![
        text(&strings.loop_bar_title)
            .size(13)
            .style(text::secondary),
        pads,
        Space::new().width(Fill),
        button(text(&strings.loop_edit).size(13))
            .padding([2, 8])
            .style(button::secondary)
            .on_press_maybe((!busy).then_some(Message::LoopEditorOpen)),
    ]
    .spacing(10)
    .align_y(Center)
    .into()
}

fn pad<'a>(
    slot: usize,
    saved: Option<&'a Loop>,
    busy: bool,
    language: Language,
) -> Element<'a, Message> {
    let strings = language.strings();
    let number = (slot + 1).to_string();

    let Some(saved) = saved else {
        let empty = button(text(number.clone()).size(13).style(text::secondary))
            .width(PAD_WIDTH)
            .padding([3, 8])
            .style(button::secondary)
            .on_press_maybe((!busy).then_some(Message::LoopAdd(slot)));
        return tooltip(
            empty,
            text(fill(&strings.loop_pad_empty_tip, &[("number", &number)])).size(13),
            tooltip::Position::Top,
        )
        .style(container::rounded_box)
        .into();
    };

    let color = saved.color();
    let text_color = cues::text_on(color);
    let label = if saved.label.is_empty() {
        number
    } else {
        format!("{number} {}", saved.label)
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
        .on_press_maybe((!busy).then_some(Message::LoopJump(slot)));

    let tip = fill(
        &strings.loop_pad_set_tip,
        &[
            ("label", &saved.label),
            ("start", &cues::format_position(saved.start)),
            ("end", &cues::format_position(saved.end)),
        ],
    );
    let pad: Element<'a, Message> = mouse_area(set)
        .on_right_press(Message::LoopRemove(slot))
        .into();
    tooltip(pad, text(tip).size(13), tooltip::Position::Top)
        .style(container::rounded_box)
        .into()
}
