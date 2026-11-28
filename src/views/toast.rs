//! A toast: a small card in the corner of the window that tells how a task in the background
//! is going, and then how it went. It lies over everything, doesn't block anything, and a
//! finished one goes away by itself (a failed one stays until it is closed).

use std::time::{Duration, Instant};

use iced::widget::{button, column, container, progress_bar, row, scrollable, text};
use iced::{Element, Fill, Theme};
use open_engine_api::v1::{OperationProgress, ProgressPhase};

use crate::Message;
use crate::analysis_queue::{Kind, Queue, Status};
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
    /// A job (an analysis, a stem generation) runs (the backend says how far, see
    /// `progress_view`).
    Job {
        kind: Kind,
        title: String,
        /// Which job of how many it is, when several are queued.
        position: Option<(u32, u32)>,
        /// There are jobs after this one that can still be left out.
        stoppable: bool,
    },
    /// It was done; says what came out.
    Done { message: String, since: Instant },
    /// It failed; says why.
    Failed { message: String },
}

impl Toast {
    /// Whether the toast is for a job that is still running.
    pub fn running(&self) -> bool {
        matches!(self, Toast::Job { .. })
    }

    /// Whether a finished toast has been shown long enough.
    pub fn expired(&self, now: Instant) -> bool {
        matches!(self, Toast::Done { since, .. } if now.duration_since(*since) >= SHOWN_FOR)
    }
}

/// The name of what the job is doing.
fn stage(progress: Option<&OperationProgress>, language: Language) -> &str {
    let strings = language.strings();
    match progress.map(OperationProgress::phase) {
        Some(ProgressPhase::AnalysingBeats) => &strings.toast_stage_beats,
        Some(ProgressPhase::AnalysingKey) => &strings.toast_stage_key,
        Some(ProgressPhase::StemsSeparating) => &strings.toast_stage_separating,
        Some(ProgressPhase::StemsWriting) => &strings.toast_stage_writing_stems,
        _ => &strings.toast_stage_decoding,
    }
}

/// The jobs of the queue, each with where it is; one that waits can be taken out.
fn queue_list<'a>(queue: &'a Queue, language: Language) -> Element<'a, Message> {
    let strings = language.strings();
    let mut list = column![].spacing(2);
    for (index, item) in queue.items().iter().enumerate() {
        let what = match item.entry.kind {
            Kind::Analyse => &strings.queue_kind_analyse,
            Kind::Stems => &strings.queue_kind_stems,
        };
        let (status, style): (&str, fn(&Theme) -> text::Style) = match &item.status {
            Status::Waiting => (&strings.queue_waiting, text::secondary),
            Status::Running => (&strings.queue_running, text::primary),
            Status::Done => (&strings.queue_done, text::success),
            Status::Failed(_) => (&strings.queue_failed, text::danger),
            Status::Stopped => (&strings.queue_stopped, text::secondary),
        };
        let mut line = row![
            text(what).size(12).width(70).style(text::secondary),
            text(&item.entry.title)
                .size(12)
                .width(Fill)
                .wrapping(text::Wrapping::None),
            text(status).size(12).style(style),
        ]
        .spacing(6)
        .align_y(iced::Center);
        if item.status == Status::Waiting {
            line = line.push(
                button(text("×").size(12))
                    .padding([0, 4])
                    .style(button::text)
                    .on_press(Message::JobRemove(index)),
            );
        }
        let line: Element<'a, Message> = match &item.status {
            Status::Failed(reason) => iced::widget::tooltip(
                line,
                text(reason).size(12),
                iced::widget::tooltip::Position::Top,
            )
            .style(container::rounded_box)
            .into(),
            _ => line.into(),
        };
        list = list.push(line);
    }
    scrollable(list).height(iced::Length::Shrink).into()
}

/// The tick in the last step of replacing the working copy (discarding the changes, copying
/// from another library) that stops the jobs queued for the library: off, they go on and
/// write their results to the library as it is then. Nothing while no job is queued.
pub fn stop_option<'a>(
    pending: usize,
    stop: bool,
    language: Language,
) -> Option<Element<'a, Message>> {
    (pending > 0).then(|| {
        iced::widget::checkbox(stop)
            .label(fill(
                &language.strings().jobs_stop_option,
                &[("count", &pending)],
            ))
            .on_toggle(Message::StopJobsToggled)
            .into()
    })
}

/// The toast in the bottom right corner of the window.
pub fn layer<'a>(
    toast: &'a Toast,
    progress: Option<&'a OperationProgress>,
    // The queue, when it is to be shown.
    queue: Option<&'a Queue>,
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
        Toast::Job {
            kind,
            title,
            position,
            stoppable,
            ..
        } => {
            // (Before the backend has said anything the bar is empty.)
            let fraction = progress
                .filter(|progress| progress.running)
                .and_then(progress_view::fraction)
                .unwrap_or(0.0);
            let (alone, queued) = match kind {
                Kind::Analyse => (&strings.toast_analysing, &strings.toast_analysing_queue),
                Kind::Stems => (&strings.toast_stems, &strings.toast_stems_queue),
            };
            let heading = match position {
                Some((done, total)) => fill(
                    queued,
                    &[("title", title), ("done", done), ("total", total)],
                ),
                None => fill(alone, &[("title", title)]),
            };
            let mut buttons = row![
                button(
                    text(if queue.is_some() {
                        &strings.toast_queue_hide
                    } else {
                        &strings.toast_queue_show
                    })
                    .size(13)
                )
                .padding([2, 8])
                .style(button::secondary)
                .on_press(Message::JobQueueToggle)
            ]
            .spacing(6);
            if *stoppable {
                buttons = buttons.push(
                    button(text(&strings.toast_queue_stop).size(13))
                        .padding([2, 8])
                        .style(button::secondary)
                        .on_press(Message::AnalysisQueueStop),
                );
            }
            let mut lines = column![
                text(heading).size(14).wrapping(text::Wrapping::None),
                progress_bar(0.0..=1.0, fraction).girth(6),
                text(stage(progress, language))
                    .size(13)
                    .style(text::secondary),
                buttons,
            ]
            .spacing(8);
            if let Some(queue) = queue {
                lines = lines.push(queue_list(queue, language));
            }
            lines.into()
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
        let running = Toast::Job {
            kind: Kind::Analyse,
            title: "Song".into(),
            position: None,
            stoppable: false,
        };
        assert!(running.running() && !running.expired(later));
        assert!(!done.running());
    }
}
