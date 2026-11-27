//! The tabs of the open libraries, along the top of the library page like the tabs of an
//! editor: click one to show that library, the cross closes it, and a star says it has
//! changes that were not saved to the library yet.

use iced::widget::scrollable::{Direction, Scrollbar};
use iced::widget::{button, container, row, scrollable, text, tooltip};
use iced::{Border, Center, Element, Theme};

use crate::Message;
use crate::config::Language;

/// The longest a tab's title is drawn, in characters.
const TITLE_MAX: usize = 28;

/// One open library as a tab.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tab {
    /// What the backend calls the library.
    pub id: u64,
    pub title: String,
    /// Where the library is, shown when the tab is pointed at.
    pub path: String,
    /// The working copy has edits that were not written to the source.
    pub unsaved: bool,
    /// The library that is shown.
    pub active: bool,
}

/// The tabs, in the order the libraries were opened, and the library that is still being
/// opened (its title), if one is, as a last tab that can't be used yet. `enabled` is false
/// while something is going on that the libraries must not be changed during.
pub fn view<'a>(
    tabs: Vec<Tab>,
    opening: Option<String>,
    enabled: bool,
    language: Language,
) -> Element<'a, Message> {
    let strings = language.strings();
    let mut bar = row![].spacing(2).align_y(Center);
    for tab in tabs {
        bar = bar.push(tab_view(tab, enabled, language));
    }
    if let Some(title) = opening {
        bar = bar.push(
            container(
                text(format!("{} {}", shorten(&title), strings.tab_opening))
                    .size(14)
                    .style(text::secondary),
            )
            .padding([6, 12])
            .style(container::bordered_box),
        );
    }
    scrollable(bar)
        .direction(Direction::Horizontal(
            Scrollbar::new().width(4).scroller_width(4),
        ))
        .into()
}

fn tab_view<'a>(tab: Tab, enabled: bool, language: Language) -> Element<'a, Message> {
    let strings = language.strings();
    let mut label = row![text(shorten(&tab.title)).size(14)]
        .spacing(6)
        .align_y(Center);
    if tab.unsaved {
        label = label.push(
            tooltip(
                text("★").size(13).style(text::warning),
                container(text(strings.tab_unsaved.as_str()).size(13))
                    .max_width(320)
                    .padding([6, 10])
                    .style(container::rounded_box),
                tooltip::Position::Bottom,
            )
            .style(container::rounded_box),
        );
    }
    let select = tooltip(
        button(label)
            .padding([6, 10])
            .style(if tab.active {
                button::primary
            } else {
                button::text
            })
            .on_press_maybe((enabled && !tab.active).then_some(Message::TabSelect(tab.id))),
        container(text(tab.path.clone()).size(13))
            .padding([4, 8])
            .style(container::rounded_box),
        tooltip::Position::Bottom,
    );
    let close = tooltip(
        button(text("✕").size(12))
            .padding([6, 8])
            .style(if tab.active {
                button::primary
            } else {
                button::text
            })
            .on_press_maybe(enabled.then_some(Message::TabClose(tab.id))),
        container(text(strings.tab_close.as_str()).size(13))
            .padding([4, 8])
            .style(container::rounded_box),
        tooltip::Position::Bottom,
    );
    container(row![select, close].align_y(Center))
        .style(move |theme: &Theme| {
            let palette = theme.extended_palette();
            container::Style {
                border: Border {
                    color: if tab.active {
                        palette.primary.base.color
                    } else {
                        palette.background.strong.color
                    },
                    width: 1.0,
                    radius: 4.0.into(),
                },
                ..container::Style::default()
            }
        })
        .into()
}

/// A title that is too long, cut with an ellipsis.
fn shorten(title: &str) -> String {
    if title.chars().count() <= TITLE_MAX {
        return title.to_owned();
    }
    let kept: String = title.chars().take(TITLE_MAX - 1).collect();
    format!("{kept}…")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_long_title_is_cut_and_a_short_one_is_not() {
        assert_eq!(shorten("DRIVE"), "DRIVE");
        let long = "x".repeat(60);
        let cut = shorten(&long);
        assert_eq!(cut.chars().count(), TITLE_MAX);
        assert!(cut.ends_with('…'));
    }
}
