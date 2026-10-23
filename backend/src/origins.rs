//! Open Engine Lib's own database of where tracks came from: the location of their file,
//! as saved from a library that had it. Each library has one, `open-engine-lib.db` in
//! its `Database2` folder next to Engine's databases. The working copy has its own like
//! the others, so the origins are synced to the source with them (see `sync`), and to
//! another library when its playlists are synced (see `mirror`). Tracks are identified
//! the way Engine identifies where a track came from, by `(originDatabaseUuid,
//! originTrackId)`, so a track that was copied to another library has the same origin
//! there.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use rusqlite::Connection;

/// The file name, in the `Database2` folder of a library.
pub const FILE: &str = "open-engine-lib.db";

/// Identifies a track across libraries: Engine's `(originDatabaseUuid, originTrackId)`.
pub type TrackKey = (String, i64);

/// Where earlier versions kept the origins of all libraries: in `$OPEN_ENGINE_LIB_DATA`,
/// else the user data dir. It is only read now, to take over what it has.
pub fn legacy_path() -> PathBuf {
    // (The tests must not take over what is on the machine they run on.)
    if cfg!(test) {
        return std::env::temp_dir()
            .join("open-engine-lib-no-legacy")
            .join(FILE);
    }
    std::env::var_os("OPEN_ENGINE_LIB_DATA")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            dirs::data_dir()
                .unwrap_or_else(std::env::temp_dir)
                .join("open-engine-lib")
        })
        .join(FILE)
}

pub struct Origins {
    connection: Mutex<Connection>,
}

impl Origins {
    /// Opens the database at `path`, creating it (and its folder) if it is not there.
    pub fn open(path: &Path) -> Result<Self, String> {
        if let Some(folder) = path.parent() {
            std::fs::create_dir_all(folder)
                .map_err(|error| format!("could not create {}: {error}", folder.display()))?;
        }
        let connection = Connection::open(path)
            .map_err(|error| format!("could not open {}: {error}", path.display()))?;
        // Set up once: opening a database that is set up leaves the file as it is, which
        // matters because syncing compares the files.
        let version: i64 = connection
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .map_err(|error| format!("could not read {}: {error}", path.display()))?;
        if version < 1 {
            connection
                .execute_batch(
                    "CREATE TABLE IF NOT EXISTS Origin (
                        databaseUuid TEXT NOT NULL,
                        trackId INTEGER NOT NULL,
                        origin TEXT NOT NULL,
                        savedAt TEXT NOT NULL DEFAULT (datetime('now')),
                        PRIMARY KEY (databaseUuid, trackId)
                     );
                     PRAGMA user_version = 1;",
                )
                .map_err(|error| format!("could not set up {}: {error}", path.display()))?;
        }
        Ok(Self {
            connection: Mutex::new(connection),
        })
    }

    /// Opens the database at `path` if there is one, else `None`.
    pub fn open_existing(path: &Path) -> Result<Option<Self>, String> {
        if path.is_file() {
            Self::open(path).map(Some)
        } else {
            Ok(None)
        }
    }

    /// Every origin that is saved.
    pub fn all(&self) -> rusqlite::Result<HashMap<TrackKey, String>> {
        let connection = self.connection.lock().expect("origins lock");
        let mut statement =
            connection.prepare("SELECT databaseUuid, trackId, origin FROM Origin")?;
        statement
            .query_map([], |row| Ok(((row.get(0)?, row.get(1)?), row.get(2)?)))?
            .collect()
    }

    /// Saves these origins, replacing the ones the tracks have. Returns how many were new
    /// or different.
    pub fn replace(&self, origins: &[(TrackKey, String)]) -> rusqlite::Result<usize> {
        let mut connection = self.connection.lock().expect("origins lock");
        let transaction = connection.transaction()?;
        let mut changed = 0;
        {
            let mut statement = transaction.prepare(
                "INSERT INTO Origin (databaseUuid, trackId, origin) VALUES (?1, ?2, ?3) \
                 ON CONFLICT (databaseUuid, trackId) DO UPDATE SET origin = excluded.origin, \
                    savedAt = datetime('now') WHERE origin IS NOT excluded.origin",
            )?;
            for ((uuid, id), origin) in origins {
                changed += statement.execute((uuid, id, origin))?;
            }
        }
        transaction.commit()?;
        Ok(changed)
    }

    /// Saves the origins of the tracks that have none yet; an origin that is saved is
    /// kept. Returns how many were new.
    pub fn save_missing(&self, origins: &[(TrackKey, String)]) -> rusqlite::Result<usize> {
        let mut connection = self.connection.lock().expect("origins lock");
        let transaction = connection.transaction()?;
        let mut saved = 0;
        {
            let mut statement = transaction.prepare(
                "INSERT OR IGNORE INTO Origin (databaseUuid, trackId, origin) VALUES (?1, ?2, ?3)",
            )?;
            for ((uuid, id), origin) in origins {
                saved += statement.execute((uuid, id, origin))?;
            }
        }
        transaction.commit()?;
        Ok(saved)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(uuid: &str, id: i64) -> TrackKey {
        (uuid.to_string(), id)
    }

    #[test]
    fn saves_origins_and_keeps_the_ones_it_has() {
        let folder =
            std::env::temp_dir().join(format!("open-engine-lib-origins-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&folder);
        let path = folder.join("nested").join(FILE);

        let origins = Origins::open(&path).unwrap();
        assert!(origins.all().unwrap().is_empty());
        let saved = origins
            .save_missing(&[
                (key("a", 1), "/music/one.flac".into()),
                (key("a", 2), "/music/two.flac".into()),
            ])
            .unwrap();
        assert_eq!(saved, 2);

        // A second run saves what is new and leaves the rest as it was.
        let saved = origins
            .save_missing(&[
                (key("a", 1), "/stick/one.flac".into()),
                (key("b", 1), "/music/three.flac".into()),
            ])
            .unwrap();
        assert_eq!(saved, 1);

        // Replacing changes what differs and only that.
        let changed = origins
            .replace(&[
                (key("a", 1), "/stick/one.flac".into()),
                (key("a", 2), "/music/two.flac".into()),
            ])
            .unwrap();
        assert_eq!(changed, 1);
        assert_eq!(origins.all().unwrap()[&key("a", 1)], "/stick/one.flac");
        origins
            .replace(&[(key("a", 1), "/music/one.flac".into())])
            .unwrap();
        assert!(
            Origins::open_existing(&folder.join("none.db"))
                .unwrap()
                .is_none()
        );
        drop(origins);

        // Opening it again does not touch the file.
        let before = std::fs::read(&path).unwrap();
        drop(Origins::open(&path).unwrap());
        assert_eq!(std::fs::read(&path).unwrap(), before);

        // And it is still there when opened again.
        let all = Origins::open(&path).unwrap().all().unwrap();
        assert_eq!(all.len(), 3);
        assert_eq!(all[&key("a", 1)], "/music/one.flac");
        assert_eq!(all[&key("b", 1)], "/music/three.flac");
        let _ = std::fs::remove_dir_all(&folder);
    }
}
