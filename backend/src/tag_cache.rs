//! What was read from the files of a plain folder of music, kept between runs so that
//! opening the folder again only looks at the files that are new or have changed.
//!
//! An entry is keyed by the file's path and holds the file's size and modification time
//! from when it was read; it is only used while both still match, so an edited or replaced
//! file is read again. Nothing here may stop a folder from opening: a cache that can't be
//! opened or written is simply not used.

use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::time::UNIX_EPOCH;

use rusqlite::{Connection, params};

use crate::folder::Meta;

/// Bump when what is read from a file (or how it is stored) changes, so old rows are dropped.
const VERSION: i32 = 1;

/// How a file looked when it was read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stamp {
    size: i64,
    /// Nanoseconds since the epoch.
    modified: i64,
}

impl Stamp {
    /// The stamp of the file at `path`, or `None` if it can't be told (then the file is read
    /// every time and not remembered).
    pub fn of(path: &Path) -> Option<Self> {
        let metadata = fs::metadata(path).ok()?;
        let modified = metadata.modified().ok()?.duration_since(UNIX_EPOCH).ok()?;
        Some(Self {
            size: i64::try_from(metadata.len()).ok()?,
            modified: i64::try_from(modified.as_nanos()).ok()?,
        })
    }
}

pub struct TagCache(Connection);

impl TagCache {
    /// Opens (making it if need be) the cache in the file `path`. A file that is not a
    /// usable cache is replaced. `None` if there is no way to have one.
    pub fn open(path: &Path) -> Option<Self> {
        Self::try_open(path).or_else(|| {
            let _ = fs::remove_file(path);
            Self::try_open(path)
        })
    }

    fn try_open(path: &Path) -> Option<Self> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).ok()?;
        }
        let connection = Connection::open(path).ok()?;
        // (Only a cache: losing the last write to a crash costs a re-read.)
        connection
            .execute_batch("PRAGMA synchronous = OFF; PRAGMA journal_mode = MEMORY;")
            .ok()?;
        let version: i32 = connection
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .ok()?;
        if version != VERSION {
            connection
                .execute_batch(&format!(
                    "DROP TABLE IF EXISTS tags;
                     CREATE TABLE tags (
                         path TEXT PRIMARY KEY,
                         size INTEGER NOT NULL,
                         modified INTEGER NOT NULL,
                         title TEXT NOT NULL,
                         artist TEXT NOT NULL,
                         album TEXT NOT NULL,
                         genre TEXT NOT NULL,
                         bpm REAL,
                         seconds INTEGER
                     ) WITHOUT ROWID;
                     PRAGMA user_version = {VERSION};"
                ))
                .ok()?;
        }
        Some(Self(connection))
    }

    /// Everything remembered about the files below `dir`, by path.
    pub fn load(&self, dir: &Path) -> HashMap<String, (Stamp, Meta)> {
        let Some(prefix) = prefix(dir) else {
            return HashMap::new();
        };
        let load = || -> rusqlite::Result<HashMap<String, (Stamp, Meta)>> {
            let mut statement = self.0.prepare(
                "SELECT path, size, modified, title, artist, album, genre, bpm, seconds
                 FROM tags WHERE substr(path, 1, length(?1)) = ?1",
            )?;
            let rows = statement.query_map([&prefix], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    (
                        Stamp {
                            size: row.get(1)?,
                            modified: row.get(2)?,
                        },
                        Meta {
                            title: row.get(3)?,
                            artist: row.get(4)?,
                            album: row.get(5)?,
                            genre: row.get(6)?,
                            bpm: row.get(7)?,
                            seconds: row.get(8)?,
                        },
                    ),
                ))
            })?;
            rows.collect()
        };
        load().unwrap_or_default()
    }

    /// Remembers `fresh` (path, how the file looked, what it said) and forgets the `gone`
    /// paths, in one go. A failure is only reported: the folder is open all the same.
    pub fn store<'a>(
        &mut self,
        gone: impl IntoIterator<Item = &'a str>,
        fresh: impl IntoIterator<Item = (&'a str, Stamp, &'a Meta)>,
    ) {
        let store = || -> rusqlite::Result<()> {
            let transaction = self.0.transaction()?;
            {
                let mut forget = transaction.prepare("DELETE FROM tags WHERE path = ?1")?;
                for path in gone {
                    forget.execute([path])?;
                }
                let mut remember = transaction.prepare(
                    "INSERT OR REPLACE INTO tags
                     (path, size, modified, title, artist, album, genre, bpm, seconds)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                )?;
                for (path, stamp, meta) in fresh {
                    remember.execute(params![
                        path,
                        stamp.size,
                        stamp.modified,
                        meta.title,
                        meta.artist,
                        meta.album,
                        meta.genre,
                        meta.bpm,
                        meta.seconds
                    ])?;
                }
            }
            transaction.commit()
        };
        if let Err(error) = store() {
            eprintln!("could not remember the tags read: {error}");
        }
    }
}

/// The start of the paths of the files below `dir`, which are keyed by their full path.
fn prefix(dir: &Path) -> Option<String> {
    let dir = dir.to_str()?;
    Some(format!("{}/", dir.trim_end_matches('/')))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta(title: &str) -> Meta {
        Meta {
            title: title.into(),
            bpm: Some(126.5),
            seconds: Some(200),
            ..Meta::default()
        }
    }

    #[test]
    fn what_was_read_comes_back_for_the_folder_it_is_below_and_can_be_forgotten() {
        let dir =
            std::env::temp_dir().join(format!("open-engine-tag-cache-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let file = dir.join("tags.db");

        let stamp = Stamp {
            size: 10,
            modified: 20,
        };
        let one = meta("one");
        let other = meta("other");
        let mut cache = TagCache::open(&file).unwrap();
        cache.store(
            [],
            [
                ("/music/a/one.mp3", stamp, &one),
                ("/music/b/two.mp3", stamp, &one),
                ("/musical/three.mp3", stamp, &other),
            ],
        );
        drop(cache);

        // It is still there when opened again, and only what is below the folder is given
        // (`/musical` is not below `/music`).
        let mut cache = TagCache::open(&file).unwrap();
        let loaded = cache.load(Path::new("/music"));
        let mut paths: Vec<_> = loaded.keys().map(String::as_str).collect();
        paths.sort();
        assert_eq!(paths, ["/music/a/one.mp3", "/music/b/two.mp3"]);
        let (seen, tags) = &loaded["/music/a/one.mp3"];
        assert_eq!(*seen, stamp);
        assert_eq!(
            (tags.title.as_str(), tags.bpm, tags.seconds),
            ("one", Some(126.5), Some(200))
        );
        assert_eq!(cache.load(Path::new("/music/")).len(), 2);

        // A path read again is replaced; one that is gone is forgotten.
        let newer = Stamp {
            size: 11,
            modified: 21,
        };
        cache.store(["/music/b/two.mp3"], [("/music/a/one.mp3", newer, &other)]);
        let loaded = cache.load(Path::new("/music"));
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded["/music/a/one.mp3"].0, newer);
        assert_eq!(loaded["/music/a/one.mp3"].1.title, "other");
        drop(cache);

        // A file that is not a database is replaced rather than failing.
        fs::write(&file, b"this is not sqlite, not even close, not at all").unwrap();
        let cache = TagCache::open(&file).unwrap();
        assert!(cache.load(Path::new("/music")).is_empty());
        let _ = fs::remove_dir_all(&dir);
    }
}
