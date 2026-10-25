//! The menu that opens where a playlist of an Engine library (in the tree on the left) is
//! right-clicked: rename it, delete it, edit a smart playlist's rules, and analyse its
//! tracks. Like the track menu it is a layer over the whole window that closes when
//! anything else is clicked.

use iced::widget::{column, container, text};
use iced::{Element, Fill, Point};

use crate::Message;
use crate::config::Language;
use crate::i18n::fill;
use crate::track_menu::{LINE_HEIGHT, TITLE_HEIGHT, WIDTH, anchored, item_if};

/// The playlist the menu is for and where it is.
#[derive(Debug, Clone, PartialEq)]
pub struct Menu {
    /// The playlist, as an index into the open library's playlists.
    pub playlist: usize,
    /// What the menu says it is for: the playlist's title.
    pub title: String,
    /// A smart playlist is changed through its rules, and can't be renamed or deleted here.
    pub smart: bool,
    /// How many of its tracks are files (a streaming track has nothing to analyse), and how
    /// many of those have no analysis; tracks that are in the analysis queue already are not
    /// counted.
    pub tracks: usize,
    pub unanalysed: usize,
    /// "Analyse all again" was clicked once and waits for a second click.
    pub confirm_reanalyse: bool,
    /// Where it opened, in the window.
    pub at: Point,
}

/// The menu in a layer over the whole window. `busy` is true while the library is being
/// changed or a track is analysed, which renaming and deleting would collide with;
/// `analysis_blocked` while something else has the working copy (tracks to analyse join
/// the queue of the analysis that runs).
pub fn layer(
    menu: &Menu,
    busy: bool,
    analysis_blocked: bool,
    language: Language,
) -> Element<'_, Message> {
    let lines = if menu.smart { 1 } else { 2 }
        + usize::from(menu.unanalysed > 0)
        + usize::from(menu.tracks > 0);
    anchored(
        menu.at,
        TITLE_HEIGHT + lines as f32 * LINE_HEIGHT,
        Message::PlaylistMenuClose,
        move || view(menu, busy, analysis_blocked, language),
    )
}

fn view(
    menu: &Menu,
    busy: bool,
    analysis_blocked: bool,
    language: Language,
) -> Element<'_, Message> {
    let strings = language.strings();
    let title = container(
        text(&menu.title)
            .size(13)
            .style(text::secondary)
            .wrapping(text::Wrapping::None),
    )
    .padding([6, 12])
    .width(Fill)
    .clip(true);
    let mut items = column![title].width(WIDTH);

    if menu.smart {
        items = items.push(item_if(
            &strings.smart_edit,
            Message::PlaylistMenuEdit,
            false,
            !busy,
        ));
    } else {
        items = items
            .push(item_if(
                &strings.playlist_rename,
                Message::PlaylistMenuRename,
                false,
                !busy,
            ))
            .push(item_if(
                &strings.playlist_delete,
                Message::PlaylistMenuDelete,
                false,
                !busy,
            ));
    }
    if menu.unanalysed > 0 {
        items = items.push(item_if(
            fill(
                &strings.playlist_menu_analyse_all,
                &[("count", &menu.unanalysed)],
            ),
            Message::PlaylistMenuAnalyse,
            false,
            !analysis_blocked,
        ));
    }
    if menu.tracks > 0 {
        items = items.push(item_if(
            fill(
                if menu.confirm_reanalyse {
                    &strings.playlist_menu_reanalyse_confirm
                } else {
                    &strings.playlist_menu_reanalyse_all
                },
                &[("count", &menu.tracks)],
            ),
            Message::PlaylistMenuReanalyse,
            menu.confirm_reanalyse,
            !analysis_blocked,
        ));
    }
    // (No padding, so that a click on the menu is always on one of its lines.)
    container(items)
        .width(WIDTH)
        .style(container::bordered_box)
        .into()
}
