//! A toast: a small card in the corner of the window that tells how a task in the background
//! is going, and then how it went. It lies over everything, doesn't block anything, and a
//! finished one goes away by itself (a failed one stays until it is closed).

use std::time::{Duration, Instant};

use iced::widget::{button, column, container, progress_bar, row, text};
use iced::{Element, Fill, Theme};
use open_engine_api::v1::{OperationProgress, ProgressPhase};

use crate::Message;
use crate::config::Language;
use crate::i18n::fill;
use crate::progress_view;

/// How long a finished toast stays.
pub const SHOWN_FOR: Duration = Duration::from_secs(8);
/// How often a finished toast looks whether its time is up.
pub const CHECK_EVERY: Duration = Duration::from_secs(1);
/// How wide the card is.
const WIDTH: f32 = 380.0;

#[derive(Debug, Clone)]
pub enum Toast {
    /// A track is being analysed (the backend says how far, see `progress_view`).
    Analysing {
        /// The library it is in, to tell the answer from one for another library.
        library: String,
        title: String,
    },
    /// It was done; says what came out.
    Done { message: String, since: Instant },
    /// It failed; says why.
    Failed { message: String },
}

impl Toast {
    /// Whether the toast is for a task that is still running.
    pub fn running(&self) -> bool {
        matches!(self, Toast::Analysing { .. })
    }

    /// Whether a finished toast has been shown long enough.
    pub fn expired(&self, now: Instant) -> bool {
        matches!(self, Toast::Done { since, .. } if now.duration_since(*since) >= SHOWN_FOR)
    }
}

/// The name of what the analysis is doing.
fn stage(progress: Option<&OperationProgress>, language: Language) -> &str {
    let strings = language.strings();
    match progress.map(OperationProgress::phase) {
        Some(ProgressPhase::AnalysingBeats) => &strings.toast_stage_beats,
        Some(ProgressPhase::AnalysingKey) => &strings.toast_stage_key,
        _ => &strings.toast_stage_decoding,
    }
}

/// The toast in the bottom right corner of the window.
pub fn layer<'a>(
    toast: &'a Toast,
    progress: Option<&'a OperationProgress>,
    language: Language,
) -> Element<'a, Message> {
    let strings = language.strings();
    let close = || {
        button(text("×").size(16))
            .padding([0, 6])
            .style(button::text)
            .on_press(Message::ToastClose)
    };
    let content: Element<'a, Message> = match toast {
        Toast::Analysing { title, .. } => {
            // (Before the backend has said anything the bar is empty.)
            let fraction = progress
                .filter(|progress| progress.running)
                .and_then(progress_view::fraction)
                .unwrap_or(0.0);
            column![
                text(fill(&strings.toast_analysing, &[("title", title)]))
                    .size(14)
                    .wrapping(text::Wrapping::None),
                progress_bar(0.0..=1.0, fraction).girth(6),
                text(stage(progress, language))
                    .size(13)
                    .style(text::secondary),
            ]
            .spacing(8)
            .into()
        }
        Toast::Done { message, .. } => row![text(message).size(14).width(Fill), close()]
            .spacing(8)
            .align_y(iced::Center)
            .into(),
        Toast::Failed { message } => row![
            column![crate::error_text::view(message.clone(), language)].width(Fill),
            close()
        ]
        .spacing(8)
        .align_y(iced::alignment::Vertical::Top)
        .into(),
    };
    container(
        container(content)
            .padding(12)
            .width(WIDTH)
            .style(|theme: &Theme| {
                let palette = theme.extended_palette();
                container::Style {
                    background: Some(palette.background.weak.color.into()),
                    border: iced::Border {
                        color: palette.background.strong.color,
                        width: 1.0,
                        radius: 6.0.into(),
                    },
                    ..container::Style::default()
                }
            }),
    )
    .width(Fill)
    .height(Fill)
    .align_x(iced::alignment::Horizontal::Right)
    .align_y(iced::alignment::Vertical::Bottom)
    .padding(20)
    .into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_finished_toast_goes_after_a_while_and_a_running_or_failed_one_does_not() {
        let now = Instant::now();
        let done = Toast::Done {
            message: "ok".into(),
            since: now,
        };
        assert!(!done.expired(now));
        assert!(!done.expired(now + SHOWN_FOR - Duration::from_millis(1)));
        assert!(done.expired(now + SHOWN_FOR));

        let later = now + SHOWN_FOR * 10;
        assert!(
            !Toast::Failed {
                message: "no".into()
            }
            .expired(later)
        );
        let running = Toast::Analysing {
            library: "/lib".into(),
            title: "Song".into(),
        };
        assert!(running.running() && !running.expired(later));
        assert!(!done.running());
    }
}
