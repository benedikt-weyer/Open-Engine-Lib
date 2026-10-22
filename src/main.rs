#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use iced::widget::{button, column, text};
use iced::{Center, Element};

fn main() -> iced::Result {
    iced::run(update, view)
}

#[derive(Default)]
struct State {
    value: i64,
}

#[derive(Debug, Clone, Copy)]
enum Message {
    Increment,
    Decrement,
}

fn update(state: &mut State, message: Message) {
    match message {
        Message::Increment => state.value += 1,
        Message::Decrement => state.value -= 1,
    }
}

fn view(state: &State) -> Element<'_, Message> {
    column![
        button("Increment").on_press(Message::Increment),
        text(state.value).size(50),
        button("Decrement").on_press(Message::Decrement),
    ]
    .padding(20)
    .spacing(10)
    .align_x(Center)
    .into()
}
