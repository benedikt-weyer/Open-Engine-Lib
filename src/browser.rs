//! The open library as the user sees it: a playlist tree on the left and the
//! tracks of the chosen playlist on the right.

use std::collections::HashSet;
use std::ops::Range;
use std::sync::Arc;

use iced::widget::image::Handle;
use iced::widget::{Space, button, column, container, image, mouse_area, row, scrollable, text};
use iced::{ContentFit, Element, Fill, Length, Theme};

use crate::Message;
use crate::config::Language;
use crate::covers::Covers;
use crate::i18n::fill;
use crate::model::{Library, Track};

/// Height of one track row. Fixed, so only the rows in view have to be built.
const ROW_HEIGHT: f32 = 32.0;
/// Side of the cover thumbnail in a row.
const THUMBNAIL: f32 = 28.0;
/// Rows built above and below the visible ones, so scrolling doesn't show gaps.
const ROW_BUFFER: usize = 10;
/// Window heights vary and resizing sends no scroll event, so always build at least
/// enough rows for a tall window.
const MIN_VIEWPORT: f32 = 1600.0;
const INDENT: f32 = 16.0;

/// Which list the track pane shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// Every track in the library.
    Collection,
    /// An index into `Library::playlists`.
    Playlist(usize),
}

pub struct Browser {
    pub library: Arc<Library>,
    pub source: Source,
    /// Row (position in the shown list) the user clicked.
    pub selected: Option<usize>,
    all: Vec<usize>,
    /// Playlist folders that are opened up in the tree.
    expanded: HashSet<usize>,
    scroll: f32,
    viewport_height: f32,
}

impl Browser {
    pub fn new(library: Arc<Library>) -> Self {
        let all = (0..library.tracks.len()).collect();
        Self {
            library,
            source: Source::Collection,
            selected: None,
            all,
            expanded: HashSet::new(),
            scroll: 0.0,
            viewport_height: MIN_VIEWPORT,
        }
    }

    /// Indices into `Library::tracks` of the shown list, in order.
    pub fn tracks(&self) -> &[usize] {
        match self.source {
            Source::Collection => &self.all,
            Source::Playlist(index) => &self.library.playlists[index].tracks,
        }
    }

    /// The rows of the shown list that are built: the ones in view plus a margin.
    pub fn visible_rows(&self) -> Range<usize> {
        let first = ((self.scroll / ROW_HEIGHT) as usize).saturating_sub(ROW_BUFFER);
        let visible = (self.viewport_height.max(MIN_VIEWPORT) / ROW_HEIGHT).ceil() as usize;
        let last = (first + visible + 2 * ROW_BUFFER).min(self.tracks().len());
        first.min(last)..last
    }

    /// The covers of the rows that are built, which are the ones to have ready.
    pub fn visible_covers(&self) -> impl Iterator<Item = u32> + '_ {
        let list = self.tracks();
        self.visible_rows()
            .filter_map(move |row| self.library.tracks[list[row]].album_art)
    }

    pub fn show(&mut self, source: Source) {
        self.source = source;
        self.selected = None;
        self.scroll = 0.0;
    }

    pub fn toggle(&mut self, playlist: usize) {
        if !self.expanded.remove(&playlist) {
            self.expanded.insert(playlist);
        }
    }

    pub fn scrolled(&mut self, viewport: scrollable::Viewport) {
        self.scroll = viewport.absolute_offset().y;
        self.viewport_height = viewport.bounds().height;
    }

    fn title(&self, language: Language) -> &str {
        match self.source {
            Source::Collection => &language.strings().collection,
            Source::Playlist(index) => &self.library.playlists[index].title,
        }
    }
}

/// `m:ss`, or `h:mm:ss` from an hour on.
pub fn format_time(seconds: u32) -> String {
    let (hours, minutes, seconds) = (seconds / 3600, seconds / 60 % 60, seconds % 60);
    if hours > 0 {
        format!("{hours}:{minutes:02}:{seconds:02}")
    } else {
        format!("{minutes}:{seconds:02}")
    }
}

/// The playlist tree: pick what the track list shows.
pub fn playlists<'a>(browser: &'a Browser, language: Language) -> Element<'a, Message> {
    let mut list = column![].spacing(2);

    list = list.push(
        button(text(&language.strings().collection))
            .width(Fill)
            .style(if browser.source == Source::Collection {
                button::primary
            } else {
                button::text
            })
            .on_press(Message::ShowCollection),
    );

    // Everything below a closed folder is skipped.
    let mut hidden_below: Option<usize> = None;
    for (index, playlist) in browser.library.playlists.iter().enumerate() {
        if let Some(depth) = hidden_below {
            if playlist.depth > depth {
                continue;
            }
            hidden_below = None;
        }

        let expanded = browser.expanded.contains(&index);
        let toggle: Element<'a, Message> = if playlist.has_children {
            button(text(if expanded { "▾" } else { "▸" }))
                .padding([4, 6])
                .style(button::text)
                .on_press(Message::ToggleFolder(index))
                .into()
        } else {
            Space::new().width(26).into()
        };
        if playlist.has_children && !expanded {
            hidden_below = Some(playlist.depth);
        }

        let selected = browser.source == Source::Playlist(index);
        list = list.push(
            row![
                Space::new().width(playlist.depth as f32 * INDENT),
                toggle,
                button(text(&playlist.title))
                    .width(Fill)
                    .style(if selected {
                        button::primary
                    } else {
                        button::text
                    })
                    .on_press(Message::ShowPlaylist(index)),
            ]
            .align_y(iced::Center),
        );
    }

    scrollable(list).width(Fill).height(Fill).into()
}

/// Column widths, shared by the header and the rows. The first is the "now playing" marker.
const WIDTHS: [Length; 8] = [
    Length::Fixed(24.0),
    Length::Fixed(THUMBNAIL),
    Length::FillPortion(5),
    Length::FillPortion(3),
    Length::FillPortion(3),
    Length::FillPortion(2),
    Length::Fixed(64.0),
    Length::Fixed(64.0),
];

fn line<'a>(cells: [Element<'a, Message>; 8]) -> row::Row<'a, Message> {
    let mut line = row![].spacing(8).align_y(iced::Center).padding([0, 8]);
    for (cell, width) in cells.into_iter().zip(WIDTHS) {
        line = line.push(container(cell).width(width).clip(true));
    }
    line
}

fn cell<'a>(content: impl iced::widget::text::IntoFragment<'a>) -> text::Text<'a> {
    text(content).size(14).wrapping(text::Wrapping::None)
}

/// The tracks of the shown list. `playing` is the row that is playing, if it is in this list.
pub fn tracks<'a>(
    browser: &'a Browser,
    playing: Option<usize>,
    covers: &Covers,
    language: Language,
) -> Element<'a, Message> {
    let strings = language.strings();
    let list = browser.tracks();

    let heading = text(fill(
        &strings.tracks_heading,
        &[("name", &browser.title(language)), ("count", &list.len())],
    ))
    .size(20);

    let header = container(line([
        Space::new().into(),
        Space::new().into(),
        cell(&strings.col_title).style(text::secondary).into(),
        cell(&strings.col_artist).style(text::secondary).into(),
        cell(&strings.col_album).style(text::secondary).into(),
        cell(&strings.col_genre).style(text::secondary).into(),
        cell(&strings.col_bpm).style(text::secondary).into(),
        cell(&strings.col_length).style(text::secondary).into(),
    ]))
    .padding([4, 0])
    .style(container::bordered_box)
    .width(Fill);

    if list.is_empty() {
        return column![heading, header, text(&strings.no_tracks)]
            .spacing(10)
            .width(Fill)
            .into();
    }

    // Only the rows near the viewport exist; spacers stand in for the rest.
    let Range {
        start: first,
        end: last,
    } = browser.visible_rows();

    let mut body = column![Space::new().height(first as f32 * ROW_HEIGHT)];
    for (row_index, &track_index) in list.iter().enumerate().take(last).skip(first) {
        let track = &browser.library.tracks[track_index];
        body = body.push(track_row(
            track,
            row_index,
            browser.selected == Some(row_index),
            playing == Some(row_index),
            track.album_art.and_then(|id| covers.get(id)),
        ));
    }
    body = body.push(Space::new().height((list.len() - last) as f32 * ROW_HEIGHT));

    column![
        heading,
        header,
        scrollable(body)
            .id(iced::widget::Id::new(TRACKS))
            .on_scroll(Message::TrackListScrolled)
            .width(Fill)
            .height(Fill),
    ]
    .spacing(10)
    .width(Fill)
    .into()
}

/// Id of the track list's scrollable, to scroll it back to the top.
const TRACKS: &str = "tracks";

pub fn scroll_to_top() -> iced::Task<Message> {
    iced::widget::operation::scroll_to(
        iced::widget::Id::new(TRACKS),
        scrollable::AbsoluteOffset { x: 0.0, y: 0.0 },
    )
}

fn track_row<'a>(
    track: &'a Track,
    row_index: usize,
    selected: bool,
    playing: bool,
    cover: Option<&Handle>,
) -> Element<'a, Message> {
    // Streaming tracks have no file here, so they can be seen but not played.
    let style = |content: text::Text<'a>| -> Element<'a, Message> {
        if track.streaming {
            content.style(text::secondary).into()
        } else {
            content.into()
        }
    };

    let bpm = track.bpm.map(|bpm| format!("{bpm:.1}")).unwrap_or_default();
    let length = track.length.map(format_time).unwrap_or_default();

    // Covers that are still loading, or that don't exist, leave the space empty.
    let thumbnail: Element<'a, Message> = match cover {
        Some(handle) => image(handle.clone())
            .width(THUMBNAIL)
            .height(THUMBNAIL)
            .content_fit(ContentFit::Contain)
            .into(),
        None => Space::new().into(),
    };

    let cells = line([
        cell(if playing { "▶" } else { "" }).into(),
        thumbnail,
        style(cell(&track.title)),
        style(cell(&track.artist)),
        style(cell(&track.album)),
        style(cell(&track.genre)),
        style(cell(bpm)),
        style(cell(length)),
    ]);

    mouse_area(
        container(cells)
            .height(ROW_HEIGHT)
            .width(Fill)
            .align_y(iced::Center)
            .style(move |theme: &Theme| {
                let palette = theme.extended_palette();
                let pair = if playing {
                    Some(palette.primary.strong)
                } else if selected {
                    Some(palette.secondary.weak)
                } else {
                    None
                };
                container::Style {
                    background: pair.map(|pair| pair.color.into()),
                    text_color: pair.map(|pair| pair.text),
                    ..container::Style::default()
                }
            }),
    )
    .on_press(Message::SelectTrack(row_index))
    .on_double_click(Message::PlayTrack(row_index))
    .into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_times() {
        assert_eq!(format_time(0), "0:00");
        assert_eq!(format_time(75), "1:15");
        assert_eq!(format_time(3600), "1:00:00");
        assert_eq!(format_time(3725), "1:02:05");
    }
}
