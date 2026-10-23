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

use crate::cache;
use crate::cache::DATABASE_DIR;
use crate::library::{self, EditError, Library, OpenError, Playlist};
use crate::new_library;
use crate::origins;
use crate::progress::{Phase, Progress};
use crate::sync::is_busy;
use crate::track_sync;

const MAIN_DATABASE: &str = "m.db";
const SCRATCH_DIR: &str = "library-sync";
/// Where the working copy it replaces is kept, in the cache folder, by a sync the other
/// way round.
const BACK_SYNC_BACKUP: &str = "before-back-sync";
/// Where the other library's databases that a sync replaces are kept, in the open library's
/// cache folder (the other library, which may be a player's stick, gets no backup folder).
const LIBRARY_SYNC_BACKUP: &str = "before-library-sync";
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
    /// The other way round: the other library is the master and the open library's
    /// working copy is changed.
    pub reverse: bool,
    /// If there is no library at the path, make one there.
    pub create: bool,
}

/// What the work on the copy found.
struct Outcome {
    changes: Vec<api::PlaylistChange>,
    /// Streaming tracks that only the other library had, which were deleted from it.
    tracks_removed: usize,
    /// Tracks of the open library that were left out of a playlist because the other
    /// library does not have them (indices into its tracks).
    missing: Vec<usize>,
    /// How many origins of the other library are new or were different.
    origins_changed: usize,
    /// Those origins by track, for the lists: what the other library has and what it gets.
    changed_origins: Vec<api::TrackChange>,
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

/// Where the result of a sync goes.
struct Place {
    /// The `Database2` folder the changed databases are put in.
    databases: PathBuf,
    /// Where the origins go, if not with the other databases: a library keeps them in the
    /// `open-engine` folder beside its `Engine Library` folder.
    origins: Option<PathBuf>,
    /// The library the databases are put in, if it is one (and not a working copy): the
    /// origins that earlier versions left in its `Database2` are taken away.
    library: Option<PathBuf>,
    /// Where the ones they replace are kept.
    backup: PathBuf,
    /// Whether another program writing there is a reason to refuse (it is for a library,
    /// not for a working copy).
    check_busy: bool,
    /// The databases are not replaced but made: there is no library there yet.
    create: bool,
    /// The library folder to take away again if making the library fails (when it was
    /// made for this).
    undo: Option<PathBuf>,
}

/// Everything a sync starts from.
struct Prepared {
    /// The folder the work is done in, with `target` and `master` in it.
    base: PathBuf,
    /// The other library's `Engine Library` folder.
    other: PathBuf,
    /// The library that is changed, as a copy to work on.
    copy: Copy,
    place: Place,
    /// There is no library at the other path and a new one is made.
    new: bool,
    /// The library whose state is wanted, if it is not the open one: the other library, as
    /// a copy, for a sync the other way round.
    master: Option<Library>,
}

impl Prepared {
    /// The library whose state is wanted.
    fn master<'a>(&'a self, open: &'a Library) -> &'a Library {
        self.master.as_ref().unwrap_or(open)
    }
}

/// Where the main database and the origins are in a library: a working copy's `Database2`
/// folder has both, a library at `dir` keeps the origins beside its `Engine Library` folder.
#[derive(Clone)]
struct Databases {
    main: PathBuf,
    origins: PathBuf,
}

impl Databases {
    fn of_library(dir: &Path) -> Self {
        Self {
            main: dir.join(DATABASE_DIR).join(MAIN_DATABASE),
            origins: cache::source_file(dir, origins::FILE),
        }
    }

    fn of_working_copy(databases: &Path) -> Self {
        Self {
            main: databases.join(MAIN_DATABASE),
            origins: databases.join(origins::FILE),
        }
    }

    fn files(&self) -> [(&str, &Path); 2] {
        [
            (MAIN_DATABASE, self.main.as_path()),
            (origins::FILE, self.origins.as_path()),
        ]
    }
}

/// Copies the main database and the origins to the same place in `to`, which is made.
fn copy_databases(from: &Databases, to: &Path, progress: &Progress) -> Result<(), MirrorError> {
    fs::create_dir_all(to.join(DATABASE_DIR))?;
    for (name, path) in from.files() {
        if path.is_file() {
            progress.copy(path, &to.join(DATABASE_DIR).join(name), false)?;
        }
    }
    Ok(())
}

/// What the files to be copied add up to.
fn databases_size(databases: &Databases) -> u64 {
    databases
        .files()
        .iter()
        .map(|(_, path)| fs::metadata(path).map_or(0, |metadata| metadata.len()))
        .sum()
}

fn prepare(
    open: &Library,
    other: &Path,
    scratch_root: &Path,
    options: Options,
    progress: &Progress,
) -> Result<Prepared, MirrorError> {
    let reverse = options.reverse;
    let (dir, new) = match library::locate(other) {
        Some(dir) => (dir, false),
        // Somewhere without a library: a library is made there, if that is wanted. (The
        // other way round there would be nothing to copy from.)
        None if options.create && !reverse && other.is_dir() => {
            (new_library::library_dir_for(other), true)
        }
        None => return Err(MirrorError::NotALibrary(other.into())),
    };
    let same = match (fs::canonicalize(&dir), fs::canonicalize(&open.dir)) {
        (Ok(a), Ok(b)) => a == b,
        _ => dir == open.dir,
    };
    if same {
        return Err(MirrorError::SameLibrary);
    }
    let other_databases = dir.join(DATABASE_DIR);
    if !new && is_busy(&other_databases) {
        return Err(MirrorError::Busy);
    }

    let base = scratch_root.join(SCRATCH_DIR);
    let _ = fs::remove_dir_all(&base);
    let (target_scratch, master_scratch) = (base.join("target"), base.join("master"));

    // What is changed is a copy of the other library's databases; the other way round it is
    // a copy of the working copy of the open one, and the other library is only read.
    let other_files = Databases::of_library(&dir);
    let (changed, place) = if reverse {
        let working = open.cache.join(DATABASE_DIR);
        let place = Place {
            databases: working.clone(),
            origins: None,
            library: None,
            backup: open.cache.join(BACK_SYNC_BACKUP),
            check_busy: false,
            create: false,
            undo: None,
        };
        (Databases::of_working_copy(&working), place)
    } else {
        let place = Place {
            databases: other_databases.clone(),
            origins: Some(origins::source_file(&dir)),
            library: Some(dir.clone()),
            backup: open.cache.join(LIBRARY_SYNC_BACKUP),
            check_busy: !new,
            create: new,
            undo: (new && !dir.exists()).then(|| dir.clone()),
        };
        (other_files.clone(), place)
    };
    progress.phase(
        Phase::Preparing,
        databases_size(&changed)
            + if reverse {
                databases_size(&other_files)
            } else {
                0
            },
        0,
    );
    if new {
        // Nothing to copy: the schema of the open library's databases, without rows.
        new_library::create_empty_databases(&open.cache.join(DATABASE_DIR), &target_scratch)
            .map_err(|error| MirrorError::Io(io::Error::other(error)))?;
    } else {
        copy_databases(&changed, &target_scratch, progress)?;
    }
    let master = if reverse {
        copy_databases(&other_files, &master_scratch, progress)?;
        Some(Library::open_copy(
            &master_scratch.join(DATABASE_DIR).join(MAIN_DATABASE),
            dir.clone(),
            master_scratch,
        )?)
    } else {
        None
    };
    Ok(Prepared {
        base,
        other: dir.clone(),
        copy: Copy {
            // The folder the changed library's tracks are relative to (and the copied
            // files go beside).
            dir: if reverse { open.dir.clone() } else { dir },
            scratch: target_scratch,
        },
        place,
        new,
        master,
    })
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

/// What making the other library's playlists (and, if asked, tracks) the same as the open
/// library's would change, or with `reverse` the open library's the same as the other's.
pub fn diff(
    open: &Library,
    other: &Path,
    scratch_root: &Path,
    options: Options,
) -> Result<api::GetLibrarySyncDiffResponse, MirrorError> {
    // (A diff is quick and nobody asks how far it is.)
    let progress = Progress::default();
    let prepared = prepare(open, other, scratch_root, options, &progress)?;
    let master = prepared.master(open);
    let Work { outcome, tracks } = work(master, &prepared.copy, options, false, &progress)?;
    let dir = prepared.other.display().to_string();

    let mut changes = outcome.changes;
    changes.sort_by(|a, b| a.name.cmp(&b.name));
    let playlists_changed = changes.len() as u32;
    changes.truncate(MAX_LISTED);
    let reference = |index: usize| {
        let track = &master.tracks[index];
        api::TrackRef {
            title: track.title.clone(),
            artist: track.artist.clone(),
            path: track.relative_path.clone(),
        }
    };
    let response = api::GetLibrarySyncDiffResponse {
        target_path: dir,
        playlists_changed,
        changed_playlists: changes,
        tracks_missing: outcome.missing.len() as u32,
        origins_changed: outcome.origins_changed as u32,
        changed_origins: outcome
            .changed_origins
            .iter()
            .take(MAX_LISTED)
            .cloned()
            .collect(),
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
        tracks_removed: outcome.tracks_removed as u32,
        changed_tracks: tracks.changed.iter().take(MAX_LISTED).cloned().collect(),
        files_to_copy: tracks.files_to_copy,
        bytes_to_copy: tracks.bytes_to_copy,
        files_missing: tracks.files_missing,
        location_conflicts: tracks.location_conflicts,
        target_is_new: prepared.new,
    };
    let base = prepared.base.clone();
    drop(prepared);
    let _ = fs::remove_dir_all(base);
    Ok(response)
}

/// Makes the other library the same as the open one: the changes are made on a copy of its
/// `m.db`, which then replaces the original, which is kept in a backup folder. Audio files
/// are copied first (when asked for), and taken away again if the sync then fails. With
/// `reverse` it is the open library's working copy that is changed, from the other library;
/// the open library then has to be read again.
pub fn push(
    open: &Library,
    other: &Path,
    scratch_root: &Path,
    options: Options,
    progress: &Progress,
) -> Result<api::SyncToLibraryResponse, MirrorError> {
    let prepared = prepare(open, other, scratch_root, options, progress)?;
    let master = prepared.master(open);
    let Work { outcome, tracks } = work(master, &prepared.copy, options, true, progress)?;
    let Prepared {
        base,
        other: other_dir,
        copy,
        place,
        new,
        master,
    } = prepared;
    drop(master);

    let mut response = api::SyncToLibraryResponse {
        target_path: other_dir.display().to_string(),
        playlists_changed: outcome.changes.len() as u32,
        tracks_missing: outcome.missing.len() as u32,
        origins_changed: outcome.origins_changed as u32,
        tracks_added: tracks.added.len() as u32,
        tracks_changed: tracks.changed.len() as u32,
        tracks_removed: outcome.tracks_removed as u32,
        files_copied: tracks.copied.len() as u32,
        backup_path: String::new(),
        created: new,
    };
    // The databases that differ now: the playlists and tracks are in `m.db`, the origins
    // in their own.
    let mut changed = Vec::new();
    if !outcome.changes.is_empty()
        || outcome.tracks_removed > 0
        || !tracks.added.is_empty()
        || !tracks.changed.is_empty()
    {
        changed.push(MAIN_DATABASE);
    }
    if outcome.origins_changed > 0 {
        changed.push(origins::FILE);
    }
    // A library that is made gets all its databases, changed or not.
    let made: Vec<String> = if new {
        fs::read_dir(copy.scratch.join(DATABASE_DIR))?
            .flatten()
            .filter_map(|entry| entry.file_name().into_string().ok())
            .filter(|name| name.ends_with(".db"))
            .collect()
    } else {
        Vec::new()
    };
    if new {
        changed = made.iter().map(String::as_str).collect();
    }
    if !changed.is_empty() {
        let result = install(&place, &copy.scratch, &changed, progress);
        let _ = fs::remove_dir_all(&base);
        match result {
            Ok(backup) => {
                if !place.create {
                    response.backup_path = backup.display().to_string();
                }
                if options.reverse {
                    // The working copy has changes the source does not have.
                    cache::mark_edited(&open.cache)?;
                }
            }
            Err(error) => {
                remove_files(&tracks.copied);
                if let Some(made) = &place.undo {
                    let _ = fs::remove_dir_all(made);
                }
                return Err(error);
            }
        }
    } else {
        let _ = fs::remove_dir_all(&base);
    }
    Ok(response)
}

/// Puts the changed databases from the copy in `scratch` into the library's `databases`
/// folder (the origins to theirs), after keeping the ones they replace in a backup folder,
/// which is returned.
fn install(
    place: &Place,
    scratch: &Path,
    changed: &[&str],
    progress: &Progress,
) -> Result<PathBuf, MirrorError> {
    let databases = place.databases.as_path();
    // Where each database is put: the origins have a place of their own in a library.
    let destination = |name: &str| match (&place.origins, name == origins::FILE) {
        (Some(origins), true) => origins.clone(),
        _ => databases.join(name),
    };
    // Where each is now: a library's origins may still be where earlier versions kept them.
    let current = |name: &str| match &place.library {
        Some(library) => cache::source_file(library, name),
        None => databases.join(name),
    };
    // Another program may have started writing since the copy was made.
    if place.check_busy && is_busy(databases) {
        return Err(MirrorError::Busy);
    }
    if place.create {
        fs::create_dir_all(databases)?;
    }
    // Keep what is about to be replaced. (One backup, the latest. A new library replaces
    // nothing.)
    let backup = place.backup.clone();
    let size = |path: PathBuf| fs::metadata(path).map_or(0, |metadata| metadata.len());
    progress.phase(
        Phase::Writing,
        changed
            .iter()
            .map(|name| size(current(name)) + size(scratch.join(DATABASE_DIR).join(name)))
            .sum(),
        0,
    );
    if !place.create {
        let _ = fs::remove_dir_all(&backup);
        fs::create_dir_all(&backup)?;
        for name in changed {
            if current(name).is_file() {
                progress.copy(&current(name), &backup.join(name), false)?;
            }
        }
    }

    // Each is written beside the original and moved over it, so that it is never half
    // written. (If one fails, the ones before it are put back.)
    let mut replaced: Vec<&str> = Vec::new();
    for name in changed {
        let target = destination(name);
        let temporary = target.with_file_name(format!(".{name}.syncing"));
        let result = target
            .parent()
            .map_or(Ok(()), fs::create_dir_all)
            .and_then(|()| progress.copy(&scratch.join(DATABASE_DIR).join(name), &temporary, false))
            .and_then(|_| fs::rename(&temporary, &target));
        if let Err(error) = result {
            let _ = fs::remove_file(&temporary);
            for done in replaced {
                let kept = backup.join(done);
                if kept.is_file() {
                    let _ = fs::copy(kept, destination(done));
                } else {
                    let _ = fs::remove_file(destination(done));
                }
            }
            return Err(error.into());
        }
        replaced.push(name);
    }
    if let Some(library) = &place.library
        && changed.contains(&origins::FILE)
    {
        cache::remove_old_origins(library);
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

    // Streaming tracks (they have no file to lose) that the open library no longer has,
    // for instance after "Remove streaming songs". Tracks with files are never deleted.
    let tracks_removed = if remove_missing {
        let kept: HashSet<usize> = matched.iter().flatten().copied().collect();
        let gone: Vec<usize> = (0..target.tracks.len())
            .filter(|index| target.tracks[*index].streaming && !kept.contains(index))
            .collect();
        target.remove_tracks(&gone)?.tracks
    } else {
        0
    };

    // The origins the open library has for tracks that the other one has too. It is the
    // open library that counts: they replace what the other one has.
    let source_origins = source.origins();
    let target_origins = target.origins();
    let mut origins_to_write = Vec::new();
    let mut changed_origins = Vec::new();
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
        let before = target_origins.get(other_key);
        if before != Some(origin) {
            origins_to_write.push((other_key.clone(), origin.clone()));
            changed_origins.push(api::TrackChange {
                track: Some(api::TrackRef {
                    title: track.title.clone(),
                    artist: track.artist.clone(),
                    path: track.relative_path.clone(),
                }),
                fields: vec![api::FieldChange {
                    field: "origin".into(),
                    source_value: before.cloned().unwrap_or_default(),
                    working_value: origin.clone(),
                }],
            });
        }
    }
    let origins_changed = if origins_to_write.is_empty() {
        0
    } else {
        target.write_origins(&origins_to_write)?
    };

    Ok(Outcome {
        changes,
        tracks_removed,
        missing,
        origins_changed,
        changed_origins,
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
        // Kept beside the working copy of the open library, never on the other one.
        let backup = temp
            .0
            .join("source-scratch")
            .join(LIBRARY_SYNC_BACKUP)
            .join(MAIN_DATABASE);
        assert!(
            !stick
                .join(DATABASE_DIR)
                .join("open-engine-lib-backup")
                .exists()
        );
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
    fn streaming_tracks_only_in_the_other_library_go_when_asked() {
        let temp = TempDir::new("remove-streaming");
        let (pc, stick) = libraries(&temp.0);
        // The stick still has a streaming track the open library lacks, in a playlist,
        // and a track with a file that it lacks too.
        let database = stick.join(DATABASE_DIR).join(MAIN_DATABASE);
        Connection::open(&database)
            .unwrap()
            .execute_batch(
                "INSERT INTO Track (id, title, path, streamingSource, originDatabaseUuid, originTrackId)
                    VALUES (90, 'tidal', NULL, 'tidal', 'db', 90);
                 INSERT INTO Track (id, title, path, originDatabaseUuid, originTrackId)
                    VALUES (91, 'own', '../Music/own.flac', 'db', 91);
                 INSERT INTO PerformanceData (trackId) VALUES (90) ON CONFLICT DO NOTHING;
                 INSERT INTO PlaylistEntity (listId, trackId, databaseUuid, nextEntityId)
                    SELECT id, 90, 'db', 0 FROM Playlist WHERE title = 'Mine';",
            )
            .unwrap();
        let source = open(&pc, &temp.0.join("source-scratch"));

        let kept = diff(&source, &stick, &temp.0.join("scratch"), remove(false)).unwrap();
        assert_eq!(kept.tracks_removed, 0);

        let diff = diff(&source, &stick, &temp.0.join("scratch"), remove(true)).unwrap();
        assert_eq!(diff.tracks_removed, 1);

        let done = push(
            &source,
            &stick,
            &temp.0.join("scratch"),
            remove(true),
            &Progress::default(),
        )
        .unwrap();
        assert_eq!(done.tracks_removed, 1);
        let after = open(&stick, &temp.0.join("check"));
        let titles: Vec<&str> = after.tracks.iter().map(|t| t.title.as_str()).collect();
        assert!(!titles.contains(&"tidal"), "{titles:?}");
        assert!(titles.contains(&"own"), "a track with a file stays");
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
        let stick_origins = origins::source_file(&stick);
        fs::create_dir_all(stick_origins.parent().unwrap()).unwrap();
        fs::copy(
            temp.0
                .join("stick-scratch")
                .join(DATABASE_DIR)
                .join(origins::FILE),
            &stick_origins,
        )
        .unwrap();

        let scratch = temp.0.join("scratch");
        let found = diff(&source, &stick, &scratch, remove(false)).unwrap();
        assert_eq!(
            found.origins_changed, 2,
            "a is new and b differs; c has no match"
        );
        let listed: Vec<_> = found
            .changed_origins
            .iter()
            .map(|change| {
                let field = &change.fields[0];
                (
                    change.track.as_ref().unwrap().title.as_str(),
                    field.field.as_str(),
                    field.source_value.as_str(),
                    field.working_value.as_str(),
                )
            })
            .collect();
        assert_eq!(
            listed,
            [
                ("a.flac", "origin", "", "/pc/Music/a.flac"),
                ("b.flac", "origin", "/old/b.flac", "/pc/Music/b.flac"),
            ]
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
        let written = origins::Origins::open(&origins::source_file(&stick))
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
        let backup = temp.0.join("source-scratch").join(LIBRARY_SYNC_BACKUP);
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
        assert!(origins::source_file(&stick).is_file());
        assert!(
            !stick.join(DATABASE_DIR).join(origins::FILE).exists(),
            "not in the library itself"
        );
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
        // The paths differ in the open library, and one leads to a file that another track
        // of the stick already has. (The libraries are in different folders, so the same
        // file is written differently in each.)
        let connection = Connection::open(pc.join(DATABASE_DIR).join(MAIN_DATABASE)).unwrap();
        connection
            .execute_batch(
                "UPDATE Track SET path = '../Elsewhere/a.flac' WHERE id = 1;
                 UPDATE Track SET path = '../../stick/Music/zzz.flac' WHERE id = 2;",
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
            ("path", "../Music/a.flac", "../../pc/Elsewhere/a.flac")
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
                "../../pc/Elsewhere/a.flac",
                "../Music/b.flac",
                "../Music/zzz.flac"
            ]
        );
    }

    #[test]
    fn locations_synced_back_lead_to_the_copied_files() {
        let temp = TempDir::new("locations-back");
        let (pc, stick) = with_files(&temp.0);
        let open_library = open_source(&pc, &temp.0.join("pc-working"));
        let scratch = temp.0.join("scratch");

        // The tracks are copied to the stick...
        let copy = tracks(track_sync::Options {
            copy_tracks: true,
            ..Default::default()
        });
        push(&open_library, &stick, &scratch, copy, &Progress::default()).unwrap();

        // ...and the pc library takes the stick's locations back.
        let back = Options {
            reverse: true,
            tracks: track_sync::Options {
                locations: true,
                ..Default::default()
            },
            ..Options::default()
        };
        push(&open_library, &stick, &scratch, back, &Progress::default()).unwrap();

        let working = temp
            .0
            .join("pc-working")
            .join(DATABASE_DIR)
            .join(MAIN_DATABASE);
        let changed = Library::open_copy(&working, pc.clone(), temp.0.join("check")).unwrap();
        for track in &changed.tracks {
            assert!(
                track.path.is_file(),
                "{} leads to {}, which is not a file",
                track.title,
                track.relative_path
            );
        }
        assert_eq!(
            changed.tracks[0].relative_path,
            "../../stick/Engine-Tracks/AC_DC/Back_ In Black_/a.flac"
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
    fn covers_that_are_files_and_stems_go_with_the_added_tracks() {
        let temp = TempDir::new("extras");
        let hash = [7u8; 20];
        let cover = format!("{}.jpg", crate::library::base64_url(&hash));
        let pc = make(
            &temp.0,
            "pc",
            &[("a.flac", 5)],
            &format!(
                "UPDATE Track SET albumArtId = 1 WHERE id = 1;
                 INSERT INTO AlbumArt (id, hash, albumArt) VALUES (1, X'{}', NULL);",
                "07".repeat(20)
            ),
        );
        fs::create_dir_all(temp.0.join("pc/Music")).unwrap();
        fs::write(temp.0.join("pc/Music/a.flac"), b"aaaa").unwrap();
        fs::create_dir_all(pc.join("Artwork")).unwrap();
        fs::write(pc.join("Artwork").join(&cover), b"jpeg").unwrap();
        fs::create_dir_all(pc.join("Stems")).unwrap();
        fs::write(pc.join("Stems/5 db.stems"), b"stems!!").unwrap();
        let source = open_source(&pc, &temp.0.join("source"));

        // Without the stems option only the (small) cover is copied.
        let plain = make(&temp.0, "plain", &[], "");
        let options = tracks(track_sync::Options {
            copy_tracks: true,
            ..Default::default()
        });
        let found = diff(&source, &plain, &temp.0.join("scratch"), options).unwrap();
        assert_eq!((found.files_to_copy, found.bytes_to_copy), (2, 8));
        assert!(!plain.join("Artwork").exists(), "a diff copies nothing");
        let done = push(
            &source,
            &plain,
            &temp.0.join("scratch"),
            options,
            &Progress::default(),
        )
        .unwrap();
        assert_eq!(done.files_copied, 2);
        assert_eq!(
            fs::read(plain.join("Artwork").join(&cover)).unwrap(),
            b"jpeg"
        );
        assert!(!plain.join("Stems").exists());

        // With it the stems file follows, under the same name since the track keeps its origin.
        let full = make(&temp.0, "full", &[], "");
        let options = tracks(track_sync::Options {
            copy_tracks: true,
            copy_stems: true,
            ..Default::default()
        });
        let found = diff(&source, &full, &temp.0.join("scratch"), options).unwrap();
        assert_eq!((found.files_to_copy, found.bytes_to_copy), (3, 15));
        let progress = Progress::default();
        let done = push(&source, &full, &temp.0.join("scratch"), options, &progress).unwrap();
        assert_eq!(done.files_copied, 3);
        assert_eq!(
            fs::read(full.join("Artwork").join(&cover)).unwrap(),
            b"jpeg"
        );
        assert_eq!(fs::read(full.join("Stems/5 db.stems")).unwrap(), b"stems!!");

        // A second time there is nothing left to copy, and a file that is there is kept.
        fs::write(full.join("Stems/5 db.stems"), b"mine").unwrap();
        let again = push(&source, &full, &temp.0.join("scratch"), options, &progress).unwrap();
        assert_eq!(again.files_copied, 0);
        assert_eq!(fs::read(full.join("Stems/5 db.stems")).unwrap(), b"mine");
    }

    #[test]
    fn stems_can_be_copied_alone_for_the_tracks_both_libraries_have() {
        let temp = TempDir::new("stems-alone");
        let pc = make(&temp.0, "pc", &[("a.flac", 5), ("b.flac", 6)], "");
        let stick = make(&temp.0, "stick", &[("a.flac", 5)], "");
        fs::create_dir_all(pc.join("Stems")).unwrap();
        fs::write(pc.join("Stems/5 db.stems"), b"stems!!").unwrap();
        fs::write(pc.join("Stems/6 db.stems"), b"other").unwrap();
        let source = open_source(&pc, &temp.0.join("source"));
        let scratch = temp.0.join("scratch");
        let options = tracks(track_sync::Options {
            copy_stems: true,
            ..Default::default()
        });

        // Only the track the stick has gets its stems: no track is added, no audio file is
        // copied and the stick's database is not touched.
        let found = diff(&source, &stick, &scratch, options).unwrap();
        assert_eq!((found.files_to_copy, found.bytes_to_copy), (1, 7));
        assert_eq!((found.tracks_added, found.tracks_changed), (0, 0));
        assert!(!stick.join("Stems").exists(), "a diff copies nothing");
        let done = push(&source, &stick, &scratch, options, &Progress::default()).unwrap();
        assert_eq!((done.files_copied, done.tracks_added), (1, 0));
        assert!(done.backup_path.is_empty(), "nothing in a database changed");
        assert_eq!(
            fs::read(stick.join("Stems/5 db.stems")).unwrap(),
            b"stems!!"
        );
        assert!(!stick.join("Stems/6 db.stems").exists());
        assert!(!temp.0.join("stick").join(track_sync::TRACKS_DIR).exists());

        // Nothing is left to copy the next time, and what is there is kept.
        fs::write(stick.join("Stems/5 db.stems"), b"mine").unwrap();
        let again = push(&source, &stick, &scratch, options, &Progress::default()).unwrap();
        assert_eq!(again.files_copied, 0);
        assert_eq!(fs::read(stick.join("Stems/5 db.stems")).unwrap(), b"mine");
    }

    #[test]
    fn stems_go_under_the_origin_of_the_track_the_other_library_has() {
        let temp = TempDir::new("stems-origin");
        // The stick has the track under another origin (they are matched by the file name).
        let pc = make(&temp.0, "pc", &[("a.flac", 5)], "");
        let stick = make(&temp.0, "stick", &[("a.flac", 9)], "");
        fs::create_dir_all(pc.join("Stems")).unwrap();
        fs::write(pc.join("Stems/5 db.stems"), b"stems!!").unwrap();
        let source = open_source(&pc, &temp.0.join("source"));
        let options = tracks(track_sync::Options {
            copy_stems: true,
            ..Default::default()
        });
        push(
            &source,
            &stick,
            &temp.0.join("scratch"),
            options,
            &Progress::default(),
        )
        .unwrap();
        assert_eq!(
            fs::read(stick.join("Stems/9 db.stems")).unwrap(),
            b"stems!!"
        );
        assert!(!stick.join("Stems/5 db.stems").exists());
    }

    #[test]
    fn stems_can_be_copied_alone_the_other_way_round_too() {
        let temp = TempDir::new("stems-back");
        let pc = make(&temp.0, "pc", &[("a.flac", 5)], "");
        let stick = make(&temp.0, "stick", &[("a.flac", 5), ("c.flac", 7)], "");
        fs::create_dir_all(stick.join("Stems")).unwrap();
        fs::write(stick.join("Stems/5 db.stems"), b"stems!!").unwrap();
        fs::write(stick.join("Stems/7 db.stems"), b"other").unwrap();
        let open_library = open_source(&pc, &temp.0.join("pc-working"));
        let options = Options {
            reverse: true,
            tracks: track_sync::Options {
                copy_stems: true,
                ..Default::default()
            },
            ..Options::default()
        };
        let scratch = temp.0.join("scratch");

        let found = diff(&open_library, &stick, &scratch, options).unwrap();
        assert_eq!((found.files_to_copy, found.tracks_added), (1, 0));
        let done = push(
            &open_library,
            &stick,
            &scratch,
            options,
            &Progress::default(),
        )
        .unwrap();
        assert_eq!(done.files_copied, 1);
        // The open library has the stems of the track it shares with the stick, and only those.
        assert_eq!(fs::read(pc.join("Stems/5 db.stems")).unwrap(), b"stems!!");
        assert!(!pc.join("Stems/7 db.stems").exists());
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
    fn the_other_way_round_the_other_library_is_copied_into_the_working_copy() {
        let temp = TempDir::new("reverse");
        let (pc, stick) = with_files(&temp.0);
        // The stick is the open library, the pc library the one that has more.
        let open_library = open_source(&stick, &temp.0.join("working-copy"));
        let working = temp
            .0
            .join("working-copy")
            .join(DATABASE_DIR)
            .join(MAIN_DATABASE);
        let working_before = fs::read(&working).unwrap();
        let stick_before = fs::read(stick.join(DATABASE_DIR).join(MAIN_DATABASE)).unwrap();
        let pc_before = fs::read(pc.join(DATABASE_DIR).join(MAIN_DATABASE)).unwrap();
        // The pc library has origins for its tracks.
        let master = open(&pc, &temp.0.join("pc-scratch"));
        master
            .save_origins(&[(("db".to_string(), 1), "/pc/Music/a.flac".into())], false)
            .unwrap();
        fs::create_dir_all(origins::source_file(&pc).parent().unwrap()).unwrap();
        fs::copy(
            temp.0
                .join("pc-scratch")
                .join(DATABASE_DIR)
                .join(origins::FILE),
            origins::source_file(&pc),
        )
        .unwrap();
        let options = Options {
            reverse: true,
            tracks: track_sync::Options {
                stars: true,
                copy_tracks: true,
                ..Default::default()
            },
            ..Options::default()
        };
        let scratch = temp.0.join("scratch");
        let tracks_dir = temp.0.join("stick").join(track_sync::TRACKS_DIR);

        // The diff is about the open library, and changes nothing.
        let found = diff(&open_library, &pc, &scratch, options).unwrap();
        assert_eq!(
            found.target_path,
            pc.display().to_string(),
            "the other library"
        );
        assert_eq!(
            (found.tracks_added, found.added_tracks[0].title.as_str()),
            (1, "c.flac")
        );
        assert_eq!(found.tracks_changed, 2, "a: stars and path; b: path");
        assert_eq!(found.origins_changed, 1);
        let mix = found
            .changed_playlists
            .iter()
            .find(|p| p.name == "Mix")
            .unwrap();
        assert_eq!(mix.kind(), api::PlaylistChangeKind::Added);
        assert!(!tracks_dir.exists());
        assert_eq!(fs::read(&working).unwrap(), working_before);

        let done = push(&open_library, &pc, &scratch, options, &Progress::default()).unwrap();
        assert_eq!(
            (done.tracks_added, done.files_copied, done.origins_changed),
            (1, 3, 1)
        );
        // The working copy has it all; its old state is kept; it is noted as edited.
        let changed =
            Library::open_copy(&working, stick.clone(), temp.0.join("working-copy")).unwrap();
        assert_eq!(
            ratings_and_paths(&changed),
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
        let mix = changed
            .playlists()
            .iter()
            .find(|p| p.title == "Mix")
            .map(|p| p.tracks.clone());
        assert_eq!(mix, Some(vec![0, 2]));
        assert_eq!(changed.origins().len(), 1);
        let backup = temp.0.join("working-copy").join(BACK_SYNC_BACKUP);
        assert_eq!(done.backup_path, backup.display().to_string());
        assert_eq!(
            fs::read(backup.join(MAIN_DATABASE)).unwrap(),
            working_before
        );
        assert!(temp.0.join("working-copy").join(cache::EDITED).exists());
        // The files were copied beside the library that is open; nothing else was touched.
        assert_eq!(
            fs::read(tracks_dir.join("Zed/Z/c.flac")).unwrap(),
            b"cccccc"
        );
        assert_eq!(
            fs::read(stick.join(DATABASE_DIR).join(MAIN_DATABASE)).unwrap(),
            stick_before
        );
        assert_eq!(
            fs::read(pc.join(DATABASE_DIR).join(MAIN_DATABASE)).unwrap(),
            pc_before
        );
        assert!(
            !stick
                .join(DATABASE_DIR)
                .join("open-engine-lib-backup")
                .exists()
        );
    }

    #[test]
    fn a_library_is_made_where_there_is_none() {
        let temp = TempDir::new("create");
        let (pc, _) = with_files(&temp.0);
        let source = open_source(&pc, &temp.0.join("source"));
        // A drive without a library.
        let drive = temp.0.join("DRIVE");
        fs::create_dir_all(&drive).unwrap();
        let options = Options {
            create: true,
            tracks: track_sync::Options {
                stars: true,
                copy_tracks: true,
                ..Default::default()
            },
            ..Options::default()
        };
        let scratch = temp.0.join("scratch");

        // Without being asked to, a missing library is an error. Nothing is made.
        assert!(matches!(
            diff(
                &source,
                &drive,
                &scratch,
                Options {
                    create: false,
                    ..options
                }
            ),
            Err(MirrorError::NotALibrary(_))
        ));
        assert!(matches!(
            diff(&source, &temp.0.join("nowhere"), &scratch, options),
            Err(MirrorError::NotALibrary(_)),
        ));
        assert!(
            matches!(
                diff(
                    &source,
                    &drive,
                    &scratch,
                    Options {
                        reverse: true,
                        ..options
                    }
                ),
                Err(MirrorError::NotALibrary(_))
            ),
            "there is nothing to copy from"
        );

        // The diff says what would be made, and makes nothing.
        let found = diff(&source, &drive, &scratch, options).unwrap();
        assert!(found.target_is_new);
        assert_eq!(
            found.target_path,
            drive.join("Engine Library").display().to_string()
        );
        assert_eq!(found.tracks_added, 3, "every track is new there");
        assert_eq!((found.files_to_copy, found.bytes_to_copy), (3, 12));
        let mix = found
            .changed_playlists
            .iter()
            .find(|p| p.name == "Mix")
            .unwrap();
        assert_eq!(
            (mix.kind(), mix.tracks_added),
            (api::PlaylistChangeKind::Added, 2)
        );
        assert_eq!(
            fs::read_dir(&drive).unwrap().count(),
            0,
            "the drive is as it was"
        );

        // The push makes the library: its folder, the databases and the tracks.
        let done = push(&source, &drive, &scratch, options, &Progress::default()).unwrap();
        assert!(done.created);
        assert_eq!(
            (
                done.tracks_added,
                done.files_copied,
                done.backup_path.as_str()
            ),
            (3, 3, "")
        );
        let library = drive.join("Engine Library");
        assert!(library.join(DATABASE_DIR).join(MAIN_DATABASE).is_file());
        assert!(
            !library
                .join(DATABASE_DIR)
                .join("open-engine-lib-backup")
                .exists(),
            "nothing was replaced"
        );
        let made = open(&library, &temp.0.join("check"));
        assert_eq!(
            ratings_and_paths(&made),
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
        let mix = made
            .playlists()
            .iter()
            .find(|p| p.title == "Mix")
            .map(|p| p.tracks.clone());
        assert_eq!(mix, Some(vec![0, 2]));
        assert_eq!(
            fs::read(drive.join("Engine-Tracks/Zed/Z/c.flac")).unwrap(),
            b"cccccc"
        );
        drop(made);

        // From now on it is a library like any other: nothing is new, nothing is left to do.
        let again = push(&source, &drive, &scratch, options, &Progress::default()).unwrap();
        assert!(!again.created);
        assert_eq!(
            (
                again.tracks_added,
                again.playlists_changed,
                again.files_copied
            ),
            (0, 0, 0)
        );
        assert!(
            !diff(&source, &drive, &scratch, options)
                .unwrap()
                .target_is_new
        );

        // A folder that is called Engine Library is the library's folder itself.
        let named = temp.0.join("OTHER").join("Engine Library");
        fs::create_dir_all(&named).unwrap();
        let found = diff(&source, &named, &scratch, options).unwrap();
        assert!(found.target_is_new);
        assert_eq!(found.target_path, named.display().to_string());
    }

    #[cfg(unix)]
    #[test]
    fn a_library_that_cannot_be_made_leaves_nothing() {
        use std::os::unix::fs::PermissionsExt;

        let temp = TempDir::new("create-fails");
        let (pc, _) = with_files(&temp.0);
        let source = open_source(&pc, &temp.0.join("source"));
        let drive = temp.0.join("DRIVE");
        fs::create_dir_all(&drive).unwrap();
        let options = Options {
            create: true,
            tracks: track_sync::Options {
                copy_tracks: true,
                ..Default::default()
            },
            ..Options::default()
        };

        fs::set_permissions(&drive, fs::Permissions::from_mode(0o555)).unwrap();
        let result = push(
            &source,
            &drive,
            &temp.0.join("scratch"),
            options,
            &Progress::default(),
        );
        fs::set_permissions(&drive, fs::Permissions::from_mode(0o755)).unwrap();

        assert!(result.is_err());
        assert_eq!(fs::read_dir(&drive).unwrap().count(), 0);
    }

    #[test]
    fn the_other_way_round_still_refuses_the_open_library_itself() {
        let temp = TempDir::new("reverse-same");
        let (pc, _) = with_files(&temp.0);
        let open_library = open_source(&pc, &temp.0.join("working-copy"));
        let options = Options {
            reverse: true,
            ..Options::default()
        };
        assert!(matches!(
            diff(&open_library, &pc, &temp.0.join("scratch"), options),
            Err(MirrorError::SameLibrary)
        ));
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
