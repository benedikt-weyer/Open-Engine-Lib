//! The window for adding, renaming or deleting an ordinary playlist.

use iced::widget::{
    Space, button, center, column, container, opaque, pick_list, row, text, text_input,
};
use iced::{Color, Element, Fill, Theme};

use crate::Message;
use crate::config::Language;
use crate::error_text;
use crate::i18n::fill;
use crate::model::Library;
use crate::smart_editor::{Choice, choice};

const WIDTH: f32 = 520.0;

/// What the window is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// A new playlist in this playlist (an index into the library's playlists), or at
    /// the top level.
    Create { parent: Option<usize> },
    /// Another title for this playlist.
    Rename(usize),
    /// Delete this playlist.
    Delete(usize),
}

/// What the user changed in the window.
#[derive(Debug, Clone)]
pub enum Event {
    Name(String),
    Parent(Option<usize>),
}

#[derive(Debug, Clone)]
pub struct Dialog {
    pub kind: Kind,
    pub name: String,
    /// Why the last attempt failed (or what is missing).
    pub error: Option<String>,
    /// Waiting for the backend to write it.
    pub busy: bool,
}

impl Dialog {
    pub fn create() -> Self {
        Self {
            kind: Kind::Create { parent: None },
            name: String::new(),
            error: None,
            busy: false,
        }
    }

    pub fn rename(index: usize, title: &str) -> Self {
        Self {
            kind: Kind::Rename(index),
            name: title.to_string(),
            error: None,
            busy: false,
        }
    }

    pub fn delete(index: usize) -> Self {
        Self {
            kind: Kind::Delete(index),
            name: String::new(),
            error: None,
            busy: false,
        }
    }

    pub fn update(&mut self, event: Event) {
        self.error = None;
        match (event, &mut self.kind) {
            (Event::Name(name), _) => self.name = name,
            (Event::Parent(new), Kind::Create { parent }) => *parent = new,
            _ => {}
        }
    }

    pub fn needs_name(&self) -> bool {
        !matches!(self.kind, Kind::Delete(_))
    }
}

/// The window, centred over a dimmed view of the app that can't be clicked.
pub fn modal<'a>(
    dialog: &'a Dialog,
    library: &Library,
    language: Language,
) -> Element<'a, Message> {
    let strings = language.strings();

    let heading = match dialog.kind {
        Kind::Create { .. } => &strings.playlist_title_new,
        Kind::Rename(_) => &strings.playlist_title_rename,
        Kind::Delete(_) => &strings.playlist_title_delete,
    };

    let mut body = column![].spacing(14);
    if dialog.needs_name() {
        body = body.push(
            text_input(&strings.smart_name, &dialog.name)
                .on_input(|name| Message::PlaylistDialog(Event::Name(name)))
                .on_submit(Message::PlaylistConfirm),
        );
    }
    match dialog.kind {
        Kind::Create { parent } => {
            let mut places = vec![choice(None, strings.playlist_top_level.clone())];
            places.extend(
                library
                    .playlists
                    .iter()
                    .enumerate()
                    .filter(|(_, playlist)| !playlist.is_smart)
                    .map(|(index, _)| {
                        choice(Some(index), library.playlist_path(index).join(" / "))
                    }),
            );
            let selected = places.iter().find(|place| place.value == parent).cloned();
            body = body.push(
                row![
                    text(&strings.playlist_place),
                    pick_list(places, selected, |picked: Choice<Option<usize>>| {
                        Message::PlaylistDialog(Event::Parent(picked.value))
                    })
                    .width(Fill),
                ]
                .spacing(10)
                .align_y(iced::Center),
            );
        }
        Kind::Rename(_) => {}
        Kind::Delete(index) => {
            let span = library.playlist_span(index);
            body = body.push(text(fill(
                &strings.playlist_delete_question,
                &[("name", &library.playlists[index].title)],
            )));
            if span.len() > 1 {
                body = body.push(text(fill(
                    &strings.playlist_delete_inside,
                    &[("count", &(span.len() - 1))],
                )));
            }
            body = body.push(
                text(&strings.playlist_delete_note)
                    .size(13)
                    .style(text::secondary),
            );
        }
    }
    if let Some(error) = &dialog.error {
        body = body.push(error_text::view(error.as_str(), language));
    }

    let confirm_label = match dialog.kind {
        _ if dialog.busy => &strings.smart_saving,
        Kind::Create { .. } => &strings.playlist_create,
        Kind::Rename(_) => &strings.playlist_rename_confirm,
        Kind::Delete(_) => &strings.playlist_delete_confirm,
    };
    let confirm = button(text(confirm_label))
        .style(if matches!(dialog.kind, Kind::Delete(_)) {
            button::danger
        } else {
            button::primary
        })
        .on_press_maybe((!dialog.busy).then_some(Message::PlaylistConfirm));
    let cancel = button(text(&strings.smart_cancel))
        .style(button::secondary)
        .on_press_maybe((!dialog.busy).then_some(Message::PlaylistCancel));
    body = body.push(row![Space::new().width(Fill), cancel, confirm].spacing(10));

    let window = container(column![text(heading).size(24), body].spacing(14))
        .padding(20)
        .width(WIDTH)
        .style(container::rounded_box);

    // The dimmed backdrop swallows clicks, so nothing behind the window can be used.
    opaque(center(opaque(window)).style(|_: &Theme| {
        container::Style {
            background: Some(
                Color {
                    a: 0.6,
                    ..Color::BLACK
                }
                .into(),
            ),
            ..container::Style::default()
        }
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_parent_is_only_chosen_for_a_new_playlist() {
        let mut dialog = Dialog::create();
        dialog.update(Event::Parent(Some(3)));
        assert_eq!(dialog.kind, Kind::Create { parent: Some(3) });
        dialog.update(Event::Name("Mix".into()));
        assert_eq!(dialog.name, "Mix");

        let mut dialog = Dialog::rename(2, "Old");
        assert_eq!(dialog.name, "Old");
        dialog.update(Event::Parent(Some(3)));
        assert_eq!(dialog.kind, Kind::Rename(2));
        assert!(!Dialog::delete(1).needs_name());
    }
}
