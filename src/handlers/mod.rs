//! The handlers of the messages (`handle` in `main.rs` routes them), one module per
//! feature.

pub(super) mod cues;
pub(super) mod library;
pub(super) mod library_sync;
pub(super) mod loops;
pub(super) mod playback;
pub(super) mod playlists;
pub(super) mod settings;
pub(super) mod sync;
