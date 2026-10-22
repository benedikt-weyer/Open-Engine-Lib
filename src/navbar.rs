use iced::widget::{Space, button, container, row, text};
use iced::{Element, Fill};

use crate::config::Language;
use crate::{Message, Page};

pub fn view<'a>(current: Page, language: Language) -> Element<'a, Message> {
    let strings = language.strings();
    let items = [
        (Page::Home, &strings.nav_home),
        (Page::Settings, &strings.nav_settings),
    ];

    let mut bar = row![text("Open Engine Lib").size(20), Space::new().width(Fill)]
        .spacing(10)
        .align_y(iced::Center);

    for (page, label) in items {
        let style = if page == current {
            button::primary
        } else {
            button::text
        };
        bar = bar.push(
            button(label.as_str())
                .style(style)
                .on_press(Message::Navigate(page)),
        );
    }

    container(bar)
        .padding([10, 20])
        .width(Fill)
        .style(container::bordered_box)
        .into()
}
