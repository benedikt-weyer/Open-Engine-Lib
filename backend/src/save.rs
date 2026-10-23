//! Saving a library to a new place ("Save as").
//!
//! The copy is made from the cached working copy of the databases. The audio files are
//! not copied and stay where they are, so the paths the databases record for them
//! (relative to the library folder) are rewritten to point at the same files from the
//! new folder. Also copied is the `Artwork` folder: it is small, and some covers only
//! exist there. The big `Stems` and `OverviewData` folders are left behind.

use std::fmt;
use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};

use rusqlite::{Connection, OpenFlags};

use crate::cache::{self, DATABASE_DIR};
use crate::library::{ARTWORK_DIR, Library};

/// What an Engine library folder is called.
const LIBRARY_DIR: &str = "Engine Library";
const MAIN_DATABASE: &str = "m.db";

#[derive(Debug)]
pub enum SaveError {
    /// The destination is the library's own folder.
    SameAsSource,
    /// There already is a library at the destination, which is never overwritten.
    AlreadyExists(PathBuf),
    Failed(String),
}

impl fmt::Display for SaveError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SaveError::SameAsSource => f.write_str("that is the library's current location"),
            SaveError::AlreadyExists(dir) => {
                write!(f, "there already is a library in {}", dir.display())
            }
            SaveError::Failed(error) => f.write_str(error),
        }
    }
}

impl From<io::Error> for SaveError {
    fn from(error: io::Error) -> Self {
        SaveError::Failed(error.to_string())
    }
}

impl From<rusqlite::Error> for SaveError {
    fn from(error: rusqlite::Error) -> Self {
        SaveError::Failed(error.to_string())
    }
}

/// The `Engine Library` folder that saving to `destination` makes: `destination` itself
/// if it is called that, else a folder of that name inside it.
pub fn library_dir(destination: &Path) -> PathBuf {
    if destination
        .file_name()
        .is_some_and(|name| name == LIBRARY_DIR)
    {
        destination.to_path_buf()
    } else {
        destination.join(LIBRARY_DIR)
    }
}

/// Copies `library` to `destination` and returns the new library folder.
///
/// The copy is made in a temporary folder beside the final one and only moved into place
/// once it is complete, so the destination never holds a half-made library, even if the
/// program is stopped half way. If anything fails, everything made is taken away again.
pub fn save_as(library: &Library, destination: &Path) -> Result<PathBuf, SaveError> {
    let target = library_dir(destination);
    if normalize(&target) == normalize(&library.dir) {
        return Err(SaveError::SameAsSource);
    }
    if target.join(DATABASE_DIR).join(MAIN_DATABASE).exists() {
        return Err(SaveError::AlreadyExists(target));
    }
    // Only a place that is not in use is saved to: it doesn't exist or is empty.
    if target.exists() && fs::read_dir(&target)?.next().is_some() {
        return Err(SaveError::Failed(format!(
            "{} is not empty",
            target.display()
        )));
    }

    let staging = staging_dir(&target);
    // The folders this call is going to make: from the first that is missing down.
    let made = target
        .ancestors()
        .take_while(|ancestor| !ancestor.exists())
        .last()
        .map(Path::to_path_buf);

    let result = copy_library(library, &staging).and_then(|()| {
        // An empty folder is replaced by the finished library.
        if target.exists() {
            fs::remove_dir(&target)?;
        }
        fs::rename(&staging, &target)?;
        Ok(())
    });
    if result.is_err() {
        let _ = fs::remove_dir_all(&staging);
        if let Some(made) = made {
            let _ = fs::remove_dir_all(made);
        }
    }
    result.map(|()| target)
}

/// Removes a library that `save_as` made, because it turned out not to be usable.
pub fn discard(target: &Path) {
    let _ = fs::remove_dir_all(target);
}

/// Where the copy is built before it is moved to `target`: next to it, so that the move
/// stays on the same drive and is instant.
fn staging_dir(target: &Path) -> PathBuf {
    let name = target
        .file_name()
        .map_or_else(|| LIBRARY_DIR.into(), |name| name.to_string_lossy());
    target.with_file_name(format!("{name}.saving-{}", std::process::id()))
}

fn copy_library(library: &Library, target: &Path) -> Result<(), SaveError> {
    let databases = target.join(DATABASE_DIR);
    fs::create_dir_all(&databases)?;

    // The databases, from the working copy.
    let cached = library.cache.join(DATABASE_DIR);
    for file in cache::database_files(&cached)? {
        fs::copy(cached.join(&file.name), databases.join(&file.name))?;
        if file.name.ends_with(".db") {
            repoint_tracks(&databases.join(&file.name), &library.dir, target)?;
        }
    }

    // The covers that are files. Missing at the source (a drive that is gone) just means
    // there are none to copy.
    let artwork = library.dir.join(ARTWORK_DIR);
    if artwork.is_dir() {
        let to = target.join(ARTWORK_DIR);
        fs::create_dir_all(&to)?;
        for entry in fs::read_dir(&artwork)? {
            let entry = entry?;
            if entry.file_type()?.is_file() {
                fs::copy(entry.path(), to.join(entry.file_name()))?;
            }
        }
    }
    Ok(())
}

/// Rewrites the track paths of the database copy at `database`, which were relative to
/// `old_dir`, to be relative to `new_dir`, so they lead to the same files. Streaming
/// tracks have no file here and are left alone, as are databases without tracks.
fn repoint_tracks(database: &Path, old_dir: &Path, new_dir: &Path) -> Result<(), SaveError> {
    let mut connection = Connection::open_with_flags(
        database,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    let has_tracks = connection
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'Track'",
            [],
            |_| Ok(()),
        )
        .is_ok();
    if !has_tracks {
        return Ok(());
    }

    let tracks: Vec<(i64, String)> = {
        let mut statement = connection.prepare(
            "SELECT id, path FROM Track \
             WHERE path IS NOT NULL AND (streamingSource IS NULL OR streamingSource = '')",
        )?;
        statement
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<Result<_, _>>()?
    };

    let transaction = connection.transaction()?;
    // Paths are unique, and a new path can equal another track's old one, so first move
    // every one out of the way.
    transaction.execute(
        "UPDATE Track SET path = char(0) || id \
         WHERE path IS NOT NULL AND (streamingSource IS NULL OR streamingSource = '')",
        [],
    )?;
    {
        let mut update = transaction.prepare("UPDATE Track SET path = ?1 WHERE id = ?2")?;
        for (id, recorded) in tracks {
            let file = normalize(&old_dir.join(&recorded));
            update.execute((relative_path(new_dir, &file), id))?;
        }
    }
    transaction.commit()?;
    Ok(())
}

/// The path of `file` as the library records it: relative to the library folder `dir`,
/// with `/` as the separator, or the plain path if there is no relative one (another
/// drive).
fn relative_path(dir: &Path, file: &Path) -> String {
    let dir = normalize(dir);
    let file = normalize(file);
    let dir_parts: Vec<_> = dir.components().collect();
    let file_parts: Vec<_> = file.components().collect();

    let common = dir_parts
        .iter()
        .zip(&file_parts)
        .take_while(|(a, b)| a == b)
        .count();
    // Nothing in common (not even the root) means they are not on the same tree.
    if common == 0 {
        return file.to_string_lossy().replace('\\', "/");
    }

    let mut parts: Vec<String> = vec!["..".to_owned(); dir_parts.len() - common];
    parts.extend(
        file_parts[common..]
            .iter()
            .map(|part| part.as_os_str().to_string_lossy().into_owned()),
    );
    parts.join("/")
}

/// `path` with `.` and `..` worked out, without asking the file system.
pub fn normalize(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::ParentDir => {
                normalized.pop();
            }
            Component::CurDir => {}
            other => normalized.push(other.as_os_str()),
        }
    }
    normalized
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(name: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "open-engine-lib-save-{name}-{}",
                std::process::id()
            ));
            let _ = fs::remove_dir_all(&path);
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn works_out_dots_without_the_file_system() {
        assert_eq!(normalize(Path::new("/a/b/../c/./d")), Path::new("/a/c/d"));
        assert_eq!(
            normalize(Path::new("/a/../../b")),
            Path::new("/b"),
            "stops at the root"
        );
    }

    #[test]
    fn makes_the_path_a_library_records() {
        let dir = Path::new("/media/DRIVE/Engine Library");
        // A file next to the library folder, inside it, and in a different tree.
        assert_eq!(
            relative_path(dir, Path::new("/media/DRIVE/House/a.flac")),
            "../House/a.flac"
        );
        assert_eq!(
            relative_path(dir, Path::new("/media/DRIVE/Engine Library/x.flac")),
            "x.flac"
        );
        assert_eq!(
            relative_path(dir, Path::new("/home/me/Music/a.flac")),
            "../../../home/me/Music/a.flac"
        );
        // From a library folder somewhere deeper, back to the same file.
        let elsewhere = Path::new("/home/me/backup/Engine Library");
        assert_eq!(
            relative_path(elsewhere, Path::new("/media/DRIVE/House/a.flac")),
            "../../../../media/DRIVE/House/a.flac"
        );
        assert_eq!(
            normalize(&elsewhere.join("../../../../media/DRIVE/House/a.flac")),
            Path::new("/media/DRIVE/House/a.flac"),
            "and it leads back to the file"
        );
    }

    #[test]
    fn the_destination_is_a_folder_for_the_library_unless_it_is_one() {
        assert_eq!(
            library_dir(Path::new("/media/NEW")),
            Path::new("/media/NEW/Engine Library")
        );
        assert_eq!(
            library_dir(Path::new("/media/NEW/Engine Library")),
            Path::new("/media/NEW/Engine Library")
        );
    }

    /// A library on disk, opened through a cache like the real thing: tracks `a` and `b`
    /// in `Music/` next to the library folder, a streaming track, and a cover file.
    fn fixture(root: &Path) -> Library {
        let library = root.join("Engine Library");
        fs::create_dir_all(library.join("Database2")).unwrap();
        fs::create_dir_all(library.join("Artwork")).unwrap();
        fs::write(library.join("Artwork/cover.jpg"), [0xff, 0xd8, 0xff, 1]).unwrap();
        fs::create_dir_all(library.join("Stems")).unwrap();
        fs::write(library.join("Stems/1.stems"), b"huge").unwrap();

        let db = Connection::open(library.join("Database2/m.db")).unwrap();
        db.execute_batch(
            "CREATE TABLE Track (id INTEGER PRIMARY KEY, title TEXT, artist TEXT, album TEXT,
                genre TEXT, bpmAnalyzed REAL, bpm INTEGER, length INTEGER, path TEXT UNIQUE,
                streamingSource TEXT, originDatabaseUuid TEXT, originTrackId INTEGER,
                albumArtId INTEGER);
             CREATE TABLE AlbumArt (id INTEGER PRIMARY KEY, hash, albumArt);
             CREATE TABLE Playlist (id INTEGER PRIMARY KEY, title TEXT, parentListId INTEGER,
                nextListId INTEGER);
             CREATE TABLE PlaylistEntity (id INTEGER PRIMARY KEY, listId INTEGER, trackId INTEGER,
                databaseUuid TEXT, nextEntityId INTEGER);
             INSERT INTO Track VALUES
                (1, 'A', 'x', 'y', 'g', 120.0, 120, 100, '../Music/a.flac', NULL, 'db', 1, NULL),
                (2, 'B', 'x', 'y', 'g', 121.0, 121, 100, '../Music/sub/b.flac', NULL, 'db', 2, NULL),
                (3, 'S', 'x', 'y', 'g', 0.0, 0, 100, '../../../data/downloads/s.mp4', 'TIDAL', 'db', 3, NULL);",
        )
        .unwrap();
        // The history database has its own copy of the tracks.
        Connection::open(library.join("Database2/hm.db"))
            .unwrap()
            .execute_batch(
                "CREATE TABLE Track (id INTEGER PRIMARY KEY, path TEXT UNIQUE, streamingSource TEXT);
                 INSERT INTO Track VALUES (1, '../Music/a.flac', NULL);",
            )
            .unwrap();
        drop(db);
        Library::open(&library, &root.join("cache")).unwrap()
    }

    fn paths(database: &Path) -> Vec<String> {
        let db = Connection::open(database).unwrap();
        let mut statement = db.prepare("SELECT path FROM Track ORDER BY id").unwrap();
        statement
            .query_map([], |row| row.get(0))
            .unwrap()
            .map(Result::unwrap)
            .collect()
    }

    #[test]
    fn saves_a_copy_whose_tracks_still_lead_to_the_same_files() {
        let from = TempDir::new("from");
        let to = TempDir::new("to");
        let library = fixture(&from.0);
        let source_before = fs::read(library.dir.join("Database2/m.db")).unwrap();

        let saved = save_as(&library, &to.0.join("deeper/still")).unwrap();
        assert_eq!(saved, to.0.join("deeper/still/Engine Library"));

        // The track paths now start from the new folder but reach the same files.
        let new_paths = paths(&saved.join("Database2/m.db"));
        assert_eq!(
            new_paths[2], "../../../data/downloads/s.mp4",
            "a streaming track is as it was"
        );
        for (new, old) in new_paths
            .iter()
            .zip(["../Music/a.flac", "../Music/sub/b.flac"])
        {
            assert_eq!(
                normalize(&saved.join(new)),
                normalize(&library.dir.join(old)),
                "{new} should reach the file {old} did"
            );
        }
        assert_eq!(
            paths(&saved.join("Database2/hm.db")),
            [new_paths[0].clone()],
            "the history too"
        );

        // The source and its working copy are untouched.
        assert_eq!(
            fs::read(library.dir.join("Database2/m.db")).unwrap(),
            source_before
        );
        assert_eq!(
            paths(&library.cache.join("Database2/m.db"))[0],
            "../Music/a.flac"
        );
        // The covers came along, the stems did not.
        assert_eq!(
            fs::read(saved.join("Artwork/cover.jpg")).unwrap(),
            [0xff, 0xd8, 0xff, 1]
        );
        assert!(!saved.join("Stems").exists());

        // It is a library that can be opened, and its tracks lead to the original files.
        let reopened = Library::open(&saved, &to.0.join("cache")).unwrap();
        assert_eq!(reopened.tracks.len(), 3);
        assert_eq!(
            normalize(&reopened.tracks[1].path),
            normalize(&library.dir.join("../Music/sub/b.flac"))
        );
    }

    #[test]
    fn a_new_path_that_equals_another_tracks_old_one_does_not_collide() {
        // The track paths swap: what is recorded for each is what the other had.
        let from = TempDir::new("swap-from");
        let library = fixture(&from.0);
        let db = Connection::open(library.cache.join("Database2/m.db")).unwrap();
        db.execute_batch(
            "UPDATE Track SET path = 'tmp' WHERE id = 1;
             UPDATE Track SET path = '../../Engine Library/../Music/a.flac' WHERE id = 2;
             UPDATE Track SET path = '../Music/sub/b.flac' WHERE id = 1;",
        )
        .unwrap();
        drop(db);

        let to = TempDir::new("swap-to");
        let saved = save_as(&library, &to.0).unwrap();
        let new_paths = paths(&saved.join("Database2/m.db"));
        assert_eq!(new_paths.len(), 3);
        assert_ne!(new_paths[0], new_paths[1]);
    }

    #[test]
    fn refuses_the_same_place_and_an_existing_library_and_changes_nothing() {
        let from = TempDir::new("refuse-from");
        let library = fixture(&from.0);
        let before = fs::read(library.dir.join("Database2/m.db")).unwrap();

        // The library's own folder, given either way.
        assert!(matches!(
            save_as(&library, &library.dir),
            Err(SaveError::SameAsSource)
        ));
        assert!(matches!(
            save_as(&library, &from.0),
            Err(SaveError::SameAsSource)
        ));
        assert_eq!(
            fs::read(library.dir.join("Database2/m.db")).unwrap(),
            before
        );

        // A place that already holds a library is never written to.
        let to = TempDir::new("refuse-to");
        let saved = save_as(&library, &to.0).unwrap();
        let marker = saved.join("Database2/m.db");
        let length = fs::metadata(&marker).unwrap().len();
        assert!(matches!(
            save_as(&library, &to.0),
            Err(SaveError::AlreadyExists(_))
        ));
        assert_eq!(fs::metadata(&marker).unwrap().len(), length);
    }

    #[test]
    fn a_failed_save_leaves_nothing_behind() {
        let from = TempDir::new("fail-from");
        let library = fixture(&from.0);

        // A place that is not empty is not saved into, and what is there stays.
        let to = TempDir::new("fail-to");
        fs::create_dir_all(to.0.join("Engine Library")).unwrap();
        fs::write(to.0.join("Engine Library/notes.txt"), b"mine").unwrap();
        assert!(matches!(
            save_as(&library, &to.0),
            Err(SaveError::Failed(_))
        ));
        assert_eq!(
            fs::read(to.0.join("Engine Library/notes.txt")).unwrap(),
            b"mine"
        );
        assert!(!to.0.join("Engine Library/Database2").exists());

        // A failure half way (here: one of the databases can't be read) takes away the
        // folders that were made for it, and leaves no temporary folder.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;

            let broken = TempDir::new("fail-broken");
            let library = fixture(&broken.0);
            let hm = library.cache.join("Database2/hm.db");
            fs::set_permissions(&hm, fs::Permissions::from_mode(0o000)).unwrap();
            // (Someone who can read it anyway, like root, can't make this fail.)
            if fs::File::open(&hm).is_err() {
                let to = TempDir::new("fail-half");
                assert!(save_as(&library, &to.0.join("new/place")).is_err());
                assert!(
                    !to.0.join("new").exists(),
                    "the folders made for it are gone again"
                );
                assert_eq!(fs::read_dir(&to.0).unwrap().count(), 0);
            }
            fs::set_permissions(&hm, fs::Permissions::from_mode(0o600)).unwrap();
        }

        // A destination that can't be made is not left behind either.
        let blocked = TempDir::new("fail-blocked");
        fs::write(blocked.0.join("file"), b"x").unwrap();
        let library = fixture(&TempDir::new("fail-blocked-source").0);
        assert!(save_as(&library, &blocked.0.join("file/inside")).is_err());
        assert!(!blocked.0.join("file/inside").exists());
    }

    #[test]
    fn an_empty_folder_is_saved_into_and_nothing_temporary_is_left() {
        let from = TempDir::new("empty-from");
        let library = fixture(&from.0);
        let to = TempDir::new("empty-to");
        fs::create_dir_all(to.0.join("Engine Library")).unwrap();

        let saved = save_as(&library, &to.0).unwrap();
        assert!(saved.join("Database2/m.db").is_file());
        let names: Vec<_> = fs::read_dir(&to.0)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, ["Engine Library"], "no temporary folder remains");
    }
}
