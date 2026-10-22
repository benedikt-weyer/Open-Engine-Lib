//! The left side of the library page: the playlists of the open library, if there is
//! one, and the recently opened libraries.

use std::path::Path;

use iced::widget::{Space, button, column, container, row, scrollable, text, tooltip};
use iced::{Element, Fill};

use crate::Message;
use crate::browser::{self, Browser};
use crate::config::Language;
use crate::recent::{Recents, display_name};

const WIDTH: f32 = 260.0;
/// With a library open, the recent libraries get at most this much room under its playlists.
const RECENT_MAX_HEIGHT: f32 = 220.0;

/// `current` is the open library, to mark it in the list. Libraries can't be opened
/// from the list (`can_open` is false) while another one is still being opened.
pub fn view<'a>(
    browser: Option<&'a Browser>,
    recents: &'a Recents,
    recents_error: Option<&'a str>,
    current: Option<&'a str>,
    can_open: bool,
    language: Language,
) -> Element<'a, Message> {
    let strings = language.strings();

    let mut section = column![text(&strings.recent_libraries).size(16)].spacing(6);
    if recents.paths().is_empty() {
        section = section.push(text(&strings.no_recent_libraries).style(text::secondary));
    } else {
        let mut list = column![].spacing(2);
        for path in recents.paths() {
            list = list.push(entry(path, current, can_open, language));
        }
        section = section.push(scrollable(list));
    }
    if let Some(error) = recents_error {
        section = section.push(
            text(crate::i18n::fill(
                &strings.recents_save_failed,
                &[("error", &error)],
            ))
            .style(text::danger),
        );
    }

    let content = match browser {
        Some(browser) => column![
            browser::playlists(browser, language),
            container(section).max_height(RECENT_MAX_HEIGHT)
        ],
        None => column![section, Space::new().height(Fill)],
    };
    container(content.spacing(16).height(Fill))
        .width(WIDTH)
        .into()
}

fn entry<'a>(
    path: &'a Path,
    current: Option<&str>,
    can_open: bool,
    language: Language,
) -> Element<'a, Message> {
    let strings = language.strings();
    let is_current = current.is_some_and(|current| Path::new(current) == path);

    let open = button(text(display_name(path)).wrapping(text::Wrapping::None))
        .width(Fill)
        .style(if is_current {
            button::primary
        } else {
            button::text
        })
        .on_press_maybe(can_open.then(|| Message::OpenRecent(path.to_path_buf())));
    let forget = tooltip(
        button(text("×"))
            .padding([4, 8])
            .style(button::text)
            .on_press(Message::ForgetRecent(path.to_path_buf())),
        text(&strings.forget_library),
        tooltip::Position::Bottom,
    )
    .style(container::rounded_box);

    // The full path is what tells two libraries with the same name apart.
    tooltip(
        row![open, forget].align_y(iced::Center),
        text(path.display().to_string()),
        tooltip::Position::Right,
    )
    .style(container::rounded_box)
    .into()
}
