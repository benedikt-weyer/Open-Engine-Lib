//! The menu that opens where a folder of a folder of music (in the playlist tree on the left)
//! is right-clicked: import it into an Engine library as a playlist. Like the track menu it
//! is a layer over the whole window that closes when anything else is clicked.

use iced::widget::{column, container, text};
use iced::{Element, Fill, Point};

use crate::Message;
use crate::config::Language;
use crate::track_menu::{LINE_HEIGHT, TITLE_HEIGHT, WIDTH, anchored, item_if};

/// The folder the menu is for and where it is.
#[derive(Debug, Clone, PartialEq)]
pub struct Menu {
    /// The folder, as an index into the open library's playlists.
    pub playlist: usize,
    /// What the menu says it is for: the folder's name.
    pub title: String,
    /// Where it opened, in the window.
    pub at: Point,
}

/// The menu in a layer over the whole window. `can_import` is false while something else
/// (a sync, an import, an analysis) has the backend.
pub fn layer(menu: &Menu, can_import: bool, language: Language) -> Element<'_, Message> {
    let strings = language.strings();
    anchored(
        menu.at,
        TITLE_HEIGHT + LINE_HEIGHT,
        Message::FolderMenuClose,
        move || {
            let title = container(
                text(&menu.title)
                    .size(13)
                    .style(text::secondary)
                    .wrapping(text::Wrapping::None),
            )
            .padding([6, 12])
            .width(Fill)
            .clip(true);
            container(
                column![
                    title,
                    item_if(
                        &strings.folder_menu_import,
                        Message::FolderMenuImport,
                        false,
                        can_import,
                    ),
                ]
                .width(WIDTH),
            )
            .width(WIDTH)
            .style(container::bordered_box)
            .into()
        },
    )
}
