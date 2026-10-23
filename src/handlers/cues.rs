//! The cue pads and the cue editor.

use crate::*;

pub(crate) fn handle(state: &mut State, message: Message) -> Task<Message> {
    match message {
        Message::CueJump(slot) => {
            let position = state
                .playing_id()
                .and_then(|id| state.cues.get(id))
                .and_then(|cues| cues.iter().find(|cue| cue.slot == slot))
                .map(|cue| cue.position);
            if let Some(position) = position {
                return handle(state, Message::SeekTo(position as f32));
            }
        }
        // The databases are being cloned: a cue set now might miss the clone. And one
        // change is written at a time.
        Message::CueAdd(_) | Message::CueRemove(_) | Message::CueEditorSave
            if state.locked() || state.cues_busy => {}
        Message::CueEditorOpen if state.locked() || state.window_open() => {}
        Message::CueAdd(slot) => {
            if let Some(id) = state.playing_id()
                && let Some(cues) = state.cues.get(id)
                && slot < cues::SLOTS
                && cues.iter().all(|cue| cue.slot != slot)
            {
                let cues = cues::with(cues, Cue::new(slot, state.playhead()));
                return state.save_cues(id, cues);
            }
        }
        Message::CueRemove(slot) => {
            if let Some(id) = state.playing_id()
                && let Some(cues) = state.cues.get(id)
                && cues.iter().any(|cue| cue.slot == slot)
            {
                let cues = cues::without(cues, slot);
                return state.save_cues(id, cues);
            }
        }
        Message::CueEditorOpen => {
            if let Some(id) = state.playing_id()
                && let Some(cues) = state.cues.get(id)
                && let LibraryState::Open(browser) = &state.library
                && let Some(track) = browser.library.track_by_id(id)
            {
                state.cue_editor = Some(cue_editor::Editor::new(id, &track.title, cues));
            }
        }
        Message::CueEditor(event) => {
            let playhead = state.playhead();
            if let cue_editor::Event::Go(slot) = &event {
                let position = state
                    .cue_editor
                    .as_ref()
                    .and_then(|editor| editor.position_of(*slot));
                if let Some(position) = position {
                    return handle(state, Message::SeekTo(position as f32));
                }
            } else if let Some(editor) = state.cue_editor.as_mut().filter(|editor| !editor.busy) {
                editor.update(event, playhead);
            }
        }
        Message::CueEditorSave => {
            let strings = state.config.effective().language.strings();
            let Some(editor) = state.cue_editor.as_mut().filter(|editor| !editor.busy) else {
                return Task::none();
            };
            match editor.cues() {
                Err(bad) => {
                    editor.error = Some(fill(
                        &strings.cue_bad_position,
                        &[("number", &(bad.slot + 1)), ("text", &bad.text)],
                    ));
                }
                Ok(cues) => {
                    let track = editor.track;
                    editor.busy = true;
                    return state.save_cues(track, cues);
                }
            }
        }
        Message::CueEditorCancel => {
            if state.cue_editor.as_ref().is_some_and(|editor| !editor.busy) {
                state.cue_editor = None;
            }
        }
        Message::CuesLoaded(generation, id, result) => match result {
            Ok(cues) => state.cues.set(generation, id, cues),
            Err(error) => {
                // Not asked again (that would be a request after every message), so there
                // are no pads for this track.
                state.backend_error = Some(error);
            }
        },
        Message::CuesEdited {
            generation,
            track,
            result,
        } => {
            state.cues_busy = false;
            match result {
                Ok(cues) => {
                    state.backend_error = None;
                    // (An answer about another library's track means nothing here.)
                    if generation == state.cues.generation() {
                        if let LibraryState::Open(browser) = &mut state.library {
                            browser.set_cue_count(track, cues.len() as u32);
                        }
                        state.cues.set(generation, track, cues);
                    }
                    if state
                        .cue_editor
                        .as_ref()
                        .is_some_and(|editor| editor.track == track)
                    {
                        state.cue_editor = None;
                    }
                }
                Err(error) => match &mut state.cue_editor {
                    Some(editor) => {
                        let strings = state.config.effective().language.strings();
                        editor.busy = false;
                        editor.error = Some(fill(&strings.cue_failed, &[("error", &error)]));
                    }
                    None => state.backend_error = Some(error),
                },
            }
        }
        _ => {}
    }
    Task::none()
}
