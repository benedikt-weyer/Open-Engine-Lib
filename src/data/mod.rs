//! What the UI knows, remembers and asks of the backend: the backend client, the configuration, the
//! model of the open library and the rest of the logic that has nothing to do with drawing.
//!
//! `main.rs` imports these modules, so the rest of the app refers to them as `crate::config`,
//! `crate::model` and so on.

pub(crate) mod backend;
pub(crate) mod clock;
pub(crate) mod colors;
pub(crate) mod columns;
pub(crate) mod config;
pub(crate) mod covers;
pub(crate) mod cues;
pub(crate) mod drives;
pub(crate) mod i18n;
pub(crate) mod local_folders;
pub(crate) mod loops;
pub(crate) mod model;
pub(crate) mod reachable;
pub(crate) mod recent;
pub(crate) mod session;
pub(crate) mod waveform;
