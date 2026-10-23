//! The window for changing the folder the saved origins start with: it shows the folder
//! they all have in common and the folder to put in its place.

use iced::widget::{Space, button, center, column, container, opaque, row, text, text_input};
use iced::{Color, Element, Fill, Font, Theme};

use crate::Message;
use crate::config::Language;
use crate::error_text;
use crate::i18n::fill;

const WIDTH: f32 = 560.0;

/// What the user changed in the window.
#[derive(Debug, Clone)]
pub enum Event {
    New(String),
}

#[derive(Debug, Clone)]
pub struct Dialog {
    /// The folder all origins are in and how many origins there are, once the backend has
    /// said (empty if there is none in common).
    pub found: Option<(String, u32)>,
    /// The folder that is to take its place.
    pub new: String,
    /// Why the last attempt failed.
    pub error: Option<String>,
    /// Waiting for the backend to change the origins.
    pub busy: bool,
}

impl Dialog {
    pub fn new() -> Self {
        Self {
            found: None,
            new: String::new(),
            error: None,
            busy: false,
        }
    }

    /// What the backend found. The folder to put in its place starts as the one that is
    /// there, to be edited.
    pub fn loaded(&mut self, prefix: String, count: u32) {
        if self.new.is_empty() {
            self.new = prefix.clone();
        }
        self.found = Some((prefix, count));
    }

    pub fn update(&mut self, event: Event) {
        self.error = None;
        match event {
            Event::New(new) => self.new = new,
        }
    }

    /// The folder to replace and the one to put there, if that is a change to make.
    pub fn change(&self) -> Option<(&str, &str)> {
        let (prefix, count) = self.found.as_ref()?;
        let new = self.new.trim();
        (*count > 0 && !prefix.is_empty() && !new.is_empty() && new != prefix)
            .then_some((prefix.as_str(), new))
    }
}

/// The window, centred over a dimmed view of the app that can't be clicked.
pub fn modal(dialog: &Dialog, language: Language) -> Element<'_, Message> {
    let strings = language.strings();
    let mono = |content: String| text(content).font(Font::MONOSPACE);

    let mut body = column![].spacing(14);
    match &dialog.found {
        None => body = body.push(text(&strings.origin_prefix_loading)),
        Some((_, 0)) => body = body.push(text(&strings.origin_prefix_none)),
        Some((prefix, _)) if prefix.is_empty() => {
            body = body.push(text(&strings.origin_prefix_nocommon));
        }
        Some((prefix, count)) => {
            body = body.push(
                column![
                    text(fill(&strings.origin_prefix_common, &[("count", count)])),
                    mono(prefix.clone()),
                ]
                .spacing(4),
            );
            body = body.push(
                column![
                    text(&strings.origin_prefix_new),
                    row![
                        text_input(&strings.origin_prefix_placeholder, &dialog.new)
                            .font(Font::MONOSPACE)
                            .on_input(|new| Message::OriginPrefixDialog(Event::New(new)))
                            .on_submit_maybe(
                                dialog
                                    .change()
                                    .is_some()
                                    .then_some(Message::OriginPrefixApply)
                            ),
                        button(text(&strings.origin_prefix_browse))
                            .style(button::secondary)
                            .on_press_maybe((!dialog.busy).then_some(Message::OriginPrefixBrowse)),
                    ]
                    .spacing(10),
                ]
                .spacing(4),
            );
            body = body.push(
                text(&strings.origin_prefix_note)
                    .size(13)
                    .style(text::secondary),
            );
        }
    }
    if let Some(error) = &dialog.error {
        body = body.push(error_text::view(error.as_str(), language));
    }

    let apply = button(text(if dialog.busy {
        &strings.smart_saving
    } else {
        &strings.origin_prefix_apply
    }))
    .style(button::primary)
    .on_press_maybe(
        (!dialog.busy && dialog.change().is_some()).then_some(Message::OriginPrefixApply),
    );
    let cancel = button(text(&strings.smart_cancel))
        .style(button::secondary)
        .on_press_maybe((!dialog.busy).then_some(Message::OriginPrefixCancel));
    body = body.push(row![Space::new().width(Fill), cancel, apply].spacing(10));

    let window = container(column![text(&strings.origin_prefix_title).size(24), body].spacing(14))
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
    fn a_change_needs_origins_and_another_folder() {
        let mut dialog = Dialog::new();
        assert!(dialog.change().is_none(), "nothing is known yet");

        dialog.loaded("/old/music".into(), 3);
        assert_eq!(
            dialog.new, "/old/music",
            "the folder to edit starts as it is"
        );
        assert!(dialog.change().is_none(), "the same folder is no change");

        dialog.update(Event::New("  /new/music  ".into()));
        assert_eq!(dialog.change(), Some(("/old/music", "/new/music")));
        dialog.update(Event::New("   ".into()));
        assert!(dialog.change().is_none());

        // Nothing to change without origins, or without a folder they share.
        let mut none = Dialog::new();
        none.loaded(String::new(), 0);
        none.update(Event::New("/new".into()));
        assert!(none.change().is_none());
        let mut apart = Dialog::new();
        apart.loaded(String::new(), 2);
        apart.update(Event::New("/new".into()));
        assert!(apart.change().is_none());
    }
}
