//! The open library as the user sees it: a playlist tree on the left and the
//! tracks of the chosen playlist on the right.

use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::sync::Arc;

use iced::widget::canvas::{Frame, Geometry, Path};
use iced::widget::image::Handle;
use iced::widget::{
    Space, button, canvas, checkbox, column, container, image, mouse_area, row, scrollable, text,
    tooltip,
};
use iced::{Color, ContentFit, Element, Fill, Point, Rectangle, Renderer, Size, Theme, mouse};

use crate::Message;
use crate::columns::{Column, ColumnWidths, Hidden};
use crate::config::Language;
use crate::covers::Covers;
use crate::i18n::fill;
use crate::model::{
    ANALYSIS_BEATGRID, ANALYSIS_KINDS, ANALYSIS_TRACK_DATA, ANALYSIS_WAVEFORM, AnalysisOrigin,
    Kind, Library, Lighting, Playlist, Track, analysis_count, stars_of,
};
use crate::reachable::Reachability;
use crate::right_click::right_click;

/// Height of one track row. Fixed, so only the rows in view have to be built.
const ROW_HEIGHT: f32 = 32.0;
/// Side of the cover thumbnail in a row.
const THUMBNAIL: f32 = 28.0;
/// Rows built above and below the visible ones, so scrolling doesn't show gaps.
const ROW_BUFFER: usize = 10;
/// Rows kept above a row that is scrolled to, so it isn't right at the edge.
const REVEAL_MARGIN: usize = 3;
/// Window heights vary and resizing sends no scroll event, so always build at least
/// enough rows for a tall window.
const MIN_VIEWPORT: f32 = 1600.0;
const INDENT: f32 = 16.0;
/// Side of the sign that marks a path that can't be reached.
const WARNING_ICON: f32 = 14.0;
/// Side of the icon that marks a smart playlist.
const SMART_ICON: f32 = 14.0;
/// The width of the place before a playlist's title: the arrow of a folder, or the mark
/// of a smart playlist.
const TOGGLE_WIDTH: f32 = 26.0;

/// Which list the track pane shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// Every track in the library.
    Collection,
    /// An index into `Library::playlists`.
    Playlist(usize),
}

/// What the track list can be sorted by.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortKey {
    Column(Column),
    /// The file's location, the last column.
    Location,
    /// The number of the track in its playlist: only to put the list back the other way
    /// round, as it starts out in that order.
    Number,
}

/// An order the shown list was put in by clicking a header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sort {
    pub key: SortKey,
    pub descending: bool,
}

/// What a track is compared by. Text is compared without regard to case.
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Value {
    Text(String),
    Number(u64),
}

fn text_value(text: &str) -> Option<Value> {
    (!text.is_empty()).then(|| Value::Text(text.to_lowercase()))
}

/// How the numbers of the playlists moved when one was added or deleted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Change {
    /// Nothing moved (a playlist was renamed or its rules changed).
    Same,
    /// A playlist was added at this number, and the ones from there on moved down.
    Inserted(usize),
    /// The playlists in this range were deleted, and the ones after it moved up.
    Removed(Range<usize>),
}

impl Change {
    /// Where the playlist that was number `index` is now, or `None` if it was deleted.
    pub fn map(&self, index: usize) -> Option<usize> {
        match self {
            Change::Same => Some(index),
            Change::Inserted(at) => Some(if index >= *at { index + 1 } else { index }),
            Change::Removed(range) if range.contains(&index) => None,
            Change::Removed(range) => Some(if index >= range.end {
                index - range.len()
            } else {
                index
            }),
        }
    }
}

/// Origins saved since the library was fetched.
/// How many cues and loops tracks have now, by track id, for the ones that changed since the
/// library was fetched.
#[derive(Default)]
struct SavedCounts {
    cues: HashMap<u32, u32>,
    loops: HashMap<u32, u32>,
    /// The tracks analysed since the library was fetched.
    analysed: HashMap<u32, Analysed>,
}

/// What a track has after Open Engine Lib analysed it, in place of what the library said when
/// it was fetched.
#[derive(Debug, Clone, PartialEq)]
pub struct Analysed {
    pub bits: u32,
    pub origin: AnalysisOrigin,
    pub note: Option<String>,
    pub bpm: Option<f64>,
}

#[derive(Default)]
struct SavedOrigins {
    /// By track id.
    by_track: HashMap<u32, String>,
    /// What the last saving did, in words.
    notice: Option<String>,
}

pub struct Browser {
    pub library: Arc<Library>,
    pub source: Source,
    /// Row (position in the shown list) the user clicked.
    pub selected: Option<usize>,
    all: Vec<usize>,
    /// Ratings set since the library was fetched, by track id. (The library is shared
    /// and not changed; the backend has these too.)
    ratings: HashMap<u32, u32>,
    /// The cue and loop counts changed since the library was fetched (boxed to keep
    /// `Browser` small).
    counts: Box<SavedCounts>,
    /// The origins saved since the library was fetched (boxed to keep `Browser` small).
    origins: Box<SavedOrigins>,
    /// Playlist folders that are opened up in the tree.
    expanded: HashSet<usize>,
    scroll: f32,
    viewport_height: f32,
    /// The order the user chose with a header, if any: the list is then `sorted`.
    sort: Option<Sort>,
    /// The shown list in that order (boxed to keep `Browser` small).
    sorted: Box<Sorted>,
}

/// The shown list in the order of the sort, kept while a sort is on.
#[derive(Default)]
struct Sorted {
    /// Indices into `Library::tracks`.
    tracks: Vec<usize>,
    /// For each row, the place the track has in the unsorted list.
    positions: Vec<usize>,
}

impl Browser {
    /// The rating of the track with `id`, 0 to 100, as it is now.
    pub fn rating(&self, id: u32) -> Option<u32> {
        self.ratings
            .get(&id)
            .copied()
            .or_else(|| self.library.track_by_id(id).and_then(|track| track.rating))
    }

    pub fn set_rating(&mut self, id: u32, rating: Option<u32>) {
        match rating {
            Some(rating) => self.ratings.insert(id, rating),
            None => self.ratings.remove(&id),
        };
    }

    /// How many cues the track with `id` has set, as it is now.
    pub fn cue_count(&self, id: u32) -> u32 {
        self.counts.cues.get(&id).copied().unwrap_or_else(|| {
            self.library
                .track_by_id(id)
                .map_or(0, |track| track.cue_count)
        })
    }

    pub fn set_cue_count(&mut self, id: u32, count: u32) {
        self.counts.cues.insert(id, count);
    }

    /// How many loops the track with `id` has saved, as it is now.
    pub fn loop_count(&self, id: u32) -> u32 {
        self.counts.loops.get(&id).copied().unwrap_or_else(|| {
            self.library
                .track_by_id(id)
                .map_or(0, |track| track.loop_count)
        })
    }

    pub fn set_loop_count(&mut self, id: u32, count: u32) {
        self.counts.loops.insert(id, count);
    }

    /// Which analysis results (the `ANALYSIS_*` bits) the track has, as it is now.
    pub fn analysis(&self, track: &Track) -> u32 {
        self.counts
            .analysed
            .get(&track.id)
            .map_or(track.analysis, |now| now.bits)
    }

    /// Who made the analysis the track has.
    pub fn analysis_origin(&self, track: &Track) -> AnalysisOrigin {
        self.counts
            .analysed
            .get(&track.id)
            .map_or(track.analysis_origin, |now| now.origin)
    }

    /// What is known about an analysis that Open Engine Lib made.
    pub fn analysis_note<'a>(&'a self, track: &'a Track) -> Option<&'a str> {
        match self.counts.analysed.get(&track.id) {
            Some(now) => now.note.as_deref(),
            None => track.analysis_note.as_deref(),
        }
    }

    /// The tempo of the track, as it is now (an analysis changes it).
    pub fn bpm(&self, track: &Track) -> Option<f64> {
        self.counts
            .analysed
            .get(&track.id)
            .and_then(|now| now.bpm)
            .or(track.bpm)
    }

    /// The track was analysed here: this is what it has now. The shown list is put in order
    /// again, as it may be sorted by what changed.
    pub fn set_analysed(&mut self, id: u32, now: Analysed) {
        self.counts.analysed.insert(id, now);
        self.resort();
    }

    /// Where the file of `track` was when its origin was saved.
    pub fn origin<'a>(&'a self, track: &'a Track) -> Option<&'a str> {
        self.origins
            .by_track
            .get(&track.id)
            .or(track.origin.as_ref())
            .map(String::as_str)
    }

    /// Takes the origins the backend now has.
    pub fn set_origins(&mut self, origins: impl IntoIterator<Item = (u32, String)>) {
        self.origins.by_track.extend(origins);
    }

    /// What the last saving of origins did, in words.
    pub fn origin_notice(&self) -> Option<&str> {
        self.origins.notice.as_deref()
    }

    pub fn set_origin_notice(&mut self, notice: String) {
        self.origins.notice = Some(notice);
    }

    pub fn new(library: Arc<Library>) -> Self {
        let all = (0..library.tracks.len()).collect();
        Self {
            library,
            source: Source::Collection,
            selected: None,
            all,
            ratings: HashMap::new(),
            counts: Box::default(),
            origins: Box::default(),
            expanded: HashSet::new(),
            scroll: 0.0,
            viewport_height: MIN_VIEWPORT,
            sort: None,
            sorted: Box::default(),
        }
    }

    /// The shown list as the library has it, before any sorting.
    fn unsorted(&self) -> &[usize] {
        match self.source {
            Source::Collection => &self.all,
            Source::Playlist(index) => &self.library.playlists[index].tracks,
        }
    }

    /// Indices into `Library::tracks` of the shown list, in order.
    pub fn tracks(&self) -> &[usize] {
        if self.sort.is_some() {
            &self.sorted.tracks
        } else {
            self.unsorted()
        }
    }

    /// Where the track of `row` stands in the shown list when it is not sorted.
    pub fn position(&self, row: usize) -> usize {
        if self.sort.is_some() {
            self.sorted.positions[row]
        } else {
            row
        }
    }

    /// Whether the tracks of the shown list have a number: those of an ordinary playlist
    /// do, as the order is the playlist's own. (A smart playlist's tracks come from its
    /// rules, and all tracks have no order.)
    pub fn numbered(&self) -> bool {
        match self.source {
            Source::Collection => false,
            Source::Playlist(index) => !self.library.playlists[index].is_smart,
        }
    }

    pub fn sort(&self) -> Option<Sort> {
        self.sort
    }

    /// What `track` is compared by when the list is sorted by `key`. Nothing sorts last,
    /// whichever way round.
    fn value(&self, key: SortKey, track: &Track, position: usize) -> Option<Value> {
        let number = |number: u64| Some(Value::Number(number));
        match key {
            SortKey::Number => number(position as u64),
            SortKey::Location if track.streaming => None,
            SortKey::Location => text_value(&track.location),
            SortKey::Column(column) => match column {
                Column::Title => text_value(&track.title),
                Column::Artist => text_value(&track.artist),
                Column::Album => text_value(&track.album),
                Column::Genre => text_value(&track.genre),
                Column::Bpm => self
                    .bpm(track)
                    .and_then(|bpm| number((bpm * 100.0).round() as u64)),
                Column::Length => track.length.and_then(|length| number(length.into())),
                Column::Rating => self
                    .rating(track.id)
                    .and_then(|rating| number(rating.into())),
                Column::Cues => number(self.cue_count(track.id).into()),
                Column::Loops => number(self.loop_count(track.id).into()),
                Column::Analysis => number(analysis_count(self.analysis(track)).into()),
                Column::AnalysisOrigin => number(match self.analysis_origin(track) {
                    AnalysisOrigin::OpenEngineLib => 2,
                    AnalysisOrigin::Engine => 1,
                    AnalysisOrigin::None => 0,
                }),
                Column::Lighting if track.streaming => None,
                Column::Lighting => number(match track.lighting {
                    Lighting::Rendered => 2,
                    Lighting::Analysed => 1,
                    Lighting::None => 0,
                }),
                Column::Stems if track.streaming => None,
                Column::Stems => number(u64::from(track.has_stems)),
                Column::Origin => self.origin(track).and_then(text_value),
            },
        }
    }

    /// Puts the shown list in the order of `sort` again (after the list, the sort or a
    /// value it goes by changed). Tracks that compare the same keep the order they have in
    /// the playlist.
    fn resort(&mut self) {
        let Some(sort) = self.sort else {
            *self.sorted = Sorted::default();
            return;
        };
        let list = self.unsorted();
        let values: Vec<_> = list
            .iter()
            .enumerate()
            .map(|(position, &track)| self.value(sort.key, &self.library.tracks[track], position))
            .collect();
        let mut positions: Vec<usize> = (0..list.len()).collect();
        positions.sort_by(|&a, &b| match (&values[a], &values[b]) {
            (Some(a), Some(b)) if sort.descending => b.cmp(a),
            (Some(a), Some(b)) => a.cmp(b),
            (Some(_), None) => Ordering::Less,
            (None, Some(_)) => Ordering::Greater,
            (None, None) => Ordering::Equal,
        });
        *self.sorted = Sorted {
            tracks: positions.iter().map(|&position| list[position]).collect(),
            positions,
        };
    }

    /// A click on the header of `key`: sorts the list by it, then the other way round, then
    /// puts it back as it was. (The number only goes back to the start: the list is in
    /// that order already.) The row that was selected stays selected.
    pub fn sort_by(&mut self, key: SortKey) -> iced::Task<Message> {
        let selected = self.selected.map(|row| self.position(row));
        self.sort = match self.sort {
            Some(Sort {
                key: current,
                descending: false,
            }) if current == key && key != SortKey::Number => Some(Sort {
                key,
                descending: true,
            }),
            Some(Sort { key: current, .. }) if current == key => None,
            _ => Some(Sort {
                key,
                descending: key == SortKey::Number,
            }),
        };
        self.resort();
        self.selected = selected.and_then(|position| {
            (0..self.tracks().len()).find(|&row| self.position(row) == position)
        });
        match self.selected {
            Some(row) => self.reveal(row),
            None => {
                self.scroll = 0.0;
                scroll_to_top()
            }
        }
    }

    /// The rows of the shown list that are built: the ones in view plus a margin.
    pub fn visible_rows(&self) -> Range<usize> {
        let first = ((self.scroll / ROW_HEIGHT) as usize).saturating_sub(ROW_BUFFER);
        let visible = (self.viewport_height.max(MIN_VIEWPORT) / ROW_HEIGHT).ceil() as usize;
        let last = (first + visible + 2 * ROW_BUFFER).min(self.tracks().len());
        first.min(last)..last
    }

    /// The locations and saved origins of the rows that are built, which are the paths to
    /// have checked.
    pub fn visible_paths(&self) -> impl Iterator<Item = &str> + '_ {
        let list = self.tracks();
        self.visible_rows().flat_map(move |row| {
            let track = &self.library.tracks[list[row]];
            let location = (!track.streaming).then_some(track.location.as_str());
            [location, self.origin(track)].into_iter().flatten()
        })
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
        self.resort();
    }

    /// Opens the folders on the way down to the last playlist of `chain` (see
    /// `Library::playlist_chain`), so that it is visible in the tree.
    pub fn expand(&mut self, chain: &[usize]) {
        if let Some((_, parents)) = chain.split_last() {
            self.expanded.extend(parents.iter().copied());
        }
    }

    /// Takes the playlists as they are after one was added, changed or deleted. What was
    /// shown stays shown (unless it is gone, then it is all tracks), and so do the open
    /// folders. With `show` that playlist is shown instead.
    pub fn replace_playlists(
        &mut self,
        playlists: Vec<Playlist>,
        change: &Change,
        show: Option<usize>,
    ) {
        let library = Arc::new(self.library.with_playlists(playlists));
        self.expanded = self
            .expanded
            .iter()
            .filter_map(|&index| change.map(index))
            .collect();
        let source = match self.source {
            Source::Playlist(index) => change.map(index).map(Source::Playlist),
            Source::Collection => Some(Source::Collection),
        };
        let shown = show
            .filter(|&index| index < library.playlists.len())
            .map(Source::Playlist);
        self.library = library;
        match (shown, source) {
            (Some(shown), _) => {
                if let Source::Playlist(index) = shown {
                    self.expand(&self.library.playlist_chain(index));
                }
                self.show(shown);
            }
            (None, Some(source)) => {
                self.source = source;
                self.resort();
            }
            (None, None) => self.show(Source::Collection),
        }
    }

    /// Scrolls the track list so that `row` is near its top.
    pub fn reveal(&mut self, row: usize) -> iced::Task<Message> {
        let y = row.saturating_sub(REVEAL_MARGIN) as f32 * ROW_HEIGHT;
        self.scroll = y;
        scroll_to(y)
    }

    pub fn is_expanded(&self, playlist: usize) -> bool {
        self.expanded.contains(&playlist)
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

        let expanded = browser.is_expanded(index);
        let toggle: Element<'a, Message> = if playlist.has_children {
            button(text(if expanded { "▾" } else { "▸" }))
                .padding([4, 6])
                .style(button::text)
                .on_press(Message::ToggleFolder(index))
                .into()
        } else if playlist.is_smart {
            // Smart playlists have no children, so their mark takes the place of the arrow.
            container(canvas(SmartIcon).width(SMART_ICON).height(SMART_ICON))
                .center_x(TOGGLE_WIDTH)
                .into()
        } else {
            Space::new().width(TOGGLE_WIDTH).into()
        };
        if playlist.has_children && !expanded {
            hidden_below = Some(playlist.depth);
        }

        let selected = browser.source == Source::Playlist(index);
        let mut line = row![
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
        .align_y(iced::Center);
        // A smart playlist always has its edit button. An ordinary one gets its buttons
        // when it is the one shown, so the tree isn't full of them.
        let strings = language.strings();
        let small = |label: &'static str, tip: &'a String, message: Message| {
            tooltip(
                button(text(label))
                    .padding([4, 8])
                    .style(button::text)
                    .on_press(message),
                text(tip),
                tooltip::Position::Right,
            )
            .style(container::rounded_box)
        };
        if playlist.is_smart {
            line = line.push(small("✎", &strings.smart_edit, Message::SmartEdit(index)));
        } else if selected && browser.library.is_engine() {
            line = line
                .push(small(
                    "✎",
                    &strings.playlist_rename,
                    Message::PlaylistRename(index),
                ))
                .push(small(
                    "×",
                    &strings.playlist_delete,
                    Message::PlaylistDelete(index),
                ));
        }
        list = list.push(line);
    }

    scrollable(list).width(Fill).height(Fill).into()
}

/// The mark of a smart playlist: a sparkle, in the text color of the page.
/// Drawn rather than written as a character so that it looks the same whatever fonts
/// there are.
struct SmartIcon;

impl canvas::Program<Message> for SmartIcon {
    type State = ();

    fn draw(
        &self,
        _state: &(),
        renderer: &Renderer,
        theme: &Theme,
        bounds: Rectangle,
        _cursor: mouse::Cursor,
    ) -> Vec<Geometry> {
        let color: Color = theme.extended_palette().background.base.text;
        let side = bounds.width.min(bounds.height);

        // A four-pointed star: points on the axes, pinched in between.
        const POINTS: [(f32, f32); 8] = [
            (0.5, 0.0),
            (0.62, 0.38),
            (1.0, 0.5),
            (0.62, 0.62),
            (0.5, 1.0),
            (0.38, 0.62),
            (0.0, 0.5),
            (0.38, 0.38),
        ];
        let mut frame = Frame::new(renderer, Size::new(side, side));
        let star = Path::new(|builder| {
            let mut corners = POINTS.iter().map(|&(x, y)| Point::new(x * side, y * side));
            if let Some(first) = corners.next() {
                builder.move_to(first);
                corners.for_each(|corner| builder.line_to(corner));
                builder.close();
            }
        });
        frame.fill(&star, color);
        vec![frame.into_geometry()]
    }
}

/// The width of the marker column that shows which track is playing.
const MARKER_WIDTH: f32 = 24.0;
/// The width of the column with the number of a track in its playlist.
const NUMBER_WIDTH: f32 = 36.0;
/// The gap after every column. In the header it holds the handle to drag the column's
/// width, in the rows it is empty, so that the columns line up.
const GAP: f32 = 10.0;
/// The height of a header handle: the text of a header and a little more, so that it is
/// easy to grab.
const HANDLE_HEIGHT: f32 = 24.0;

/// How the lines of the track list are laid out.
#[derive(Clone, Copy)]
struct Layout<'a> {
    widths: &'a ColumnWidths,
    /// The columns the user hid.
    hidden: &'a Hidden,
    /// What the library is made of, which decides the columns.
    kind: Kind,
}

/// One line of the track list, the header or a track: the number (in an ordinary
/// playlist), the marker, the cover, the resizable columns and then the location, which
/// takes the rest of the width.
/// `gap` is what goes after each column (`None` for the number, the marker and the cover).
fn line<'a>(
    number: Option<Element<'a, Message>>,
    marker: Element<'a, Message>,
    cover: Element<'a, Message>,
    cells: [Element<'a, Message>; Column::COUNT],
    location: Element<'a, Message>,
    layout: Layout<'_>,
    gap: impl Fn(Option<Column>) -> Element<'a, Message>,
) -> row::Row<'a, Message> {
    let Layout {
        widths,
        hidden,
        kind,
    } = layout;
    let mut line = row![].align_y(iced::Center).padding([0, 8]);
    if let Some(number) = number {
        line = line
            .push(container(number).width(NUMBER_WIDTH).clip(true))
            .push(gap(None));
    }
    line = line
        .push(container(marker).width(MARKER_WIDTH))
        .push(gap(None))
        .push(container(cover).width(THUMBNAIL))
        .push(gap(None));
    for (column, cell) in Column::ALL.into_iter().zip(cells) {
        if !shown(column, kind, hidden) {
            continue;
        }
        line = line
            .push(container(cell).width(widths.get(column)).clip(true))
            .push(gap(Some(column)));
    }
    line.push(container(location).width(Fill).clip(true))
}

/// Whether the track list has the column: not if the user hid it, and not an Engine
/// library's column in a folder of music, which has no ratings, cues and so on to show.
pub fn shown(column: Column, kind: Kind, hidden: &Hidden) -> bool {
    !hidden.contains(column) && !(kind == Kind::Folder && column.is_engine_only())
}

/// The empty gap of a track row.
fn plain_gap<'a>(_: Option<Column>) -> Element<'a, Message> {
    Space::new().width(GAP).into()
}

/// The gap after a column in the header: a thin divider that can be dragged sideways to
/// change the width of the column before it.
fn handle<'a>(column: Option<Column>) -> Element<'a, Message> {
    let Some(column) = column else {
        return plain_gap(None);
    };
    let divider =
        container(Space::new().width(1).height(HANDLE_HEIGHT - 6.0)).style(|theme: &Theme| {
            container::Style {
                background: Some(theme.extended_palette().background.strong.color.into()),
                ..container::Style::default()
            }
        });
    // A fixed height: a handle that filled the height would make the whole header as
    // tall as the list.
    mouse_area(container(divider).center_x(GAP).center_y(HANDLE_HEIGHT))
        .on_press(Message::ResizeStarted(column))
        .interaction(mouse::Interaction::ResizingHorizontally)
        .into()
}

fn cell<'a>(content: impl iced::widget::text::IntoFragment<'a>) -> text::Text<'a> {
    text(content).size(14).wrapping(text::Wrapping::None)
}

/// The bar of column titles. Its height is that of its text and the handles in it,
/// whatever the height of the window. Clicking a title sorts the list by it; the one it is
/// sorted by shows which way.
fn header<'a>(
    language: Language,
    layout: Layout<'_>,
    sort: Option<Sort>,
    numbered: bool,
) -> Element<'a, Message> {
    let strings = language.strings();
    let heading_cell = |label: &str, key: SortKey| -> Element<'a, Message> {
        let sorted = sort.filter(|sort| sort.key == key);
        let arrow = match sorted {
            Some(Sort {
                descending: true, ..
            }) => " ▼",
            Some(_) => " ▲",
            None => "",
        };
        let label = cell(format!("{label}{arrow}"));
        mouse_area(
            container(if sorted.is_some() {
                label
            } else {
                label.style(text::secondary)
            })
            .width(Fill),
        )
        .on_press(Message::SortBy(key))
        .interaction(mouse::Interaction::Pointer)
        .into()
    };
    let by = |column: Column, label: &str| heading_cell(label, SortKey::Column(column));
    container(line(
        numbered.then(|| heading_cell("#", SortKey::Number)),
        Space::new().into(),
        Space::new().into(),
        [
            by(Column::Title, &strings.col_title),
            by(Column::Artist, &strings.col_artist),
            by(Column::Album, &strings.col_album),
            by(Column::Genre, &strings.col_genre),
            by(Column::Bpm, &strings.col_bpm),
            by(Column::Length, &strings.col_length),
            by(Column::Rating, &strings.col_rating),
            by(Column::Cues, &strings.col_cues),
            by(Column::Loops, &strings.col_loops),
            by(Column::Analysis, &strings.col_analysis),
            by(Column::AnalysisOrigin, &strings.col_analysis_origin),
            by(Column::Lighting, &strings.col_lighting),
            by(Column::Stems, &strings.col_stems),
            by(Column::Origin, &strings.col_origin),
        ],
        heading_cell(&strings.col_location, SortKey::Location),
        layout,
        handle,
    ))
    .padding([4, 0])
    .style(container::bordered_box)
    .width(Fill)
    .into()
}

/// What decides the columns of the track list.
#[derive(Clone, Copy)]
pub struct Columns<'a> {
    pub widths: &'a ColumnWidths,
    /// The columns the user hid.
    pub hidden: &'a Hidden,
    /// Whether the menu to show and hide columns is open.
    pub picking: bool,
}

/// The name of a column in the UI language.
fn column_name(column: Column, language: Language) -> &'static str {
    // (The strings live as long as the program: `Language::strings` hands out statics.)
    let strings = language.strings();
    match column {
        Column::Title => &strings.col_title,
        Column::Artist => &strings.col_artist,
        Column::Album => &strings.col_album,
        Column::Genre => &strings.col_genre,
        Column::Bpm => &strings.col_bpm,
        Column::Length => &strings.col_length,
        Column::Rating => &strings.col_rating,
        Column::Cues => &strings.col_cues,
        Column::Loops => &strings.col_loops,
        Column::Analysis => &strings.col_analysis,
        Column::AnalysisOrigin => &strings.col_analysis_origin,
        Column::Lighting => &strings.col_lighting,
        Column::Stems => &strings.col_stems,
        Column::Origin => &strings.col_origin,
    }
}

/// A tick for each column that the library has, to choose which ones the list shows. (The
/// location, which takes the rest of the width, is always there.)
fn column_picker<'a>(kind: Kind, hidden: &Hidden, language: Language) -> Element<'a, Message> {
    let strings = language.strings();
    let mut ticks = row![].spacing(18).width(Fill);
    for column in Column::ALL {
        if kind == Kind::Folder && column.is_engine_only() {
            continue;
        }
        ticks = ticks.push(
            checkbox(!hidden.contains(column))
                .label(column_name(column, language))
                .size(16)
                .text_size(14)
                .on_toggle(move |shown| Message::ColumnShown(column, shown)),
        );
    }
    container(
        column![
            text(&strings.columns_hint).size(13).style(text::secondary),
            ticks.wrap().vertical_spacing(6)
        ]
        .spacing(6),
    )
    .padding([8, 12])
    .width(Fill)
    .style(container::bordered_box)
    .into()
}

/// The tracks of the shown list. `playing` is the row that is playing, if it is in this list.
pub fn tracks<'a>(
    browser: &'a Browser,
    playing: Option<usize>,
    covers: &Covers,
    reachable: &Reachability,
    columns: Columns<'_>,
    language: Language,
) -> Element<'a, Message> {
    let strings = language.strings();
    let list = browser.tracks();
    let Columns {
        widths,
        hidden,
        picking,
    } = columns;

    let kind = browser.library.kind;
    let title = text(fill(
        &strings.tracks_heading,
        &[("name", &browser.title(language)), ("count", &list.len())],
    ))
    .size(20);
    let columns_button = button(text(format!(
        "{} {}",
        strings.columns_button,
        if picking { "▴" } else { "▾" }
    )))
    .padding([2, 10])
    .style(button::secondary)
    .on_press(Message::ColumnsMenuToggle);
    let mut heading =
        column![row![title, Space::new().width(Fill), columns_button].align_y(iced::Center)]
            .spacing(8);
    if picking {
        heading = heading.push(column_picker(kind, hidden, language));
    }

    let layout = Layout {
        widths,
        hidden,
        kind,
    };
    let numbered = browser.numbered();
    let header = header(language, layout, browser.sort(), numbered);

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
            RowState {
                selected: browser.selected == Some(row_index),
                origin: browser.origin(track),
                location_reachable: reachable.get(&track.location),
                origin_reachable: browser
                    .origin(track)
                    .and_then(|origin| reachable.get(origin)),
                playing: playing == Some(row_index),
                stars: stars_of(browser.rating(track.id)),
                cues: browser.cue_count(track.id),
                loops: browser.loop_count(track.id),
                analysis: browser.analysis(track),
                analysis_origin: browser.analysis_origin(track),
                analysis_note: browser.analysis_note(track),
                bpm: browser.bpm(track),
                number: numbered.then(|| browser.position(row_index) + 1),
            },
            track.album_art.and_then(|id| covers.get(id)),
            layout,
            language,
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
    scroll_to(0.0)
}

fn scroll_to(y: f32) -> iced::Task<Message> {
    iced::widget::operation::scroll_to(
        iced::widget::Id::new(TRACKS),
        scrollable::AbsoluteOffset { x: 0.0, y },
    )
}

/// The rating as five stars, filled for the ones the track has. Clicking a star gives the
/// track that many stars; clicking the last filled one takes them all away.
fn stars_cell<'a>(track: &Track, stars: u8, dim: bool) -> Element<'a, Message> {
    let mut stars_row = row![];
    for star in 1..=5u8 {
        let mark = cell(if star <= stars { "★" } else { "☆" });
        let mark = if dim {
            mark.style(text::secondary)
        } else {
            mark
        };
        stars_row = stars_row.push(
            mouse_area(mark)
                .on_press(Message::SetStars {
                    track: track.id,
                    stars: if star == stars { 0 } else { star },
                })
                .interaction(mouse::Interaction::Pointer),
        );
    }
    stars_row.into()
}

/// How many cues (or loops) the track has set, a dash for none.
fn cues_cell<'a>(count: u32) -> Element<'a, Message> {
    if count == 0 {
        cell("–").style(text::secondary).into()
    } else {
        cell(count.to_string()).into()
    }
}

/// How many of the kinds of analysis (track data, waveform, beatgrid; the cues and loops
/// have columns of their own) the library has for the track, like `2/3`. Pointing at it
/// says which.
fn analysis_cell<'a>(bits: u32, language: Language) -> Element<'a, Message> {
    let strings = language.strings();
    let count = analysis_count(bits);
    let parts: Vec<&str> = [
        (ANALYSIS_TRACK_DATA, &strings.analysis_track_data),
        (ANALYSIS_WAVEFORM, &strings.analysis_waveform),
        (ANALYSIS_BEATGRID, &strings.analysis_beatgrid),
    ]
    .into_iter()
    .filter(|(bit, _)| bits & bit != 0)
    .map(|(_, name)| name.as_str())
    .collect();
    let tip = if count == 0 {
        strings.analysis_none.clone()
    } else {
        fill(
            &strings.analysis_present,
            &[
                ("count", &count),
                ("total", &ANALYSIS_KINDS),
                ("parts", &parts.join(", ")),
            ],
        )
    };
    let style: fn(&Theme) -> text::Style = match count {
        ANALYSIS_KINDS => text::success,
        0 => text::secondary,
        _ => text::warning,
    };
    tooltip(
        cell(format!("{count}/{ANALYSIS_KINDS}")).style(style),
        text(tip).size(13),
        tooltip::Position::FollowCursor,
    )
    .style(container::rounded_box)
    .into()
}

/// Who made the track's analysis: Engine, Open Engine Lib (pointing at it says what was used
/// and when), or a dash for none.
fn analysis_origin_cell<'a>(
    origin: AnalysisOrigin,
    note: Option<&str>,
    language: Language,
) -> Element<'a, Message> {
    let strings = language.strings();
    let (label, style, tip): (&str, fn(&Theme) -> text::Style, String) = match origin {
        AnalysisOrigin::OpenEngineLib => (
            &strings.analysis_origin_ours,
            text::primary,
            fill(
                &strings.analysis_origin_ours_tip,
                &[("note", &note.unwrap_or_default())],
            ),
        ),
        AnalysisOrigin::Engine => (
            &strings.analysis_origin_engine,
            text::base,
            strings.analysis_origin_engine_tip.clone(),
        ),
        AnalysisOrigin::None => (
            "–",
            text::secondary,
            strings.analysis_origin_none_tip.clone(),
        ),
    };
    tooltip(
        cell(label.to_owned()).style(style),
        text(tip).size(13),
        tooltip::Position::FollowCursor,
    )
    .style(container::rounded_box)
    .into()
}

/// Whether SoundSwitch has lighting for the track: a filled dot for rendered, a ring for
/// analysed only, a dash for neither. Pointing at it says in words, with the projects.
/// (Streaming tracks have no file for SoundSwitch to know, so they get no mark.)
fn lighting_cell<'a>(track: &'a Track, language: Language) -> Element<'a, Message> {
    if track.streaming {
        return Space::new().into();
    }
    let strings = language.strings();
    let (mark, style, tip): (&str, fn(&Theme) -> text::Style, String) = match track.lighting {
        Lighting::Rendered => (
            "●",
            text::success,
            fill(
                &strings.lighting_rendered,
                &[("projects", &track.lighting_projects.join(", "))],
            ),
        ),
        Lighting::Analysed => ("○", text::secondary, strings.lighting_analysed.clone()),
        Lighting::None => ("–", text::secondary, strings.lighting_none.clone()),
    };
    tooltip(
        cell(mark).style(style),
        text(tip).size(13),
        tooltip::Position::FollowCursor,
    )
    .style(container::rounded_box)
    .into()
}

/// Whether the library has stems rendered for the track: a filled dot, or a dash. Pointing
/// at it says so in words. (Streaming tracks have no file to have stems for.)
fn stems_cell<'a>(track: &'a Track, language: Language) -> Element<'a, Message> {
    if track.streaming {
        return Space::new().into();
    }
    let strings = language.strings();
    let (mark, style, tip) = if track.has_stems {
        (
            "●",
            text::success as fn(&Theme) -> text::Style,
            &strings.stems_rendered,
        )
    } else {
        (
            "–",
            text::secondary as fn(&Theme) -> text::Style,
            &strings.stems_none,
        )
    };
    tooltip(
        cell(mark).style(style),
        text(tip).size(13),
        tooltip::Position::FollowCursor,
    )
    .style(container::rounded_box)
    .into()
}

/// Where the track came from, as saved.
fn origin_cell<'a>(
    origin: Option<&'a str>,
    reachable: Option<bool>,
    language: Language,
) -> Element<'a, Message> {
    match origin {
        Some(origin) => path_cell(origin, reachable, language),
        None => cell(&language.strings().origin_none)
            .style(text::secondary)
            .into(),
    }
}

/// A path. One that can't be reached (`reachable` is `Some(false)`) is red with a warning
/// sign in front. Pointing at it shows all of a long path, and says so if it can't be reached.
fn path_cell<'a>(
    path: &'a str,
    reachable: Option<bool>,
    language: Language,
) -> Element<'a, Message> {
    if reachable != Some(false) {
        return tooltip(
            cell(path),
            text(path).size(13),
            tooltip::Position::FollowCursor,
        )
        .style(container::rounded_box)
        .into();
    }
    let shown = row![
        canvas(WarningIcon).width(WARNING_ICON).height(WARNING_ICON),
        cell(path).style(text::danger),
    ]
    .spacing(6)
    .align_y(iced::Center);
    tooltip(
        shown,
        column![
            text(path).size(13),
            text(&language.strings().path_unreachable)
                .size(13)
                .style(text::danger),
        ],
        tooltip::Position::FollowCursor,
    )
    .style(container::rounded_box)
    .into()
}

/// The mark of a path that can't be reached: a triangle with an exclamation mark, in red.
/// Drawn rather than written as a character so that it looks the same whatever fonts
/// there are.
struct WarningIcon;

impl canvas::Program<Message> for WarningIcon {
    type State = ();

    fn draw(
        &self,
        _state: &(),
        renderer: &Renderer,
        theme: &Theme,
        bounds: Rectangle,
        _cursor: mouse::Cursor,
    ) -> Vec<Geometry> {
        let palette = theme.extended_palette();
        let side = bounds.width.min(bounds.height);
        let point = |x: f32, y: f32| Point::new(x * side, y * side);

        let mut frame = Frame::new(renderer, Size::new(side, side));
        let triangle = Path::new(|builder| {
            builder.move_to(point(0.5, 0.06));
            builder.line_to(point(0.97, 0.92));
            builder.line_to(point(0.03, 0.92));
            builder.close();
        });
        frame.fill(&triangle, palette.danger.base.color);

        // The mark in it, in the color that is read on red.
        let ink = palette.danger.base.text;
        let bar = Path::new(|builder| {
            builder.move_to(point(0.46, 0.34));
            builder.line_to(point(0.54, 0.34));
            builder.line_to(point(0.53, 0.64));
            builder.line_to(point(0.47, 0.64));
            builder.close();
        });
        frame.fill(&bar, ink);
        frame.fill(&Path::circle(point(0.5, 0.77), 0.05 * side), ink);
        vec![frame.into_geometry()]
    }
}

/// What a row shows besides its track.
struct RowState<'a> {
    selected: bool,
    /// Where the track's file was when its origin was saved.
    origin: Option<&'a str>,
    /// Whether the location and the origin could be reached, if that is known.
    location_reachable: Option<bool>,
    origin_reachable: Option<bool>,
    playing: bool,
    /// The track's rating as 0 to 5 stars.
    stars: u8,
    /// How many cues the track has set.
    cues: u32,
    /// How many loops the track has saved.
    loops: u32,
    /// Which analysis results the track has (the `ANALYSIS_*` bits).
    analysis: u32,
    /// Who made them, and what is known about an analysis of Open Engine Lib's.
    analysis_origin: AnalysisOrigin,
    analysis_note: Option<&'a str>,
    /// The track's tempo, as it is now.
    bpm: Option<f64>,
    /// The number of the track in its playlist, if the list has numbers.
    number: Option<usize>,
}

fn track_row<'a>(
    track: &'a Track,
    row_index: usize,
    state: RowState<'a>,
    cover: Option<&Handle>,
    layout: Layout<'_>,
    language: Language,
) -> Element<'a, Message> {
    let RowState {
        selected,
        origin,
        location_reachable,
        origin_reachable,
        playing,
        stars,
        cues,
        loops,
        analysis,
        analysis_origin,
        analysis_note,
        bpm,
        number,
    } = state;
    let kind = layout.kind;
    // Streaming tracks have no file here, so they can be seen but not played.
    let style = |content: text::Text<'a>| -> Element<'a, Message> {
        if track.streaming {
            content.style(text::secondary).into()
        } else {
            content.into()
        }
    };

    let bpm = bpm.map(|bpm| format!("{bpm:.1}")).unwrap_or_default();
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

    // Where the file is. Streaming tracks have none here; the full path is also shown
    // when pointing at it, as a long one is cut off.
    let location: Element<'a, Message> = if track.streaming {
        cell(&language.strings().location_streaming)
            .style(text::secondary)
            .into()
    } else {
        path_cell(&track.location, location_reachable, language)
    };

    let cells = line(
        number.map(|number| cell(number.to_string()).style(text::secondary).into()),
        cell(if playing { "▶" } else { "" }).into(),
        thumbnail,
        [
            style(cell(&track.title)),
            style(cell(&track.artist)),
            style(cell(&track.album)),
            style(cell(&track.genre)),
            style(cell(bpm)),
            style(cell(length)),
            stars_cell(track, stars, track.streaming),
            cues_cell(cues),
            cues_cell(loops),
            analysis_cell(analysis, language),
            analysis_origin_cell(analysis_origin, analysis_note, language),
            lighting_cell(track, language),
            stems_cell(track, language),
            origin_cell(origin, origin_reachable, language),
        ],
        location,
        layout,
        plain_gap,
    );

    let row = mouse_area(
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
    .on_double_click(Message::PlayTrack(row_index));
    // What an Engine library's tracks can be taken out of is in the context menu; a folder
    // of music has nothing to change.
    if kind == Kind::Folder {
        row.into()
    } else {
        right_click(row, move |at| Message::TrackMenuOpen { row: row_index, at }).into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// How much room an element takes up vertically: `None` if it wants all it is given.
    fn height_of(element: &Element<'_, Message>) -> Option<f32> {
        match element.as_widget().size().height {
            iced::Length::Fixed(height) => Some(height),
            iced::Length::Shrink => Some(0.0),
            _ => None,
        }
    }

    #[test]
    fn the_header_is_as_tall_as_its_text_not_as_tall_as_the_window() {
        // A header that "fills" its height grows to the height of the whole list.
        let handle = handle(Some(Column::Title));
        assert!(
            height_of(&handle).is_some(),
            "a divider handle must not fill the height"
        );

        for language in Language::ALL {
            let widths = ColumnWidths::default();
            let hidden = Hidden::default();
            let layout = Layout {
                widths: &widths,
                hidden: &hidden,
                kind: Kind::Engine,
            };
            for numbered in [false, true] {
                let header = header(language, layout, None, numbered);
                assert!(
                    height_of(&header).is_some(),
                    "the header must not fill the height"
                );
            }
        }
    }

    #[test]
    fn playlist_numbers_move_with_additions_and_deletions() {
        assert_eq!(Change::Same.map(3), Some(3));
        assert_eq!(Change::Inserted(2).map(1), Some(1));
        assert_eq!(Change::Inserted(2).map(2), Some(3));
        assert_eq!(Change::Removed(2..4).map(1), Some(1));
        assert_eq!(Change::Removed(2..4).map(2), None);
        assert_eq!(Change::Removed(2..4).map(3), None);
        assert_eq!(Change::Removed(2..4).map(4), Some(2));
        assert_eq!(Change::Removed(2..4).map(9), Some(7));
    }

    #[test]
    fn formats_times() {
        assert_eq!(format_time(0), "0:00");
        assert_eq!(format_time(75), "1:15");
        assert_eq!(format_time(3600), "1:00:00");
        assert_eq!(format_time(3725), "1:02:05");
    }
}
