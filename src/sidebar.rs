//! The left side of the library page: the playlists of the open library, if there is
//! one, the connected USB drives and the recently opened libraries.

use std::path::Path;

use iced::widget::{Space, button, column, container, row, scrollable, text, tooltip};
use iced::{Element, Fill};

use crate::Message;
use crate::browser::{self, Browser};
use crate::config::Language;
use crate::drives::Drive;
use crate::recent::{Recents, display_name};

const WIDTH: f32 = 260.0;
/// With a library open, the recent libraries get at most this much room under its playlists.
const RECENT_MAX_HEIGHT: f32 = 220.0;
/// And so do the drives.
const DRIVES_MAX_HEIGHT: f32 = 160.0;

/// Where libraries can be opened from, besides the open dialog.
pub struct Sources<'a> {
    pub recents: &'a Recents,
    /// Why the recent libraries could not be saved.
    pub recents_error: Option<&'a str>,
    pub drives: &'a [Drive],
}

/// `current` is the open library, to mark it in the list. Libraries can't be opened
/// from the list (`can_open` is false) while another one is still being opened.
pub fn view<'a>(
    browser: Option<&'a Browser>,
    sources: Sources<'a>,
    current: Option<&'a str>,
    can_open: bool,
    can_sync: bool,
    language: Language,
) -> Element<'a, Message> {
    let Sources {
        recents,
        recents_error,
        drives,
    } = sources;
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

    let mut drive_section = column![text(&strings.drives).size(16)].spacing(6);
    if drives.is_empty() {
        drive_section = drive_section.push(text(&strings.no_drives).style(text::secondary));
    } else {
        let mut list = column![].spacing(2);
        for drive in drives {
            list = list.push(drive_entry(drive, current, can_open, language));
        }
        drive_section = drive_section.push(scrollable(list));
    }

    let content = match browser {
        Some(browser) => column![
            browser::playlists(browser, language),
            button(text(&strings.smart_new))
                .width(Fill)
                .style(button::secondary)
                .on_press_maybe(can_sync.then_some(Message::SmartNew)),
            // Compare the working copy with the library it was made from, and write it there.
            button(text(&strings.sync_button))
                .width(Fill)
                .on_press_maybe(can_sync.then_some(Message::SyncOpen)),
            container(drive_section).max_height(DRIVES_MAX_HEIGHT),
            container(section).max_height(RECENT_MAX_HEIGHT)
        ],
        None => column![drive_section, section, Space::new().height(Fill)],
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

/// A drive and what is on it. One with a library opens it when clicked; one without
/// is only shown, so it is clear why it can't be opened.
fn drive_entry<'a>(
    drive: &'a Drive,
    current: Option<&str>,
    can_open: bool,
    language: Language,
) -> Element<'a, Message> {
    let strings = language.strings();
    let is_current = drive
        .library
        .as_deref()
        .zip(current)
        .is_some_and(|(library, current)| Path::new(current) == library);

    let status = if drive.library.is_some() {
        &strings.drive_has_library
    } else {
        &strings.drive_no_library
    };
    let label = column![
        text(&drive.name).wrapping(text::Wrapping::None),
        text(status).size(12).style(if is_current {
            text::default
        } else {
            text::secondary
        }),
    ];
    let open = button(label)
        .width(Fill)
        .style(if is_current {
            button::primary
        } else {
            button::text
        })
        .on_press_maybe(
            drive
                .library
                .clone()
                .filter(|_| can_open)
                .map(Message::OpenDrive),
        );

    tooltip(
        open,
        text(drive.mount.display().to_string()),
        tooltip::Position::Right,
    )
    .style(container::rounded_box)
    .into()
}
