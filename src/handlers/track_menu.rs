//! The context menu of a track of the list: taking the track out of the playlist that is
//! shown, or out of the library. (Taking out the streaming songs, which works the same, is
//! with the playlists'.)

use crate::*;

pub(crate) fn handle(state: &mut State, message: Message) -> Task<Message> {
    match message {
        Message::TrackMenuOpen { row, at } => {
            // (While a track is analysed the menu opens, with what changes the library greyed out.)
            if state.locked() && !state.analysing() {
                return Task::none();
            }
            let LibraryState::Open(browser) = &mut state.library else {
                return Task::none();
            };
            let Some(&track) = browser.tracks().get(row) else {
                return Task::none();
            };
            if !browser.library.is_engine() {
                return Task::none();
            }
            // The row clicked is the one the menu is about.
            browser.selected = Some(row);
            state.playlist_menu = None;
            let track = &browser.library.tracks[track];
            // An ordinary playlist can lose the track; "All tracks" and a smart playlist (its
            // tracks come from its rules) can't.
            let playlist = match browser.source {
                Source::Playlist(index)
                    if browser
                        .library
                        .playlists
                        .get(index)
                        .is_some_and(|playlist| !playlist.is_smart) =>
                {
                    Some(index)
                }
                _ => None,
            };
            state.track_menu = Some(track_menu::Menu {
                row,
                track: track.id,
                title: if track.title.is_empty() {
                    track.path.clone()
                } else {
                    track.title.clone()
                },
                playlist,
                at,
                confirm_library: false,
                can_analyse: !track.streaming,
                analysed: browser.analysis(track) != 0,
                analyse_all: unanalysed(browser).len(),
            });
        }
        Message::TrackMenuClose => state.track_menu = None,
        Message::TrackMenuAnalyse => {
            let Some(menu) = state.track_menu.take() else {
                return Task::none();
            };
            if !menu.can_analyse || state.locked() {
                return Task::none();
            }
            let LibraryState::Open(browser) = &state.library else {
                return Task::none();
            };
            let library = browser.library.path.clone();
            return state.start_analysis(library, vec![(menu.track, menu.title)], menu.analysed);
        }
        Message::TrackMenuAnalyseAll => {
            state.track_menu = None;
            if state.locked() {
                return Task::none();
            }
            let LibraryState::Open(browser) = &state.library else {
                return Task::none();
            };
            let (library, tracks) = (browser.library.path.clone(), unanalysed(browser));
            return state.start_analysis(library, tracks, false);
        }
        Message::AnalysisQueueStop => {
            if let Some(queue) = &mut state.analysis_queue {
                queue.stop();
            }
            // The track that is being analysed is the last one.
            if let Some(toast::Toast::Analysing { stoppable, .. }) = &mut state.toast {
                *stoppable = false;
            }
        }
        Message::AnalysisFinished {
            library,
            track,
            title,
            result,
        } => {
            state.progress = None;
            let strings = state.config.effective().language.strings();
            // The answer is only for the toast that waits for it (another library may have
            // been opened since).
            let waiting = matches!(
                &state.toast,
                Some(toast::Toast::Analysing { library: waiting, .. }) if *waiting == library
            );
            if !waiting {
                return Task::none();
            }
            let error = result.as_ref().err().cloned();
            let finished = match result {
                Ok(done) => {
                    if let LibraryState::Open(browser) = &mut state.library
                        && browser.library.path == library
                    {
                        browser.set_analysed(
                            track,
                            browser::Analysed {
                                bits: done.analysis,
                                origin: model::AnalysisOrigin::OpenEngineLib,
                                note: done.analysis_note.clone(),
                                bpm: done.bpm,
                            },
                        );
                    }
                    let bpm = done.bpm.map_or(String::new(), |bpm| format!("{bpm:.1}"));
                    toast::Toast::Done {
                        message: match &done.key {
                            Some(key) => fill(
                                &strings.toast_analysed,
                                &[("title", &title), ("bpm", &bpm), ("key", key)],
                            ),
                            None => fill(
                                &strings.toast_analysed_no_key,
                                &[("title", &title), ("bpm", &bpm)],
                            ),
                        },
                        since: std::time::Instant::now(),
                    }
                }
                Err(error) => toast::Toast::Failed {
                    message: fill(
                        &strings.toast_analysis_failed,
                        &[("title", &title), ("error", &error)],
                    ),
                },
            };
            // The next track of the queue, if there is one; else how it all went.
            if let Some(queue) = &mut state.analysis_queue {
                queue.finished(&title, error.as_deref());
                if queue.has_next() {
                    return state.analyse_next();
                }
                let queue = state.analysis_queue.take().expect("the queue is there");
                let summary = queue.summary();
                state.toast = Some(if summary.total == 1 {
                    finished
                } else {
                    queue_summary(&summary, strings)
                });
            } else {
                state.toast = Some(finished);
            }
        }
        Message::ToastClose => {
            // (A running one can't be closed: the analysis can't be stopped.)
            if state.toast.as_ref().is_some_and(|toast| !toast.running()) {
                state.toast = None;
            }
        }
        Message::ToastTick => {
            if state
                .toast
                .as_ref()
                .is_some_and(|toast| toast.expired(std::time::Instant::now()))
            {
                state.toast = None;
            }
        }
        // The analysis writes to the library when it is done: nothing else changes it meanwhile.
        Message::TrackMenuRemoveFromPlaylist | Message::TrackMenuRemoveFromLibrary
            if state.analysing() => {}
        Message::TrackMenuRemoveFromPlaylist => {
            let (Some(menu), LibraryState::Open(browser)) = (&state.track_menu, &state.library)
            else {
                return Task::none();
            };
            let (Some(playlist), Some(backend)) = (menu.playlist, state.backend()) else {
                return Task::none();
            };
            let task = Task::perform(
                backend::remove_track_from_playlist(
                    backend,
                    Arc::clone(&browser.library),
                    playlist,
                    menu.track,
                ),
                Message::TrackRemovedFromPlaylist,
            );
            state.track_menu = None;
            return task;
        }
        Message::TrackRemovedFromPlaylist(result) => match result {
            Ok(edited) => {
                state.backend_error = None;
                state.playlists_changed(edited.playlists, Change::Same, None);
                // The row that was selected is another track now.
                if let LibraryState::Open(browser) = &mut state.library {
                    browser.selected = None;
                }
            }
            Err(error) => state.backend_error = Some(error),
        },
        Message::TrackMenuRemoveFromLibrary => {
            let Some(menu) = &mut state.track_menu else {
                return Task::none();
            };
            // Deleting a track takes a second click; the first one only asks.
            if !menu.confirm_library {
                menu.confirm_library = true;
                return Task::none();
            }
            let track = menu.track;
            let Some(backend) = state.backend() else {
                return Task::none();
            };
            state.track_menu = None;
            // Nothing else may change the library until it has been read again.
            state.reloading = true;
            return Task::perform(
                backend::remove_tracks(backend, vec![track]),
                Message::TracksRemoved,
            );
        }
        Message::TracksRemoved(result) => {
            return removal_finished(
                state,
                result.map(|done| (done.removed, done.playlist_entries)),
                |strings| &strings.track_deleted,
            );
        }
        _ => {}
    }
    Task::none()
}

/// What follows a removal of tracks from the library, which the backend did in the working
/// copy and then read again: tell what went (`message` is the text for the count of tracks
/// and of playlist entries) and fetch the library again, as the ids of the tracks after the
/// first one removed changed and the backend stopped the player. A removal that found nothing
/// to remove only unlocks the library, and a failure is shown.
pub(crate) fn removal_finished(
    state: &mut State,
    result: Result<(u32, u32), String>,
    message: fn(&i18n::Strings) -> &String,
) -> Task<Message> {
    match result {
        Ok((removed, entries)) if removed > 0 => {
            state.backend_error = None;
            let strings = state.config.effective().language.strings();
            state.reload_result = Some((
                fill(
                    message(strings),
                    &[("tracks", &removed), ("entries", &entries)],
                ),
                false,
            ));
            Task::batch([state.reload_library(), state.control(Control::Poll)])
        }
        Ok(_) => {
            state.reloading = false;
            Task::none()
        }
        Err(error) => {
            state.reloading = false;
            state.backend_error = Some(error);
            Task::none()
        }
    }
}

/// The tracks of the list that is shown that have no analysis: files, not streaming tracks.
fn unanalysed(browser: &Browser) -> Vec<analysis_queue::Entry> {
    browser
        .tracks()
        .iter()
        .map(|&index| &browser.library.tracks[index])
        .filter(|track| !track.streaming && browser.analysis(track) == 0)
        .map(|track| (track.id, track.title.clone()))
        .collect()
}

/// The toast that tells how the analysis of a list of tracks went.
fn queue_summary(summary: &analysis_queue::Summary, strings: &i18n::Strings) -> toast::Toast {
    match &summary.first_failure {
        None => toast::Toast::Done {
            message: fill(
                &strings.toast_queue_done,
                &[("done", &summary.done), ("total", &summary.total)],
            ),
            since: std::time::Instant::now(),
        },
        Some((title, error)) => toast::Toast::Failed {
            message: fill(
                &strings.toast_queue_failed,
                &[
                    ("failed", &summary.failed),
                    ("total", &summary.total),
                    ("title", title),
                    ("error", error),
                ],
            ),
        },
    }
}

impl State {
    /// Starts analysing `tracks` (their ids and titles) of the open library, which is at
    /// `library`, one after the other.
    pub(crate) fn start_analysis(
        &mut self,
        library: String,
        tracks: Vec<analysis_queue::Entry>,
        overwrite: bool,
    ) -> Task<Message> {
        if tracks.is_empty() || self.locked() {
            return Task::none();
        }
        self.analysis_queue = Some(analysis_queue::Queue::new(library, tracks, overwrite));
        self.analyse_next()
    }

    /// Asks the backend to analyse the next track of the queue. When there is none left (or
    /// the library is not the one open any more, whose track ids they would be) it is over.
    fn analyse_next(&mut self) -> Task<Message> {
        let backend = self.backend();
        let Some(queue) = &mut self.analysis_queue else {
            return Task::none();
        };
        let still_open = matches!(
            &self.library,
            LibraryState::Open(browser) if browser.library.path == queue.library
        );
        if !still_open || backend.is_none() {
            queue.stop();
        }
        let (Some((track, title)), Some(backend)) = (queue.next(), backend) else {
            let queue = self.analysis_queue.take().expect("the queue is there");
            let strings = self.config.effective().language.strings();
            self.toast = Some(queue_summary(&queue.summary(), strings));
            return Task::none();
        };
        let (library, overwrite) = (queue.library.clone(), queue.overwrite);
        let (started, total) = queue.position();
        self.toast = Some(toast::Toast::Analysing {
            library: library.clone(),
            title: title.clone(),
            position: (total > 1).then_some((started, total)),
            stoppable: queue.has_next(),
        });
        self.progress = None;
        Task::perform(
            backend::analyze_track(backend, track, overwrite),
            move |result| Message::AnalysisFinished {
                library: library.clone(),
                track,
                title: title.clone(),
                result,
            },
        )
    }
}
