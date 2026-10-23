//! The window that repairs a library.

use crate::*;

pub(crate) fn handle(state: &mut State, message: Message) -> Task<Message> {
    match message {
        Message::RepairOpen => {
            if !state.locked() && !state.window_open() && !state.syncing() {
                state.repair = Some(RepairStage::Pick);
            }
        }
        Message::RepairAdd => {
            if matches!(state.repair, Some(RepairStage::Pick)) {
                let title = state
                    .config
                    .effective()
                    .language
                    .strings()
                    .repair_add_title
                    .clone();
                return Task::perform(
                    async move {
                        rfd::AsyncFileDialog::new()
                            .set_title(title)
                            .pick_folder()
                            .await
                            .map(|folder| folder.path().to_path_buf())
                    },
                    Message::RepairAdded,
                );
            }
        }
        Message::RepairAdded(Some(path)) => return handle(state, Message::RepairTarget(path)),
        Message::RepairAdded(None) => {}
        Message::RepairTarget(target) => {
            if matches!(state.repair, Some(RepairStage::Pick))
                && let Some(backend) = state.backend()
            {
                state.repair = Some(RepairStage::Working {
                    target: target.clone(),
                });
                let answered = target.clone();
                return Task::perform(backend::repair_library(backend, target), move |result| {
                    Message::RepairFinished {
                        target: answered.clone(),
                        result,
                    }
                });
            }
        }
        Message::RepairFinished { target, result } => {
            // (An answer for a window that is not waiting for it is not wanted.)
            if matches!(&state.repair, Some(RepairStage::Working { target: waiting }) if *waiting == target)
            {
                state.repair = Some(match result {
                    Ok(done) => RepairStage::Done(done),
                    Err(error) => RepairStage::Failed { target, error },
                });
            }
        }
        Message::RepairBack => {
            if state.repair.as_ref().is_some_and(RepairStage::can_close) {
                state.repair = Some(RepairStage::Pick);
            }
        }
        Message::RepairClose if state.repair.as_ref().is_some_and(RepairStage::can_close) => {
            state.repair = None;
        }
        _ => {}
    }
    Task::none()
}
