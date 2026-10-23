//! The window for repairing a library: choose one (it does not have to be the open one) and
//! it is put in the shape this version keeps libraries in: the folder of old backups in
//! `Database2` is deleted and the origins are moved to the `open-engine` folder beside the
//! `Engine Library` folder.

use std::path::{Path, PathBuf};

use iced::widget::{Space, button, center, column, container, opaque, row, scrollable, text};
use iced::{Color, Element, Fill, Theme};
use open_engine_api::v1::RepairLibraryResponse;

use crate::Message;
use crate::config::Language;
use crate::drives::Drive;
use crate::error_text;
use crate::i18n::fill;
use crate::recent::{Recents, display_name};

const WIDTH: f32 = 560.0;
/// The height of the list in the window; it scrolls inside it.
const LIST_HEIGHT: f32 = 260.0;

/// Where the window is in its work.
#[derive(Debug, Clone)]
pub enum Stage {
    /// Choosing the library to repair.
    Pick,
    /// Repairing the library at `target`.
    Working { target: PathBuf },
    /// It failed.
    Failed { target: PathBuf, error: String },
    /// Done.
    Done(RepairLibraryResponse),
}

impl Stage {
    /// The window can be closed unless it is in the middle of its work.
    pub fn can_close(&self) -> bool {
        !matches!(self, Stage::Working { .. })
    }
}

/// What the window lists to choose from.
pub struct Sources<'a> {
    pub recents: &'a Recents,
    pub drives: &'a [Drive],
}

/// The window, centred over a dimmed view of the app that can't be clicked.
pub fn modal<'a>(
    stage: &'a Stage,
    sources: Sources<'a>,
    language: Language,
) -> Element<'a, Message> {
    let strings = language.strings();
    let secondary = |content: String| text(content).size(13).style(text::secondary);
    let close = || button(text(&strings.sync_close)).on_press(Message::RepairClose);

    let body: Element<'a, Message> = match stage {
        Stage::Pick => column![
            secondary(strings.repair_hint.clone()),
            choices(sources, language),
            row![
                button(text(&strings.repair_add))
                    .style(button::secondary)
                    .on_press(Message::RepairAdd),
                Space::new().width(Fill),
                close(),
            ]
            .spacing(10),
        ]
        .spacing(12)
        .into(),
        Stage::Working { target } => column![
            secondary(fill(
                &strings.repair_target,
                &[("path", &target.display().to_string())]
            )),
            text(&strings.repair_working),
        ]
        .spacing(12)
        .into(),
        Stage::Failed { target, error } => column![
            secondary(fill(
                &strings.repair_target,
                &[("path", &target.display().to_string())]
            )),
            error_text::view(fill(&strings.repair_failed, &[("error", error)]), language),
            row![
                button(text(&strings.repair_back))
                    .style(button::secondary)
                    .on_press(Message::RepairBack),
                Space::new().width(Fill),
                close(),
            ]
            .spacing(10),
        ]
        .spacing(12)
        .into(),
        Stage::Done(done) => {
            let mut report = column![].spacing(4);
            for (happened, message) in [
                (done.origins_moved, &strings.repair_origins_moved),
                (done.backup_removed, &strings.repair_backup_removed),
            ] {
                if happened {
                    report = report.push(text(message));
                }
            }
            if !done.origins_moved && !done.backup_removed && !done.origins_conflict {
                report = report.push(text(&strings.repair_nothing));
            }
            if done.origins_conflict {
                report = report.push(text(&strings.repair_conflict).style(text::danger));
            }
            column![
                secondary(fill(&strings.repair_target, &[("path", &done.target_path)])),
                report,
                row![
                    button(text(&strings.repair_back))
                        .style(button::secondary)
                        .on_press(Message::RepairBack),
                    Space::new().width(Fill),
                    close(),
                ]
                .spacing(10),
            ]
            .spacing(12)
            .into()
        }
    };

    let dialog = container(column![text(&strings.repair_title).size(24), body].spacing(14))
        .padding(20)
        .width(WIDTH)
        .style(container::rounded_box);

    // The dimmed backdrop swallows clicks, so nothing behind the window can be used.
    opaque(center(opaque(dialog)).style(|_: &Theme| {
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

/// The recent libraries and the drives with a library.
fn choices<'a>(sources: Sources<'a>, language: Language) -> Element<'a, Message> {
    let strings = language.strings();

    let pick = |path: &Path, label: String, detail: String| -> Element<'a, Message> {
        button(column![
            text(label).wrapping(text::Wrapping::None),
            text(detail)
                .size(12)
                .style(text::secondary)
                .wrapping(text::Wrapping::None)
        ])
        .width(Fill)
        .style(button::text)
        .on_press(Message::RepairTarget(path.to_path_buf()))
        .into()
    };

    let mut recent = column![text(&strings.recent_libraries).size(16)].spacing(4);
    if sources.recents.paths().is_empty() {
        recent = recent.push(text(&strings.no_recent_libraries).style(text::secondary));
    }
    for path in sources.recents.paths() {
        recent = recent.push(pick(path, display_name(path), path.display().to_string()));
    }

    // (A drive without a library has nothing to repair.)
    let mut drives = column![text(&strings.drives).size(16)].spacing(4);
    let with_library: Vec<(&Drive, &PathBuf)> = sources
        .drives
        .iter()
        .filter_map(|drive| Some((drive, drive.library.as_ref()?)))
        .collect();
    if with_library.is_empty() {
        drives = drives.push(text(&strings.no_drives).style(text::secondary));
    }
    for (drive, library) in with_library {
        drives = drives.push(pick(
            library,
            drive.name.clone(),
            drive.mount.display().to_string(),
        ));
    }

    container(scrollable(
        column![recent, drives].spacing(14).padding(8).width(Fill),
    ))
    .height(LIST_HEIGHT)
    .style(container::bordered_box)
    .into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_window_can_be_closed_unless_it_is_working() {
        assert!(Stage::Pick.can_close());
        assert!(
            Stage::Failed {
                target: "/x".into(),
                error: String::new()
            }
            .can_close()
        );
        assert!(Stage::Done(RepairLibraryResponse::default()).can_close());
        assert!(
            !Stage::Working {
                target: "/x".into()
            }
            .can_close()
        );
    }
}
