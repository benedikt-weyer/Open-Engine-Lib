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
use crate::origins;
use crate::progress::{Phase, Progress};
use crate::sync::{BACKUP_DIR, is_busy};
use crate::track_sync;

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
    Database(rusqlite::Error),
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
            MirrorError::Database(error) => write!(f, "{error}"),
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

impl From<rusqlite::Error> for MirrorError {
    fn from(error: rusqlite::Error) -> Self {
        MirrorError::Database(error)
    }
}

impl From<EditError> for MirrorError {
    fn from(error: EditError) -> Self {
        MirrorError::Edit(error)
    }
}

/// What to sync besides the playlists.
#[derive(Debug, Clone, Copy, Default)]
pub struct Options {
    /// Also delete playlists and smart playlists that only the other library has.
    pub remove_missing: bool,
    pub tracks: track_sync::Options,
}

/// What the work on the copy found.
struct Outcome {
    changes: Vec<api::PlaylistChange>,
    /// Tracks of the open library that were left out of a playlist because the other
    /// library does not have them (indices into its tracks).
    missing: Vec<usize>,
    /// How many origins of the other library are new or were different.
    origins_changed: usize,
}

/// A copy of the other library's databases that is worked on.
struct Copy {
    /// The other library's `Engine Library` folder.
    dir: PathBuf,
    scratch: PathBuf,
}

impl Copy {
    fn database(&self) -> PathBuf {
        self.scratch.join(DATABASE_DIR).join(MAIN_DATABASE)
    }

    /// The copy opened as a library. (Opened again after something changed it.)
    fn open(&self) -> Result<Library, OpenError> {
        Library::open_copy(&self.database(), self.dir.clone(), self.scratch.clone())
    }
}

/// Everything the two libraries' work found.
struct Work {
    outcome: Outcome,
    tracks: track_sync::Report,
}

fn prepare(
    source: &Library,
    target: &Path,
    scratch_root: &Path,
    progress: &Progress,
) -> Result<Copy, MirrorError> {
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
    let origins = databases.join(origins::FILE);
    let size = |path: &Path| fs::metadata(path).map_or(0, |metadata| metadata.len());
    progress.phase(
        Phase::Preparing,
        size(&databases.join(MAIN_DATABASE)) + size(&origins),
        0,
    );
    progress.copy(
        &databases.join(MAIN_DATABASE),
        &scratch.join(DATABASE_DIR).join(MAIN_DATABASE),
        false,
    )?;
    // The origins it has, if any, to be changed along with the playlists.
    if origins.is_file() {
        progress.copy(
            &origins,
            &scratch.join(DATABASE_DIR).join(origins::FILE),
            false,
        )?;
    }
    Ok(Copy { dir, scratch })
}

/// The work: first the tracks (stars, paths, files, tracks the other library lacks), which
/// change what the other library has, then the playlists and origins on what it has then.
/// With `copy_files` false no audio file is copied, which is what a diff does.
fn work(
    source: &Library,
    copy: &Copy,
    options: Options,
    copy_files: bool,
    progress: &Progress,
) -> Result<Work, MirrorError> {
    let tracks = if options.tracks.any() {
        let probe = copy.open()?;
        let matched = match_tracks(source, &probe);
        track_sync::sync(
            source,
            &probe,
            &matched,
            &copy.database(),
            options.tracks,
            copy_files,
            progress,
        )?
    } else {
        track_sync::Report::default()
    };
    progress.phase(Phase::Updating, 0, 0);
    let library = match copy.open() {
        Ok(library) => library,
        Err(error) => {
            remove_files(&tracks.copied);
            return Err(error.into());
        }
    };
    match mirror(source, &library, options.remove_missing) {
        Ok(outcome) => Ok(Work { outcome, tracks }),
        Err(error) => {
            remove_files(&tracks.copied);
            Err(error)
        }
    }
}

/// Takes away files that were copied for a sync that did not happen.
fn remove_files(files: &[PathBuf]) {
    for file in files {
        let _ = fs::remove_file(file);
    }
}

/// What making the other library's playlists (and, if asked, tracks) the same as `source`'s
/// would change.
pub fn diff(
    source: &Library,
    target: &Path,
    scratch_root: &Path,
    options: Options,
) -> Result<api::GetLibrarySyncDiffResponse, MirrorError> {
    // (A diff is quick and nobody asks how far it is.)
    let progress = Progress::default();
    let copy = prepare(source, target, scratch_root, &progress)?;
    let Work { outcome, tracks } = work(source, &copy, options, false, &progress)?;
    let dir = copy.dir.display().to_string();
    let scratch = copy.scratch.clone();
    drop(copy);
    let _ = fs::remove_dir_all(scratch);

    let mut changes = outcome.changes;
    changes.sort_by(|a, b| a.name.cmp(&b.name));
    let playlists_changed = changes.len() as u32;
    changes.truncate(MAX_LISTED);
    let reference = |index: usize| {
        let track = &source.tracks[index];
        api::TrackRef {
            title: track.title.clone(),
            artist: track.artist.clone(),
            path: track.relative_path.clone(),
        }
    };
    Ok(api::GetLibrarySyncDiffResponse {
        target_path: dir,
        playlists_changed,
        changed_playlists: changes,
        tracks_missing: outcome.missing.len() as u32,
        origins_changed: outcome.origins_changed as u32,
        missing_tracks: outcome
            .missing
            .iter()
            .take(MAX_LISTED)
            .map(|&index| reference(index))
            .collect(),
        tracks_added: tracks.added.len() as u32,
        added_tracks: tracks
            .added
            .iter()
            .take(MAX_LISTED)
            .map(|&index| reference(index))
            .collect(),
        tracks_changed: tracks.changed.len() as u32,
        changed_tracks: tracks.changed.iter().take(MAX_LISTED).cloned().collect(),
        files_to_copy: tracks.files_to_copy,
        bytes_to_copy: tracks.bytes_to_copy,
        files_missing: tracks.files_missing,
        location_conflicts: tracks.location_conflicts,
    })
}

/// Makes the other library the same as `source`: the changes are made on a copy of its
/// `m.db`, which then replaces the original, which is kept in a backup folder. Audio files
/// are copied first (when asked for), and taken away again if the sync then fails.
pub fn push(
    source: &Library,
    target: &Path,
    scratch_root: &Path,
    options: Options,
    progress: &Progress,
) -> Result<api::SyncToLibraryResponse, MirrorError> {
    let copy = prepare(source, target, scratch_root, progress)?;
    let Work { outcome, tracks } = work(source, &copy, options, true, progress)?;
    let Copy { dir, scratch } = copy;

    let mut response = api::SyncToLibraryResponse {
        target_path: dir.display().to_string(),
        playlists_changed: outcome.changes.len() as u32,
        tracks_missing: outcome.missing.len() as u32,
        origins_changed: outcome.origins_changed as u32,
        tracks_added: tracks.added.len() as u32,
        tracks_changed: tracks.changed.len() as u32,
        files_copied: tracks.copied.len() as u32,
        backup_path: String::new(),
    };
    // The databases that differ now: the playlists and tracks are in `m.db`, the origins
    // in their own.
    let mut changed = Vec::new();
    if !outcome.changes.is_empty() || !tracks.added.is_empty() || !tracks.changed.is_empty() {
        changed.push(MAIN_DATABASE);
    }
    if outcome.origins_changed > 0 {
        changed.push(origins::FILE);
    }
    if !changed.is_empty() {
        let databases = dir.join(DATABASE_DIR);
        let result = install(&databases, &scratch, &changed, progress);
        let _ = fs::remove_dir_all(&scratch);
        match result {
            Ok(backup) => response.backup_path = backup.display().to_string(),
            Err(error) => {
                remove_files(&tracks.copied);
                return Err(error);
            }
        }
    } else {
        let _ = fs::remove_dir_all(&scratch);
    }
    Ok(response)
}

/// Puts the changed databases from the copy in `scratch` into the library's `databases`
/// folder, after keeping the ones they replace in a backup folder, which is returned.
fn install(
    databases: &Path,
    scratch: &Path,
    changed: &[&str],
    progress: &Progress,
) -> Result<PathBuf, MirrorError> {
    // Another program may have started writing since the copy was made.
    if is_busy(databases) {
        return Err(MirrorError::Busy);
    }
    // Keep what is about to be replaced. (One backup, the latest.)
    let backup = databases.join(BACKUP_DIR);
    let size = |path: PathBuf| fs::metadata(path).map_or(0, |metadata| metadata.len());
    progress.phase(
        Phase::Writing,
        changed
            .iter()
            .map(|name| size(databases.join(name)) + size(scratch.join(DATABASE_DIR).join(name)))
            .sum(),
        0,
    );
    let _ = fs::remove_dir_all(&backup);
    fs::create_dir_all(&backup)?;
    for name in changed {
        if databases.join(name).is_file() {
            progress.copy(&databases.join(name), &backup.join(name), false)?;
        }
    }

    // Each is written beside the original and moved over it, so that it is never half
    // written. (If one fails, the ones before it are put back.)
    let mut replaced: Vec<&str> = Vec::new();
    for name in changed {
        let temporary = databases.join(format!(".{name}.syncing"));
        let result = progress
            .copy(&scratch.join(DATABASE_DIR).join(name), &temporary, false)
            .and_then(|_| fs::rename(&temporary, databases.join(name)));
        if let Err(error) = result {
            let _ = fs::remove_file(&temporary);
            for done in replaced {
                let kept = backup.join(done);
                if kept.is_file() {
                    let _ = fs::copy(kept, databases.join(done));
                } else {
                    let _ = fs::remove_file(databases.join(done));
                }
            }
            return Err(error.into());
        }
        replaced.push(name);
    }
    Ok(backup)
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
pub fn match_tracks(source: &Library, target: &Library) -> Vec<Option<usize>> {
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

    // The origins the open library has for tracks that the other one has too. It is the
    // open library that counts: they replace what the other one has.
    let source_origins = source.origins();
    let target_origins = target.origins();
    let mut origins_to_write = Vec::new();
    for (index, track) in source.tracks.iter().enumerate() {
        let (Some(key), Some(other)) = (track.origin_key.as_ref(), matched[index]) else {
            continue;
        };
        let (Some(origin), Some(other_key)) = (
            source_origins.get(key),
            target.tracks[other].origin_key.as_ref(),
        ) else {
            continue;
        };
        if target_origins.get(other_key) != Some(origin) {
            origins_to_write.push((other_key.clone(), origin.clone()));
        }
    }
    let origins_changed = if origins_to_write.is_empty() {
        0
    } else {
        target.save_origins(&origins_to_write, true)?
    };

    Ok(Outcome {
        changes,
        missing,
        origins_changed,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    use rusqlite::Connection;

    /// Options that only say whether to delete what only the other library has.
    fn remove(remove_missing: bool) -> Options {
        Options {
            remove_missing,
            ..Options::default()
        }
    }

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
            streamingSource TEXT, originDatabaseUuid TEXT, originTrackId INTEGER, albumArtId INTEGER,
            rating INTEGER, filename TEXT, UNIQUE (originDatabaseUuid, originTrackId), UNIQUE (path));
        CREATE TABLE AlbumArt (id INTEGER PRIMARY KEY AUTOINCREMENT, hash TEXT, albumArt BLOB);
        CREATE TABLE PerformanceData (trackId INTEGER PRIMARY KEY, trackData BLOB, beatData BLOB);
        CREATE TRIGGER trigger_after_insert_Track_insert_performance_data AFTER INSERT ON Track
        BEGIN INSERT INTO PerformanceData(trackId) VALUES(NEW.id); END;
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

        let diff = diff(&source, &stick, &temp.0.join("scratch"), remove(false)).unwrap();
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

        let done = push(
            &source,
            &stick,
            &temp.0.join("scratch"),
            remove(false),
            &Progress::default(),
        )
        .unwrap();
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
        let again = push(
            &source,
            &stick,
            &temp.0.join("scratch"),
            remove(false),
            &Progress::default(),
        )
        .unwrap();
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

        let diff = diff(&source, &stick, &temp.0.join("scratch"), remove(true)).unwrap();
        let mine = diff
            .changed_playlists
            .iter()
            .find(|p| p.name == "Mine")
            .unwrap();
        assert_eq!(mine.kind(), api::PlaylistChangeKind::Removed);

        push(
            &source,
            &stick,
            &temp.0.join("scratch"),
            remove(true),
            &Progress::default(),
        )
        .unwrap();
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
        let diff = diff(&source, &stick, &temp.0.join("scratch"), remove(false)).unwrap();
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
    fn the_origins_of_matching_tracks_go_to_the_other_library_too() {
        let temp = TempDir::new("origins");
        let (pc, stick) = libraries(&temp.0);
        let source = open(&pc, &temp.0.join("source-scratch"));
        let key = |id: i64| ("db".to_string(), id);
        // Tracks a (1), b (2) and c (3); the stick has a and b, and c has no match there.
        source
            .save_origins(
                &[
                    (key(1), "/pc/Music/a.flac".into()),
                    (key(2), "/pc/Music/b.flac".into()),
                    (key(3), "/pc/Music/c.flac".into()),
                ],
                false,
            )
            .unwrap();
        // The stick already has a different origin for b.
        let target = open(&stick, &temp.0.join("stick-scratch"));
        target
            .save_origins(&[(key(2), "/old/b.flac".into())], false)
            .unwrap();
        drop(target);
        fs::copy(
            temp.0
                .join("stick-scratch")
                .join(DATABASE_DIR)
                .join(origins::FILE),
            stick.join(DATABASE_DIR).join(origins::FILE),
        )
        .unwrap();

        let scratch = temp.0.join("scratch");
        let found = diff(&source, &stick, &scratch, remove(false)).unwrap();
        assert_eq!(
            found.origins_changed, 2,
            "a is new and b differs; c has no match"
        );

        let done = push(
            &source,
            &stick,
            &scratch,
            remove(false),
            &Progress::default(),
        )
        .unwrap();
        assert_eq!(done.origins_changed, 2);
        let written = origins::Origins::open(&stick.join(DATABASE_DIR).join(origins::FILE))
            .unwrap()
            .all()
            .unwrap();
        assert_eq!(written.len(), 2);
        assert_eq!(written[&key(1)], "/pc/Music/a.flac");
        assert_eq!(
            written[&key(2)],
            "/pc/Music/b.flac",
            "the open library's replaces"
        );
        let backup = stick.join(DATABASE_DIR).join(BACKUP_DIR);
        assert!(backup.join(origins::FILE).is_file());
        assert!(
            backup.join(MAIN_DATABASE).is_file(),
            "the playlists changed as well"
        );

        let again = push(
            &source,
            &stick,
            &scratch,
            remove(false),
            &Progress::default(),
        )
        .unwrap();
        assert_eq!((again.playlists_changed, again.origins_changed), (0, 0));
    }

    #[test]
    fn only_the_origins_differing_writes_only_their_file() {
        let temp = TempDir::new("origins-only");
        let tracks = [("a.flac", 1)];
        let playlists = "INSERT INTO Playlist (id, title, parentListId, isPersisted, nextListId) VALUES (1, 'Mix', 0, 1, 0);";
        let pc = make(&temp.0, "pc", &tracks, playlists);
        let stick = make(&temp.0, "stick", &tracks, playlists);
        let source = open(&pc, &temp.0.join("source-scratch"));
        source
            .save_origins(&[(("db".to_string(), 1), "/pc/a.flac".into())], false)
            .unwrap();
        let before = fs::read(stick.join(DATABASE_DIR).join(MAIN_DATABASE)).unwrap();

        let done = push(
            &source,
            &stick,
            &temp.0.join("scratch"),
            remove(false),
            &Progress::default(),
        )
        .unwrap();
        assert_eq!((done.playlists_changed, done.origins_changed), (0, 1));
        assert_eq!(
            fs::read(stick.join(DATABASE_DIR).join(MAIN_DATABASE)).unwrap(),
            before
        );
        assert!(stick.join(DATABASE_DIR).join(origins::FILE).is_file());
    }

    /// Like `open`, for the library whose working copy is read: the copy of its database
    /// is `scratch`'s, as a library's cache is.
    fn open_source(dir: &Path, scratch: &Path) -> Library {
        fs::create_dir_all(scratch.join(DATABASE_DIR)).unwrap();
        fs::copy(
            dir.join(DATABASE_DIR).join(MAIN_DATABASE),
            scratch.join(DATABASE_DIR).join(MAIN_DATABASE),
        )
        .unwrap();
        open(scratch, scratch);
        Library::open_copy(
            &scratch.join(DATABASE_DIR).join(MAIN_DATABASE),
            dir.to_path_buf(),
            scratch.to_path_buf(),
        )
        .unwrap()
    }

    /// The open library `pc` with three tracks that have files, and the `stick` with the
    /// first two and the playlist for them.
    fn with_files(root: &Path) -> (PathBuf, PathBuf) {
        let pc = make(
            root,
            "pc",
            &[("a.flac", 1), ("b.flac", 2), ("c.flac", 3)],
            "UPDATE Track SET artist = 'AC/DC', album = 'Back: In Black?', rating = 80 WHERE id = 1;
             UPDATE Track SET rating = 40 WHERE id = 2;
             UPDATE Track SET artist = 'Zed', album = 'Z', rating = 100 WHERE id = 3;
             INSERT INTO Playlist (id, title, parentListId, isPersisted, nextListId) VALUES (1, 'Mix', 0, 1, 0);
             INSERT INTO PlaylistEntity VALUES (1, 1, 1, 'db', 2, 0), (2, 1, 3, 'db', 0, 0);",
        );
        fs::create_dir_all(root.join("pc/Music")).unwrap();
        fs::write(root.join("pc/Music/a.flac"), b"aaaa").unwrap();
        fs::write(root.join("pc/Music/b.flac"), b"bb").unwrap();
        fs::write(root.join("pc/Music/c.flac"), b"cccccc").unwrap();
        let stick = make(
            root,
            "stick",
            &[("a.flac", 1), ("b.flac", 2)],
            "UPDATE Track SET rating = 20 WHERE id = 1; UPDATE Track SET rating = 40 WHERE id = 2;",
        );
        (pc, stick)
    }

    fn tracks(options: track_sync::Options) -> Options {
        Options {
            tracks: options,
            ..Options::default()
        }
    }

    fn ratings_and_paths(library: &Library) -> Vec<(String, Option<u32>, String)> {
        library
            .tracks
            .iter()
            .map(|track| {
                (
                    track.title.clone(),
                    track.rating,
                    track.relative_path.clone(),
                )
            })
            .collect()
    }

    #[test]
    fn stars_are_synced_and_listed() {
        let temp = TempDir::new("stars");
        let (pc, stick) = with_files(&temp.0);
        let source = open_source(&pc, &temp.0.join("source"));
        let options = tracks(track_sync::Options {
            stars: true,
            ..Default::default()
        });

        let found = diff(&source, &stick, &temp.0.join("scratch"), options).unwrap();
        assert_eq!(found.tracks_changed, 1, "b has the same stars already");
        let change = &found.changed_tracks[0];
        assert_eq!(change.track.as_ref().unwrap().title, "a.flac");
        let field = &change.fields[0];
        assert_eq!(
            (
                field.field.as_str(),
                field.source_value.as_str(),
                field.working_value.as_str()
            ),
            ("stars", "1", "4")
        );
        assert_eq!((found.tracks_added, found.files_to_copy), (0, 0));

        let done = push(
            &source,
            &stick,
            &temp.0.join("scratch"),
            options,
            &Progress::default(),
        )
        .unwrap();
        assert_eq!(done.tracks_changed, 1);
        let after = open(&stick, &temp.0.join("check"));
        assert_eq!(
            ratings_and_paths(&after),
            [
                (
                    "a.flac".to_string(),
                    Some(80),
                    "../Music/a.flac".to_string()
                ),
                (
                    "b.flac".to_string(),
                    Some(40),
                    "../Music/b.flac".to_string()
                ),
            ]
        );
        assert_eq!(
            push(
                &source,
                &stick,
                &temp.0.join("scratch"),
                options,
                &Progress::default()
            )
            .unwrap()
            .tracks_changed,
            0
        );
    }

    #[test]
    fn locations_are_overwritten_with_the_open_librarys() {
        let temp = TempDir::new("locations");
        let (pc, stick) = with_files(&temp.0);
        // The paths differ in the open library, and one is already taken on the stick by
        // another track.
        let connection = Connection::open(pc.join(DATABASE_DIR).join(MAIN_DATABASE)).unwrap();
        connection
            .execute_batch(
                "UPDATE Track SET path = '../Elsewhere/a.flac' WHERE id = 1;
                 UPDATE Track SET path = '../Music/zzz.flac' WHERE id = 2;",
            )
            .unwrap();
        drop(connection);
        Connection::open(stick.join(DATABASE_DIR).join(MAIN_DATABASE))
            .unwrap()
            .execute_batch(
                "INSERT INTO Track (id, title, path, originDatabaseUuid, originTrackId)
                 VALUES (9, 'zzz', '../Music/zzz.flac', 'db', 9);",
            )
            .unwrap();
        let source = open_source(&pc, &temp.0.join("source"));
        let options = tracks(track_sync::Options {
            locations: true,
            ..Default::default()
        });

        let found = diff(&source, &stick, &temp.0.join("scratch"), options).unwrap();
        assert_eq!(found.tracks_changed, 1);
        let field = &found.changed_tracks[0].fields[0];
        assert_eq!(
            (
                field.field.as_str(),
                field.source_value.as_str(),
                field.working_value.as_str()
            ),
            ("path", "../Music/a.flac", "../Elsewhere/a.flac")
        );
        assert_eq!(
            found.location_conflicts, 1,
            "b would get a's old path, which a has"
        );

        push(
            &source,
            &stick,
            &temp.0.join("scratch"),
            options,
            &Progress::default(),
        )
        .unwrap();
        let after = open(&stick, &temp.0.join("check"));
        let paths: Vec<_> = after
            .tracks
            .iter()
            .map(|t| t.relative_path.as_str())
            .collect();
        assert_eq!(
            paths,
            [
                "../Elsewhere/a.flac",
                "../Music/b.flac",
                "../Music/zzz.flac"
            ]
        );
    }

    #[test]
    fn tracks_are_copied_into_artist_and_album_folders_and_added() {
        let temp = TempDir::new("copy");
        let (pc, stick) = with_files(&temp.0);
        let source = open_source(&pc, &temp.0.join("source"));
        let options = tracks(track_sync::Options {
            stars: true,
            copy_tracks: true,
            ..Default::default()
        });
        let tracks_dir = temp.0.join("stick").join(track_sync::TRACKS_DIR);

        // The diff says what would happen and copies nothing.
        let found = diff(&source, &stick, &temp.0.join("scratch"), options).unwrap();
        assert_eq!(
            (found.tracks_added, found.added_tracks[0].title.as_str()),
            (1, "c.flac")
        );
        assert_eq!((found.files_to_copy, found.bytes_to_copy), (3, 12));
        assert_eq!(found.tracks_changed, 2, "a: stars and path; b: path");
        let new_paths: Vec<_> = found
            .changed_tracks
            .iter()
            .flat_map(|change| &change.fields)
            .filter(|field| field.field == "path")
            .map(|field| field.working_value.as_str())
            .collect();
        assert_eq!(
            new_paths,
            [
                "../Engine-Tracks/AC_DC/Back_ In Black_/a.flac",
                "../Engine-Tracks/Unknown Artist/Unknown Album/b.flac",
            ]
        );
        assert!(!tracks_dir.exists(), "nothing is copied by a diff");
        // The added track is in the playlist, which a sync of the playlists alone could not do.
        let mix = found
            .changed_playlists
            .iter()
            .find(|p| p.name == "Mix")
            .unwrap();
        assert_eq!((mix.tracks_added, mix.tracks_missing), (2, 0));

        let done = push(
            &source,
            &stick,
            &temp.0.join("scratch"),
            options,
            &Progress::default(),
        )
        .unwrap();
        assert_eq!(
            (done.tracks_added, done.tracks_changed, done.files_copied),
            (1, 2, 3)
        );
        assert_eq!(
            fs::read(tracks_dir.join("AC_DC/Back_ In Black_/a.flac")).unwrap(),
            b"aaaa"
        );
        assert_eq!(
            fs::read(tracks_dir.join("Unknown Artist/Unknown Album/b.flac")).unwrap(),
            b"bb"
        );
        assert_eq!(
            fs::read(tracks_dir.join("Zed/Z/c.flac")).unwrap(),
            b"cccccc"
        );

        let after = open(&stick, &temp.0.join("check"));
        assert_eq!(
            ratings_and_paths(&after),
            [
                (
                    "a.flac".into(),
                    Some(80),
                    "../Engine-Tracks/AC_DC/Back_ In Black_/a.flac".into()
                ),
                (
                    "b.flac".into(),
                    Some(40),
                    "../Engine-Tracks/Unknown Artist/Unknown Album/b.flac".into()
                ),
                (
                    "c.flac".into(),
                    Some(100),
                    "../Engine-Tracks/Zed/Z/c.flac".into()
                ),
            ]
        );
        // The added track keeps its origin, so that it is the same track from now on.
        assert_eq!(after.tracks[2].origin_key, Some(("db".to_string(), 3)));
        let mix = after
            .playlists()
            .iter()
            .find(|p| p.title == "Mix")
            .map(|p| p.tracks.clone());
        assert_eq!(mix, Some(vec![0, 2]));
        let performance: i64 = Connection::open(stick.join(DATABASE_DIR).join(MAIN_DATABASE))
            .unwrap()
            .query_row("SELECT count(*) FROM PerformanceData", [], |row| row.get(0))
            .unwrap();
        assert_eq!(performance, 3);

        // A second time everything is there.
        let again = push(
            &source,
            &stick,
            &temp.0.join("scratch"),
            options,
            &Progress::default(),
        )
        .unwrap();
        assert_eq!(
            (
                again.tracks_added,
                again.tracks_changed,
                again.files_copied,
                again.playlists_changed
            ),
            (0, 0, 0, 0)
        );
        assert_eq!(again.backup_path, "");
    }

    #[test]
    fn files_that_would_share_a_name_get_a_number_and_missing_files_are_skipped() {
        let temp = TempDir::new("names");
        let pc = make(
            &temp.0,
            "pc",
            &[("x.mp3", 1), ("gone.mp3", 2), ("y.mp3", 3)],
            "UPDATE Track SET artist = 'Same', album = 'Same';
             UPDATE Track SET path = '../One/x.mp3' WHERE id = 1;
             UPDATE Track SET path = '../Two/X.MP3' WHERE id = 3;",
        );
        fs::create_dir_all(temp.0.join("pc/One")).unwrap();
        fs::create_dir_all(temp.0.join("pc/Two")).unwrap();
        fs::write(temp.0.join("pc/One/x.mp3"), b"1").unwrap();
        fs::write(temp.0.join("pc/Two/X.MP3"), b"22").unwrap();
        let stick = make(&temp.0, "stick", &[], "");
        let source = open_source(&pc, &temp.0.join("source"));
        let options = tracks(track_sync::Options {
            copy_tracks: true,
            ..Default::default()
        });

        let done = push(
            &source,
            &stick,
            &temp.0.join("scratch"),
            options,
            &Progress::default(),
        )
        .unwrap();
        assert_eq!((done.tracks_added, done.files_copied), (2, 2));
        let found = diff(&source, &stick, &temp.0.join("scratch"), options).unwrap();
        assert_eq!(found.files_missing, 1, "gone.mp3 has no file");
        assert_eq!(found.tracks_added, 0);

        let after = open(&stick, &temp.0.join("check"));
        let paths: Vec<_> = after
            .tracks
            .iter()
            .map(|t| t.relative_path.as_str())
            .collect();
        assert_eq!(
            paths,
            [
                "../Engine-Tracks/Same/Same/x.mp3",
                "../Engine-Tracks/Same/Same/X (2).MP3",
            ],
            "exFAT ignores case, so these are the same name"
        );
    }

    #[test]
    fn the_progress_follows_the_phases_and_the_bytes() {
        use std::sync::atomic::{AtomicBool, Ordering};

        let temp = TempDir::new("progress");
        let (pc, stick) = with_files(&temp.0);
        // A file big enough to be copied in more than one step.
        fs::write(
            temp.0.join("pc/Music/c.flac"),
            vec![1u8; 3 * 1024 * 1024 + 5],
        )
        .unwrap();
        let source = open_source(&pc, &temp.0.join("source"));
        let options = tracks(track_sync::Options {
            copy_tracks: true,
            ..Default::default()
        });
        let progress = Progress::default();
        let finished = AtomicBool::new(false);

        let seen = std::thread::scope(|scope| {
            let sampler = scope.spawn(|| {
                let mut seen = Vec::new();
                while !finished.load(Ordering::Relaxed) {
                    seen.push(progress.snapshot());
                    std::thread::yield_now();
                }
                seen
            });
            let _running = progress.begin();
            let done = push(&source, &stick, &temp.0.join("scratch"), options, &progress);
            let last = progress.snapshot();
            finished.store(true, Ordering::Relaxed);
            assert!(done.is_ok());
            (sampler.join().unwrap(), last)
        });
        let (seen, last) = seen;

        // Whatever was seen made sense: never more done than to do.
        assert!(seen.iter().all(|s| s.done_bytes <= s.total_bytes));
        assert!(seen.iter().all(|s| s.done_files <= s.total_files));
        // The copying of the tracks was seen: 3 files, the big one and two small ones.
        let copying: Vec<_> = seen
            .iter()
            .filter(|s| s.phase == Phase::CopyingTracks)
            .collect();
        assert!(!copying.is_empty(), "the copying phase was not seen");
        assert!(copying.iter().all(|s| s.total_files == 3));
        assert!(
            copying
                .iter()
                .all(|s| s.total_bytes == 4 + 2 + 3 * 1024 * 1024 + 5)
        );
        // At the end the databases were written, all of it.
        assert_eq!(last.phase, Phase::Writing);
        assert!(last.total_bytes > 0 && last.done_bytes == last.total_bytes);
    }

    #[cfg(unix)]
    #[test]
    fn a_failed_sync_takes_the_copied_files_away_again() {
        use std::os::unix::fs::PermissionsExt;

        let temp = TempDir::new("failed");
        let (pc, stick) = with_files(&temp.0);
        let source = open_source(&pc, &temp.0.join("source"));
        let options = tracks(track_sync::Options {
            copy_tracks: true,
            ..Default::default()
        });
        let databases = stick.join(DATABASE_DIR);
        let tracks_dir = temp.0.join("stick").join(track_sync::TRACKS_DIR);
        let before = fs::read(databases.join(MAIN_DATABASE)).unwrap();

        // The files are copied before the database is put in place; when that cannot be
        // done (the database folder can't be written to), they are taken away again.
        fs::set_permissions(&databases, fs::Permissions::from_mode(0o555)).unwrap();
        let result = push(
            &source,
            &stick,
            &temp.0.join("scratch"),
            options,
            &Progress::default(),
        );
        fs::set_permissions(&databases, fs::Permissions::from_mode(0o755)).unwrap();

        assert!(result.is_err());
        let left: Vec<_> = walk(&tracks_dir);
        assert!(left.is_empty(), "files were left behind: {left:?}");
        assert_eq!(fs::read(databases.join(MAIN_DATABASE)).unwrap(), before);
    }

    /// The files below `folder`.
    fn walk(folder: &Path) -> Vec<PathBuf> {
        let Ok(entries) = fs::read_dir(folder) else {
            return Vec::new();
        };
        entries
            .flatten()
            .flat_map(|entry| {
                if entry.path().is_dir() {
                    walk(&entry.path())
                } else {
                    vec![entry.path()]
                }
            })
            .collect()
    }

    #[test]
    fn refuses_the_open_library_a_non_library_and_a_busy_one() {
        let temp = TempDir::new("refuse");
        let (pc, stick) = libraries(&temp.0);
        let source = open(&pc, &temp.0.join("source-scratch"));
        let scratch = temp.0.join("scratch");

        assert!(matches!(
            diff(&source, &pc, &scratch, remove(false)),
            Err(MirrorError::SameLibrary)
        ));
        assert!(matches!(
            diff(&source, &temp.0.join("nowhere"), &scratch, remove(false)),
            Err(MirrorError::NotALibrary(_))
        ));
        fs::write(stick.join(DATABASE_DIR).join("m.db-journal"), b"x").unwrap();
        assert!(matches!(
            diff(&source, &stick, &scratch, remove(false)),
            Err(MirrorError::Busy)
        ));
    }
}
