//! The widgets and windows: what is drawn and the messages it sends.
//!
//! `main.rs` imports these modules, so the rest of the app refers to them as `crate::deck`,
//! `crate::sidebar` and so on.

pub(crate) mod browser;
pub(crate) mod cue_bar;
pub(crate) mod cue_editor;
pub(crate) mod deck;
pub(crate) mod error_text;
pub(crate) mod library_sync_view;
pub(crate) mod loop_bar;
pub(crate) mod loop_editor;
pub(crate) mod menubar;
pub(crate) mod navbar;
pub(crate) mod player_bar;
pub(crate) mod playlist_dialog;
pub(crate) mod progress_view;
pub(crate) mod settings;
pub(crate) mod sidebar;
pub(crate) mod smart_editor;
pub(crate) mod sync_view;
