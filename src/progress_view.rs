//! The progress bar of a sync that is running (its window shows it in place of the
//! buttons), with what it is doing and how much of it is done.

use iced::widget::{column, progress_bar, text};
use iced::{Element, Fill};
use open_engine_api::v1::{OperationProgress, ProgressPhase};

use crate::Message;
use crate::config::Language;
use crate::i18n::fill;

/// How often the backend is asked how far the sync is.
pub const POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(250);

/// `1.4 GB`, `23.0 MB`, `512 B`: a size for people.
pub fn format_size(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KB", "MB", "GB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// How much of the current phase is done, from 0 to 1; `None` if the phase has nothing
/// that can be counted (then it is just named).
pub fn fraction(progress: &OperationProgress) -> Option<f32> {
    (progress.total_bytes > 0)
        .then(|| (progress.done_bytes as f64 / progress.total_bytes as f64).clamp(0.0, 1.0) as f32)
}

/// The bar and what it is about. Before the backend has said anything (`None`) it only
/// says it is working.
pub fn view<'a>(
    progress: Option<&'a OperationProgress>,
    language: Language,
) -> Element<'a, Message> {
    let strings = language.strings();
    let Some(progress) = progress.filter(|progress| progress.running) else {
        return text(&strings.libsync_writing).into();
    };

    let name = match progress.phase() {
        ProgressPhase::Preparing => &strings.progress_preparing,
        ProgressPhase::CopyingTracks => &strings.progress_copying,
        ProgressPhase::Updating => &strings.progress_updating,
        ProgressPhase::Writing => &strings.progress_writing,
        ProgressPhase::Pulling => &strings.progress_pulling,
        ProgressPhase::Unspecified => &strings.libsync_writing,
    };
    let mut lines = column![text(name)].spacing(6).width(Fill);
    if let Some(fraction) = fraction(progress) {
        lines = lines.push(progress_bar(0.0..=1.0, fraction));
        let mut detail = fill(
            &strings.progress_bytes,
            &[
                ("done", &format_size(progress.done_bytes)),
                ("total", &format_size(progress.total_bytes)),
            ],
        );
        if progress.total_files > 0 {
            detail = format!(
                "{detail} · {}",
                fill(
                    &strings.progress_files,
                    &[
                        ("done", &progress.done_files),
                        ("total", &progress.total_files),
                    ],
                )
            );
        }
        lines = lines.push(text(detail).size(13).style(text::secondary));
        if !progress.current.is_empty() {
            lines = lines.push(
                text(&progress.current)
                    .size(13)
                    .style(text::secondary)
                    .wrapping(text::Wrapping::None),
            );
        }
    }
    lines.into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_are_written_for_people() {
        assert_eq!(format_size(512), "512 B");
        assert_eq!(format_size(1536), "1.5 KB");
        assert_eq!(format_size(23 * 1024 * 1024), "23.0 MB");
        assert_eq!(
            format_size(3 * 1024 * 1024 * 1024 + 512 * 1024 * 1024),
            "3.5 GB"
        );
    }

    #[test]
    fn a_phase_without_bytes_has_no_fraction() {
        let mut progress = OperationProgress {
            running: true,
            ..Default::default()
        };
        assert_eq!(fraction(&progress), None);
        progress.total_bytes = 200;
        progress.done_bytes = 50;
        assert_eq!(fraction(&progress), Some(0.25));
        progress.done_bytes = 300; // (a bit over is still a full bar)
        assert_eq!(fraction(&progress), Some(1.0));
    }
}
