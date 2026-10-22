use iced::widget::{Space, button, container, row, text};
use iced::{Element, Fill};

use crate::{Message, Page};

const ITEMS: [(Page, &str); 2] = [(Page::Home, "Home"), (Page::Settings, "Settings")];

pub fn view<'a>(current: Page) -> Element<'a, Message> {
    let mut bar = row![text("Open Engine Lib").size(20), Space::new().width(Fill)]
        .spacing(10)
        .align_y(iced::Center);

    for (page, label) in ITEMS {
        let style = if page == current {
            button::primary
        } else {
            button::text
        };
        bar = bar.push(button(label).style(style).on_press(Message::Navigate(page)));
    }

    container(bar)
        .padding([10, 20])
        .width(Fill)
        .style(container::bordered_box)
        .into()
}
