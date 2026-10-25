//! The handlers of the messages (`handle` in `main.rs` routes them), one module per
//! feature.

pub(super) mod controller;
pub(super) mod cues;
pub(super) mod import;
pub(super) mod library;
pub(super) mod library_sync;
pub(super) mod loops;
pub(super) mod mpris;
pub(super) mod origin_prefix;
pub(super) mod playback;
pub(super) mod playlist_drag;
pub(super) mod playlists;
pub(super) mod repair;
pub(super) mod settings;
pub(super) mod sync;
pub(super) mod track_drag;
pub(super) mod track_menu;
