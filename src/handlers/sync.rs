//! The window that syncs the working copy with the library on the drive.

use crate::*;

pub(crate) fn handle(state: &mut State, message: Message) -> Task<Message> {
    match message {
        Message::SyncOpen => {
            let busy = state.locked()
                || matches!(state.sync, Some(SyncStage::Syncing { .. }))
                || state.library_sync.is_some()
                || state.smart.is_some()
                || state.playlist_dialog.is_some()
                || state.cue_editor.is_some()
                || state.loop_editor.is_some();
            // (The button starts in the usual direction; "compare again" keeps it.)
            if !busy && state.sync.is_none() {
                state.sync_reverse = false;
            }
            if !busy
                && matches!(state.library, LibraryState::Open(_))
                && let Some(backend) = state.backend()
            {
                state.sync = Some(SyncStage::Loading);
                return Task::perform(
                    backend::sync_diff(backend, state.sync_reverse),
                    Message::SyncDiffLoaded,
                );
            }
        }
        Message::SyncDiffLoaded(result) => {
            // Only if the window is still waiting for it.
            if matches!(state.sync, Some(SyncStage::Loading)) {
                state.sync = Some(match result {
                    Ok(diff) => SyncStage::Ready {
                        diff,
                        acknowledged: false,
                        error: None,
                    },
                    Err(error) => SyncStage::Failed(error),
                });
            }
        }
        Message::SyncAcknowledged(ticked) => {
            if let Some(SyncStage::Ready { acknowledged, .. }) = &mut state.sync {
                *acknowledged = ticked;
            }
        }
        Message::SyncDirection(reverse) => {
            if matches!(
                state.sync,
                Some(SyncStage::Ready { .. } | SyncStage::Failed(_))
            ) {
                state.sync_reverse = reverse;
                return handle(state, Message::SyncOpen);
            }
        }
        Message::SyncStart => {
            let reverse = state.sync_reverse;
            if let (
                Some(SyncStage::Ready {
                    diff, acknowledged, ..
                }),
                Some(backend),
            ) = (&state.sync, state.backend())
                && if reverse {
                    sync_view::can_pull(diff)
                } else {
                    sync_view::can_sync(diff, *acknowledged)
                }
            {
                // Overwriting what was changed outside is allowed only if it was ticked.
                let overwrite = diff.source_changed_outside;
                state.progress = None;
                state.sync = Some(SyncStage::Syncing {
                    diff: Arc::clone(diff),
                    acknowledged: *acknowledged,
                });
                return if reverse {
                    Task::perform(backend::pull_from_source(backend), Message::SyncPulled)
                } else {
                    Task::perform(
                        backend::sync_to_source(backend, overwrite),
                        Message::SyncFinished,
                    )
                };
            }
        }
        Message::SyncPulled(result) => {
            if let Some(SyncStage::Syncing { diff, acknowledged }) = &state.sync {
                match result {
                    Ok(done) => {
                        state.sync = Some(SyncStage::Pulled(done));
                        // The backend reads the new working copy; so must this.
                        return state.reload_library();
                    }
                    // Back to what was shown, with the reason.
                    Err(error) => {
                        state.sync = Some(SyncStage::Ready {
                            diff: Arc::clone(diff),
                            acknowledged: *acknowledged,
                            error: Some(error),
                        });
                    }
                }
            }
        }
        Message::LibraryReloaded(result) => {
            state.reloading = false;
            match result {
                Ok(library) => {
                    // What was open stays open: the list, the song and where it is in the list,
                    // but nothing is started.
                    let session = state.snapshot().map(|session| Session {
                        loaded: false,
                        ..session
                    });
                    state.covers.clear();
                    state.reachable.clear();
                    // Pulling from the source can have replaced the cues.
                    state.cues.clear();
                    state.loops.clear();
                    state.library = LibraryState::Open(Browser::new(library));
                    if let Some(session) = session {
                        return state.restore_session(session);
                    }
                }
                Err(error) => state.backend_error = Some(error),
            }
        }
        Message::SyncFinished(result) => {
            if let Some(SyncStage::Syncing { diff, acknowledged }) = &state.sync {
                state.sync = Some(match result {
                    Ok(done) => SyncStage::Done(done),
                    // Back to what was shown, with the reason.
                    Err(error) => SyncStage::Ready {
                        diff: Arc::clone(diff),
                        acknowledged: *acknowledged,
                        error: Some(error),
                    },
                });
            }
        }
        Message::SyncClose if state.sync.as_ref().is_some_and(SyncStage::can_close) => {
            state.sync = None;
        }
        _ => {}
    }
    Task::none()
}
