//! The window for comparing the working copy with the source and writing it there.

use std::sync::Arc;

use iced::widget::{
    Space, button, center, checkbox, column, container, opaque, row, scrollable, text,
};
use iced::{Color, Element, Fill, Theme};
use open_engine_api::v1::{
    GetSyncDiffResponse, PlaylistChange, PlaylistChangeKind, SyncToSourceResponse, TrackChange,
    TrackRef,
};

use crate::Message;
use crate::config::Language;
use crate::i18n::fill;

const WIDTH: f32 = 760.0;
/// The height of the list of differences; it scrolls inside the window.
const LIST_HEIGHT: f32 = 300.0;

/// Where the sync window is in its work.
#[derive(Debug, Clone)]
pub enum Stage {
    /// Comparing.
    Loading,
    /// The comparison failed.
    Failed(String),
    /// The differences are shown. `acknowledged` is the tick that allows overwriting
    /// changes made outside this app, `error` why the last attempt to sync failed.
    Ready {
        diff: Arc<GetSyncDiffResponse>,
        acknowledged: bool,
        error: Option<String>,
    },
    /// Writing; what was shown is kept, to go back to if it fails.
    Syncing {
        diff: Arc<GetSyncDiffResponse>,
        acknowledged: bool,
    },
    Done(SyncToSourceResponse),
}

impl Stage {
    /// The window can be closed unless it is in the middle of writing.
    pub fn can_close(&self) -> bool {
        !matches!(self, Stage::Syncing { .. })
    }
}

/// Whether syncing may be started: there is something to write, the source can be written
/// to, and if someone else changed it, the user has said that is to be overwritten.
pub fn can_sync(diff: &GetSyncDiffResponse, acknowledged: bool) -> bool {
    diff.source_available
        && !diff.source_busy
        && !diff.changed_databases.is_empty()
        && (!diff.source_changed_outside || acknowledged)
}

/// The window, centred over a dimmed view of the app that can't be clicked.
pub fn modal<'a>(stage: &'a Stage, language: Language) -> Element<'a, Message> {
    let strings = language.strings();

    let body: Element<'a, Message> = match stage {
        Stage::Loading => text(&strings.sync_comparing).into(),
        Stage::Failed(error) => column![
            text(fill(&strings.sync_load_failed, &[("error", error)])).style(text::danger),
            row![
                button(text(&strings.sync_refresh)).on_press(Message::SyncOpen),
                button(text(&strings.sync_close)).on_press(Message::SyncClose),
            ]
            .spacing(10),
        ]
        .spacing(12)
        .into(),
        Stage::Ready {
            diff,
            acknowledged,
            error,
        } => ready(diff, *acknowledged, error.as_deref(), language),
        Stage::Syncing { .. } => text(&strings.sync_writing).into(),
        Stage::Done(done) => {
            let message = if done.files_written == 0 {
                strings.sync_nothing_written.clone()
            } else {
                fill(
                    &strings.sync_done,
                    &[
                        ("count", &done.files_written),
                        ("backup", &done.backup_path),
                    ],
                )
            };
            column![
                text(message),
                button(text(&strings.sync_close)).on_press(Message::SyncClose),
            ]
            .spacing(12)
            .into()
        }
    };

    let dialog = container(column![text(&strings.sync_title).size(24), body].spacing(14))
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

fn ready<'a>(
    diff: &'a Arc<GetSyncDiffResponse>,
    acknowledged: bool,
    error: Option<&'a str>,
    language: Language,
) -> Element<'a, Message> {
    let strings = language.strings();
    let secondary = |content: String| text(content).size(13).style(text::secondary);

    let mut content = column![
        secondary(fill(&strings.sync_source, &[("path", &diff.source_path)])),
        secondary(fill(
            &strings.sync_working_copy,
            &[("path", &diff.cache_path)]
        )),
    ]
    .spacing(4);

    if !diff.source_available {
        content = content.push(text(&strings.sync_unavailable).style(text::danger));
    } else if diff.changed_databases.is_empty() {
        content = content.push(text(&strings.sync_no_differences));
    } else {
        content = content
            .push(summary(diff, language))
            .push(changes(diff, language));
    }
    if diff.source_busy {
        content = content.push(text(&strings.sync_busy).style(text::danger));
    }
    if diff.source_changed_outside && diff.source_available {
        content = content
            .push(text(&strings.sync_outside_warning).style(text::danger))
            .push(
                checkbox(acknowledged)
                    .label(&strings.sync_outside_confirm)
                    .on_toggle(Message::SyncAcknowledged),
            );
    }
    if !diff.changed_databases.is_empty() && diff.source_available {
        content = content.push(secondary(strings.sync_direction_hint.clone()));
    }
    if let Some(error) = error {
        content = content
            .push(text(fill(&strings.sync_failed, &[("error", &error)])).style(text::danger));
    }

    let buttons = row![
        button(text(&strings.sync_run))
            .style(button::primary)
            .on_press_maybe(can_sync(diff, acknowledged).then_some(Message::SyncStart)),
        button(text(&strings.sync_refresh))
            .style(button::secondary)
            .on_press(Message::SyncOpen),
        Space::new().width(Fill),
        button(text(&strings.sync_close)).on_press(Message::SyncClose),
    ]
    .spacing(10);

    column![content.spacing(10), buttons].spacing(14).into()
}

fn summary<'a>(diff: &'a GetSyncDiffResponse, language: Language) -> Element<'a, Message> {
    let strings = language.strings();
    column![
        text(fill(
            &strings.sync_tracks_summary,
            &[
                ("added", &diff.tracks_added),
                ("removed", &diff.tracks_removed),
                ("changed", &diff.tracks_changed),
            ],
        )),
        text(fill(
            &strings.sync_playlists_summary,
            &[("count", &diff.playlists_changed)],
        )),
        text(fill(
            &strings.sync_databases_summary,
            &[("names", &diff.changed_databases.join(", "))],
        )),
    ]
    .spacing(2)
    .into()
}

/// "Title — Artist", or the path for a track without either.
fn describe(track: &TrackRef) -> String {
    match (track.title.is_empty(), track.artist.is_empty()) {
        (false, false) => format!("{} — {}", track.title, track.artist),
        (false, true) => track.title.clone(),
        _ => track.path.clone(),
    }
}

/// The scrolling list of what differs.
fn changes<'a>(diff: &'a GetSyncDiffResponse, language: Language) -> Element<'a, Message> {
    let strings = language.strings();
    let mut list = column![].spacing(3);

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

    for track in &diff.added_tracks {
        list = list.push(
            text(format!("+  {}", describe(track)))
                .size(14)
                .style(text::success),
        );
    }
    list = more(list, diff.tracks_added, diff.added_tracks.len());
    for track in &diff.removed_tracks {
        list = list.push(
            text(format!("−  {}", describe(track)))
                .size(14)
                .style(text::danger),
        );
    }
    list = more(list, diff.tracks_removed, diff.removed_tracks.len());
    for change in &diff.changed_tracks {
        list = list.push(track_change(change));
    }
    list = more(list, diff.tracks_changed, diff.changed_tracks.len());
    for change in &diff.changed_playlists {
        list = list.push(playlist_change(change, language));
    }
    list = more(list, diff.playlists_changed, diff.changed_playlists.len());

    container(scrollable(list.padding(8).width(Fill)))
        .height(LIST_HEIGHT)
        .style(container::bordered_box)
        .into()
}

fn track_change<'a>(change: &'a TrackChange) -> Element<'a, Message> {
    let mut lines = column![
        text(format!(
            "~  {}",
            change.track.as_ref().map(describe).unwrap_or_default()
        ))
        .size(14)
    ]
    .spacing(1);
    for field in &change.fields {
        let from = if field.source_value.is_empty() {
            "—"
        } else {
            &field.source_value
        };
        let to = if field.working_value.is_empty() {
            "—"
        } else {
            &field.working_value
        };
        lines = lines.push(
            text(format!("      {}: {from} → {to}", field.field))
                .size(12)
                .style(text::secondary),
        );
    }
    lines.into()
}

fn playlist_change<'a>(change: &'a PlaylistChange, language: Language) -> Element<'a, Message> {
    let strings = language.strings();
    let (mark, style): (&str, fn(&Theme) -> text::Style) = match change.kind() {
        PlaylistChangeKind::Added => ("+", text::success),
        PlaylistChangeKind::Removed => ("−", text::danger),
        _ => ("~", text::base),
    };

    let mut notes: Vec<String> = Vec::new();
    match change.kind() {
        PlaylistChangeKind::Added => notes.push(strings.sync_playlist_added.clone()),
        PlaylistChangeKind::Removed => notes.push(strings.sync_playlist_removed.clone()),
        PlaylistChangeKind::Renamed => notes.push(fill(
            &strings.sync_playlist_renamed,
            &[("old", &change.detail)],
        )),
        PlaylistChangeKind::Moved => notes.push(fill(
            &strings.sync_playlist_moved,
            &[("old", &change.detail)],
        )),
        _ => {}
    }
    if change.tracks_added > 0 || change.tracks_removed > 0 {
        notes.push(fill(
            &strings.sync_playlist_track_counts,
            &[
                ("added", &change.tracks_added),
                ("removed", &change.tracks_removed),
            ],
        ));
    }
    if change.order_changed {
        notes.push(strings.sync_playlist_order.clone());
    }
    if notes.is_empty() {
        notes.push(strings.sync_playlist_tracks.clone());
    }

    text(format!("{mark}  {}  ({})", change.name, notes.join(", ")))
        .size(14)
        .style(style)
        .into()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn diff(changed: &[&str]) -> GetSyncDiffResponse {
        GetSyncDiffResponse {
            source_available: true,
            changed_databases: changed.iter().map(|name| name.to_string()).collect(),
            ..Default::default()
        }
    }

    #[test]
    fn syncing_needs_something_to_write_and_a_source_to_write_to() {
        assert!(can_sync(&diff(&["m.db"]), false));
        assert!(!can_sync(&diff(&[]), false), "nothing to write");

        let mut gone = diff(&["m.db"]);
        gone.source_available = false;
        assert!(!can_sync(&gone, true));
        let mut busy = diff(&["m.db"]);
        busy.source_busy = true;
        assert!(
            !can_sync(&busy, true),
            "not while another program is writing"
        );
    }

    #[test]
    fn overwriting_changes_made_outside_must_be_confirmed() {
        let mut outside = diff(&["m.db"]);
        outside.source_changed_outside = true;
        assert!(!can_sync(&outside, false));
        assert!(can_sync(&outside, true));
    }

    #[test]
    fn the_window_cannot_be_closed_while_writing() {
        assert!(Stage::Loading.can_close());
        assert!(Stage::Failed(String::new()).can_close());
        assert!(Stage::Done(SyncToSourceResponse::default()).can_close());
        let syncing = Stage::Syncing {
            diff: Arc::new(GetSyncDiffResponse::default()),
            acknowledged: false,
        };
        assert!(!syncing.can_close());
    }

    #[test]
    fn a_track_is_named_by_title_and_artist_or_failing_that_its_path() {
        let track = |title: &str, artist: &str| TrackRef {
            title: title.into(),
            artist: artist.into(),
            path: "../a.flac".into(),
        };
        assert_eq!(describe(&track("One", "A")), "One — A");
        assert_eq!(describe(&track("One", "")), "One");
        assert_eq!(describe(&track("", "")), "../a.flac");
    }
}
