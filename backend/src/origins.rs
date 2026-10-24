//! Open Engine Lib's own database of where tracks came from: the location of their file,
//! as saved from a library that had it. Each library has one, `open-engine-lib.db` in an
//! `open-engine` folder beside its `Engine Library` folder (not in it: a player has no use
//! for it). Earlier versions kept it in `Database2`; that is still read when the new place
//! has none, and it is taken away when the file is written to the new place. The working
//! copy has its own in its `Database2` folder, so the origins are synced to the source with
//! the other databases (see `sync`), and to
//! another library when its playlists are synced (see `mirror`). Tracks are identified
//! the way Engine identifies where a track came from, by `(originDatabaseUuid,
//! originTrackId)`, so a track that was copied to another library has the same origin
//! there.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use rusqlite::Connection;

/// The file name, in the `open-engine` folder beside a library's `Engine Library` folder (and
/// in the `Database2` folder of the working copy).
pub const FILE: &str = "open-engine-lib.db";
/// The folder beside the `Engine Library` folder that Open Engine Lib keeps its files in.
pub const FOLDER: &str = "open-engine";

/// Where the origins of the library at `library_dir` (its `Engine Library` folder) are kept.
pub fn source_file(library_dir: &Path) -> PathBuf {
    library_dir
        .parent()
        .unwrap_or(library_dir)
        .join(FOLDER)
        .join(FILE)
}

/// Where earlier versions kept them: in the library's `Database2` folder.
pub fn old_source_file(library_dir: &Path) -> PathBuf {
    library_dir.join(crate::cache::DATABASE_DIR).join(FILE)
}

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

/// The folder that all of these paths are in: the longest run of leading folders they
/// have in common (whole folders, never half a name), without a trailing slash. `/` if the
/// only thing they share is the root, and empty if they share nothing (or there are none).
/// The last part of a path is its file, so it is never part of the prefix.
pub fn common_prefix<'a>(paths: impl IntoIterator<Item = &'a str>) -> String {
    let mut common: Option<Vec<&str>> = None;
    for path in paths {
        let mut folders: Vec<&str> = path.split('/').collect();
        folders.pop();
        common = Some(match common {
            None => folders,
            Some(common) => common
                .into_iter()
                .zip(folders)
                .take_while(|(a, b)| a == b)
                .map(|(a, _)| a)
                .collect(),
        });
    }
    match common.as_deref() {
        None | Some([]) => String::new(),
        // (Only the root: the folders of `/a` are `[""]`.)
        Some([""]) => "/".into(),
        Some(folders) => folders.join("/"),
    }
}

/// `path` with its leading folders `from` replaced by `to`, or `None` if it is not in
/// `from` (a folder is only in `from` as a whole: `/music2/a` is not in `/music`). A
/// trailing slash on either is ignored; `/` is the root, which every absolute path is in.
pub fn replace_prefix(path: &str, from: &str, to: &str) -> Option<String> {
    let (from, to) = (from.trim_end_matches('/'), to.trim_end_matches('/'));
    let rest = path.strip_prefix(from)?;
    (rest.is_empty() || rest.starts_with('/') || from.is_empty())
        .then(|| format!("{to}{rest}"))
        .filter(|_| !from.is_empty() || path.starts_with('/'))
}

/// A track that Open Engine Lib analysed (see `track_analysis`).
#[derive(Debug, Clone, PartialEq)]
pub struct AnalysisRecord {
    /// What made the analysis.
    pub tool: String,
    /// When, as `datetime('now')` writes it (UTC).
    pub analysed_at: String,
    /// `track_analysis::fingerprint` of the `beatData` that was written. If the track has
    /// another now (Engine analysed it again), it is no longer ours.
    pub beat_data: i64,
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

    /// The tracks that Open Engine Lib analysed. The table is made when the first one is
    /// recorded (so that a database that is only read is never changed), so a database
    /// without it has none.
    pub fn analyses(&self) -> HashMap<TrackKey, AnalysisRecord> {
        let connection = self.connection.lock().expect("origins lock");
        let Ok(mut statement) = connection
            .prepare("SELECT databaseUuid, trackId, tool, analysedAt, beatData FROM Analysis")
        else {
            return HashMap::new();
        };
        statement
            .query_map([], |row| {
                Ok((
                    (row.get(0)?, row.get(1)?),
                    AnalysisRecord {
                        tool: row.get(2)?,
                        analysed_at: row.get(3)?,
                        beat_data: row.get(4)?,
                    },
                ))
            })
            .map(|rows| rows.flatten().collect())
            .unwrap_or_default()
    }

    /// Records that Open Engine Lib analysed the track, replacing an earlier record of it.
    pub fn record_analysis(
        &self,
        track: &TrackKey,
        tool: &str,
        beat_data: i64,
    ) -> rusqlite::Result<()> {
        let connection = self.connection.lock().expect("origins lock");
        connection.execute_batch(
            "CREATE TABLE IF NOT EXISTS Analysis (
                databaseUuid TEXT NOT NULL,
                trackId INTEGER NOT NULL,
                tool TEXT NOT NULL,
                analysedAt TEXT NOT NULL DEFAULT (datetime('now')),
                beatData INTEGER NOT NULL,
                PRIMARY KEY (databaseUuid, trackId)
             )",
        )?;
        connection.execute(
            "INSERT OR REPLACE INTO Analysis (databaseUuid, trackId, tool, beatData) \
             VALUES (?1, ?2, ?3, ?4)",
            (&track.0, track.1, tool, beat_data),
        )?;
        Ok(())
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

    /// Replaces the leading folders `from` of every origin that is in them by `to` (see
    /// `replace_prefix`). Whether a file is at the new place is not looked at. Returns how
    /// many origins changed.
    pub fn replace_prefix(&self, from: &str, to: &str) -> rusqlite::Result<usize> {
        let mut connection = self.connection.lock().expect("origins lock");
        let transaction = connection.transaction()?;
        let moved: Vec<(TrackKey, String)> = {
            let mut statement =
                transaction.prepare("SELECT databaseUuid, trackId, origin FROM Origin")?;
            statement
                .query_map([], |row| {
                    Ok(((row.get(0)?, row.get(1)?), row.get::<_, String>(2)?))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?
                .into_iter()
                .filter_map(|(key, origin)| {
                    replace_prefix(&origin, from, to)
                        .filter(|new| *new != origin)
                        .map(|new| (key, new))
                })
                .collect()
        };
        let mut changed = 0;
        {
            let mut statement = transaction.prepare(
                "UPDATE Origin SET origin = ?3, savedAt = datetime('now') \
                 WHERE databaseUuid = ?1 AND trackId = ?2",
            )?;
            for ((uuid, id), origin) in &moved {
                changed += statement.execute((uuid, id, origin))?;
            }
        }
        transaction.commit()?;
        Ok(changed)
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
    fn the_common_prefix_is_whole_folders() {
        let prefix = |paths: &[&str]| common_prefix(paths.iter().copied());
        assert_eq!(prefix(&[]), "");
        // The last part is the file, so one path has its folder as the prefix.
        assert_eq!(prefix(&["/music/house/a.flac"]), "/music/house");
        assert_eq!(
            prefix(&["/music/house/a.flac", "/music/house/deep/b.flac"]),
            "/music/house"
        );
        // Half a name is not a folder.
        assert_eq!(
            prefix(&["/music/house/a.flac", "/music/housing/b.flac"]),
            "/music"
        );
        assert_eq!(prefix(&["/music/a.flac", "/stick/b.flac"]), "/");
        assert_eq!(prefix(&["a.flac", "b.flac"]), "");
        // A file next to a folder of the same name does not make the folder common.
        assert_eq!(prefix(&["/music/a.flac", "/music/a.flac/b.flac"]), "/music");
    }

    #[test]
    fn a_prefix_is_replaced_by_folders() {
        let moved = |path: &str, from: &str, to: &str| replace_prefix(path, from, to);
        assert_eq!(
            moved("/old/music/a.flac", "/old", "/new/place"),
            Some("/new/place/music/a.flac".into())
        );
        // Slashes at the end make no difference, and the new place may be the root.
        assert_eq!(
            moved("/old/a.flac", "/old/", "/new/"),
            Some("/new/a.flac".into())
        );
        assert_eq!(moved("/old/a.flac", "/old", "/"), Some("/a.flac".into()));
        // A different folder that starts with the same letters is left alone.
        assert_eq!(moved("/older/a.flac", "/old", "/new"), None);
        assert_eq!(moved("/elsewhere/a.flac", "/old", "/new"), None);
        // The root holds every absolute path, and nothing else.
        assert_eq!(moved("/a.flac", "/", "/mnt"), Some("/mnt/a.flac".into()));
        assert_eq!(moved("a.flac", "/", "/mnt"), None);
        assert_eq!(moved("a.flac", "", "/mnt"), None);
    }

    #[test]
    fn replacing_a_prefix_changes_only_origins_in_it() {
        let folder = std::env::temp_dir().join(format!(
            "open-engine-lib-origins-prefix-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&folder);
        let origins = Origins::open(&folder.join(FILE)).unwrap();
        origins
            .save_missing(&[
                (key("a", 1), "/old/one.flac".into()),
                (key("a", 2), "/old/deep/two.flac".into()),
                (key("a", 3), "/older/three.flac".into()),
            ])
            .unwrap();
        assert_eq!(origins.replace_prefix("/old", "/new").unwrap(), 2);
        let all = origins.all().unwrap();
        assert_eq!(all[&key("a", 1)], "/new/one.flac");
        assert_eq!(all[&key("a", 2)], "/new/deep/two.flac");
        assert_eq!(all[&key("a", 3)], "/older/three.flac");
        // The same again, or the same place, changes nothing.
        assert_eq!(origins.replace_prefix("/old", "/new").unwrap(), 0);
        assert_eq!(origins.replace_prefix("/new", "/new").unwrap(), 0);
        let _ = std::fs::remove_dir_all(&folder);
    }

    #[test]
    fn analysed_tracks_are_recorded_without_changing_a_database_that_is_only_read() {
        let folder = std::env::temp_dir().join(format!(
            "open-engine-lib-origins-analysis-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&folder);
        let path = folder.join(FILE);
        let origins = Origins::open(&path).unwrap();
        origins
            .save_missing(&[(key("a", 1), "/music/one.flac".into())])
            .unwrap();
        assert!(origins.analyses().is_empty(), "no table is no analysis");
        drop(origins);

        // Reading the analyses leaves the file as it is.
        let before = std::fs::read(&path).unwrap();
        let origins = Origins::open(&path).unwrap();
        assert!(origins.analyses().is_empty());
        drop(origins);
        assert_eq!(std::fs::read(&path).unwrap(), before);

        let origins = Origins::open(&path).unwrap();
        origins.record_analysis(&key("a", 1), "tool", 7).unwrap();
        origins.record_analysis(&key("a", 2), "tool", 8).unwrap();
        origins
            .record_analysis(&key("a", 1), "newer tool", 9)
            .unwrap();
        let analyses = origins.analyses();
        assert_eq!(analyses.len(), 2);
        assert_eq!(analyses[&key("a", 1)].tool, "newer tool");
        assert_eq!(analyses[&key("a", 1)].beat_data, 9);
        assert_eq!(analyses[&key("a", 2)].beat_data, 8);
        assert!(!analyses[&key("a", 2)].analysed_at.is_empty());
        // The origins are still there.
        assert_eq!(origins.all().unwrap().len(), 1);
        let _ = std::fs::remove_dir_all(&folder);
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
