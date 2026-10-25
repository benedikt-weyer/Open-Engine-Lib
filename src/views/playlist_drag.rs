//! Moving playlists in the sidebar's tree by dragging them: what a drop on a row means, the
//! state of a drag, and the wrapper of a row that reports where the pointer is on it and
//! draws the line that shows where the playlist would go.
//!
//! The line is painted over the row's edge on a layer of its own, so it takes no room in
//! the layout and nothing shifts while it moves from row to row.

use std::time::{Duration, Instant};

use iced::advanced::widget::{Operation, Tree, Widget, tree};
use iced::advanced::{Clipboard, Layout, Renderer as _, Shell, layout, mouse, overlay, renderer};
use iced::{
    Border, Element, Event, Length, Point, Rectangle, Renderer, Size, Theme, Vector, border,
};

use crate::model::Playlist;

/// How long the pointer has to stay over a closed folder for it to open.
pub const OPEN_AFTER: Duration = Duration::from_millis(700);
/// How often the drag looks at how long the pointer has stayed.
pub const CHECK_EVERY: Duration = Duration::from_millis(100);
/// How far the pointer has to move after the button went down for it to be a drag and not a
/// click.
const THRESHOLD: f32 = 6.0;
/// The share of a row's height, at the top and at the bottom, that means before and after it.
const EDGE: f32 = 0.3;
/// How thick the line is.
const LINE: f32 = 2.0;
/// The size of the dot at the start of the line.
const DOT: f32 = 6.0;

/// Where on a row the pointer is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Zone {
    /// In its upper part: before the row, at its level.
    Before,
    /// In the middle: in the row, as its last playlist.
    Inside,
    /// In its lower part: after the row, at its level (or first in it if it is open).
    After,
}

impl Zone {
    fn at(y: f32, height: f32) -> Self {
        if y < height * EDGE {
            Zone::Before
        } else if y > height * (1.0 - EDGE) {
            Zone::After
        } else {
            Zone::Inside
        }
    }
}

/// A row and where on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Target {
    pub row: usize,
    pub zone: Zone,
}

/// A playlist being dragged (or that the button went down on; it is a drag once the pointer
/// has moved).
#[derive(Debug, Clone)]
pub struct Drag {
    /// The playlist, as an index into the library's playlists.
    pub playlist: usize,
    start: Option<Point>,
    pub active: bool,
    pub over: Option<Target>,
    /// Since when the pointer has been over the row it is over.
    pub over_since: Instant,
}

impl Drag {
    pub fn pressed(playlist: usize) -> Self {
        Self {
            playlist,
            start: None,
            active: false,
            over: None,
            over_since: Instant::now(),
        }
    }

    /// The pointer moved to `at`: the drag starts once it has gone far enough.
    pub fn moved(&mut self, at: Point) {
        let start = *self.start.get_or_insert(at);
        if !self.active && start.distance(at) > THRESHOLD {
            self.active = true;
        }
    }

    /// The pointer is over `zone` of `row` now (`None`: no longer over it).
    pub fn hover(&mut self, row: usize, zone: Option<Zone>) {
        match zone {
            Some(zone) => {
                if self.over.map(|over| over.row) != Some(row) {
                    self.over_since = Instant::now();
                }
                self.over = Some(Target { row, zone });
            }
            None if self.over.is_some_and(|over| over.row == row) => self.over = None,
            None => {}
        }
    }

    /// The row the pointer has stayed over for long enough to open it, if it is a closed
    /// folder.
    pub fn to_open(
        &self,
        playlists: &[Playlist],
        expanded: impl Fn(usize) -> bool,
        now: Instant,
    ) -> Option<usize> {
        let over = self.over?;
        let waited = now.duration_since(self.over_since) >= OPEN_AFTER;
        (self.active
            && waited
            && playlists.get(over.row).is_some_and(|row| row.has_children)
            && !expanded(over.row))
        .then_some(over.row)
    }
}

/// Where a playlist goes: the end of the playlists of `parent` (the top level for `None`),
/// or in front of the playlist `before`, one of that parent's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Place {
    pub parent: Option<usize>,
    pub before: Option<usize>,
}

/// The first row after the playlist `index` and what is in it.
fn after_span(playlists: &[Playlist], index: usize) -> usize {
    let depth = playlists[index].depth;
    playlists[index + 1..]
        .iter()
        .position(|inner| inner.depth <= depth)
        .map_or(playlists.len(), |offset| index + 1 + offset)
}

fn parent_of(playlists: &[Playlist], index: usize) -> Option<usize> {
    let depth = playlists[index].depth;
    (depth > 0).then(|| {
        (0..index)
            .rev()
            .find(|&earlier| playlists[earlier].depth == depth - 1)
            .unwrap_or(0)
    })
}

/// The playlist after `index` (and what is in it) at its level, not a smart one: smart
/// playlists are put where they are by the playlists around them, not moved.
fn next_sibling(playlists: &[Playlist], index: usize) -> Option<usize> {
    let depth = playlists[index].depth;
    let mut at = after_span(playlists, index);
    while let Some(row) = playlists.get(at) {
        if row.depth < depth {
            return None;
        }
        if !row.is_smart {
            return Some(at);
        }
        at = after_span(playlists, at);
    }
    None
}

/// The first playlist in `index`, not a smart one.
fn first_child(playlists: &[Playlist], index: usize) -> Option<usize> {
    let depth = playlists[index].depth + 1;
    playlists[index + 1..]
        .iter()
        .take_while(|inner| inner.depth >= depth)
        .position(|inner| inner.depth == depth && !inner.is_smart)
        .map(|offset| index + 1 + offset)
}

/// Where dropping the playlist `dragged` on `target` puts it, or `None` if it can't go there
/// (into itself) or would stay where it is. `expanded` says which folders are open: below an
/// open one, the row after it is its first playlist.
pub fn place(
    playlists: &[Playlist],
    expanded: impl Fn(usize) -> bool,
    dragged: usize,
    target: Target,
) -> Option<Place> {
    let hovered = playlists.get(target.row)?;
    if hovered.is_smart || playlists.get(dragged)?.is_smart {
        return None;
    }
    let span = dragged..after_span(playlists, dragged);
    if span.contains(&target.row) {
        return None;
    }
    let open = hovered.has_children && expanded(target.row);
    let place = match target.zone {
        Zone::Before => Place {
            parent: parent_of(playlists, target.row),
            before: Some(target.row),
        },
        Zone::After | Zone::Inside if open => Place {
            parent: Some(target.row),
            before: first_child(playlists, target.row),
        },
        Zone::After => Place {
            parent: parent_of(playlists, target.row),
            before: next_sibling(playlists, target.row),
        },
        Zone::Inside => Place {
            parent: Some(target.row),
            before: None,
        },
    };
    // In front of itself is where it is.
    if place.before == Some(dragged) {
        return None;
    }
    let stays = place.parent == parent_of(playlists, dragged)
        && place.before == next_sibling(playlists, dragged);
    (!stays).then_some(place)
}

/// Which edge of its row the line is on, and how many levels it is indented.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Line {
    pub bottom: bool,
    pub level: usize,
}

/// The line to draw on `target`'s row for the playlist being dragged, if it can go there.
pub fn line(
    playlists: &[Playlist],
    expanded: impl Fn(usize) -> bool,
    dragged: usize,
    target: Target,
) -> Option<Line> {
    place(playlists, &expanded, dragged, target)?;
    let depth = playlists[target.row].depth;
    let open = playlists[target.row].has_children && expanded(target.row);
    Some(match target.zone {
        Zone::Before => Line {
            bottom: false,
            level: depth,
        },
        Zone::After if !open => Line {
            bottom: true,
            level: depth,
        },
        // In the row: its playlists are one level in.
        Zone::After | Zone::Inside => Line {
            bottom: true,
            level: depth + 1,
        },
    })
}

/// What a row of the tree is told to be for a drag.
pub struct Row<Message> {
    /// The button going down on it starts a drag.
    pub press: Option<Message>,
    /// The pointer is over this zone of it (`None`: it left): sent while a drag is going on,
    /// by rows that a playlist can be dropped on.
    pub over: Option<Box<dyn Fn(Option<Zone>) -> Message>>,
    /// A drag is going on.
    pub dragging: bool,
    /// The line to draw, and how far in from the row's left edge it starts.
    pub line: Option<(Line, f32)>,
}

/// What the wrapper remembers between events.
#[derive(Default)]
struct State {
    zone: Option<Zone>,
}

pub struct DragRow<'a, Message> {
    content: Element<'a, Message>,
    row: Row<Message>,
}

/// Wraps the row `content` of the tree.
pub fn drag_row<'a, Message>(
    content: impl Into<Element<'a, Message>>,
    row: Row<Message>,
) -> DragRow<'a, Message> {
    DragRow {
        content: content.into(),
        row,
    }
}

impl<Message: Clone> Widget<Message, Theme, Renderer> for DragRow<'_, Message> {
    fn tag(&self) -> tree::Tag {
        tree::Tag::of::<State>()
    }

    fn state(&self) -> tree::State {
        tree::State::new(State::default())
    }

    fn children(&self) -> Vec<Tree> {
        vec![Tree::new(&self.content)]
    }

    fn diff(&self, tree: &mut Tree) {
        tree.diff_children(std::slice::from_ref(&self.content));
    }

    fn size(&self) -> Size<Length> {
        self.content.as_widget().size()
    }

    fn layout(
        &mut self,
        tree: &mut Tree,
        renderer: &Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        self.content
            .as_widget_mut()
            .layout(&mut tree.children[0], renderer, limits)
    }

    fn operate(
        &mut self,
        tree: &mut Tree,
        layout: Layout<'_>,
        renderer: &Renderer,
        operation: &mut dyn Operation,
    ) {
        self.content
            .as_widget_mut()
            .operate(&mut tree.children[0], layout, renderer, operation);
    }

    fn update(
        &mut self,
        tree: &mut Tree,
        event: &Event,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        renderer: &Renderer,
        clipboard: &mut dyn Clipboard,
        shell: &mut Shell<'_, Message>,
        viewport: &Rectangle,
    ) {
        let bounds = layout.bounds();
        // The button going down starts a drag candidate, and still reaches what is inside
        // (a click on the title shows the playlist).
        if let Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)) = event
            && cursor.is_over(bounds)
            && let Some(press) = &self.row.press
        {
            shell.publish(press.clone());
        }

        self.content.as_widget_mut().update(
            &mut tree.children[0],
            event,
            layout,
            cursor,
            renderer,
            clipboard,
            shell,
            viewport,
        );

        let state = tree.state.downcast_mut::<State>();
        let (true, Some(over)) = (self.row.dragging, &self.row.over) else {
            state.zone = None;
            return;
        };
        if let Event::Mouse(mouse::Event::CursorMoved { .. }) = event {
            let zone = cursor
                .position_in(bounds)
                .map(|at| Zone::at(at.y, bounds.height));
            if state.zone != zone {
                state.zone = zone;
                shell.publish(over(zone));
            }
        }
    }

    fn mouse_interaction(
        &self,
        tree: &Tree,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
        renderer: &Renderer,
    ) -> mouse::Interaction {
        if self.row.dragging {
            return mouse::Interaction::Grabbing;
        }
        self.content.as_widget().mouse_interaction(
            &tree.children[0],
            layout,
            cursor,
            viewport,
            renderer,
        )
    }

    fn draw(
        &self,
        tree: &Tree,
        renderer: &mut Renderer,
        theme: &Theme,
        style: &renderer::Style,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
    ) {
        self.content.as_widget().draw(
            &tree.children[0],
            renderer,
            theme,
            style,
            layout,
            cursor,
            viewport,
        );
        let Some((line, indent)) = self.row.line else {
            return;
        };
        let bounds = layout.bounds();
        let y = if line.bottom {
            bounds.y + bounds.height
        } else {
            bounds.y
        };
        let x = bounds.x + indent;
        let color = theme.extended_palette().primary.strong.color;
        let bar = Rectangle {
            x,
            y: y - LINE / 2.0,
            width: (bounds.x + bounds.width - x).max(0.0),
            height: LINE,
        };
        let dot = Rectangle {
            x: x - DOT / 2.0,
            y: y - DOT / 2.0,
            width: DOT,
            height: DOT,
        };
        // On a layer of its own, so that the rows drawn after this one do not paint over
        // it. It lies on the edge between two rows and takes no room in the layout.
        renderer.with_layer(*viewport, |renderer| {
            renderer.fill_quad(
                renderer::Quad {
                    bounds: bar,
                    border: Border {
                        radius: border::radius(LINE / 2.0),
                        ..Border::default()
                    },
                    ..renderer::Quad::default()
                },
                color,
            );
            renderer.fill_quad(
                renderer::Quad {
                    bounds: dot,
                    border: Border {
                        radius: border::radius(DOT / 2.0),
                        ..Border::default()
                    },
                    ..renderer::Quad::default()
                },
                color,
            );
        });
    }

    fn overlay<'b>(
        &'b mut self,
        tree: &'b mut Tree,
        layout: Layout<'b>,
        renderer: &Renderer,
        viewport: &Rectangle,
        translation: Vector,
    ) -> Option<overlay::Element<'b, Message, Theme, Renderer>> {
        self.content.as_widget_mut().overlay(
            &mut tree.children[0],
            layout,
            renderer,
            viewport,
            translation,
        )
    }
}

impl<'a, Message: Clone + 'a> From<DragRow<'a, Message>> for Element<'a, Message> {
    fn from(widget: DragRow<'a, Message>) -> Self {
        Element::new(widget)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(depth: usize, has_children: bool, is_smart: bool) -> Playlist {
        Playlist {
            title: String::new(),
            depth,
            has_children,
            is_smart,
            rules: None,
            tracks: Vec::new(),
        }
    }

    /// A (open: B, C(D)), E, a smart playlist, F:
    /// 0 A, 1 B, 2 C, 3 D, 4 E, 5 smart, 6 F.
    fn tree() -> Vec<Playlist> {
        vec![
            row(0, true, false),
            row(1, false, false),
            row(1, true, false),
            row(2, false, false),
            row(0, false, false),
            row(0, false, true),
            row(0, false, false),
        ]
    }

    fn at(row: usize, zone: Zone) -> Target {
        Target { row, zone }
    }

    #[test]
    fn a_drop_before_or_after_a_row_is_at_its_level() {
        let tree = tree();
        let none = |_: usize| false;
        // E before F: in front of the next playlist, past the smart one.
        assert_eq!(
            place(&tree, none, 6, at(4, Zone::Before)),
            Some(Place {
                parent: None,
                before: Some(4)
            })
        );
        assert_eq!(
            place(&tree, none, 0, at(4, Zone::After)),
            Some(Place {
                parent: None,
                before: Some(6)
            })
        );
        // Last of a level: no playlist to be in front of.
        assert_eq!(
            place(&tree, none, 4, at(6, Zone::After)),
            Some(Place {
                parent: None,
                before: None
            })
        );
        // D after C (nested): C's level, in A.
        assert_eq!(
            place(&tree, |_| true, 4, at(3, Zone::After)),
            Some(Place {
                parent: Some(2),
                before: None
            })
        );
    }

    #[test]
    fn a_drop_in_the_middle_goes_in_the_row_and_below_an_open_one_it_is_the_first() {
        let tree = tree();
        // Into E (a leaf): its last playlist.
        assert_eq!(
            place(&tree, |_| false, 6, at(4, Zone::Inside)),
            Some(Place {
                parent: Some(4),
                before: None
            })
        );
        // Into A, which is closed: its last. Open: its first.
        assert_eq!(
            place(&tree, |_| false, 6, at(0, Zone::Inside)),
            Some(Place {
                parent: Some(0),
                before: None
            })
        );
        let open = |row: usize| row == 0;
        assert_eq!(
            place(&tree, open, 6, at(0, Zone::Inside)),
            Some(Place {
                parent: Some(0),
                before: Some(1)
            })
        );
        assert_eq!(
            place(&tree, open, 6, at(0, Zone::After)),
            Some(Place {
                parent: Some(0),
                before: Some(1)
            })
        );
    }

    #[test]
    fn a_playlist_does_not_go_into_itself_or_stay_where_it_is_or_on_a_smart_one() {
        let tree = tree();
        // A with everything in it can't be dropped on any of it.
        for row in 0..4 {
            assert_eq!(place(&tree, |_| true, 0, at(row, Zone::Inside)), None);
            assert_eq!(place(&tree, |_| true, 0, at(row, Zone::Before)), None);
        }
        // E is where it is: before itself, after the row before it, and before the one after.
        assert_eq!(place(&tree, |_| false, 4, at(4, Zone::After)), None);
        assert_eq!(place(&tree, |_| false, 4, at(6, Zone::Before)), None);
        assert_eq!(place(&tree, |_| false, 4, at(0, Zone::After)), None);
        // The smart playlist is not a place, and is not moved.
        assert_eq!(place(&tree, |_| false, 6, at(5, Zone::Before)), None);
        assert_eq!(place(&tree, |_| false, 5, at(0, Zone::Before)), None);
    }

    #[test]
    fn the_line_is_on_the_edge_and_indented_to_the_level_the_playlist_gets() {
        let tree = tree();
        let open = |row: usize| row == 0 || row == 2;
        let line = |dragged, target| line(&tree, open, dragged, target);
        assert_eq!(
            line(6, at(4, Zone::Before)),
            Some(Line {
                bottom: false,
                level: 0
            })
        );
        assert_eq!(
            line(6, at(4, Zone::After)),
            None,
            "after E is in front of F: where F is already"
        );
        // After an open folder, or in the middle of any: one level in.
        assert_eq!(
            line(6, at(0, Zone::After)),
            Some(Line {
                bottom: true,
                level: 1
            })
        );
        assert_eq!(
            line(6, at(4, Zone::Inside)),
            Some(Line {
                bottom: true,
                level: 1
            })
        );
        assert_eq!(
            line(4, at(3, Zone::After)),
            Some(Line {
                bottom: true,
                level: 2
            })
        );
    }

    #[test]
    fn zones_are_the_top_the_middle_and_the_bottom_of_a_row() {
        assert_eq!(Zone::at(1.0, 32.0), Zone::Before);
        assert_eq!(Zone::at(16.0, 32.0), Zone::Inside);
        assert_eq!(Zone::at(31.0, 32.0), Zone::After);
    }

    #[test]
    fn a_drag_starts_after_the_pointer_moved_and_opens_a_folder_it_stays_over() {
        let mut drag = Drag::pressed(4);
        drag.moved(Point::new(10.0, 10.0));
        assert!(!drag.active, "the first move is where it started");
        drag.moved(Point::new(12.0, 11.0));
        assert!(!drag.active);
        drag.moved(Point::new(30.0, 11.0));
        assert!(drag.active);

        let tree = tree();
        drag.hover(0, Some(Zone::Inside));
        let soon = drag.over_since + Duration::from_millis(100);
        let later = drag.over_since + OPEN_AFTER;
        assert_eq!(drag.to_open(&tree, |_| false, soon), None);
        assert_eq!(drag.to_open(&tree, |_| false, later), Some(0));
        assert_eq!(drag.to_open(&tree, |_| true, later), None, "already open");
        // A leaf is not opened, and leaving the row ends the wait.
        drag.hover(4, Some(Zone::Inside));
        assert_eq!(
            drag.to_open(&tree, |_| false, drag.over_since + OPEN_AFTER),
            None
        );
        drag.hover(4, None);
        assert_eq!(drag.over, None);
        drag.hover(0, None);
        assert_eq!(drag.over, None, "another row's leaving changes nothing");
    }
}
