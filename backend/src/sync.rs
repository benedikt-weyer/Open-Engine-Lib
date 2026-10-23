//! Comparing the working copy of a library with its source, and writing the working copy
//! to the source.
//!
//! The backend reads (and may change) a working copy of the library's databases, see
//! `cache`. "Syncing" writes that working copy to the library it was made from. Before
//! that, `diff` says what would change there. Two things make writing to the source
//! dangerous, and are guarded against:
//!
//! - someone else may have changed the source since the working copy was made (Engine
//!   DJ, say), in which case syncing undoes their changes. That is detected, and only
//!   done if the caller says it is meant;
//! - the source may be in the middle of being written to, which is detected and refused.
//!
//! The databases that are replaced are kept in a backup folder first, and each file is
//! replaced in one step; if a later one fails, the earlier ones are put back.

use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::fs::{self, File};
use std::io::{self, Read};
use std::path::{Path, PathBuf};

use open_engine_api::v1 as api;
use rusqlite::{Connection, OpenFlags};

use crate::cache::{self, DATABASE_DIR};
use crate::library::Library;
use crate::origins;
use crate::progress::{Phase, Progress};

/// Where the databases that a sync replaces are kept: in the working copy's cache folder,
/// never in the library (which may be on a player's stick).
pub const BACKUP_DIR: &str = "before-sync";
const MAIN_DATABASE: &str = "m.db";
/// Lists in a diff are cut here, so that a library that has changed completely doesn't
/// produce a message of many megabytes. The counts are complete.
const MAX_LISTED: usize = 200;

/// The track columns whose changes are listed, as users see them.
const TRACK_FIELDS: [&str; 17] = [
    "title",
    "artist",
    "album",
    "genre",
    "comment",
    "label",
    "composer",
    "remixer",
    "year",
    "bpmAnalyzed",
    "key",
    "rating",
    "path",
    "length",
    "fileType",
    "isAnalyzed",
    "isBeatGridLocked",
];
/// The analysis blobs of a track and what they are called in the list.
const ANALYSIS_FIELDS: [(&str, &str); 3] = [
    ("beatData", "beatgrid"),
    ("quickCues", "cues"),
    ("loops", "loops"),
];

#[derive(Debug)]
pub enum SyncError {
    /// The source can't be reached.
    Unavailable(PathBuf),
    /// Another program is writing to the source right now.
    Busy,
    /// The source was changed by someone else, and that may not be overwritten.
    ChangedOutside,
    Failed(String),
}

impl fmt::Display for SyncError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SyncError::Unavailable(dir) => write!(
                f,
                "the source {} can't be reached (is its drive connected?)",
                dir.display()
            ),
            SyncError::Busy => f.write_str(
                "the source is being written to by another program (it has an uncommitted journal)",
            ),
            SyncError::ChangedOutside => f.write_str(
                "the source was changed outside this app after the working copy was made; \
                 syncing would undo those changes",
            ),
            SyncError::Failed(error) => f.write_str(error),
        }
    }
}

impl From<io::Error> for SyncError {
    fn from(error: io::Error) -> Self {
        SyncError::Failed(error.to_string())
    }
}

impl From<rusqlite::Error> for SyncError {
    fn from(error: rusqlite::Error) -> Self {
        SyncError::Failed(error.to_string())
    }
}

fn source_databases(library: &Library) -> PathBuf {
    library.dir.join(DATABASE_DIR)
}

fn working_databases(library: &Library) -> PathBuf {
    library.cache.join(DATABASE_DIR)
}

/// Whether another program has a write in progress in the source: a journal or a
/// write-ahead log with something in it.
pub fn is_busy(source: &Path) -> bool {
    fs::read_dir(source).is_ok_and(|entries| {
        entries.flatten().any(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            let side_file = name.ends_with(".db-journal") || name.ends_with(".db-wal");
            side_file && entry.metadata().is_ok_and(|metadata| metadata.len() > 0)
        })
    })
}

/// Whether two files have the same content. A file that can't be read is different.
pub fn same_content(a: &Path, b: &Path) -> bool {
    fn compare(a: &Path, b: &Path) -> io::Result<bool> {
        let (mut a, mut b) = (File::open(a)?, File::open(b)?);
        if a.metadata()?.len() != b.metadata()?.len() {
            return Ok(false);
        }
        let (mut left, mut right) = (vec![0u8; 1 << 20], vec![0u8; 1 << 20]);
        loop {
            let read = a.read(&mut left)?;
            if read == 0 {
                return Ok(true);
            }
            b.read_exact(&mut right[..read])?;
            if left[..read] != right[..read] {
                return Ok(false);
            }
        }
    }
    compare(a, b).unwrap_or(false)
}

/// The database files of the working copy that differ from the source's.
fn changed_databases(library: &Library) -> io::Result<Vec<String>> {
    let working = working_databases(library);
    Ok(cache::database_files(&working)?
        .into_iter()
        .map(|file| file.name)
        .filter(|name| {
            let there = cache::source_file(&library.dir, name);
            // (Origins that are still where earlier versions kept them are to be moved.)
            there != cache::destination_file(&library.dir, name)
                || !same_content(&working.join(name), &there)
        })
        .collect())
}

/// What would change in the source if the working copy were written to it, or with
/// `reverse` what would change in the working copy if the source replaced it.
pub fn diff(library: &Library, reverse: bool) -> Result<api::GetSyncDiffResponse, SyncError> {
    let mut response = api::GetSyncDiffResponse {
        source_path: library.dir.display().to_string(),
        cache_path: library.cache.display().to_string(),
        ..Default::default()
    };
    let source = source_databases(library);
    if !source.join(MAIN_DATABASE).is_file() {
        return Ok(response);
    }
    response.source_available = true;
    response.source_busy = is_busy(&source);
    response.source_changed_outside = cache::source_changed(&library.dir, &library.cache)?;
    response.changed_databases = changed_databases(library)?;
    if response
        .changed_databases
        .iter()
        .any(|name| name == origins::FILE)
    {
        let (working, source) = (
            working_databases(library).join(origins::FILE),
            cache::source_file(&library.dir, origins::FILE),
        );
        response.origins_changed = if reverse {
            changed_origins(&source, &working)
        } else {
            changed_origins(&working, &source)
        };
    }

    if response
        .changed_databases
        .iter()
        .any(|name| name == MAIN_DATABASE)
    {
        let (working, source) = (
            working_databases(library).join(MAIN_DATABASE),
            source.join(MAIN_DATABASE),
        );
        // The first is what the second would become.
        if reverse {
            compare_main_databases(&source, &working, &mut response)?;
        } else {
            compare_main_databases(&working, &source, &mut response)?;
        }
    }
    Ok(response)
}

/// Opens the working copy's main database with the source's attached as `src`, both only
/// for reading. (`immutable` makes SQLite leave the source's locks and journals alone,
/// which matters when another program has it open.)
fn compare_main_databases(
    working: &Path,
    source: &Path,
    response: &mut api::GetSyncDiffResponse,
) -> Result<(), SyncError> {
    let connection = Connection::open_with_flags(
        source_uri(working),
        OpenFlags::SQLITE_OPEN_READ_ONLY
            | OpenFlags::SQLITE_OPEN_URI
            | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    connection.execute("ATTACH DATABASE ?1 AS src", [source_uri(source)])?;

    compare_tracks(&connection, response)?;
    compare_playlists(&connection, response)?;
    Ok(())
}

/// How many origins the database at `working` has that are new or different from those in
/// the one at `source` (which may not exist). Both are only read, and in a way that leaves
/// the source's locks and journals alone.
fn changed_origins(working: &Path, source: &Path) -> u32 {
    let read = |path: &Path| -> HashMap<(String, i64), String> {
        let Ok(connection) = Connection::open_with_flags(
            source_uri(path),
            OpenFlags::SQLITE_OPEN_READ_ONLY
                | OpenFlags::SQLITE_OPEN_URI
                | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        ) else {
            return HashMap::new();
        };
        let Ok(mut statement) =
            connection.prepare("SELECT databaseUuid, trackId, origin FROM Origin")
        else {
            return HashMap::new();
        };
        statement
            .query_map([], |row| Ok(((row.get(0)?, row.get(1)?), row.get(2)?)))
            .and_then(|rows| rows.collect())
            .unwrap_or_default()
    };
    let before = read(source);
    read(working)
        .iter()
        .filter(|(key, origin)| before.get(*key) != Some(origin))
        .count() as u32
}

/// A `file:` URI for opening `path` read-only and immutable.
pub fn source_uri(path: &Path) -> String {
    let mut uri = String::from("file:");
    for byte in path.to_string_lossy().bytes() {
        match byte {
            b'%' | b'?' | b'#' | b' ' => uri.push_str(&format!("%{byte:02X}")),
            _ => uri.push(char::from(byte)),
        }
    }
    // (Non-ASCII bytes are fine as they are in a SQLite URI.)
    uri.push_str("?mode=ro&immutable=1");
    uri
}

fn has_table(connection: &Connection, schema: &str, table: &str) -> bool {
    connection
        .query_row(
            &format!("SELECT 1 FROM {schema}.sqlite_master WHERE type = 'table' AND name = ?1"),
            [table],
            |_| Ok(()),
        )
        .is_ok()
}

/// Whether `table` has `column` in both the working copy (`main`) and the source (`src`).
fn has_column_in_both(connection: &Connection, table: &str, column: &str) -> bool {
    ["main", "src"].into_iter().all(|schema| {
        connection
            .query_row(
                &format!("SELECT 1 FROM pragma_table_info('{table}', '{schema}') WHERE name = ?1"),
                [column],
                |_| Ok(()),
            )
            .is_ok()
    })
}

fn track_refs(connection: &Connection, sql: &str) -> rusqlite::Result<Vec<api::TrackRef>> {
    let mut statement = connection.prepare(sql)?;
    statement
        .query_map([], |row| {
            Ok(api::TrackRef {
                title: row.get(0)?,
                artist: row.get(1)?,
                path: row.get(2)?,
            })
        })?
        .collect()
}

fn count(connection: &Connection, sql: &str) -> rusqlite::Result<u32> {
    connection.query_row(sql, [], |row| row.get(0))
}

fn compare_tracks(
    connection: &Connection,
    response: &mut api::GetSyncDiffResponse,
) -> Result<(), SyncError> {
    if !has_table(connection, "main", "Track") || !has_table(connection, "src", "Track") {
        return Ok(());
    }
    const LIST: &str = "COALESCE(title, ''), COALESCE(artist, ''), COALESCE(path, '')";

    let only_in = |schema: &str, other: &str| {
        format!("FROM {schema}.Track WHERE id NOT IN (SELECT id FROM {other}.Track)")
    };
    let (added, removed) = (only_in("main", "src"), only_in("src", "main"));
    response.tracks_added = count(connection, &format!("SELECT count(*) {added}"))?;
    response.tracks_removed = count(connection, &format!("SELECT count(*) {removed}"))?;
    response.added_tracks = track_refs(
        connection,
        &format!("SELECT {LIST} {added} ORDER BY id LIMIT {MAX_LISTED}"),
    )?;
    response.removed_tracks = track_refs(
        connection,
        &format!("SELECT {LIST} {removed} ORDER BY id LIMIT {MAX_LISTED}"),
    )?;

    // Tracks in both that differ, by field.
    let mut changes: BTreeMap<i64, Vec<api::FieldChange>> = BTreeMap::new();
    let mut collect = |sql: String, label: &str| -> rusqlite::Result<()> {
        let mut statement = connection.prepare(&sql)?;
        let rows = statement.query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })?;
        for row in rows {
            let (id, source_value, working_value) = row?;
            changes.entry(id).or_default().push(api::FieldChange {
                field: label.to_owned(),
                source_value,
                working_value,
            });
        }
        Ok(())
    };
    for column in TRACK_FIELDS {
        if has_column_in_both(connection, "Track", column) {
            collect(
                format!(
                    "SELECT t.id, COALESCE(CAST(s.{column} AS TEXT), ''), COALESCE(CAST(t.{column} AS TEXT), '') \
                     FROM main.Track t JOIN src.Track s ON s.id = t.id WHERE t.{column} IS NOT s.{column}"
                ),
                column,
            )?;
        }
    }
    if has_table(connection, "main", "PerformanceData")
        && has_table(connection, "src", "PerformanceData")
    {
        for (column, label) in ANALYSIS_FIELDS {
            if has_column_in_both(connection, "PerformanceData", column) {
                collect(
                    format!(
                        "SELECT t.trackId, '', '(changed)' FROM main.PerformanceData t \
                         JOIN src.PerformanceData s ON s.trackId = t.trackId WHERE t.{column} IS NOT s.{column}"
                    ),
                    label,
                )?;
            }
        }
    }

    response.tracks_changed = changes.len() as u32;
    for (id, fields) in changes.into_iter().take(MAX_LISTED) {
        let track = connection.query_row(
            "SELECT COALESCE(title, ''), COALESCE(artist, ''), COALESCE(path, '') FROM main.Track WHERE id = ?1",
            [id],
            |row| {
                Ok(api::TrackRef {
                    title: row.get(0)?,
                    artist: row.get(1)?,
                    path: row.get(2)?,
                })
            },
        );
        // A change to the analysis of a track that is not in the track list is not shown.
        if let Ok(track) = track {
            response.changed_tracks.push(api::TrackChange {
                track: Some(track),
                fields,
            });
        }
    }
    Ok(())
}

/// The titles of the playlists by id, with their parents' ids.
fn playlist_names(
    connection: &Connection,
    schema: &str,
) -> rusqlite::Result<HashMap<i64, (String, i64)>> {
    let mut statement = connection.prepare(&format!(
        "SELECT id, COALESCE(title, ''), COALESCE(parentListId, 0) FROM {schema}.Playlist"
    ))?;
    statement
        .query_map([], |row| Ok((row.get(0)?, (row.get(1)?, row.get(2)?))))?
        .collect()
}

/// `id` as a path of titles from the top, e.g. `House / Deep`.
fn playlist_path(names: &HashMap<i64, (String, i64)>, mut id: i64) -> String {
    let mut parts = Vec::new();
    // (The depth limit guards against a parent loop in broken data.)
    while let Some((title, parent)) = names.get(&id) {
        parts.push(title.as_str());
        if parts.len() > 32 {
            break;
        }
        id = *parent;
    }
    parts.reverse();
    parts.join(" / ")
}

/// What differs about the tracks of each playlist, by playlist id: how many were added,
/// how many removed, and whether the order changed.
fn playlist_track_changes(
    connection: &Connection,
) -> rusqlite::Result<HashMap<i64, (u32, u32, bool)>> {
    let mut changes: HashMap<i64, (u32, u32, bool)> = HashMap::new();
    if !has_table(connection, "main", "PlaylistEntity")
        || !has_table(connection, "src", "PlaylistEntity")
    {
        return Ok(changes);
    }
    // Entries are the same if they list the same track of the same database in the same
    // playlist.
    let missing_from = |schema: &str, other: &str| {
        format!(
            "SELECT e.listId, count(*) FROM {schema}.PlaylistEntity e WHERE NOT EXISTS \
             (SELECT 1 FROM {other}.PlaylistEntity o WHERE o.listId = e.listId AND o.trackId = e.trackId \
              AND o.databaseUuid IS e.databaseUuid) GROUP BY e.listId"
        )
    };
    for (sql, added) in [
        (missing_from("main", "src"), true),
        (missing_from("src", "main"), false),
    ] {
        let mut statement = connection.prepare(&sql)?;
        for row in
            statement.query_map([], |row| Ok((row.get::<_, i64>(0)?, row.get::<_, u32>(1)?)))?
        {
            let (list, count) = row?;
            let entry = changes.entry(list).or_default();
            if added {
                entry.0 = count;
            } else {
                entry.1 = count;
            }
        }
    }
    let mut statement = connection.prepare(
        "SELECT DISTINCT e.listId FROM main.PlaylistEntity e JOIN src.PlaylistEntity o \
         ON o.listId = e.listId AND o.trackId = e.trackId AND o.databaseUuid IS e.databaseUuid \
         WHERE o.nextEntityId IS NOT e.nextEntityId",
    )?;
    for list in statement.query_map([], |row| row.get::<_, i64>(0))? {
        changes.entry(list?).or_default().2 = true;
    }
    Ok(changes)
}

fn compare_playlists(
    connection: &Connection,
    response: &mut api::GetSyncDiffResponse,
) -> Result<(), SyncError> {
    let mut changes = playlist_changes(connection)?;
    changes.extend(smartlist_changes(connection)?);

    changes.sort_by(|a, b| a.name.cmp(&b.name));
    response.playlists_changed = changes.len() as u32;
    changes.truncate(MAX_LISTED);
    response.changed_playlists = changes;
    Ok(())
}

/// A smart playlist as the working copy and the source keep it.
struct Smartlist {
    title: String,
    /// Its parent as the titles from the top, each followed by `;`.
    parent: String,
    rules: String,
}

impl Smartlist {
    /// Like `playlist_path`: `House / Deep`, empty at the top level.
    fn parent_path(&self) -> String {
        let parts: Vec<&str> = self.parent.split(';').filter(|s| !s.is_empty()).collect();
        parts.join(" / ")
    }

    /// Like `playlist_path`: `House / Deep / Title`.
    fn path(&self) -> String {
        let parent = self.parent_path();
        if parent.is_empty() {
            self.title.clone()
        } else {
            format!("{parent} / {}", self.title)
        }
    }
}

/// The smart playlists by `listUuid`. (A library from an older version has none.)
fn smartlists(
    connection: &Connection,
    schema: &str,
) -> rusqlite::Result<HashMap<String, Smartlist>> {
    if !has_table(connection, schema, "Smartlist") {
        return Ok(HashMap::new());
    }
    let mut statement = connection.prepare(&format!(
        "SELECT listUuid, COALESCE(title, ''), COALESCE(parentPlaylistPath, ''), \
                COALESCE(rules, '') FROM {schema}.Smartlist"
    ))?;
    statement
        .query_map([], |row| {
            Ok((
                row.get(0)?,
                Smartlist {
                    title: row.get(1)?,
                    parent: row.get(2)?,
                    rules: row.get(3)?,
                },
            ))
        })?
        .collect()
}

/// Smart playlists that were added, removed, renamed, moved or given other rules. (Their
/// order among each other is not compared.)
fn smartlist_changes(connection: &Connection) -> rusqlite::Result<Vec<api::PlaylistChange>> {
    let working = smartlists(connection, "main")?;
    let source = smartlists(connection, "src")?;
    let change =
        |list: &Smartlist, kind: api::PlaylistChangeKind, detail: String| api::PlaylistChange {
            name: list.path(),
            kind: kind.into(),
            detail,
            smart: true,
            ..Default::default()
        };

    let mut changes = Vec::new();
    for (uuid, list) in &working {
        match source.get(uuid) {
            None => changes.push(change(list, api::PlaylistChangeKind::Added, String::new())),
            Some(old) => {
                let rules_changed = old.rules != list.rules;
                let mut entry = if old.title != list.title {
                    Some(change(
                        list,
                        api::PlaylistChangeKind::Renamed,
                        old.title.clone(),
                    ))
                } else if old.parent != list.parent {
                    let parent = old.parent_path();
                    Some(change(
                        list,
                        api::PlaylistChangeKind::Moved,
                        if parent.is_empty() {
                            "(top level)".to_owned()
                        } else {
                            parent
                        },
                    ))
                } else if rules_changed {
                    Some(change(list, api::PlaylistChangeKind::Rules, String::new()))
                } else {
                    None
                };
                if let Some(entry) = &mut entry {
                    entry.rules_changed = rules_changed;
                }
                changes.extend(entry);
            }
        }
    }
    for (uuid, list) in &source {
        if !working.contains_key(uuid) {
            changes.push(change(
                list,
                api::PlaylistChangeKind::Removed,
                String::new(),
            ));
        }
    }
    Ok(changes)
}

/// Playlists that were added, removed, renamed, moved or whose tracks changed.
fn playlist_changes(connection: &Connection) -> rusqlite::Result<Vec<api::PlaylistChange>> {
    if !has_table(connection, "main", "Playlist") || !has_table(connection, "src", "Playlist") {
        return Ok(Vec::new());
    }
    let working = playlist_names(connection, "main")?;
    let source = playlist_names(connection, "src")?;
    let mut track_changes = playlist_track_changes(connection)?;

    let mut changes: Vec<api::PlaylistChange> = Vec::new();
    let change =
        |name: String, kind: api::PlaylistChangeKind, detail: String| api::PlaylistChange {
            name,
            kind: kind.into(),
            detail,
            ..Default::default()
        };

    for (id, (title, parent)) in &working {
        match source.get(id) {
            None => changes.push(change(
                playlist_path(&working, *id),
                api::PlaylistChangeKind::Added,
                String::new(),
            )),
            Some((old_title, old_parent)) => {
                let mut entry = if title != old_title {
                    Some(change(
                        playlist_path(&working, *id),
                        api::PlaylistChangeKind::Renamed,
                        old_title.clone(),
                    ))
                } else if parent != old_parent {
                    let old = playlist_path(&source, *old_parent);
                    Some(change(
                        playlist_path(&working, *id),
                        api::PlaylistChangeKind::Moved,
                        if old.is_empty() {
                            "(top level)".to_owned()
                        } else {
                            old
                        },
                    ))
                } else {
                    None
                };
                if let Some((added, removed, order)) = track_changes.remove(id) {
                    let entry = entry.get_or_insert_with(|| {
                        change(
                            playlist_path(&working, *id),
                            api::PlaylistChangeKind::Tracks,
                            String::new(),
                        )
                    });
                    entry.tracks_added = added;
                    entry.tracks_removed = removed;
                    entry.order_changed = order;
                }
                changes.extend(entry);
            }
        }
    }
    for id in source.keys().filter(|id| !working.contains_key(id)) {
        changes.push(change(
            playlist_path(&source, *id),
            api::PlaylistChangeKind::Removed,
            String::new(),
        ));
    }

    Ok(changes)
}

/// Replaces the working copy by the databases of the source (the reverse of `push`). The
/// working copy's edits that never reached the source are first moved to `unsynced-edits`
/// in the cache folder; the response says where, if there were any.
pub fn pull(
    library: &Library,
    progress: &Progress,
) -> Result<api::PullFromSourceResponse, SyncError> {
    let source = source_databases(library);
    if !source.join(MAIN_DATABASE).is_file() {
        return Err(SyncError::Unavailable(library.dir.clone()));
    }
    if is_busy(&source) {
        return Err(SyncError::Busy);
    }

    let files = cache::source_files(&library.dir)?;
    let size = |name: &str| {
        fs::metadata(cache::source_file(&library.dir, name)).map_or(0, |metadata| metadata.len())
    };
    progress.phase(
        Phase::Pulling,
        files.iter().map(|file| size(&file.name)).sum(),
        0,
    );

    let had_edits = library.cache.join(cache::EDITED).exists();
    if had_edits {
        cache::keep_unsynced_edits(&library.cache)?;
    }
    let working = working_databases(library);
    match fs::remove_dir_all(&working) {
        Err(error) if error.kind() != io::ErrorKind::NotFound => return Err(error.into()),
        _ => {}
    }
    fs::create_dir_all(&working)?;
    for file in &files {
        progress.copy(
            &cache::source_file(&library.dir, &file.name),
            &working.join(&file.name),
            false,
        )?;
    }
    // The working copy and the source are the same now.
    cache::record_source(&library.dir, &library.cache)?;
    Ok(api::PullFromSourceResponse {
        files_written: files.len() as u32,
        kept_edits_path: if had_edits {
            library.cache.join(cache::UNSYNCED).display().to_string()
        } else {
            String::new()
        },
    })
}

/// Writes the working copy's databases to the source. Returns how many files were written
/// and where the replaced ones were kept.
pub fn push(
    library: &Library,
    overwrite_outside_changes: bool,
    progress: &Progress,
) -> Result<api::SyncToSourceResponse, SyncError> {
    let (working, source) = (working_databases(library), source_databases(library));
    if !source.join(MAIN_DATABASE).is_file() {
        return Err(SyncError::Unavailable(library.dir.clone()));
    }
    if is_busy(&source) {
        return Err(SyncError::Busy);
    }
    if cache::source_changed(&library.dir, &library.cache)? && !overwrite_outside_changes {
        return Err(SyncError::ChangedOutside);
    }

    let changed = changed_databases(library)?;
    let backup = library.cache.join(BACKUP_DIR);
    // What there is to copy: the old files to the backup and the new ones to the source.
    let size = |path: PathBuf| fs::metadata(path).map_or(0, |metadata| metadata.len());
    progress.phase(
        Phase::Writing,
        changed
            .iter()
            .map(|name| size(cache::source_file(&library.dir, name)) + size(working.join(name)))
            .sum(),
        0,
    );
    if !changed.is_empty() {
        // Keep what is about to be replaced, here and not in the library. (One backup, the
        // latest.)
        let _ = fs::remove_dir_all(&backup);
        fs::create_dir_all(&backup)?;
        for name in &changed {
            let old = cache::source_file(&library.dir, name);
            if old.is_file() {
                progress.copy(&old, &backup.join(name), false)?;
            }
        }
    }

    let mut replaced: Vec<&String> = Vec::new();
    for name in &changed {
        // Written beside the file and moved over it, so that a file is never half written.
        let destination = cache::destination_file(&library.dir, name);
        let temporary = destination.with_file_name(format!(".{name}.syncing"));
        let result = destination
            .parent()
            .map_or(Ok(()), fs::create_dir_all)
            .and_then(|()| progress.copy(&working.join(name), &temporary, false))
            .and_then(|_| fs::rename(&temporary, &destination));
        if let Err(error) = result {
            let _ = fs::remove_file(&temporary);
            // Put back the ones that were already replaced.
            for done in replaced {
                let (kept, place) = (
                    backup.join(done),
                    cache::destination_file(&library.dir, done),
                );
                if kept.is_file() {
                    let _ = fs::copy(kept, place);
                } else {
                    let _ = fs::remove_file(place);
                }
            }
            return Err(error.into());
        }
        replaced.push(name);
    }
    if changed.iter().any(|name| name == origins::FILE) {
        cache::remove_old_origins(&library.dir);
    }

    // The working copy and the source are the same now.
    cache::record_source(&library.dir, &library.cache)?;
    Ok(api::SyncToSourceResponse {
        files_written: changed.len() as u32,
        backup_path: if changed.is_empty() {
            String::new()
        } else {
            backup.display().to_string()
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(name: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "open-engine-lib-sync-{name}-{}",
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

    /// A library on disk, opened through a cache like the real thing.
    fn library(root: &Path) -> Library {
        let dir = root.join("Engine Library");
        fs::create_dir_all(dir.join("Database2")).unwrap();
        let db = Connection::open(dir.join("Database2/m.db")).unwrap();
        db.execute_batch(
            "CREATE TABLE Track (id INTEGER PRIMARY KEY, title TEXT, artist TEXT, album TEXT,
                genre TEXT, comment TEXT, label TEXT, composer TEXT, remixer TEXT, year INTEGER,
                bpmAnalyzed REAL, bpm INTEGER, key INTEGER, rating INTEGER, path TEXT UNIQUE,
                length INTEGER, fileType TEXT, isAnalyzed INTEGER, isBeatGridLocked INTEGER,
                streamingSource TEXT, originDatabaseUuid TEXT, originTrackId INTEGER,
                albumArtId INTEGER);
             CREATE TABLE PerformanceData (trackId INTEGER PRIMARY KEY, beatData BLOB,
                quickCues BLOB, loops BLOB);
             CREATE TABLE Playlist (id INTEGER PRIMARY KEY, title TEXT, parentListId INTEGER,
                nextListId INTEGER);
             CREATE TABLE PlaylistEntity (id INTEGER PRIMARY KEY, listId INTEGER, trackId INTEGER,
                databaseUuid TEXT, nextEntityId INTEGER);
             INSERT INTO Track (id, title, artist, genre, bpmAnalyzed, bpm, path, length, originDatabaseUuid, originTrackId)
                VALUES (1, 'One', 'A', 'House', 124.0, 124, '../Music/one.flac', 200, 'db', 1),
                       (2, 'Two', 'B', 'Techno', 130.0, 130, '../Music/two.flac', 210, 'db', 2),
                       (3, 'Three', 'C', 'Pop', 100.0, 100, '../Music/three.flac', 220, 'db', 3);
             INSERT INTO PerformanceData VALUES (1, x'01', x'02', x'03'), (2, x'01', x'02', x'03');
             INSERT INTO Playlist VALUES (1, 'House', 0, 0), (2, 'Deep', 1, 0), (3, 'Pop', 0, 1);
             INSERT INTO PlaylistEntity VALUES (10, 2, 1, 'db', 11), (11, 2, 2, 'db', 0), (12, 3, 3, 'db', 0);",
        )
        .unwrap();
        drop(db);
        fs::write(dir.join("Database2/hm.db"), b"history v1").unwrap();
        Library::open(&dir, &root.join("cache")).unwrap()
    }

    fn edit(database: &Path, sql: &str) {
        Connection::open(database)
            .unwrap()
            .execute_batch(sql)
            .unwrap();
    }

    fn working(library: &Library) -> PathBuf {
        library.cache.join("Database2/m.db")
    }

    fn source(library: &Library) -> PathBuf {
        library.dir.join("Database2/m.db")
    }

    fn field<'a>(change: &'a api::TrackChange, name: &str) -> &'a api::FieldChange {
        change
            .fields
            .iter()
            .find(|f| f.field == name)
            .unwrap_or_else(|| panic!("no change to {name}"))
    }

    #[test]
    fn a_working_copy_that_was_not_changed_has_no_differences() {
        let temp = TempDir::new("same");
        let library = library(&temp.0);
        let diff = diff(&library, false).unwrap();

        assert!(diff.source_available && !diff.source_busy && !diff.source_changed_outside);
        assert!(diff.changed_databases.is_empty());
        assert_eq!(
            (diff.tracks_added, diff.tracks_removed, diff.tracks_changed),
            (0, 0, 0)
        );
        assert_eq!(diff.playlists_changed, 0);
    }

    #[test]
    fn lists_what_was_changed_in_the_working_copy() {
        let temp = TempDir::new("changes");
        let library = library(&temp.0);
        edit(
            &working(&library),
            "UPDATE Track SET title = 'Uno', rating = 80 WHERE id = 1;
             UPDATE PerformanceData SET beatData = x'FF' WHERE trackId = 2;
             INSERT INTO Track (id, title, artist, path, originDatabaseUuid, originTrackId)
                VALUES (4, 'Four', 'D', '../Music/four.flac', 'db', 4);
             DELETE FROM Track WHERE id = 3;
             UPDATE Playlist SET title = 'Deeper' WHERE id = 2;
             UPDATE Playlist SET parentListId = 1 WHERE id = 3; -- Pop goes into House
             INSERT INTO Playlist VALUES (4, 'New', 0, 0);
             DELETE FROM PlaylistEntity WHERE id = 11;
             INSERT INTO PlaylistEntity VALUES (13, 2, 4, 'db', 0);",
        );

        let diff = diff(&library, false).unwrap();
        assert_eq!(diff.changed_databases, ["m.db"], "the history is as it was");

        // Tracks: one added, one removed, two changed (one by its fields, one by its beatgrid).
        assert_eq!(
            (diff.tracks_added, diff.tracks_removed, diff.tracks_changed),
            (1, 1, 2)
        );
        assert_eq!(diff.added_tracks[0].title, "Four");
        assert_eq!(diff.removed_tracks[0].title, "Three");
        let one = diff
            .changed_tracks
            .iter()
            .find(|c| c.track.as_ref().unwrap().path == "../Music/one.flac")
            .unwrap();
        let title = field(one, "title");
        assert_eq!(
            (title.source_value.as_str(), title.working_value.as_str()),
            ("One", "Uno")
        );
        assert_eq!(field(one, "rating").working_value, "80");
        let two = diff
            .changed_tracks
            .iter()
            .find(|c| c.track.as_ref().unwrap().title == "Two")
            .unwrap();
        assert_eq!(field(two, "beatgrid").working_value, "(changed)");

        // Playlists: renamed (and its tracks changed), moved, added.
        let by_name = |name: &str| {
            diff.changed_playlists
                .iter()
                .find(|p| p.name == name)
                .unwrap_or_else(|| panic!("no {name}: {:?}", diff.changed_playlists))
        };
        let renamed = by_name("House / Deeper");
        assert_eq!(renamed.kind(), api::PlaylistChangeKind::Renamed);
        assert_eq!(renamed.detail, "Deep");
        assert_eq!((renamed.tracks_added, renamed.tracks_removed), (1, 1));
        let moved = by_name("House / Pop");
        assert_eq!(moved.kind(), api::PlaylistChangeKind::Moved);
        assert_eq!(moved.detail, "(top level)", "where it was");
        assert_eq!(by_name("New").kind(), api::PlaylistChangeKind::Added);
    }

    #[test]
    fn smart_playlists_are_listed_when_added_removed_renamed_or_given_other_rules() {
        let temp = TempDir::new("smartlists");
        let library = library(&temp.0);
        let table = "CREATE TABLE Smartlist (listUuid TEXT NOT NULL PRIMARY KEY, title TEXT,
            parentPlaylistPath TEXT, nextPlaylistPath TEXT, nextListUuid TEXT, rules TEXT,
            lastEditTime DATETIME);
            INSERT INTO Smartlist VALUES
                ('a', 'Same', '', '', '', 'r1', NULL),
                ('b', 'Old name', '', '', '', 'r2', NULL),
                ('c', 'Rules', 'House;', '', '', 'r3', NULL),
                ('d', 'Gone', '', '', '', 'r4', NULL),
                ('f', 'Moves', 'House;', '', '', 'r6', NULL);";
        edit(&source(&library), table);
        edit(&working(&library), table);
        assert_eq!(diff(&library, false).unwrap().playlists_changed, 0);

        edit(
            &working(&library),
            "UPDATE Smartlist SET title = 'New name', nextListUuid = 'x' WHERE listUuid = 'b';
             UPDATE Smartlist SET rules = 'other' WHERE listUuid = 'c';
             DELETE FROM Smartlist WHERE listUuid = 'd';
             UPDATE Smartlist SET parentPlaylistPath = '' WHERE listUuid = 'f';
             INSERT INTO Smartlist VALUES ('e', 'Fresh', '', '', '', 'r5', NULL);",
        );
        let diff = diff(&library, false).unwrap();
        assert_eq!(diff.playlists_changed, 5);
        let by_name = |name: &str| {
            diff.changed_playlists
                .iter()
                .find(|p| p.name == name)
                .unwrap_or_else(|| panic!("no {name}: {:?}", diff.changed_playlists))
        };
        assert!(diff.changed_playlists.iter().all(|p| p.smart));
        let renamed = by_name("New name");
        assert_eq!(renamed.kind(), api::PlaylistChangeKind::Renamed);
        assert_eq!(renamed.detail, "Old name");
        assert!(
            !renamed.rules_changed,
            "only the order links changed besides"
        );
        let rules = by_name("House / Rules");
        assert_eq!(rules.kind(), api::PlaylistChangeKind::Rules);
        assert!(rules.rules_changed);
        assert_eq!(by_name("Gone").kind(), api::PlaylistChangeKind::Removed);
        assert_eq!(by_name("Fresh").kind(), api::PlaylistChangeKind::Added);
        let moved = by_name("Moves");
        assert_eq!(moved.kind(), api::PlaylistChangeKind::Moved);
        assert_eq!(moved.detail, "House");
    }

    #[test]
    fn a_playlist_whose_tracks_were_reordered_or_that_was_removed_is_noticed() {
        let temp = TempDir::new("reorder");
        let library = library(&temp.0);
        edit(
            &working(&library),
            "UPDATE PlaylistEntity SET nextEntityId = 0 WHERE id = 11; -- same tracks, order differs
             UPDATE PlaylistEntity SET nextEntityId = 10 WHERE id = 11;
             UPDATE PlaylistEntity SET nextEntityId = 0 WHERE id = 10;
             DELETE FROM Playlist WHERE id = 3;
             DELETE FROM PlaylistEntity WHERE id = 12;",
        );
        let diff = diff(&library, false).unwrap();
        let deep = diff
            .changed_playlists
            .iter()
            .find(|p| p.name == "House / Deep")
            .unwrap();
        assert_eq!(deep.kind(), api::PlaylistChangeKind::Tracks);
        assert!(deep.order_changed);
        assert_eq!((deep.tracks_added, deep.tracks_removed), (0, 0));
        let removed = diff
            .changed_playlists
            .iter()
            .find(|p| p.name == "Pop")
            .unwrap();
        assert_eq!(removed.kind(), api::PlaylistChangeKind::Removed);
    }

    #[test]
    fn saved_origins_are_synced_to_the_source_with_the_other_databases() {
        let temp = TempDir::new("origins");
        let library = library(&temp.0);
        // (Beside the library folder, not in it.)
        let database = library
            .dir
            .parent()
            .unwrap()
            .join("open-engine")
            .join(origins::FILE);
        assert_eq!(diff(&library, false).unwrap().origins_changed, 0);

        // Saved in the working copy, the origins are a difference the source lacks.
        let entries = [
            (("db".to_string(), 1), "/pc/one.flac".to_string()),
            (("db".to_string(), 2), "/pc/two.flac".to_string()),
        ];
        assert_eq!(library.save_origins(&entries, false).unwrap(), 2);
        let found = diff(&library, false).unwrap();
        assert_eq!(found.changed_databases, [origins::FILE]);
        assert_eq!(found.origins_changed, 2);
        assert!(!database.exists(), "the source has none yet");

        let done = push(&library, false, &Progress::default()).unwrap();
        assert_eq!(done.files_written, 1);
        assert!(database.is_file());
        assert_eq!(
            origins::Origins::open(&database).unwrap().all().unwrap(),
            entries.iter().cloned().collect::<HashMap<_, _>>()
        );
        assert_eq!(diff(&library, false).unwrap().origins_changed, 0);

        // A changed origin and a new one are counted; the file is replaced, and the old
        // one is kept in the backup.
        library
            .save_origins(
                &[
                    (("db".to_string(), 1), "/stick/one.flac".to_string()),
                    (("db".to_string(), 3), "/pc/three.flac".to_string()),
                ],
                true,
            )
            .unwrap();
        assert_eq!(diff(&library, false).unwrap().origins_changed, 2);
        let done = push(&library, false, &Progress::default()).unwrap();
        assert!(Path::new(&done.backup_path).join(origins::FILE).is_file());
        let synced = origins::Origins::open(&database).unwrap().all().unwrap();
        assert_eq!(synced[&("db".to_string(), 1)], "/stick/one.flac");
        assert_eq!(synced.len(), 3);
        assert!(
            !library.dir.join("Database2").join(origins::FILE).exists(),
            "none in the library itself"
        );
    }

    #[test]
    fn origins_that_earlier_versions_left_in_database2_are_moved_out() {
        let temp = TempDir::new("origins-move");
        let old = temp.0.join("Engine Library/Database2").join(origins::FILE);
        let dir = library(&temp.0).dir.clone();
        origins::Origins::open(&old)
            .unwrap()
            .save_missing(&[(("db".to_string(), 1), "/pc/one.flac".to_string())])
            .unwrap();
        // The old file is read as the library's origins ...
        let library = Library::open(&dir, &temp.0.join("cache")).unwrap();
        assert_eq!(library.origins().len(), 1);
        // ... and the next sync puts it beside the library folder and takes it away.
        let found = diff(&library, false).unwrap();
        assert_eq!(found.changed_databases, [origins::FILE]);
        push(&library, false, &Progress::default()).unwrap();
        let new = temp.0.join("open-engine").join(origins::FILE);
        assert!(new.is_file());
        assert!(!old.exists());
        assert_eq!(
            origins::Origins::open(&new).unwrap().all().unwrap().len(),
            1
        );
        assert!(diff(&library, false).unwrap().changed_databases.is_empty());
    }

    #[test]
    fn the_reverse_diff_says_what_the_working_copy_would_get() {
        let temp = TempDir::new("reverse-diff");
        let library = library(&temp.0);
        edit(
            &working(&library),
            "UPDATE Track SET title = 'Uno' WHERE id = 1;
             INSERT INTO Track (id, title, artist, path, originDatabaseUuid, originTrackId)
                VALUES (4, 'Four', 'D', '../Music/four.flac', 'db', 4);",
        );

        // Writing: the source gets 'Uno' and a track.
        let forward = diff(&library, false).unwrap();
        assert_eq!((forward.tracks_added, forward.tracks_removed), (1, 0));
        let title = field(&forward.changed_tracks[0], "title");
        assert_eq!(
            (title.source_value.as_str(), title.working_value.as_str()),
            ("One", "Uno")
        );

        // The other way round: the working copy loses the track and gets 'One' back.
        let reverse = diff(&library, true).unwrap();
        assert_eq!((reverse.tracks_added, reverse.tracks_removed), (0, 1));
        assert_eq!(reverse.removed_tracks[0].title, "Four");
        let title = field(&reverse.changed_tracks[0], "title");
        assert_eq!(
            (title.source_value.as_str(), title.working_value.as_str()),
            ("Uno", "One")
        );
        assert_eq!(reverse.changed_databases, forward.changed_databases);
    }

    #[test]
    fn pulling_replaces_the_working_copy_and_keeps_what_was_not_synced() {
        let temp = TempDir::new("pull");
        let library = library(&temp.0);
        let source_before = fs::read(source(&library)).unwrap();

        // Nothing edited: the working copy is replaced, nothing is kept.
        edit(
            &working(&library),
            "UPDATE Track SET title = 'Uno' WHERE id = 1;",
        );
        let done = pull(&library, &Progress::default()).unwrap();
        assert_eq!((done.files_written, done.kept_edits_path.as_str()), (2, ""));
        assert_eq!(fs::read(working(&library)).unwrap(), source_before);

        // Edited through the library (read again, as the backend does after a pull), so that
        // the edit is noted: it is kept aside.
        let library = Library::open(&library.dir, &temp.0.join("cache")).unwrap();
        library.set_rating(0, 60).unwrap();
        assert!(library.cache.join(cache::EDITED).exists());
        let edited = fs::read(working(&library)).unwrap();
        let done = pull(&library, &Progress::default()).unwrap();
        let kept = library.cache.join(cache::UNSYNCED);
        assert_eq!(done.kept_edits_path, kept.display().to_string());
        assert_eq!(fs::read(kept.join("Database2/m.db")).unwrap(), edited);
        assert_eq!(fs::read(working(&library)).unwrap(), source_before);
        assert!(!library.cache.join(cache::EDITED).exists());
        assert_eq!(diff(&library, false).unwrap().changed_databases.len(), 0);
        assert!(!diff(&library, false).unwrap().source_changed_outside);
    }

    #[test]
    fn pulling_is_refused_when_the_source_is_gone_or_busy() {
        let temp = TempDir::new("pull-refused");
        let library = library(&temp.0);
        fs::write(library.dir.join("Database2/m.db-journal"), b"x").unwrap();
        assert!(matches!(
            pull(&library, &Progress::default()),
            Err(SyncError::Busy)
        ));
        fs::remove_file(library.dir.join("Database2/m.db-journal")).unwrap();
        fs::remove_file(library.dir.join("Database2/m.db")).unwrap();
        assert!(matches!(
            pull(&library, &Progress::default()),
            Err(SyncError::Unavailable(_))
        ));
    }

    #[test]
    fn other_databases_that_differ_are_named() {
        let temp = TempDir::new("others");
        let library = library(&temp.0);
        fs::write(library.cache.join("Database2/hm.db"), b"history v2, longer").unwrap();
        let diff = diff(&library, false).unwrap();
        assert_eq!(diff.changed_databases, ["hm.db"]);
        assert_eq!(diff.tracks_changed, 0, "only m.db is itemised");
    }

    #[test]
    fn says_so_when_the_source_cannot_be_reached_or_is_busy_or_was_changed() {
        let temp = TempDir::new("states");
        let library = library(&temp.0);

        // Someone else changes the source while the app has the library open.
        edit(
            &source(&library),
            "UPDATE Track SET title = 'Edited elsewhere' WHERE id = 1;",
        );
        let outside = diff(&library, false).unwrap();
        assert!(outside.source_changed_outside);
        // The difference is there too: syncing would put 'One' back.
        let change = &outside.changed_tracks[0];
        assert_eq!(field(change, "title").source_value, "Edited elsewhere");
        assert_eq!(field(change, "title").working_value, "One");

        // A journal with something in it means a write is going on.
        fs::write(library.dir.join("Database2/m.db-journal"), b"half a write").unwrap();
        assert!(diff(&library, false).unwrap().source_busy);

        // And a source that is gone (the drive was unplugged).
        fs::rename(&library.dir, temp.0.join("gone")).unwrap();
        let gone = diff(&library, false).unwrap();
        assert!(!gone.source_available);
        assert!(gone.changed_databases.is_empty());
    }

    #[test]
    fn syncing_makes_the_source_the_working_copy_and_keeps_a_backup() {
        let temp = TempDir::new("push");
        let library = library(&temp.0);
        edit(
            &working(&library),
            "UPDATE Track SET title = 'Uno' WHERE id = 1;",
        );
        fs::write(library.cache.join("Database2/hm.db"), b"history v2").unwrap();
        let old_main = fs::read(source(&library)).unwrap();

        let done = push(&library, false, &Progress::default()).unwrap();
        assert_eq!(done.files_written, 2);

        // The source holds what the working copy does, and what was there is kept.
        assert_eq!(
            fs::read(source(&library)).unwrap(),
            fs::read(working(&library)).unwrap()
        );
        assert_eq!(
            fs::read(library.dir.join("Database2/hm.db")).unwrap(),
            b"history v2"
        );
        let backup = Path::new(&done.backup_path);
        assert_eq!(fs::read(backup.join("m.db")).unwrap(), old_main);
        assert_eq!(fs::read(backup.join("hm.db")).unwrap(), b"history v1");
        assert!(
            !library.dir.join("Database2/.m.db.syncing").exists(),
            "no temporary file is left"
        );

        // Now there is nothing left to sync, and nothing looks changed from outside.
        let after = diff(&library, false).unwrap();
        assert!(after.changed_databases.is_empty() && !after.source_changed_outside);
        assert_eq!(
            push(&library, false, &Progress::default())
                .unwrap()
                .files_written,
            0,
            "nothing to write"
        );
    }

    #[test]
    fn a_source_changed_by_someone_else_is_only_overwritten_when_that_is_meant() {
        let temp = TempDir::new("outside-push");
        let library = library(&temp.0);
        edit(
            &working(&library),
            "UPDATE Track SET title = 'Mine' WHERE id = 1;",
        );
        edit(
            &source(&library),
            "UPDATE Track SET title = 'Theirs' WHERE id = 2;",
        );
        let theirs = fs::read(source(&library)).unwrap();

        assert!(matches!(
            push(&library, false, &Progress::default()),
            Err(SyncError::ChangedOutside)
        ));
        assert_eq!(
            fs::read(source(&library)).unwrap(),
            theirs,
            "refusing changes nothing"
        );

        assert_eq!(
            push(&library, true, &Progress::default())
                .unwrap()
                .files_written,
            1
        );
        assert_eq!(
            fs::read(source(&library)).unwrap(),
            fs::read(working(&library)).unwrap()
        );
        // Their changes are in the backup.
        let backup = library.cache.join(BACKUP_DIR).join("m.db");
        assert_eq!(fs::read(backup).unwrap(), theirs);
        assert!(
            !library.dir.join("Database2").join(BACKUP_DIR).exists(),
            "nothing is kept in the library"
        );
    }

    #[test]
    fn syncing_is_refused_when_the_source_is_busy_or_gone() {
        let temp = TempDir::new("refuse");
        let library = library(&temp.0);
        edit(
            &working(&library),
            "UPDATE Track SET title = 'Uno' WHERE id = 1;",
        );
        let before = fs::read(source(&library)).unwrap();

        fs::write(library.dir.join("Database2/m.db-wal"), b"uncommitted").unwrap();
        assert!(matches!(
            push(&library, true, &Progress::default()),
            Err(SyncError::Busy)
        ));
        assert_eq!(fs::read(source(&library)).unwrap(), before);
        fs::remove_file(library.dir.join("Database2/m.db-wal")).unwrap();

        fs::rename(&library.dir, temp.0.join("gone")).unwrap();
        assert!(matches!(
            push(&library, true, &Progress::default()),
            Err(SyncError::Unavailable(_))
        ));
    }

    #[test]
    fn a_failure_part_way_puts_back_what_was_already_replaced() {
        let temp = TempDir::new("rollback");
        let library = library(&temp.0);
        edit(
            &working(&library),
            "UPDATE Track SET title = 'Uno' WHERE id = 1;",
        );
        fs::write(library.cache.join("Database2/hm.db"), b"history v2").unwrap();

        // Files are written in name order: hm.db goes through, then m.db can't be written
        // because its temporary file is blocked by a folder.
        fs::create_dir_all(library.dir.join("Database2/.m.db.syncing/in-the-way")).unwrap();
        let old_main = fs::read(source(&library)).unwrap();

        assert!(matches!(
            push(&library, false, &Progress::default()),
            Err(SyncError::Failed(_))
        ));
        assert_eq!(
            fs::read(library.dir.join("Database2/hm.db")).unwrap(),
            b"history v1",
            "put back"
        );
        assert_eq!(fs::read(source(&library)).unwrap(), old_main);
    }

    #[test]
    fn a_path_with_odd_characters_still_opens_as_a_database_uri() {
        assert_eq!(
            source_uri(Path::new("/a b/c%d/e?f#g/m.db")),
            "file:/a%20b/c%25d/e%3Ff%23g/m.db?mode=ro&immutable=1"
        );
    }
}
