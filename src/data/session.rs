//! What was open when the app was last used: which library, the playlist, the song
//! and how far into it, so the next start can pick up there.
//!
//! This is state, not configuration (whether to restore it is a setting), so it lives
//! in its own file in the user state dir (`<state dir>/open-engine-lib/session.toml`).

use std::fs;
use std::io;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

const APP_DIR: &str = "open-engine-lib";
const FILE: &str = "session.toml";
/// Playback positions are saved while a song plays, but only when they have moved on
/// by this much, so the file isn't rewritten several times a second.
const POSITION_STEP_MS: u64 = 5_000;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Session {
    /// The library that was open. `None` if none was, or it was closed on purpose.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub library: Option<PathBuf>,
    /// `library` is a plain folder of music, not an Engine library.
    pub folder: bool,
    /// Titles from the top-level playlist down to the one that was shown. Empty for
    /// the list of all tracks. Titles are unique within their parent, so this is
    /// enough to find the playlist again even if the library was edited.
    pub playlist: Vec<String>,
    /// The selected or playing song, by its path in the library.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub track: Option<String>,
    /// Whether `track` was loaded in the player (playing or paused) rather than just
    /// selected.
    pub loaded: bool,
    /// How far into `track` the player was, in milliseconds. 0 unless `loaded`.
    pub position_ms: u64,
}

impl Session {
    /// Whether `other` is not worth writing over `self`: it only differs in a playback
    /// position that moved by less than a few seconds.
    fn is_about_the_same_as(&self, other: &Session) -> bool {
        self.position_ms.abs_diff(other.position_ms) < POSITION_STEP_MS
            && Session {
                position_ms: self.position_ms,
                ..other.clone()
            } == *self
    }
}

pub struct SessionStore {
    file: PathBuf,
    /// What is in the file.
    saved: Session,
}

impl SessionStore {
    pub fn discover() -> Self {
        let file = dirs::state_dir()
            .or_else(dirs::data_local_dir)
            .unwrap_or_default()
            .join(APP_DIR)
            .join(FILE);
        Self::load(file)
    }

    /// Reads the file. A missing or unreadable one is an empty session: it is only a
    /// convenience, so it must never stop the app from starting.
    pub fn load(file: PathBuf) -> Self {
        let saved = fs::read_to_string(&file)
            .ok()
            .and_then(|text| toml::from_str(&text).ok())
            .unwrap_or_default();
        Self { file, saved }
    }

    pub fn session(&self) -> &Session {
        &self.saved
    }

    /// Writes `session` unless the file already holds more or less the same. A `force`d
    /// save (when the app closes) also writes a position that only moved a little.
    pub fn save(&mut self, session: &Session, force: bool) -> io::Result<()> {
        let unchanged = if force {
            self.saved == *session
        } else {
            self.saved.is_about_the_same_as(session)
        };
        if unchanged {
            return Ok(());
        }

        if let Some(parent) = self.file.parent() {
            fs::create_dir_all(parent)?;
        }
        let text = toml::to_string(session).map_err(io::Error::other)?;
        // Write beside the file and move it into place, so a crash can't leave half of it.
        let temporary = self.file.with_extension("toml.tmp");
        fs::write(&temporary, text)?;
        fs::rename(&temporary, &self.file)?;
        self.saved = session.clone();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TempFile(PathBuf);

    impl TempFile {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "open-engine-lib-session-{name}-{}",
                std::process::id()
            ));
            let _ = fs::remove_dir_all(&dir);
            Self(dir.join("state").join(FILE))
        }
    }

    impl Drop for TempFile {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(self.0.parent().unwrap().parent().unwrap());
        }
    }

    fn playing() -> Session {
        Session {
            library: Some("/media/DRIVE/Engine Library".into()),
            folder: false,
            playlist: vec!["House".into(), "Deep".into()],
            track: Some("../House/a.flac".into()),
            loaded: true,
            position_ms: 61_250,
        }
    }

    #[test]
    fn is_remembered_between_runs() {
        let file = TempFile::new("roundtrip");
        SessionStore::load(file.0.clone())
            .save(&playing(), true)
            .unwrap();
        assert_eq!(SessionStore::load(file.0.clone()).session(), &playing());
    }

    #[test]
    fn an_empty_session_round_trips_and_a_bad_file_is_empty() {
        let file = TempFile::new("empty");
        assert_eq!(
            SessionStore::load(file.0.clone()).session(),
            &Session::default()
        );

        let mut store = SessionStore::load(file.0.clone());
        store.save(&playing(), true).unwrap();
        store.save(&Session::default(), true).unwrap();
        assert_eq!(
            SessionStore::load(file.0.clone()).session(),
            &Session::default()
        );

        fs::write(&file.0, "library = [[[ not toml").unwrap();
        assert_eq!(
            SessionStore::load(file.0.clone()).session(),
            &Session::default()
        );
    }

    #[test]
    fn a_moving_position_is_not_written_every_time() {
        let file = TempFile::new("throttle");
        let mut store = SessionStore::load(file.0.clone());
        store.save(&playing(), false).unwrap();

        let mut later = playing();
        later.position_ms += 1_000;
        store.save(&later, false).unwrap();
        assert_eq!(
            store.session().position_ms,
            61_250,
            "too little has changed"
        );

        later.position_ms += POSITION_STEP_MS;
        store.save(&later, false).unwrap();
        assert_eq!(store.session().position_ms, later.position_ms);

        // Anything but the position is written at once, and so is any change when forced.
        let mut other = later.clone();
        other.track = Some("../House/b.flac".into());
        store.save(&other, false).unwrap();
        assert_eq!(store.session().track.as_deref(), Some("../House/b.flac"));

        other.position_ms += 1_000;
        store.save(&other, true).unwrap();
        assert_eq!(store.session().position_ms, other.position_ms);
    }
}
