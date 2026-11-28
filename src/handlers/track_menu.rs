//! The context menu of a track of the list: taking the track out of the playlist that is
//! shown, or out of the library. (Taking out the streaming songs, which works the same, is
//! with the playlists'.)

use crate::*;

pub(crate) fn handle(state: &mut State, message: Message) -> Task<Message> {
    match message {
        Message::TrackMenuOpen { row, at } => {
            // (Jobs that run don't stop the menu: they find their track again when they end.)
            if state.locked() {
                return Task::none();
            }
            let has_stems_key = state.config.effective().stems_key().is_some();
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
            let queue = state.analysis_queue.as_ref();
            let track = &browser.library.tracks[track];
            let reference = analysis_queue::TrackRef::of(track);
            let queued = |kind| queue.is_some_and(|queue| queue.contains(kind, &reference));
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
                queued: queued(analysis_queue::Kind::Analyse),
                analyse_all: unanalysed(browser, queue).len(),
                can_make_stems: !track.streaming,
                has_stems: browser.has_stems(track),
                has_stems_key,
                stems_queued: queued(analysis_queue::Kind::Stems),
            });
        }
        Message::TrackMenuClose => state.track_menu = None,
        Message::TrackMenuAnalyse | Message::TrackMenuStems => {
            let Some(menu) = state.track_menu.take() else {
                return Task::none();
            };
            let LibraryState::Open(browser) = &state.library else {
                return Task::none();
            };
            let Some(track) = browser.library.track_by_id(menu.track) else {
                return Task::none();
            };
            let entry = match message {
                Message::TrackMenuAnalyse if menu.can_analyse && !menu.queued => {
                    analysis_queue::Entry::analyse(track, menu.analysed)
                }
                Message::TrackMenuStems
                    if menu.can_make_stems && menu.has_stems_key && !menu.stems_queued =>
                {
                    analysis_queue::Entry::new(analysis_queue::Kind::Stems, track, menu.has_stems)
                }
                _ => return Task::none(),
            };
            let library = browser.library.path.clone();
            return state.start_jobs(library, vec![entry]);
        }
        Message::TrackMenuAnalyseAll => {
            state.track_menu = None;
            let LibraryState::Open(browser) = &state.library else {
                return Task::none();
            };
            let (library, entries) = (
                browser.library.path.clone(),
                unanalysed(browser, state.analysis_queue.as_ref()),
            );
            return state.start_jobs(library, entries);
        }
        Message::AnalysisQueueStop => {
            if let Some(queue) = &mut state.analysis_queue {
                queue.stop();
            }
            state.update_job_toast();
        }
        Message::JobRemove(index) => {
            if let Some(queue) = &mut state.analysis_queue {
                queue.remove(index);
            }
            state.update_job_toast();
        }
        Message::JobQueueToggle => state.queue_shown = !state.queue_shown,
        Message::StopJobsToggled(stop) => state.stop_jobs_tick = stop,
        Message::JobFinished {
            library,
            entry,
            result,
        } => {
            // (Only for the job that runs: one that was stopped is not waited for.)
            let running = state
                .analysis_queue
                .as_ref()
                .is_some_and(|queue| queue.library == library && queue.running() == Some(&entry));
            if !running {
                return Task::none();
            }
            state.job_progress = None;
            state.job_abort = None;
            let strings = state.config.effective().language.strings();
            let title = &entry.title;
            // The track as the library has it now (it may have been read again meanwhile).
            if let Ok(done) = &result
                && let Some(browser) = state.browser_at_mut(&library)
                && let Some(id) = entry.track.find(&browser.library).map(|track| track.id)
            {
                match done {
                    JobDone::Analysed(done) => browser.set_analysed(
                        id,
                        browser::Analysed {
                            bits: done.analysis,
                            origin: model::AnalysisOrigin::OpenEngineLib,
                            note: done.analysis_note.clone(),
                            bpm: done.bpm,
                        },
                    ),
                    JobDone::Stems => browser.set_has_stems(id),
                }
            }
            let error = result.as_ref().err().cloned();
            let finished = match (&result, entry.kind) {
                (Ok(JobDone::Analysed(done)), _) => {
                    let bpm = done.bpm.map_or(String::new(), |bpm| format!("{bpm:.1}"));
                    toast::Toast::Done {
                        message: match &done.key {
                            Some(key) => fill(
                                &strings.toast_analysed,
                                &[("title", title), ("bpm", &bpm), ("key", key)],
                            ),
                            None => fill(
                                &strings.toast_analysed_no_key,
                                &[("title", title), ("bpm", &bpm)],
                            ),
                        },
                        since: std::time::Instant::now(),
                    }
                }
                (Ok(JobDone::Stems), _) => toast::Toast::Done {
                    message: fill(&strings.toast_stems_done, &[("title", title)]),
                    since: std::time::Instant::now(),
                },
                (Err(error), kind) => toast::Toast::Failed {
                    message: fill(
                        match kind {
                            analysis_queue::Kind::Analyse => &strings.toast_analysis_failed,
                            analysis_queue::Kind::Stems => &strings.toast_stems_failed,
                        },
                        &[("title", title), ("error", error)],
                    ),
                },
            };
            let queue = state.analysis_queue.as_mut().expect("the job ran from it");
            queue.finished(error.as_deref());
            if queue.has_next() {
                return state.run_next_job();
            }
            let queue = state.analysis_queue.take().expect("the queue is there");
            let summary = queue.summary();
            state.toast = Some(if summary.total == 1 {
                finished
            } else {
                queue_summary(&summary, strings)
            });
        }
        Message::ToastClose => {
            // (A running one can't be closed: it shows the queue.)
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
fn unanalysed(
    browser: &Browser,
    queue: Option<&analysis_queue::Queue>,
) -> Vec<analysis_queue::Entry> {
    browser
        .tracks()
        .iter()
        .map(|&index| &browser.library.tracks[index])
        .filter(|track| {
            !track.streaming
                && browser.analysis(track) == 0
                && !queue.is_some_and(|queue| {
                    queue.contains(
                        analysis_queue::Kind::Analyse,
                        &analysis_queue::TrackRef::of(track),
                    )
                })
        })
        .map(|track| analysis_queue::Entry::analyse(track, false))
        .collect()
}

/// The toast that tells how the jobs of a queue went.
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
    /// Puts `entries` (jobs for tracks of the library at `library`) in the queue and starts
    /// it if nothing runs; if jobs of this library run already, they join the end of the queue,
    /// except those that are in it. (Jobs of another library wait for that queue to be over.)
    pub(crate) fn start_jobs(
        &mut self,
        library: String,
        entries: Vec<analysis_queue::Entry>,
    ) -> Task<Message> {
        if entries.is_empty() {
            return Task::none();
        }
        if let Some(queue) = &mut self.analysis_queue {
            if queue.library == library {
                queue.add(entries);
                self.update_job_toast();
            }
            return Task::none();
        }
        self.analysis_queue = Some(analysis_queue::Queue::new(library, entries));
        self.run_next_job()
    }

    /// Asks the backend to run the next job of the queue. A track that is no longer in its
    /// library is left out (and said so); when there is no job left, or the library was closed,
    /// it is over and the toast says how it went.
    pub(crate) fn run_next_job(&mut self) -> Task<Message> {
        loop {
            let Some(path) = self
                .analysis_queue
                .as_ref()
                .map(|queue| queue.library.clone())
            else {
                return Task::none();
            };
            let library = self
                .browsers()
                .find(|browser| browser.library.path == path)
                .map(|browser| Arc::clone(&browser.library));
            let backend = library
                .as_ref()
                .and_then(|library| self.backend_about(library.id));
            let key = self.config.effective().stems_key();
            let strings = self.config.effective().language.strings();
            let queue = self.analysis_queue.as_mut().expect("there is a queue");
            let (Some(library), Some(backend)) = (library, backend) else {
                queue.stop();
                let queue = self.analysis_queue.take().expect("the queue is there");
                self.toast = Some(queue_summary(&queue.summary(), strings));
                return Task::none();
            };
            let Some(entry) = queue.next() else {
                let queue = self.analysis_queue.take().expect("the queue is there");
                self.toast = Some(queue_summary(&queue.summary(), strings));
                return Task::none();
            };
            // The track as the library has it now: it may have gone since it was queued.
            let Some(track) = entry.track.find(&library) else {
                queue.finished(Some(&strings.job_track_gone));
                continue;
            };
            let id = track.id;
            let job = match entry.kind {
                analysis_queue::Kind::Analyse => {
                    let overwrite = entry.overwrite;
                    Task::perform(
                        async move {
                            backend::analyze_track(backend, id, overwrite)
                                .await
                                .map(JobDone::Analysed)
                        },
                        std::convert::identity,
                    )
                }
                analysis_queue::Kind::Stems => {
                    let Some(key) = key else {
                        queue.finished(Some(&strings.stems_no_key_hint));
                        continue;
                    };
                    let overwrite = entry.overwrite;
                    Task::perform(
                        async move {
                            backend::generate_stems(backend, id, key.to_vec(), overwrite)
                                .await
                                .map(|()| JobDone::Stems)
                        },
                        std::convert::identity,
                    )
                }
            };
            let library = path;
            let finished = entry.clone();
            let (task, handle) = job
                .map(move |result| Message::JobFinished {
                    library: library.clone(),
                    entry: finished.clone(),
                    result,
                })
                .abortable();
            self.job_abort = Some(handle);
            self.job_progress = None;
            self.update_job_toast();
            return task;
        }
    }

    /// Shows the job that runs, which of how many it is and whether more wait.
    pub(crate) fn update_job_toast(&mut self) {
        let Some(queue) = &self.analysis_queue else {
            return;
        };
        let Some(running) = queue.running() else {
            return;
        };
        let (started, total) = queue.position();
        self.toast = Some(toast::Toast::Job {
            kind: running.kind,
            title: running.title.clone(),
            position: (total > 1).then_some((started, total)),
            stoppable: queue.has_next(),
        });
    }

    /// Stops the jobs of the library at `library`: the one that runs is not waited for (the
    /// backend is told by dropping the call, and writes nothing) and the rest are dropped.
    pub(crate) fn stop_jobs(&mut self, library: &str) {
        if !self
            .analysis_queue
            .as_ref()
            .is_some_and(|queue| queue.library == library)
        {
            return;
        }
        if let Some(handle) = self.job_abort.take() {
            handle.abort();
        }
        let mut queue = self.analysis_queue.take().expect("the queue is there");
        queue.stop_all();
        self.job_progress = None;
        let strings = self.config.effective().language.strings();
        self.toast = Some(toast::Toast::Done {
            message: fill(
                &strings.toast_jobs_stopped,
                &[("done", &queue.summary().done)],
            ),
            since: std::time::Instant::now(),
        });
    }

    /// How many jobs of the library at `library` wait or run.
    pub(crate) fn pending_jobs(&self, library: &str) -> usize {
        self.analysis_queue
            .as_ref()
            .filter(|queue| queue.library == library)
            .map_or(0, analysis_queue::Queue::pending)
    }
}
