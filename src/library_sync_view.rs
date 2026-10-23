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
use crate::sync_view::{describe, playlist_change, track_change};

const WIDTH: f32 = 760.0;
/// The height of the lists in the window; they scroll inside it.
const LIST_HEIGHT: f32 = 260.0;

/// What to sync besides the playlists.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Options {
    /// Also delete playlists that only the other library has.
    pub remove_missing: bool,
    /// The stars of the open library replace those of the other library's tracks.
    pub stars: bool,
    /// The track paths of the open library replace those of the other library, as written.
    pub locations: bool,
    /// The audio files are copied to `Engine-Tracks` beside the other library (in an
    /// `Artist/Album` structure) and the tracks lead there; missing tracks are added.
    pub copy_tracks: bool,
}

impl Default for Options {
    /// Stars are synced, the rest is for the user to ask for.
    fn default() -> Self {
        Self {
            remove_missing: false,
            stars: true,
            locations: false,
            copy_tracks: false,
        }
    }
}

/// A tick of the window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Setting {
    RemoveMissing,
    Stars,
    Locations,
    CopyTracks,
}

impl Options {
    pub fn with(mut self, setting: Setting, on: bool) -> Self {
        match setting {
            Setting::RemoveMissing => self.remove_missing = on,
            Setting::Stars => self.stars = on,
            Setting::Locations => self.locations = on,
            Setting::CopyTracks => self.copy_tracks = on,
        }
        self
    }
}

/// Whether there is anything to write.
pub fn has_changes(diff: &GetLibrarySyncDiffResponse) -> bool {
    diff.playlists_changed > 0
        || diff.origins_changed > 0
        || diff.tracks_added > 0
        || diff.tracks_changed > 0
        || diff.files_to_copy > 0
}

/// Where the window is in its work.
#[derive(Debug, Clone)]
pub enum Stage {
    /// Choosing the library to sync to.
    Pick,
    /// Comparing with `target`.
    Loading {
        target: PathBuf,
        options: Options,
    },
    /// The comparison failed.
    Failed {
        target: PathBuf,
        error: String,
    },
    /// The differences are shown. `error` is why the last attempt to sync failed.
    Ready {
        target: PathBuf,
        options: Options,
        diff: Arc<GetLibrarySyncDiffResponse>,
        error: Option<String>,
    },
    /// Writing; what was shown is kept, to go back to if it fails.
    Syncing {
        target: PathBuf,
        options: Options,
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
            options,
            diff,
            error,
            ..
        } => ready(diff, *options, error.as_deref(), true, language),
        Stage::Syncing { options, diff, .. } => column![
            ready(diff, *options, None, false, language),
            text(&strings.libsync_writing),
        ]
        .spacing(12)
        .into(),
        Stage::Done(done) => {
            let message = if done.backup_path.is_empty() && done.files_copied == 0 {
                strings.libsync_none_written.clone()
            } else {
                fill(
                    &strings.libsync_done,
                    &[
                        ("count", &done.playlists_changed),
                        ("tracks", &(done.tracks_added + done.tracks_changed)),
                        ("origins", &done.origins_changed),
                        ("files", &done.files_copied),
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
    options: Options,
    error: Option<&'a str>,
    active: bool,
    language: Language,
) -> Element<'a, Message> {
    let strings = language.strings();
    let secondary = |content: String| text(content).size(13).style(text::secondary);
    let tick = |label: &'a String, on: bool, setting: Setting, enabled: bool| {
        checkbox(on).label(label).on_toggle_maybe(
            (active && enabled).then_some(move |on| Message::LibrarySyncOption(setting, on)),
        )
    };

    let mut content = column![
        secondary(fill(
            &strings.libsync_target,
            &[("path", &diff.target_path)]
        )),
        secondary(strings.libsync_hint.clone()),
        tick(
            &strings.libsync_remove,
            options.remove_missing,
            Setting::RemoveMissing,
            true
        ),
        tick(&strings.libsync_stars, options.stars, Setting::Stars, true),
        // The copied files' places are the locations, so there is nothing to overwrite with.
        tick(
            &strings.libsync_locations,
            options.locations && !options.copy_tracks,
            Setting::Locations,
            !options.copy_tracks,
        ),
        tick(
            &strings.libsync_copy,
            options.copy_tracks,
            Setting::CopyTracks,
            true
        ),
    ]
    .spacing(6);

    if !has_changes(diff) {
        content = content.push(text(&strings.libsync_nothing));
    } else {
        content = content.push(changes(diff, language));
        let mut summary = column![text(fill(
            &strings.libsync_summary,
            &[("count", &diff.playlists_changed)],
        ))]
        .spacing(2);
        summary = summary.push(text(fill(
            &strings.libsync_tracks_summary,
            &[
                ("added", &diff.tracks_added),
                ("changed", &diff.tracks_changed),
            ],
        )));
        if diff.files_to_copy > 0 {
            summary = summary.push(text(fill(
                &strings.libsync_files,
                &[
                    ("count", &diff.files_to_copy),
                    ("size", &format_size(diff.bytes_to_copy)),
                ],
            )));
        }
        summary = summary.push(text(fill(
            &strings.libsync_origins,
            &[("count", &diff.origins_changed)],
        )));
        content = content.push(summary);
    }
    for (count, label, style) in [
        (
            diff.tracks_missing,
            &strings.libsync_missing,
            text::base as fn(&Theme) -> text::Style,
        ),
        (
            diff.files_missing,
            &strings.libsync_files_missing,
            text::danger,
        ),
        (
            diff.location_conflicts,
            &strings.libsync_conflicts,
            text::danger,
        ),
    ] {
        if count > 0 {
            content = content.push(text(fill(label, &[("count", &count)])).style(style));
        }
    }
    if let Some(error) = error {
        content = content.push(
            text(fill(&strings.libsync_run_failed, &[("error", &error)])).style(text::danger),
        );
    }

    let buttons = row![
        button(text(&strings.libsync_run))
            .style(button::primary)
            .on_press_maybe((active && has_changes(diff)).then_some(Message::LibrarySyncStart)),
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

/// `1.4 GB`, `23.0 MB`, `512 B`: a size for people.
fn format_size(bytes: u64) -> String {
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

/// The scrolling list of what would change: playlists, tracks to add, tracks that change.
fn changes<'a>(diff: &'a GetLibrarySyncDiffResponse, language: Language) -> Element<'a, Message> {
    let strings = language.strings();
    let more = |list: iced::widget::Column<'a, Message>, total: u32, shown: usize| {
        let hidden = (total as usize).saturating_sub(shown);
        if hidden > 0 {
            list.push(
                text(fill(&strings.sync_more, &[("count", &hidden)]))
                    .size(13)
                    .style(text::secondary),
            )
        } else {
            list
        }
    };

    let mut list = column![].spacing(3);
    for change in &diff.changed_playlists {
        list = list.push(playlist_change(change, language));
    }
    list = more(list, diff.playlists_changed, diff.changed_playlists.len());
    for track in &diff.added_tracks {
        list = list.push(
            text(format!(
                "+  {}  ({})",
                describe(track),
                strings.libsync_new_track
            ))
            .size(14)
            .style(text::success),
        );
    }
    list = more(list, diff.tracks_added, diff.added_tracks.len());
    for change in &diff.changed_tracks {
        list = list.push(track_change(change));
    }
    list = more(list, diff.tracks_changed, diff.changed_tracks.len());

    container(scrollable(list.padding(8).width(Fill)))
        .height(LIST_HEIGHT)
        .style(container::bordered_box)
        .into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stars_are_on_by_default_and_the_rest_is_asked_for() {
        let options = Options::default();
        assert!(options.stars);
        assert!(!options.remove_missing && !options.locations && !options.copy_tracks);

        let options = options
            .with(Setting::CopyTracks, true)
            .with(Setting::Stars, false)
            .with(Setting::RemoveMissing, true);
        assert!(options.copy_tracks && options.remove_missing && !options.stars);
        assert!(!options.locations);
    }

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
    fn anything_to_write_enables_the_sync() {
        let nothing = GetLibrarySyncDiffResponse::default();
        assert!(!has_changes(&nothing));
        for diff in [
            GetLibrarySyncDiffResponse {
                tracks_changed: 1,
                ..Default::default()
            },
            GetLibrarySyncDiffResponse {
                files_to_copy: 2,
                ..Default::default()
            },
            GetLibrarySyncDiffResponse {
                tracks_added: 1,
                ..Default::default()
            },
        ] {
            assert!(has_changes(&diff));
        }
    }
}
