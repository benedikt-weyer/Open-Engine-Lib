//! Making the playlists of another library the same as the open one's ("sync to other
//! library"). Playlists and smart playlists only: tracks and audio files are never
//! copied, so a playlist gets the tracks the other library has, and the rest are counted
//! as missing.
//!
//! The other library is never edited in place. Its `m.db` is copied into a scratch
//! folder, the copy is changed (through the same `Library` calls the UI's own edits use),
//! and only a `push` puts the result in the other library, after keeping the old
//! `m.db`. A `diff` is the same work on the copy, thrown away afterwards, so what it
//! lists is what a push does.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use open_engine_api::v1 as api;

use crate::cache::DATABASE_DIR;
use crate::library::{self, EditError, Library, OpenError, Playlist};
use crate::sync::{BACKUP_DIR, is_busy};

const MAIN_DATABASE: &str = "m.db";
const SCRATCH_DIR: &str = "library-sync";
/// Lists are cut at this many entries; the counts are complete.
const MAX_LISTED: usize = 200;

#[derive(Debug)]
pub enum MirrorError {
    /// There is no Engine library at or just below the path.
    NotALibrary(PathBuf),
    /// The path is the open library itself.
    SameLibrary,
    /// Another program is writing to the other library.
    Busy,
    Io(io::Error),
    Open(OpenError),
    Edit(EditError),
}

impl fmt::Display for MirrorError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MirrorError::NotALibrary(path) => write!(
                f,
                "{} does not contain an Engine library (Database2/m.db)",
                path.display()
            ),
            MirrorError::SameLibrary => write!(f, "that is the library that is open"),
            MirrorError::Busy => write!(
                f,
                "another program is writing to that library right now; try again in a moment"
            ),
            MirrorError::Io(error) => write!(f, "{error}"),
            MirrorError::Open(error) => write!(f, "{error}"),
            MirrorError::Edit(error) => write!(f, "{error}"),
        }
    }
}

impl From<io::Error> for MirrorError {
    fn from(error: io::Error) -> Self {
        MirrorError::Io(error)
    }
}

impl From<OpenError> for MirrorError {
    fn from(error: OpenError) -> Self {
        MirrorError::Open(error)
    }
}

impl From<EditError> for MirrorError {
    fn from(error: EditError) -> Self {
        MirrorError::Edit(error)
    }
}

/// What the work on the copy found.
struct Outcome {
    changes: Vec<api::PlaylistChange>,
    /// Tracks of the open library that were left out of a playlist because the other
    /// library does not have them (indices into its tracks).
    missing: Vec<usize>,
}

/// A copy of the other library's `m.db`, opened as a library.
struct Copy {
    /// The other library's `Engine Library` folder.
    dir: PathBuf,
    scratch: PathBuf,
    library: Library,
}

fn prepare(source: &Library, target: &Path, scratch_root: &Path) -> Result<Copy, MirrorError> {
    let dir = library::locate(target).ok_or_else(|| MirrorError::NotALibrary(target.into()))?;
    let same = match (fs::canonicalize(&dir), fs::canonicalize(&source.dir)) {
        (Ok(a), Ok(b)) => a == b,
        _ => dir == source.dir,
    };
    if same {
        return Err(MirrorError::SameLibrary);
    }
    let databases = dir.join(DATABASE_DIR);
    if is_busy(&databases) {
        return Err(MirrorError::Busy);
    }

    let scratch = scratch_root.join(SCRATCH_DIR);
    let _ = fs::remove_dir_all(&scratch);
    fs::create_dir_all(scratch.join(DATABASE_DIR))?;
    let copy = scratch.join(DATABASE_DIR).join(MAIN_DATABASE);
    fs::copy(databases.join(MAIN_DATABASE), &copy)?;
    let library = Library::open_copy(&copy, dir.clone(), scratch.clone())?;
    Ok(Copy {
        dir,
        scratch,
        library,
    })
}

/// What making the other library's playlists the same as `source`'s would change.
pub fn diff(
    source: &Library,
    target: &Path,
    scratch_root: &Path,
    remove_missing: bool,
) -> Result<api::GetLibrarySyncDiffResponse, MirrorError> {
    let copy = prepare(source, target, scratch_root)?;
    let outcome = mirror(source, &copy.library, remove_missing)?;
    let dir = copy.dir.display().to_string();
    let scratch = copy.scratch.clone();
    drop(copy);
    let _ = fs::remove_dir_all(scratch);

    let mut changes = outcome.changes;
    changes.sort_by(|a, b| a.name.cmp(&b.name));
    let playlists_changed = changes.len() as u32;
    changes.truncate(MAX_LISTED);
    Ok(api::GetLibrarySyncDiffResponse {
        target_path: dir,
        playlists_changed,
        changed_playlists: changes,
        tracks_missing: outcome.missing.len() as u32,
        missing_tracks: outcome
            .missing
            .iter()
            .take(MAX_LISTED)
            .map(|&index| {
                let track = &source.tracks[index];
                api::TrackRef {
                    title: track.title.clone(),
                    artist: track.artist.clone(),
                    path: track.relative_path.clone(),
                }
            })
            .collect(),
    })
}

/// Makes the other library's playlists the same as `source`'s: the change is made on a
/// copy of its `m.db`, which then replaces the original, which is kept in a backup folder.
pub fn push(
    source: &Library,
    target: &Path,
    scratch_root: &Path,
    remove_missing: bool,
) -> Result<api::SyncToLibraryResponse, MirrorError> {
    let copy = prepare(source, target, scratch_root)?;
    let outcome = mirror(source, &copy.library, remove_missing)?;
    let Copy {
        dir,
        scratch,
        library,
    } = copy;
    drop(library); // closes the database, so that the file is complete

    let mut response = api::SyncToLibraryResponse {
        target_path: dir.display().to_string(),
        playlists_changed: outcome.changes.len() as u32,
        tracks_missing: outcome.missing.len() as u32,
        backup_path: String::new(),
    };
    if !outcome.changes.is_empty() {
        let databases = dir.join(DATABASE_DIR);
        // Another program may have started writing since the copy was made.
        if is_busy(&databases) {
            let _ = fs::remove_dir_all(&scratch);
            return Err(MirrorError::Busy);
        }
        // Keep what is about to be replaced. (One backup, the latest.)
        let backup = databases.join(BACKUP_DIR);
        let _ = fs::remove_dir_all(&backup);
        fs::create_dir_all(&backup)?;
        fs::copy(databases.join(MAIN_DATABASE), backup.join(MAIN_DATABASE))?;

        // Written beside the file and moved over it, so that it is never half written.
        let temporary = databases.join(format!(".{MAIN_DATABASE}.syncing"));
        let result = fs::copy(scratch.join(DATABASE_DIR).join(MAIN_DATABASE), &temporary)
            .and_then(|_| fs::rename(&temporary, databases.join(MAIN_DATABASE)));
        if let Err(error) = result {
            let _ = fs::remove_file(&temporary);
            let _ = fs::remove_dir_all(&scratch);
            return Err(error.into());
        }
        response.backup_path = backup.display().to_string();
    }
    let _ = fs::remove_dir_all(&scratch);
    Ok(response)
}

/// The titles from the top down to the playlist at `index`.
fn titles(playlists: &[Playlist], index: usize) -> Vec<String> {
    let mut titles = Vec::new();
    let mut at = Some(index);
    while let Some(index) = at {
        titles.push(playlists[index].title.clone());
        at = playlists[index].parent;
    }
    titles.reverse();
    titles
}

/// Index in `library`'s playlists of the playlist (or smart playlist) with these titles.
fn find(library: &Library, path: &[String], smart: bool) -> Option<usize> {
    let playlists = library.playlists();
    (0..playlists.len())
        .find(|&index| playlists[index].smart == smart && titles(&playlists, index) == path)
}

/// For each track of `source`, the track of `target` it is: the one with the same origin,
/// or else the one with the same file name if only one track of each has it.
fn match_tracks(source: &Library, target: &Library) -> Vec<Option<usize>> {
    let by_origin: HashMap<&(String, i64), usize> = target
        .tracks
        .iter()
        .enumerate()
        .filter_map(|(index, track)| Some((track.origin_key.as_ref()?, index)))
        .collect();

    let file_name = |track: &library::Track| {
        track
            .relative_path
            .rsplit('/')
            .next()
            .unwrap_or_default()
            .to_owned()
    };
    let count = |library: &Library| {
        let mut counts: HashMap<String, usize> = HashMap::new();
        for track in &library.tracks {
            *counts.entry(file_name(track)).or_default() += 1;
        }
        counts
    };
    let (source_counts, target_counts) = (count(source), count(target));
    let by_name: HashMap<String, usize> = target
        .tracks
        .iter()
        .enumerate()
        .map(|(index, track)| (file_name(track), index))
        .collect();

    source
        .tracks
        .iter()
        .map(|track| {
            if let Some(&index) = track
                .origin_key
                .as_ref()
                .and_then(|origin| by_origin.get(origin))
            {
                return Some(index);
            }
            let name = file_name(track);
            let unique =
                source_counts.get(&name) == Some(&1) && target_counts.get(&name) == Some(&1);
            if unique {
                by_name.get(&name).copied()
            } else {
                None
            }
        })
        .collect()
}

/// A playlist of the open library, with what is needed to make it in the other one.
struct Wanted {
    path: Vec<String>,
    smart: bool,
    /// Indices into the open library's tracks.
    tracks: Vec<usize>,
    rules: Option<String>,
}

fn wanted(source: &Library) -> Vec<Wanted> {
    let playlists = source.playlists();
    (0..playlists.len())
        .map(|index| Wanted {
            path: titles(&playlists, index),
            smart: playlists[index].smart,
            tracks: playlists[index].tracks.clone(),
            rules: playlists[index].raw_rules.clone(),
        })
        .collect()
}

/// Changes `target` until its playlists are those of `source`, and tells what changed.
fn mirror(
    source: &Library,
    target: &Library,
    remove_missing: bool,
) -> Result<Outcome, MirrorError> {
    let matched = match_tracks(source, target);
    let wanted = wanted(source);
    let change =
        |path: &[String], kind: api::PlaylistChangeKind, smart: bool| api::PlaylistChange {
            name: path.join(" / "),
            kind: kind.into(),
            smart,
            ..Default::default()
        };

    let mut changes = Vec::new();
    let mut missing: Vec<usize> = Vec::new();
    let mut missing_seen = HashSet::new();

    // Ordinary playlists first, the ones above before the ones in them (the open library
    // lists them that way), then the smart ones, which may be in them.
    for list in wanted.iter().filter(|list| !list.smart) {
        let parent = if list.path.len() > 1 {
            match find(target, &list.path[..list.path.len() - 1], false) {
                Some(parent) => Some(parent),
                None => continue,
            }
        } else {
            None
        };
        let (index, added) = match find(target, &list.path, false) {
            Some(index) => (index, false),
            None => {
                let title = list.path.last().expect("a playlist has a title");
                (target.create_playlist(title, parent)?, true)
            }
        };

        let mut desired: Vec<usize> = Vec::new();
        let mut left_out = 0;
        for &track in &list.tracks {
            match matched[track] {
                Some(other) => {
                    if !desired.contains(&other) {
                        desired.push(other);
                    }
                }
                None => {
                    left_out += 1;
                    if missing_seen.insert(track) {
                        missing.push(track);
                    }
                }
            }
        }
        let current = target.playlists()[index].tracks.clone();

        let count =
            |of: &[usize], in_: &[usize]| of.iter().filter(|t| !in_.contains(t)).count() as u32;
        let (tracks_added, tracks_removed) = (count(&desired, &current), count(&current, &desired));
        let order_changed = tracks_added == 0 && tracks_removed == 0 && current != desired;
        if current != desired {
            target.set_playlist_tracks(index, &desired)?;
        }
        if added || current != desired {
            let kind = if added {
                api::PlaylistChangeKind::Added
            } else {
                api::PlaylistChangeKind::Tracks
            };
            let mut entry = change(&list.path, kind, false);
            entry.tracks_added = tracks_added;
            entry.tracks_removed = tracks_removed;
            entry.order_changed = order_changed;
            entry.tracks_missing = left_out;
            changes.push(entry);
        }
    }

    for list in wanted.iter().filter(|list| list.smart) {
        let Some(rules) = &list.rules else { continue };
        let parent = if list.path.len() > 1 {
            match find(target, &list.path[..list.path.len() - 1], false) {
                Some(parent) => Some(parent),
                None => continue,
            }
        } else {
            None
        };
        let title = list.path.last().expect("a playlist has a title");
        match find(target, &list.path, true) {
            None => {
                target.create_smart_playlist_in(title, parent, rules)?;
                changes.push(change(&list.path, api::PlaylistChangeKind::Added, true));
            }
            Some(index) => {
                let same = target.playlists()[index].raw_rules.as_deref() == Some(rules.as_str());
                if !same {
                    target.update_smart_playlist_json(index, title, rules)?;
                    let mut entry = change(&list.path, api::PlaylistChangeKind::Rules, true);
                    entry.rules_changed = true;
                    changes.push(entry);
                }
            }
        }
    }

    if remove_missing {
        let kept: HashSet<(&[String], bool)> = wanted
            .iter()
            .map(|list| (list.path.as_slice(), list.smart))
            .collect();
        // What is only in the other library, the ones higher up first: taking one away
        // takes what is in it too.
        let mut extra: Vec<(Vec<String>, bool)> = {
            let playlists = target.playlists();
            (0..playlists.len())
                .map(|index| (titles(&playlists, index), playlists[index].smart))
                .filter(|(path, smart)| !kept.contains(&(path.as_slice(), *smart)))
                .collect()
        };
        extra.sort_by_key(|(path, smart)| (path.len(), *smart));
        for (path, smart) in extra {
            let Some(index) = find(target, &path, smart) else {
                continue;
            };
            if smart {
                target.delete_smart_playlist(index)?;
            } else {
                target.delete_playlist(index)?;
            }
            changes.push(change(&path, api::PlaylistChangeKind::Removed, smart));
        }
    }

    Ok(Outcome { changes, missing })
}

#[cfg(test)]
mod tests {
    use super::*;

    use rusqlite::Connection;

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "open-engine-lib-mirror-{name}-{}",
                std::process::id()
            ));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    /// The tables of a library as Engine creates them (the parts used here).
    const SCHEMA: &str = "
        CREATE TABLE Track (id INTEGER PRIMARY KEY, title TEXT, artist TEXT, album TEXT,
            genre TEXT, bpmAnalyzed REAL, bpm INTEGER, length INTEGER, path TEXT,
            streamingSource TEXT, originDatabaseUuid TEXT, originTrackId INTEGER, albumArtId INTEGER);
        CREATE TABLE Playlist (id INTEGER PRIMARY KEY AUTOINCREMENT, title TEXT,
            parentListId INTEGER, isPersisted BOOLEAN, nextListId INTEGER,
            lastEditTime DATETIME, isExplicitlyExported BOOLEAN,
            UNIQUE (title, parentListId), UNIQUE (parentListId, nextListId));
        CREATE TRIGGER trigger_before_insert_List BEFORE INSERT ON Playlist
        FOR EACH ROW BEGIN
            UPDATE Playlist SET nextListId = -(1 + nextListId)
            WHERE nextListId = NEW.nextListId AND parentListId = NEW.parentListId;
        END;
        CREATE TRIGGER trigger_after_insert_List AFTER INSERT ON Playlist
        FOR EACH ROW BEGIN
            UPDATE Playlist SET nextListId = NEW.id
            WHERE nextListId = -(1 + NEW.nextListId) AND parentListId = NEW.parentListId;
        END;
        CREATE TRIGGER trigger_after_delete_List AFTER DELETE ON Playlist
        FOR EACH ROW BEGIN
            UPDATE Playlist SET nextListId = OLD.nextListId WHERE nextListId = OLD.id;
            DELETE FROM Playlist WHERE parentListId = OLD.id;
        END;
        CREATE TABLE PlaylistEntity (id INTEGER PRIMARY KEY AUTOINCREMENT, listId INTEGER,
            trackId INTEGER, databaseUuid TEXT, nextEntityId INTEGER, membershipReference INTEGER,
            UNIQUE (listId, databaseUuid, trackId));
        CREATE TRIGGER trigger_before_delete_PlaylistEntity BEFORE DELETE ON PlaylistEntity
        WHEN OLD.trackId > 0 BEGIN
            UPDATE PlaylistEntity SET nextEntityId = OLD.nextEntityId
            WHERE nextEntityId = OLD.id AND listId = OLD.listId;
        END;
        CREATE TABLE Smartlist (listUuid TEXT NOT NULL PRIMARY KEY, title TEXT,
            parentPlaylistPath TEXT, nextPlaylistPath TEXT, nextListUuid TEXT, rules TEXT,
            lastEditTime DATETIME, UNIQUE (title, parentPlaylistPath),
            UNIQUE (parentPlaylistPath, nextPlaylistPath, nextListUuid));";

    /// A library folder in `root/name` with tracks `(file, origin id)` (origin database
    /// `db`) and the SQL that fills the playlists.
    fn make(root: &Path, name: &str, tracks: &[(&str, i64)], playlists: &str) -> PathBuf {
        let library = root.join(name).join("Engine Library");
        fs::create_dir_all(library.join(DATABASE_DIR)).unwrap();
        let connection = Connection::open(library.join(DATABASE_DIR).join(MAIN_DATABASE)).unwrap();
        connection.execute_batch(SCHEMA).unwrap();
        for (position, (file, origin)) in tracks.iter().enumerate() {
            connection
                .execute(
                    "INSERT INTO Track (id, title, path, originDatabaseUuid, originTrackId) \
                     VALUES (?1, ?2, ?3, 'db', ?4)",
                    (
                        position as i64 + 1,
                        file,
                        format!("../Music/{file}"),
                        origin,
                    ),
                )
                .unwrap();
        }
        connection.execute_batch(playlists).unwrap();
        library
    }

    fn open(dir: &Path, scratch: &Path) -> Library {
        Library::open_copy(
            &dir.join(DATABASE_DIR).join(MAIN_DATABASE),
            dir.to_path_buf(),
            scratch.to_path_buf(),
        )
        .unwrap()
    }

    fn lists(library: &Library) -> Vec<(String, Vec<String>)> {
        let playlists = library.playlists();
        (0..playlists.len())
            .map(|index| {
                (
                    titles(&playlists, index).join(" / "),
                    playlists[index]
                        .tracks
                        .iter()
                        .map(|&track| library.tracks[track].title.clone())
                        .collect(),
                )
            })
            .collect()
    }

    /// The open library has tracks a, b, c with playlists House (b, a) holding Deep (c),
    /// Pop (a) and the smart "Techno". The other has the same tracks in another order
    /// of ids, lacks c, and has a playlist of its own.
    fn libraries(root: &Path) -> (PathBuf, PathBuf) {
        let smart = "INSERT INTO Smartlist VALUES ('s1', 'Techno', '', '', '', '{\"match\":\"all\"}', NULL);";
        let source = make(
            root,
            "pc",
            &[("a.flac", 1), ("b.flac", 2), ("c.flac", 3)],
            &format!(
                "INSERT INTO Playlist (id, title, parentListId, isPersisted, nextListId) VALUES
                    (1, 'House', 0, 1, 3), (2, 'Deep', 1, 1, 0), (3, 'Pop', 0, 1, 0);
                 INSERT INTO PlaylistEntity VALUES
                    (1, 1, 2, 'db', 2, 0), (2, 1, 1, 'db', 0, 0),
                    (3, 2, 3, 'db', 0, 0), (4, 3, 1, 'db', 0, 0);
                 {smart}"
            ),
        );
        let target = make(
            root,
            "stick",
            &[("b.flac", 2), ("a.flac", 1)],
            "INSERT INTO Playlist (id, title, parentListId, isPersisted, nextListId) VALUES
                (1, 'Mine', 0, 1, 0), (2, 'House', 0, 1, 1);
             INSERT INTO PlaylistEntity VALUES (1, 2, 1, 'db', 0, 0), (2, 1, 2, 'db', 0, 0);",
        );
        (source, target)
    }

    #[test]
    fn the_diff_lists_what_a_push_does_and_changes_nothing() {
        let temp = TempDir::new("diff");
        let (pc, stick) = libraries(&temp.0);
        let before = fs::read(stick.join(DATABASE_DIR).join(MAIN_DATABASE)).unwrap();
        let source = open(&pc, &temp.0.join("source-scratch"));

        let diff = diff(&source, &stick, &temp.0.join("scratch"), false).unwrap();
        assert_eq!(diff.target_path, stick.display().to_string());
        let by_name = |name: &str| {
            diff.changed_playlists
                .iter()
                .find(|p| p.name == name)
                .unwrap_or_else(|| panic!("no {name}: {:?}", diff.changed_playlists))
        };
        // House exists in both, but only holds a there; b, a is wanted.
        let house = by_name("House");
        assert_eq!(house.kind(), api::PlaylistChangeKind::Tracks);
        assert_eq!((house.tracks_added, house.tracks_removed), (1, 0));
        // Deep is new, and its only track (c) is not in the other library.
        let deep = by_name("House / Deep");
        assert_eq!(deep.kind(), api::PlaylistChangeKind::Added);
        assert_eq!((deep.tracks_added, deep.tracks_missing), (0, 1));
        assert_eq!(by_name("Pop").kind(), api::PlaylistChangeKind::Added);
        assert_eq!(by_name("Pop").tracks_added, 1);
        let techno = by_name("Techno");
        assert!(techno.smart);
        assert_eq!(techno.kind(), api::PlaylistChangeKind::Added);
        assert_eq!(
            diff.playlists_changed, 4,
            "Mine is only in the other library"
        );
        assert_eq!(
            (diff.tracks_missing, diff.missing_tracks[0].title.as_str()),
            (1, "c.flac")
        );

        // Nothing was written, and nothing is left behind.
        assert_eq!(
            fs::read(stick.join(DATABASE_DIR).join(MAIN_DATABASE)).unwrap(),
            before
        );
        assert!(!temp.0.join("scratch").join(SCRATCH_DIR).exists());
    }

    #[test]
    fn a_push_makes_the_playlists_the_same_and_keeps_a_backup() {
        let temp = TempDir::new("push");
        let (pc, stick) = libraries(&temp.0);
        let before = fs::read(stick.join(DATABASE_DIR).join(MAIN_DATABASE)).unwrap();
        let source = open(&pc, &temp.0.join("source-scratch"));

        let done = push(&source, &stick, &temp.0.join("scratch"), false).unwrap();
        assert_eq!((done.playlists_changed, done.tracks_missing), (4, 1));
        let backup = stick
            .join(DATABASE_DIR)
            .join(BACKUP_DIR)
            .join(MAIN_DATABASE);
        assert_eq!(
            done.backup_path,
            backup.parent().unwrap().display().to_string()
        );
        assert_eq!(
            fs::read(&backup).unwrap(),
            before,
            "the old database is kept"
        );

        let after = open(&stick, &temp.0.join("check"));
        let lists = lists(&after);
        let tracks_of = |name: &str| {
            lists
                .iter()
                .find(|(n, _)| n == name)
                .unwrap_or_else(|| panic!("no {name}: {lists:?}"))
                .1
                .clone()
        };
        assert_eq!(
            tracks_of("House"),
            ["b.flac", "a.flac"],
            "in the open library's order"
        );
        assert!(tracks_of("House / Deep").is_empty());
        assert_eq!(tracks_of("Pop"), ["a.flac"]);
        assert!(
            lists.iter().any(|(n, _)| n == "Mine"),
            "its own playlist stays"
        );
        assert!(
            after
                .playlists()
                .iter()
                .any(|p| p.smart && p.title == "Techno")
        );

        // Pushing again has nothing left to do and writes nothing.
        let again = push(&source, &stick, &temp.0.join("scratch"), false).unwrap();
        assert_eq!(
            (again.playlists_changed, again.backup_path.as_str()),
            (0, "")
        );
        drop(after);
    }

    #[test]
    fn playlists_only_in_the_other_library_go_when_asked() {
        let temp = TempDir::new("remove");
        let (pc, stick) = libraries(&temp.0);
        let source = open(&pc, &temp.0.join("source-scratch"));

        let diff = diff(&source, &stick, &temp.0.join("scratch"), true).unwrap();
        let mine = diff
            .changed_playlists
            .iter()
            .find(|p| p.name == "Mine")
            .unwrap();
        assert_eq!(mine.kind(), api::PlaylistChangeKind::Removed);

        push(&source, &stick, &temp.0.join("scratch"), true).unwrap();
        let after = open(&stick, &temp.0.join("check"));
        assert!(!lists(&after).iter().any(|(n, _)| n == "Mine"));
    }

    #[test]
    fn changed_rules_and_files_with_unique_names_are_matched() {
        let temp = TempDir::new("rules");
        let smart = "INSERT INTO Smartlist VALUES ('s1', 'Techno', '', '', '', '{\"v\":2}', NULL);";
        let pc = make(
            &temp.0,
            "pc",
            &[("same name.flac", 7)],
            &format!(
                "INSERT INTO Playlist (id, title, parentListId, isPersisted, nextListId) VALUES (1, 'Mix', 0, 1, 0);
                 INSERT INTO PlaylistEntity VALUES (1, 1, 7, 'db', 0, 0); {smart}"
            ),
        );
        // The same file under another origin id, and an older version of the rules.
        let stick = make(
            &temp.0,
            "stick",
            &[("same name.flac", 99)],
            "INSERT INTO Smartlist VALUES ('x', 'Techno', '', '', '', '{\"v\":1}', NULL);",
        );
        let source = open(&pc, &temp.0.join("source-scratch"));
        let diff = diff(&source, &stick, &temp.0.join("scratch"), false).unwrap();
        let mix = diff
            .changed_playlists
            .iter()
            .find(|p| p.name == "Mix")
            .unwrap();
        assert_eq!(
            (mix.tracks_added, mix.tracks_missing),
            (1, 0),
            "matched by its file name"
        );
        let techno = diff
            .changed_playlists
            .iter()
            .find(|p| p.name == "Techno")
            .unwrap();
        assert_eq!(techno.kind(), api::PlaylistChangeKind::Rules);
        assert!(techno.rules_changed);
    }

    #[test]
    fn refuses_the_open_library_a_non_library_and_a_busy_one() {
        let temp = TempDir::new("refuse");
        let (pc, stick) = libraries(&temp.0);
        let source = open(&pc, &temp.0.join("source-scratch"));
        let scratch = temp.0.join("scratch");

        assert!(matches!(
            diff(&source, &pc, &scratch, false),
            Err(MirrorError::SameLibrary)
        ));
        assert!(matches!(
            diff(&source, &temp.0.join("nowhere"), &scratch, false),
            Err(MirrorError::NotALibrary(_))
        ));
        fs::write(stick.join(DATABASE_DIR).join("m.db-journal"), b"x").unwrap();
        assert!(matches!(
            diff(&source, &stick, &scratch, false),
            Err(MirrorError::Busy)
        ));
    }
}
