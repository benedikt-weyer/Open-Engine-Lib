//! The menu that opens where a track of the list is right-clicked: take it out of the
//! playlist that is shown, or out of the library. It is drawn over the whole window by a
//! layer that closes it when anything else is clicked, and is kept inside the window.

use iced::widget::{button, column, container, mouse_area, responsive, text};
use iced::{Border, Color, Element, Fill, Padding, Point, Theme};

use crate::Message;
use crate::config::Language;

/// How wide the menu is.
const WIDTH: f32 = 290.0;
/// About how tall a line of it is, to keep it inside the window when it opens near the
/// bottom.
const LINE_HEIGHT: f32 = 34.0;
const TITLE_HEIGHT: f32 = 30.0;

/// The track the menu is for and where it is.
#[derive(Debug, Clone, PartialEq)]
pub struct Menu {
    /// The row of the track list that was clicked.
    pub row: usize,
    /// The id of its track.
    pub track: u32,
    /// What the menu says it is for: the track's title.
    pub title: String,
    /// The playlist (an index into the library's) the track list shows, if it is one the
    /// track can be taken out of: not "All tracks", not a smart playlist.
    pub playlist: Option<usize>,
    /// Where it opened, in the window.
    pub at: Point,
    /// Deleting from the library was clicked once and waits for a second click.
    pub confirm_library: bool,
}

/// The menu in a layer over the window, whose clicks outside the menu close it.
pub fn layer(menu: &Menu, language: Language) -> Element<'_, Message> {
    let lines = 1 + usize::from(menu.playlist.is_some());
    let height = TITLE_HEIGHT + lines as f32 * LINE_HEIGHT;
    let at = menu.at;
    responsive(move |size| {
        // Inside the window, whichever way it opens.
        let padding = Padding {
            top: at.y.min((size.height - height).max(0.0)),
            left: at.x.min((size.width - WIDTH).max(0.0)),
            right: 0.0,
            bottom: 0.0,
        };
        mouse_area(
            container(view(menu, language))
                .padding(padding)
                .width(Fill)
                .height(Fill),
        )
        .on_press(Message::TrackMenuClose)
        .on_right_press(Message::TrackMenuClose)
        .into()
    })
    .into()
}

fn view(menu: &Menu, language: Language) -> Element<'_, Message> {
    let strings = language.strings();

    let title = container(
        text(&menu.title)
            .size(13)
            .style(text::secondary)
            .wrapping(iced::widget::text::Wrapping::None),
    )
    .padding([6, 12])
    .width(Fill)
    .clip(true);
    let mut items = column![title].width(WIDTH);

    if menu.playlist.is_some() {
        items = items.push(item(
            &strings.track_menu_playlist,
            Message::TrackMenuRemoveFromPlaylist,
            false,
        ));
    }
    items = items.push(item(
        if menu.confirm_library {
            &strings.track_menu_library_confirm
        } else {
            &strings.track_menu_library
        },
        Message::TrackMenuRemoveFromLibrary,
        menu.confirm_library,
    ));
    // (No padding, so that a click on the menu is always on one of its lines.)
    container(items)
        .width(WIDTH)
        .style(container::bordered_box)
        .into()
}

fn item(label: &str, message: Message, danger: bool) -> Element<'_, Message> {
    button(text(label).size(14))
        .width(Fill)
        .padding([7, 12])
        .style(move |theme: &Theme, status| style(theme, status, danger))
        .on_press(message)
        .into()
}

fn style(theme: &Theme, status: button::Status, danger: bool) -> button::Style {
    let palette = theme.extended_palette();
    let text_color = if danger {
        palette.danger.base.color
    } else {
        palette.background.base.text
    };
    let base = button::Style {
        background: None,
        text_color,
        border: Border::default(),
        ..button::Style::default()
    };
    match status {
        button::Status::Hovered | button::Status::Pressed => button::Style {
            background: Some(palette.background.strong.color.into()),
            ..base
        },
        button::Status::Disabled => button::Style {
            text_color: Color {
                a: 0.5,
                ..text_color
            },
            ..base
        },
        button::Status::Active => base,
    }
}
