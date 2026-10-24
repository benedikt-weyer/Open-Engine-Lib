//! The window that imports a folder of the open folder of music into an Engine library as a
//! playlist: first the library is chosen, then the title of the playlist and whether the
//! files are copied and the tracks analysed, then the work is done and told.

use std::path::PathBuf;

use iced::widget::{
    Space, button, center, checkbox, column, container, opaque, row, text, text_input,
};
use iced::{Color, Element, Fill, Theme};
use open_engine_api::v1::{ImportFolderResponse, OperationProgress};

use crate::Message;
use crate::config::Language;
use crate::error_text;
use crate::i18n::fill;
use crate::library_sync_view::{Sources, choices};
use crate::progress_view;

const WIDTH: f32 = 680.0;

/// What the user can choose besides the library.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Options {
    /// The title of the playlist the tracks go in.
    pub title: String,
    /// The audio files are copied into `Engine-Tracks` beside the library (else the tracks
    /// lead to the files where they are).
    pub copy_files: bool,
    /// The new tracks are analysed once the library is open.
    pub analyse: bool,
}

/// A tick of the window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Setting {
    CopyFiles,
    Analyse,
}

impl Options {
    pub fn with(mut self, setting: Setting, on: bool) -> Self {
        match setting {
            Setting::CopyFiles => self.copy_files = on,
            Setting::Analyse => self.analyse = on,
        }
        self
    }
}

/// Where the window is in its work.
#[derive(Debug, Clone)]
pub enum Stage {
    /// Choosing the library to import into.
    Pick,
    /// The library is chosen; the options are set. `error` is why the last try failed.
    Options {
        target: PathBuf,
        options: Options,
        error: Option<String>,
    },
    /// Importing; what was chosen is kept, to go back to if it fails.
    Importing { target: PathBuf, options: Options },
    /// Done; whether the new tracks are to be analysed when the library is opened.
    Done {
        done: ImportFolderResponse,
        analyse: bool,
    },
}

impl Stage {
    /// The window can be closed unless it is in the middle of the work.
    pub fn can_close(&self) -> bool {
        !matches!(self, Stage::Importing { .. })
    }
}

/// The folder being imported, and the window's stage.
#[derive(Debug, Clone)]
pub struct Import {
    /// The folder, as an index into the open library's playlists.
    pub playlist: usize,
    /// The folder's name, which is the playlist's title unless the user changes it.
    pub name: String,
    /// How many tracks the folder has (with the folders below it).
    pub tracks: usize,
    pub stage: Stage,
}

/// The window, centred over a dimmed view of the app that can't be clicked.
pub fn modal<'a>(
    import: &'a Import,
    sources: Sources<'a>,
    progress: Option<&'a OperationProgress>,
    language: Language,
) -> Element<'a, Message> {
    let strings = language.strings();
    let secondary = |content: String| text(content).size(13).style(text::secondary);
    let close = || button(text(&strings.sync_close)).on_press(Message::ImportClose);
    let folder = secondary(fill(
        &strings.import_folder,
        &[("name", &import.name), ("count", &import.tracks)],
    ));

    let body: Element<'a, Message> = match &import.stage {
        Stage::Pick => column![
            folder,
            text(&strings.import_pick),
            choices(sources, language, Message::ImportTarget, false),
            row![
                button(text(&strings.libsync_add))
                    .style(button::secondary)
                    .on_press(Message::ImportAdd),
                Space::new().width(Fill),
                close(),
            ]
            .spacing(10),
        ]
        .spacing(12)
        .into(),
        Stage::Options {
            target,
            options,
            error,
        } => {
            let mut lines = column![
                folder,
                secondary(fill(&strings.import_target, &[("path", &target.display())])),
                text(&strings.import_playlist_title).size(14),
                text_input("", &options.title)
                    .on_input(Message::ImportTitle)
                    .on_submit_maybe(
                        (!options.title.trim().is_empty()).then_some(Message::ImportStart)
                    ),
                checkbox(options.copy_files)
                    .label(&strings.import_copy)
                    .on_toggle(|on| Message::ImportOption(Setting::CopyFiles, on)),
                checkbox(options.analyse)
                    .label(&strings.import_analyse)
                    .on_toggle(|on| Message::ImportOption(Setting::Analyse, on)),
            ]
            .spacing(10);
            if let Some(error) = error {
                lines = lines.push(error_text::view(
                    fill(&strings.import_failed, &[("error", error)]),
                    language,
                ));
            }
            lines
                .push(
                    row![
                        button(text(&strings.libsync_back))
                            .style(button::secondary)
                            .on_press(Message::ImportBack),
                        Space::new().width(Fill),
                        button(text(&strings.import_start)).on_press_maybe(
                            (!options.title.trim().is_empty()).then_some(Message::ImportStart)
                        ),
                        close(),
                    ]
                    .spacing(10),
                )
                .into()
        }
        Stage::Importing { target, .. } => column![
            folder,
            secondary(fill(&strings.import_target, &[("path", &target.display())])),
            text(&strings.import_working),
            progress_view::view(progress, language),
        ]
        .spacing(12)
        .into(),
        Stage::Done { done, analyse } => {
            let mut lines = column![
                secondary(fill(&strings.import_target, &[("path", &done.target_path)])),
                text(fill(
                    &strings.import_done,
                    &[
                        ("added", &done.tracks_added),
                        ("title", &done.playlist_title),
                        ("existing", &done.tracks_existing),
                        ("files", &done.files_copied),
                        ("missing", &done.files_missing),
                    ],
                )),
            ]
            .spacing(12);
            if !done.backup_path.is_empty() {
                lines = lines.push(secondary(fill(
                    &strings.import_done_backup,
                    &[("backup", &done.backup_path)],
                )));
            }
            // What is analysed is what the library has no analysis for, of the new tracks.
            let label = if *analyse && done.tracks_added > 0 {
                fill(
                    &strings.import_open_analyse,
                    &[("count", &done.tracks_added)],
                )
            } else {
                strings.import_open.clone()
            };
            lines
                .push(
                    row![
                        Space::new().width(Fill),
                        button(text(label))
                            .style(button::secondary)
                            .on_press(Message::ImportOpenLibrary),
                        close(),
                    ]
                    .spacing(10),
                )
                .into()
        }
    };

    let dialog = container(column![text(&strings.import_title).size(24), body].spacing(14))
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
