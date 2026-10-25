//! The menu that opens where a track of the list is right-clicked: take it out of the
//! playlist that is shown, or out of the library. It is drawn over the whole window by a
//! layer that closes it when anything else is clicked, and is kept inside the window.

use iced::widget::{button, column, container, mouse_area, responsive, text};
use iced::{Border, Color, Element, Fill, Padding, Point, Theme};

use crate::Message;
use crate::config::Language;
use crate::i18n::fill;

/// How wide the menu is.
pub(crate) const WIDTH: f32 = 290.0;
/// About how tall a line of it is, to keep it inside the window when it opens near the
/// bottom.
pub(crate) const LINE_HEIGHT: f32 = 34.0;
pub(crate) const TITLE_HEIGHT: f32 = 30.0;

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
    /// The track can be analysed: it is a file, not a streaming track.
    pub can_analyse: bool,
    /// The track has an analysis already, which analysing it again replaces.
    pub analysed: bool,
    /// The track is being analysed or waits to be: it can't be put in the queue again.
    pub queued: bool,
    /// How many tracks of the list that is shown have no analysis (files, not streaming
    /// tracks): the menu offers to analyse them all one after the other when there are
    /// more than one.
    pub analyse_all: usize,
}

/// The menu in a layer over the whole window, whose clicks outside the menu close it.
pub fn layer(menu: &Menu, analysing: bool, language: Language) -> Element<'_, Message> {
    let lines = 1
        + usize::from(menu.playlist.is_some())
        + usize::from(menu.can_analyse)
        + usize::from(menu.analyse_all > 1);
    let height = TITLE_HEIGHT + lines as f32 * LINE_HEIGHT;
    anchored(menu.at, height, Message::TrackMenuClose, move || {
        view(menu, analysing, language)
    })
}

/// A menu of `height` in a layer over the window: placed at `at`, or as near to it as the
/// window lets it be, with a click anywhere else (or a right click anywhere) sending `close`.
pub(crate) fn anchored<'a>(
    at: Point,
    height: f32,
    close: Message,
    view: impl Fn() -> Element<'a, Message> + 'a,
) -> Element<'a, Message> {
    responsive(move |size| {
        // Inside the window, whichever way it opens.
        let padding = Padding {
            top: at.y.min((size.height - height).max(0.0)),
            left: at.x.min((size.width - WIDTH).max(0.0)),
            right: 0.0,
            bottom: 0.0,
        };
        mouse_area(container(view()).padding(padding).width(Fill).height(Fill))
            .on_press(close.clone())
            .on_right_press(close.clone())
            .into()
    })
    .into()
}

fn view(menu: &Menu, analysing: bool, language: Language) -> Element<'_, Message> {
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

    if menu.can_analyse {
        // One track at a time: the progress shown is that of one.
        // (While tracks are analysed, this one joins them.)
        items = items.push(if menu.queued {
            disabled_item(&strings.track_menu_queued)
        } else {
            item(
                if menu.analysed {
                    &strings.track_menu_analyse_again
                } else {
                    &strings.track_menu_analyse
                },
                Message::TrackMenuAnalyse,
                false,
            )
        });
    }
    if menu.analyse_all > 1 {
        items = items.push(item(
            fill(
                &strings.track_menu_analyse_all,
                &[("count", &menu.analyse_all)],
            ),
            Message::TrackMenuAnalyseAll,
            false,
        ));
    }
    if menu.playlist.is_some() {
        items = items.push(item_if(
            &strings.track_menu_playlist,
            Message::TrackMenuRemoveFromPlaylist,
            false,
            !analysing,
        ));
    }
    // (What changes the library waits for the analysis, which writes to it when it is done.)
    items = items.push(item_if(
        if menu.confirm_library {
            &strings.track_menu_library_confirm
        } else {
            &strings.track_menu_library
        },
        Message::TrackMenuRemoveFromLibrary,
        menu.confirm_library,
        !analysing,
    ));
    // (No padding, so that a click on the menu is always on one of its lines.)
    container(items)
        .width(WIDTH)
        .style(container::bordered_box)
        .into()
}

fn item<'a>(
    label: impl text::IntoFragment<'a>,
    message: Message,
    danger: bool,
) -> Element<'a, Message> {
    item_if(label, message, danger, true)
}

/// A line that does something only if `enabled`.
pub(crate) fn item_if<'a>(
    label: impl text::IntoFragment<'a>,
    message: Message,
    danger: bool,
    enabled: bool,
) -> Element<'a, Message> {
    button(text(label).size(14))
        .width(Fill)
        .padding([7, 12])
        .style(move |theme: &Theme, status| style(theme, status, danger))
        .on_press_maybe(enabled.then_some(message))
        .into()
}

/// A line that is there but can't be used now.
fn disabled_item(label: &str) -> Element<'_, Message> {
    item_if(label, Message::TrackMenuClose, false, false)
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
