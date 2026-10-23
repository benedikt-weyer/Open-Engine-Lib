//! An error message in the danger color with a button that copies it to the clipboard
//! (messages like a file that was not found hold a path or a reason worth pasting).

use iced::widget::{button, row, text};
use iced::{Element, Fill};

use crate::Message;
use crate::config::Language;

/// The message at its full width, with the button beside it.
pub fn view<'a>(content: impl Into<String>, language: Language) -> Element<'a, Message> {
    let content = content.into();
    row![
        text(content.clone()).style(text::danger).width(Fill),
        copy_button(content, 13, language),
    ]
    .spacing(8)
    .align_y(iced::Center)
    .into()
}

/// For a message in a row of other things, where it must not take all the room (and
/// `size` is that of the text around it).
pub fn compact<'a>(
    content: impl Into<String>,
    size: u32,
    language: Language,
) -> Element<'a, Message> {
    let content = content.into();
    row![
        text(content.clone()).size(size).style(text::danger),
        copy_button(content, size.saturating_sub(1), language),
    ]
    .spacing(6)
    .align_y(iced::Center)
    .into()
}

fn copy_button<'a>(content: String, size: u32, language: Language) -> Element<'a, Message> {
    button(text(language.strings().copy_error.clone()).size(size))
        .padding([1, 6])
        .style(button::secondary)
        .on_press(Message::CopyText(content))
        .into()
}
