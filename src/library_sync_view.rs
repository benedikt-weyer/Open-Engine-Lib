//! The window for syncing the playlists of the open library to another library: first
//! the other library is chosen, then what would change is shown and can be written.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use iced::widget::{
    Space, button, center, checkbox, column, container, opaque, row, scrollable, text,
};
use iced::{Color, Element, Fill, Theme};
use open_engine_api::v1::{GetLibrarySyncDiffResponse, SyncToLibraryResponse};

use crate::Message;
use crate::config::Language;
use crate::drives::Drive;
use crate::i18n::fill;
use crate::recent::{Recents, display_name};
use crate::sync_view::playlist_change;

const WIDTH: f32 = 760.0;
/// The height of the lists in the window; they scroll inside it.
const LIST_HEIGHT: f32 = 260.0;

/// Where the window is in its work.
#[derive(Debug, Clone)]
pub enum Stage {
    /// Choosing the library to sync to.
    Pick,
    /// Comparing with `target`.
    Loading {
        target: PathBuf,
        remove_missing: bool,
    },
    /// The comparison failed.
    Failed {
        target: PathBuf,
        error: String,
    },
    /// The differences are shown. `error` is why the last attempt to sync failed.
    Ready {
        target: PathBuf,
        remove_missing: bool,
        diff: Arc<GetLibrarySyncDiffResponse>,
        error: Option<String>,
    },
    /// Writing; what was shown is kept, to go back to if it fails.
    Syncing {
        target: PathBuf,
        remove_missing: bool,
        diff: Arc<GetLibrarySyncDiffResponse>,
    },
    Done(SyncToLibraryResponse),
}

impl Stage {
    /// The window can be closed unless it is in the middle of writing.
    pub fn can_close(&self) -> bool {
        !matches!(self, Stage::Syncing { .. })
    }
}

/// What the window lists to choose from.
pub struct Sources<'a> {
    pub recents: &'a Recents,
    pub drives: &'a [Drive],
    /// The open library's `Engine Library` folder, which can't be the one to sync to.
    pub current: &'a str,
}

/// The window, centred over a dimmed view of the app that can't be clicked.
pub fn modal<'a>(
    stage: &'a Stage,
    sources: Sources<'a>,
    language: Language,
) -> Element<'a, Message> {
    let strings = language.strings();
    let secondary = |content: String| text(content).size(13).style(text::secondary);

    let close = || button(text(&strings.sync_close)).on_press(Message::LibrarySyncClose);

    let body: Element<'a, Message> = match stage {
        Stage::Pick => column![
            text(&strings.libsync_pick),
            choices(sources, language),
            row![
                button(text(&strings.libsync_add))
                    .style(button::secondary)
                    .on_press(Message::LibrarySyncAdd),
                Space::new().width(Fill),
                close(),
            ]
            .spacing(10),
        ]
        .spacing(12)
        .into(),
        Stage::Loading { target, .. } => column![
            secondary(fill(
                &strings.libsync_target,
                &[("path", &target.display())]
            )),
            text(&strings.libsync_comparing),
        ]
        .spacing(8)
        .into(),
        Stage::Failed { target, error } => column![
            secondary(fill(
                &strings.libsync_target,
                &[("path", &target.display())]
            )),
            text(fill(&strings.libsync_failed, &[("error", error)])).style(text::danger),
            row![
                button(text(&strings.libsync_back))
                    .style(button::secondary)
                    .on_press(Message::LibrarySyncBack),
                Space::new().width(Fill),
                close(),
            ]
            .spacing(10),
        ]
        .spacing(12)
        .into(),
        Stage::Ready {
            remove_missing,
            diff,
            error,
            ..
        } => ready(diff, *remove_missing, error.as_deref(), true, language),
        Stage::Syncing {
            remove_missing,
            diff,
            ..
        } => column![
            ready(diff, *remove_missing, None, false, language),
            text(&strings.libsync_writing),
        ]
        .spacing(12)
        .into(),
        Stage::Done(done) => {
            let message = if done.backup_path.is_empty() {
                strings.libsync_none_written.clone()
            } else {
                fill(
                    &strings.libsync_done,
                    &[
                        ("count", &done.playlists_changed),
                        ("origins", &done.origins_changed),
                        ("backup", &done.backup_path),
                    ],
                )
            };
            column![
                secondary(fill(
                    &strings.libsync_target,
                    &[("path", &done.target_path)]
                )),
                text(message),
                row![Space::new().width(Fill), close()],
            ]
            .spacing(12)
            .into()
        }
    };

    let dialog = container(column![text(&strings.libsync_title).size(24), body].spacing(14))
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

/// The recent libraries and the drives with a library, except the open one.
fn choices<'a>(sources: Sources<'a>, language: Language) -> Element<'a, Message> {
    let strings = language.strings();
    let current = Path::new(sources.current);

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
        .on_press(Message::LibrarySyncTarget(path.to_path_buf()))
        .into()
    };

    let mut recent = column![text(&strings.recent_libraries).size(16)].spacing(4);
    let others: Vec<&PathBuf> = sources
        .recents
        .paths()
        .iter()
        .filter(|path| path.as_path() != current)
        .collect();
    if others.is_empty() {
        recent = recent.push(text(&strings.no_recent_libraries).style(text::secondary));
    }
    for path in others {
        recent = recent.push(pick(path, display_name(path), path.display().to_string()));
    }

    let mut drives = column![text(&strings.drives).size(16)].spacing(4);
    let with_library: Vec<(&Drive, &PathBuf)> = sources
        .drives
        .iter()
        .filter_map(|drive| Some((drive, drive.library.as_ref()?)))
        .filter(|(_, library)| library.as_path() != current)
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

/// The differences, with the options and the buttons. `active` is false while writing.
fn ready<'a>(
    diff: &'a Arc<GetLibrarySyncDiffResponse>,
    remove_missing: bool,
    error: Option<&'a str>,
    active: bool,
    language: Language,
) -> Element<'a, Message> {
    let strings = language.strings();
    let secondary = |content: String| text(content).size(13).style(text::secondary);

    let mut content = column![
        secondary(fill(
            &strings.libsync_target,
            &[("path", &diff.target_path)]
        )),
        secondary(strings.libsync_hint.clone()),
        checkbox(remove_missing)
            .label(&strings.libsync_remove)
            .on_toggle_maybe(active.then_some(Message::LibrarySyncRemove)),
    ]
    .spacing(8);

    if diff.playlists_changed == 0 && diff.origins_changed == 0 {
        content = content.push(text(&strings.libsync_nothing));
    } else {
        content = content
            .push(text(fill(
                &strings.libsync_summary,
                &[("count", &diff.playlists_changed)],
            )))
            .push(changes(diff, language))
            .push(text(fill(
                &strings.libsync_origins,
                &[("count", &diff.origins_changed)],
            )));
    }
    if diff.tracks_missing > 0 {
        content = content.push(text(fill(
            &strings.libsync_missing,
            &[("count", &diff.tracks_missing)],
        )));
    }
    if let Some(error) = error {
        content = content.push(
            text(fill(&strings.libsync_run_failed, &[("error", &error)])).style(text::danger),
        );
    }

    let buttons = row![
        button(text(&strings.libsync_run))
            .style(button::primary)
            .on_press_maybe(
                (active && (diff.playlists_changed > 0 || diff.origins_changed > 0))
                    .then_some(Message::LibrarySyncStart)
            ),
        button(text(&strings.libsync_back))
            .style(button::secondary)
            .on_press_maybe(active.then_some(Message::LibrarySyncBack)),
        Space::new().width(Fill),
        button(text(&strings.sync_close))
            .on_press_maybe(active.then_some(Message::LibrarySyncClose)),
    ]
    .spacing(10);

    column![content, buttons].spacing(14).into()
}

/// The scrolling list of playlists that would change.
fn changes<'a>(diff: &'a GetLibrarySyncDiffResponse, language: Language) -> Element<'a, Message> {
    let strings = language.strings();
    let mut list = column![].spacing(3);
    for change in &diff.changed_playlists {
        list = list.push(playlist_change(change, language));
    }
    let hidden = (diff.playlists_changed as usize).saturating_sub(diff.changed_playlists.len());
    if hidden > 0 {
        list = list.push(
            text(fill(&strings.sync_more, &[("count", &hidden)]))
                .size(13)
                .style(text::secondary),
        );
    }
    container(scrollable(list.padding(8).width(Fill)))
        .height(LIST_HEIGHT)
        .style(container::bordered_box)
        .into()
}
