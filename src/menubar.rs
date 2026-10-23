//! The menu bar at the top of the window.

use std::fmt;
use std::path::Path;

use iced::widget::{pick_list, row, text};
use iced::{Background, Border, Color, Element, Fill, Theme};

use crate::Message;
use crate::config::Language;
use crate::i18n::fill;

/// What the File menu can do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MenuItem {
    Open,
    SaveAs,
    Exit,
}

/// An entry of the File menu: what it does and its name in the UI language.
#[derive(Clone, Copy, PartialEq, Eq)]
struct Entry {
    item: MenuItem,
    label: &'static str,
}

impl fmt::Display for Entry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label)
    }
}

/// The items of the File menu: Save as only when there is a library to save, and nothing
/// that changes the library while one is being saved.
pub fn items(library_is_open: bool, saving: bool) -> Vec<MenuItem> {
    let mut items = Vec::new();
    if !saving {
        items.push(MenuItem::Open);
        if library_is_open {
            items.push(MenuItem::SaveAs);
        }
    }
    items.push(MenuItem::Exit);
    items
}

/// `saving` is where the library is being saved to, `error` why the last save failed.
pub fn view<'a>(
    language: Language,
    library_is_open: bool,
    saving: Option<&'a Path>,
    error: Option<&'a str>,
) -> Element<'a, Message> {
    let strings = language.strings();
    let entries: Vec<Entry> = items(library_is_open, saving.is_some())
        .into_iter()
        .map(|item| Entry {
            item,
            label: match item {
                MenuItem::Open => &strings.menu_open,
                MenuItem::SaveAs => &strings.menu_save_as,
                MenuItem::Exit => &strings.menu_exit,
            },
        })
        .collect();

    // A drop-down list that always shows "File" and acts on what is picked.
    let file_menu = pick_list(entries, None::<Entry>, |entry: Entry| {
        Message::MenuSelected(entry.item)
    })
    .placeholder(&strings.menu_file)
    .handle(pick_list::Handle::None)
    .text_size(14)
    .padding([2, 10])
    .width(110)
    .style(|theme: &Theme, status| {
        let palette = theme.extended_palette();
        // Flat like a menu bar, lit up under the pointer or while open.
        let lit = matches!(
            status,
            pick_list::Status::Hovered | pick_list::Status::Opened { .. }
        );
        pick_list::Style {
            text_color: palette.background.base.text,
            placeholder_color: palette.background.base.text,
            handle_color: palette.background.base.text,
            background: Background::Color(if lit {
                palette.background.weak.color
            } else {
                Color::TRANSPARENT
            }),
            border: Border::default().rounded(3),
        }
    });

    let mut bar = row![file_menu].spacing(12).align_y(iced::Center);
    if let Some(path) = saving {
        bar = bar.push(text(fill(&strings.saving_library, &[("path", &path.display())])).size(14));
    }
    if let Some(error) = error {
        bar = bar.push(
            text(fill(&strings.save_library_failed, &[("error", &error)]))
                .size(14)
                .style(text::danger),
        );
    }
    iced::widget::container(bar.width(Fill))
        .padding([2, 12])
        .into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn save_as_needs_a_library_to_save() {
        assert_eq!(items(false, false), [MenuItem::Open, MenuItem::Exit]);
        assert_eq!(
            items(true, false),
            [MenuItem::Open, MenuItem::SaveAs, MenuItem::Exit]
        );
    }

    #[test]
    fn while_saving_only_exit_is_left() {
        assert_eq!(items(true, true), [MenuItem::Exit]);
        assert_eq!(items(false, true), [MenuItem::Exit]);
    }
}
