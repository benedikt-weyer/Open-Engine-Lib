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
    /// Reads the library from the disk again, replacing the working copy.
    Reload,
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

/// The items of the File menu: Reload and Save as only when there is a library, and nothing
/// that changes the library while one is being saved or reloaded.
pub fn items(library_is_open: bool, busy: bool) -> Vec<MenuItem> {
    let mut items = Vec::new();
    if !busy {
        items.push(MenuItem::Open);
        if library_is_open {
            items.push(MenuItem::Reload);
            items.push(MenuItem::SaveAs);
        }
    }
    items.push(MenuItem::Exit);
    items
}

/// What the bar says besides the menu.
pub struct Status<'a> {
    /// Where the library is being saved to.
    pub saving: Option<&'a Path>,
    /// Why the last save failed.
    pub error: Option<&'a str>,
    /// Whether the library is being read from the disk again.
    pub reloading: bool,
    /// How the last reload went: what to say, and whether it failed.
    pub reload: Option<(&'a str, bool)>,
}

pub fn view<'a>(
    language: Language,
    library_is_open: bool,
    status: Status<'a>,
) -> Element<'a, Message> {
    let strings = language.strings();
    let Status {
        saving,
        error,
        reloading,
        reload,
    } = status;
    let entries: Vec<Entry> = items(library_is_open, saving.is_some() || reloading)
        .into_iter()
        .map(|item| Entry {
            item,
            label: match item {
                MenuItem::Open => &strings.menu_open,
                MenuItem::Reload => &strings.menu_reload,
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
    if reloading {
        bar = bar.push(text(&strings.reloading_library).size(14));
    } else if let Some((message, failed)) = reload {
        let message = text(message).size(14);
        bar = bar.push(if failed {
            message.style(text::danger)
        } else {
            message
        });
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
            [
                MenuItem::Open,
                MenuItem::Reload,
                MenuItem::SaveAs,
                MenuItem::Exit
            ]
        );
    }

    #[test]
    fn while_busy_only_exit_is_left() {
        assert_eq!(items(true, true), [MenuItem::Exit]);
        assert_eq!(items(false, true), [MenuItem::Exit]);
    }
}
