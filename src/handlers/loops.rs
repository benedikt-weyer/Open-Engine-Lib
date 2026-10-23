//! The saved loops and the loop editor.

use crate::*;

pub(crate) fn handle(state: &mut State, message: Message) -> Task<Message> {
    match message {
        Message::LoopJump(slot) => {
            let start = state
                .playing_id()
                .and_then(|id| state.loops.get(id))
                .and_then(|loops| loops.iter().find(|saved| saved.slot == slot))
                .map(|saved| saved.start);
            if let Some(start) = start {
                return handle(state, Message::SeekTo(start as f32));
            }
        }
        // The databases are being cloned: a loop set now might miss the clone. And one
        // change is written at a time.
        Message::LoopAdd(_) | Message::LoopRemove(_) | Message::LoopEditorSave
            if state.locked() || state.loops_busy => {}
        Message::LoopEditorOpen if state.locked() || state.window_open() => {}
        Message::LoopAdd(slot) => {
            if let Some(id) = state.playing_id()
                && let Some(loops) = state.loops.get(id)
                && slot < cues::SLOTS
                && loops.iter().all(|saved| saved.slot != slot)
            {
                let length = state.new_loop_length();
                let mut start = state.playhead();
                let mut end = start + length;
                // A loop can't reach past the end of the track: it is moved back to fit.
                if let Some(duration) = state.playing_duration()
                    && end > duration
                {
                    end = duration;
                    start = (duration - length).max(0.0);
                }
                if end > start {
                    let loops = loops::with(loops, Loop::new(slot, start, end));
                    return state.save_loops(id, loops);
                }
            }
        }
        Message::LoopRemove(slot) => {
            if let Some(id) = state.playing_id()
                && let Some(loops) = state.loops.get(id)
                && loops.iter().any(|saved| saved.slot == slot)
            {
                let loops = loops::without(loops, slot);
                return state.save_loops(id, loops);
            }
        }
        Message::LoopEditorOpen => {
            if let Some(id) = state.playing_id()
                && let Some(loops) = state.loops.get(id)
                && let LibraryState::Open(browser) = &state.library
                && let Some(track) = browser.library.track_by_id(id)
            {
                state.loop_editor = Some(loop_editor::Editor::new(
                    id,
                    &track.title,
                    loops,
                    state.new_loop_length(),
                ));
            }
        }
        Message::LoopEditor(event) => {
            let playhead = state.playhead();
            if let loop_editor::Event::Go(slot) = &event {
                let start = state
                    .loop_editor
                    .as_ref()
                    .and_then(|editor| editor.start_of(*slot));
                if let Some(start) = start {
                    return handle(state, Message::SeekTo(start as f32));
                }
            } else if let Some(editor) = state.loop_editor.as_mut().filter(|editor| !editor.busy) {
                editor.update(event, playhead);
            }
        }
        Message::LoopEditorSave => {
            let strings = state.config.effective().language.strings();
            let Some(editor) = state.loop_editor.as_mut().filter(|editor| !editor.busy) else {
                return Task::none();
            };
            match editor.loops() {
                Err(loop_editor::Problem::BadPosition { slot, text }) => {
                    editor.error = Some(fill(
                        &strings.loop_bad_position,
                        &[("number", &(slot + 1)), ("text", &text)],
                    ));
                }
                Err(loop_editor::Problem::BadRange { slot }) => {
                    editor.error = Some(fill(&strings.loop_bad_range, &[("number", &(slot + 1))]));
                }
                Ok(loops) => {
                    let track = editor.track;
                    editor.busy = true;
                    return state.save_loops(track, loops);
                }
            }
        }
        Message::LoopEditorCancel => {
            if state
                .loop_editor
                .as_ref()
                .is_some_and(|editor| !editor.busy)
            {
                state.loop_editor = None;
            }
        }
        Message::LoopsLoaded(generation, id, result) => match result {
            Ok(loops) => state.loops.set(generation, id, loops),
            Err(error) => {
                // Not asked again (that would be a request after every message), so there
                // are no pads for this track.
                state.backend_error = Some(error);
            }
        },
        Message::LoopsEdited {
            generation,
            track,
            result,
        } => {
            state.loops_busy = false;
            match result {
                Ok(loops) => {
                    state.backend_error = None;
                    // (An answer about another library's track means nothing here.)
                    if generation == state.loops.generation() {
                        if let LibraryState::Open(browser) = &mut state.library {
                            browser.set_loop_count(track, loops.len() as u32);
                        }
                        state.loops.set(generation, track, loops);
                    }
                    if state
                        .loop_editor
                        .as_ref()
                        .is_some_and(|editor| editor.track == track)
                    {
                        state.loop_editor = None;
                    }
                }
                Err(error) => match &mut state.loop_editor {
                    Some(editor) => {
                        let strings = state.config.effective().language.strings();
                        editor.busy = false;
                        editor.error = Some(fill(&strings.loop_failed, &[("error", &error)]));
                    }
                    None => state.backend_error = Some(error),
                },
            }
        }
        _ => {}
    }
    Task::none()
}
