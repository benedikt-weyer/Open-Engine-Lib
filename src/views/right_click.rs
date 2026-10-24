//! A wrapper that tells where on the window the right mouse button went down on what it
//! wraps, which `mouse_area` does not (it only says that it did), so that a context menu can
//! open at the pointer. Everything else is left to the wrapped widget.

use iced::advanced::widget::{Operation, Tree, Widget, tree};
use iced::advanced::{Clipboard, Layout, Shell, layout, mouse, overlay, renderer};
use iced::{Element, Event, Length, Point, Rectangle, Renderer, Size, Theme, Vector};

/// How far what the wrapper is in has been moved, by the scrollables around it, from where
/// it is on the window. Inside a scrollable the pointer is where it is in the content, which
/// is the position on the window plus the distance scrolled, so it has to be taken off
/// again. A widget is told this only when it is asked for its overlay, which happens before
/// every batch of events.
#[derive(Default)]
struct State {
    translation: Vector,
}

pub struct RightClick<'a, Message> {
    content: Element<'a, Message>,
    on_right_click: Box<dyn Fn(Point) -> Message + 'a>,
}

/// Wraps `content`; `on_right_click` makes the message for a right click, from where the
/// pointer is in the window.
pub fn right_click<'a, Message>(
    content: impl Into<Element<'a, Message>>,
    on_right_click: impl Fn(Point) -> Message + 'a,
) -> RightClick<'a, Message> {
    RightClick {
        content: content.into(),
        on_right_click: Box::new(on_right_click),
    }
}

impl<Message> Widget<Message, Theme, Renderer> for RightClick<'_, Message> {
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
        // What is inside (a button...) may have used the click.
        if shell.is_event_captured() {
            return;
        }
        if let Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Right)) = event
            && cursor.is_over(layout.bounds())
            && let Some(position) = cursor.position()
        {
            let translation = tree.state.downcast_ref::<State>().translation;
            shell.publish((self.on_right_click)(position + translation));
            shell.capture_event();
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
    }

    fn overlay<'b>(
        &'b mut self,
        tree: &'b mut Tree,
        layout: Layout<'b>,
        renderer: &Renderer,
        viewport: &Rectangle,
        translation: Vector,
    ) -> Option<overlay::Element<'b, Message, Theme, Renderer>> {
        tree.state.downcast_mut::<State>().translation = translation;
        self.content.as_widget_mut().overlay(
            &mut tree.children[0],
            layout,
            renderer,
            viewport,
            translation,
        )
    }
}

impl<'a, Message: 'a> From<RightClick<'a, Message>> for Element<'a, Message> {
    fn from(widget: RightClick<'a, Message>) -> Self {
        Element::new(widget)
    }
}
