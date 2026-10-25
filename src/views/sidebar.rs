//! The left side of the library page: the playlists of the open library (the folders, for
//! a folder of music), if there is one, the local music folders that were added, the
//! connected USB drives and the recently opened libraries.

use std::path::Path;

use iced::widget::{Space, button, column, container, row, scrollable, text, tooltip};
use iced::{Center, Element, Fill, Theme};

use crate::Message;
use crate::browser::{self, Browser};
use crate::config::Language;
use crate::drives::Drive;
use crate::error_text;
use crate::local_folders::LocalFolders;
use crate::playlist_drag::Drag;
use crate::recent::{Recents, display_name};

const WIDTH: f32 = 260.0;
/// With a library open, the recent libraries get at most this much room under its playlists.
const RECENT_MAX_HEIGHT: f32 = 220.0;
/// And so do the drives.
const DRIVES_MAX_HEIGHT: f32 = 160.0;
/// And the local music folders.
const LOCAL_MAX_HEIGHT: f32 = 200.0;

/// Where libraries can be opened from, besides the open dialog.
pub struct Sources<'a> {
    pub recents: &'a Recents,
    /// Why the recent libraries could not be saved.
    pub recents_error: Option<&'a str>,
    /// Why the last drive could not be unmounted.
    pub drive_error: Option<&'a str>,
    pub drives: &'a [Drive],
    /// The folders of music that were added.
    pub local: &'a LocalFolders,
    /// The playlist being dragged in the tree, if one is.
    pub drag: Option<&'a Drag>,
}

/// The libraries that are open, to mark them in the list: the shown one, and the one of the
/// other kind (an Engine library and a folder of music can be open at the same time) that
/// a click on its entry brings back.
#[derive(Clone, Copy, Default)]
pub struct Open<'a> {
    pub shown: Option<&'a str>,
    pub parked: Option<&'a str>,
}

impl Open<'_> {
    fn is(self, which: Option<&str>, path: &Path) -> bool {
        which.is_some_and(|open| Path::new(open) == path)
    }

    fn is_shown(self, path: &Path) -> bool {
        self.is(self.shown, path)
    }

    fn style(self, path: &Path) -> fn(&Theme, button::Status) -> button::Style {
        if self.is_shown(path) {
            button::primary
        } else if self.is(self.parked, path) {
            button::secondary
        } else {
            button::text
        }
    }
}

/// `open` says which libraries are open, to mark them in the list. Libraries can't be opened
/// from the list (`can_open` is false) while another one is still being opened.
pub fn view<'a>(
    browser: Option<&'a Browser>,
    sources: Sources<'a>,
    open: Open<'a>,
    can_open: bool,
    can_sync: bool,
    language: Language,
) -> Element<'a, Message> {
    let Sources {
        recents,
        recents_error,
        drive_error,
        drives,
        local,
        drag,
    } = sources;
    let strings = language.strings();

    let mut section = column![text(&strings.recent_libraries).size(16)].spacing(6);
    if recents.paths().is_empty() {
        section = section.push(text(&strings.no_recent_libraries).style(text::secondary));
    } else {
        let mut list = column![].spacing(2);
        for path in recents.paths() {
            list = list.push(entry(path, open, can_open, language));
        }
        section = section.push(scrollable(list));
    }
    if let Some(error) = recents_error {
        section = section.push(error_text::view(
            crate::i18n::fill(&strings.recents_save_failed, &[("error", &error)]),
            language,
        ));
    }

    let mut drive_section = column![text(&strings.drives).size(16)].spacing(6);
    if drives.is_empty() {
        drive_section = drive_section.push(text(&strings.no_drives).style(text::secondary));
    } else {
        let mut list = column![].spacing(2);
        for drive in drives {
            list = list.push(drive_entry(drive, open, can_open, language));
        }
        drive_section = drive_section.push(scrollable(list));
    }
    if let Some(error) = drive_error {
        drive_section = drive_section.push(error_text::view(error.to_owned(), language));
    }

    let local_section = local_section(local, open, can_open, language);

    let content = match browser {
        // The folders of a folder of music, with the local music below them. There is
        // nothing to edit or sync.
        Some(browser) if !browser.library.is_engine() => column![
            browser::playlists(browser, drag, language),
            container(local_section).max_height(LOCAL_MAX_HEIGHT),
            container(drive_section).max_height(DRIVES_MAX_HEIGHT),
            container(section).max_height(RECENT_MAX_HEIGHT)
        ],
        Some(browser) => column![
            browser::playlists(browser, drag, language),
            button(text(&strings.playlist_new))
                .width(Fill)
                .style(button::secondary)
                .on_press_maybe(can_sync.then_some(Message::PlaylistNew)),
            button(text(&strings.smart_new))
                .width(Fill)
                .style(button::secondary)
                .on_press_maybe(can_sync.then_some(Message::SmartNew)),
            // Compare the working copy with the library it was made from, and write it there.
            button(text(&strings.sync_button))
                .width(Fill)
                .on_press_maybe(can_sync.then_some(Message::SyncOpen)),
            button(text(&strings.libsync_button))
                .width(Fill)
                .on_press_maybe(can_sync.then_some(Message::LibrarySyncOpen)),
            button(text(&strings.repair_button))
                .width(Fill)
                .style(button::secondary)
                .on_press_maybe(can_sync.then_some(Message::RepairOpen)),
            container(local_section).max_height(LOCAL_MAX_HEIGHT),
            container(drive_section).max_height(DRIVES_MAX_HEIGHT),
            container(section).max_height(RECENT_MAX_HEIGHT)
        ],
        None => column![
            container(local_section).max_height(LOCAL_MAX_HEIGHT),
            drive_section,
            section,
            Space::new().height(Fill),
            button(text(&strings.repair_button))
                .width(Fill)
                .style(button::secondary)
                .on_press_maybe(can_sync.then_some(Message::RepairOpen)),
        ],
    };
    container(content.spacing(16).height(Fill))
        .width(WIDTH)
        .into()
}

/// The folders of music that were added, with the button to add one. Clicking a folder opens
/// it (the open one is marked); the × takes it out of the list, closing it if it is open.
fn local_section<'a>(
    local: &'a LocalFolders,
    open: Open<'_>,
    can_open: bool,
    language: Language,
) -> Element<'a, Message> {
    let strings = language.strings();

    let mut section = column![text(&strings.local_music).size(16)].spacing(6);
    if local.paths().is_empty() {
        section = section.push(text(&strings.no_local_music).style(text::secondary));
    } else {
        let mut list = column![].spacing(2);
        for path in local.paths() {
            let entry = button(text(display_name(path)).wrapping(text::Wrapping::None))
                .width(Fill)
                .style(open.style(path))
                .on_press_maybe(can_open.then(|| Message::OpenLocalFolder(path.to_path_buf())));
            let remove = tooltip(
                button(text("×"))
                    .padding([4, 8])
                    .style(button::text)
                    .on_press(Message::RemoveLocalFolder(path.to_path_buf())),
                text(&strings.local_remove),
                tooltip::Position::Bottom,
            )
            .style(container::rounded_box);
            // The full path is what tells two folders with the same name apart.
            list = list.push(
                tooltip(
                    row![entry, remove].align_y(iced::Center),
                    text(path.display().to_string()),
                    tooltip::Position::Right,
                )
                .style(container::rounded_box),
            );
        }
        section = section.push(scrollable(list).height(iced::Shrink));
    }
    section
        .push(
            button(text(&strings.local_add))
                .width(Fill)
                .style(button::secondary)
                .on_press_maybe(can_open.then_some(Message::AddLocalFolder)),
        )
        .into()
}

fn entry<'a>(
    path: &'a Path,
    open: Open<'_>,
    can_open: bool,
    language: Language,
) -> Element<'a, Message> {
    let strings = language.strings();

    let entry = button(text(display_name(path)).wrapping(text::Wrapping::None))
        .width(Fill)
        .style(open.style(path))
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
        row![entry, forget].align_y(iced::Center),
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
    open: Open<'_>,
    can_open: bool,
    language: Language,
) -> Element<'a, Message> {
    let strings = language.strings();
    let library = drive.library.as_deref();
    let is_current = library.is_some_and(|library| open.is_shown(library));
    let style = library.map_or(
        button::text as fn(&Theme, button::Status) -> button::Style,
        |library| open.style(library),
    );

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
    let entry = button(label).width(Fill).style(style).on_press_maybe(
        drive
            .library
            .clone()
            .filter(|_| can_open)
            .map(Message::OpenDrive),
    );

    let entry = tooltip(
        entry,
        text(drive.mount.display().to_string()),
        tooltip::Position::Right,
    )
    .style(container::rounded_box);

    // Unmounting writes out what is still in memory, so the drive can be pulled out safely.
    let unmount = tooltip(
        button(text("⏏")).style(button::secondary).on_press_maybe(
            drive
                .device
                .clone()
                .filter(|_| can_open)
                .map(|device| Message::UnmountDrive(device, drive.name.clone())),
        ),
        text(&strings.drive_unmount),
        tooltip::Position::Left,
    )
    .style(container::rounded_box);

    row![entry, unmount].spacing(4).align_y(Center).into()
}
