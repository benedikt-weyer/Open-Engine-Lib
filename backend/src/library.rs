//! Reading an Engine DJ library (see `docs/engine-library-format.md`).
//!
//! A library is an `Engine Library` folder whose `Database2/m.db` is a plain SQLite
//! database. The databases are copied into a cache first (see `cache`) and read from
//! there, never in place. Only the track metadata and the playlist structure are
//! read; the analysis BLOBs are left alone. Track paths point at the original audio
//! files, which are not copied.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

use rusqlite::types::Value;
use rusqlite::{Connection, OpenFlags, OptionalExtension};

use crate::beatgrid::{self, Marker};
use crate::cache::{self, DATABASE_DIR};
use crate::cues::{self, Cue, QuickCues};
use crate::loops::{self, Loop, QuickLoops};
use crate::origins::{self, Origins, TrackKey};
use crate::smartlist::Rules;
use crate::soundswitch::{Lighting, SoundSwitch};

const DATABASE: &str = "Database2/m.db";
const LIBRARY_DIR: &str = "Engine Library";
pub const ARTWORK_DIR: &str = "Artwork";

#[derive(Debug)]
pub enum OpenError {
    /// No `Database2/m.db` was found at or just below the chosen folder.
    NotALibrary(PathBuf),
    /// The databases could not be copied into the cache.
    Cache(std::io::Error),
    Database(rusqlite::Error),
}

impl fmt::Display for OpenError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            OpenError::NotALibrary(path) => write!(
                f,
                "{} does not contain an Engine library (Database2/m.db)",
                path.display()
            ),
            OpenError::Cache(error) => {
                write!(
                    f,
                    "could not copy the library databases to the cache: {error}"
                )
            }
            OpenError::Database(error) => write!(f, "{error}"),
        }
    }
}

#[derive(Debug)]
pub enum EditError {
    /// There is no track with that index.
    NoSuchTrack,
    /// There is no playlist with that index.
    NoSuchPlaylist,
    /// The playlist is not a smart playlist.
    NotSmart,
    /// The playlist is a smart one, which is changed through its rules.
    IsSmart,
    /// A playlist needs a title.
    EmptyTitle,
    /// Another playlist in the same place has the title.
    TitleTaken,
    /// The library has no table for smart playlists (it is from an older version).
    NoSmartlists,
    /// The track has no analysis to keep cues in.
    NoAnalysis,
    /// The track has cues stored in a way that isn't understood, which are left alone.
    UnreadableCues,
    /// The cues are not valid; says what is wrong.
    BadCue(String),
    /// The track has loops stored in a way that isn't understood, which are left alone.
    UnreadableLoops,
    /// The loops are not valid; says what is wrong.
    BadLoop(String),
    /// The edit could not be noted in the cache.
    Cache(std::io::Error),
    Database(rusqlite::Error),
}

impl fmt::Display for EditError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            EditError::NoSuchTrack => write!(f, "there is no such track"),
            EditError::NoSuchPlaylist => write!(f, "there is no such playlist"),
            EditError::NotSmart => write!(f, "the playlist is not a smart playlist"),
            EditError::IsSmart => write!(f, "the playlist is a smart playlist"),
            EditError::EmptyTitle => write!(f, "a playlist needs a title"),
            EditError::TitleTaken => write!(f, "there is already a playlist with that title"),
            EditError::NoSmartlists => {
                write!(f, "this library has no smart playlists (it is too old)")
            }
            EditError::NoAnalysis => {
                write!(f, "the track has not been analysed, so it has no cues")
            }
            EditError::UnreadableCues => write!(
                f,
                "the cues of the track are in a format that is not understood"
            ),
            EditError::BadCue(reason) | EditError::BadLoop(reason) => write!(f, "{reason}"),
            EditError::UnreadableLoops => {
                write!(
                    f,
                    "the loops of the track are in a format that is not understood"
                )
            }
            EditError::Cache(error) => write!(f, "could not note the edit in the cache: {error}"),
            EditError::Database(error) => write!(f, "could not write the database: {error}"),
        }
    }
}

impl From<rusqlite::Error> for EditError {
    fn from(error: rusqlite::Error) -> Self {
        EditError::Database(error)
    }
}

impl From<rusqlite::Error> for OpenError {
    fn from(error: rusqlite::Error) -> Self {
        OpenError::Database(error)
    }
}

#[derive(Debug, Clone)]
pub struct Track {
    /// The track's id in the Engine database.
    pub db_id: i64,
    pub title: String,
    pub artist: String,
    pub album: String,
    pub genre: String,
    pub bpm: Option<f64>,
    /// Length in whole seconds.
    pub length: Option<u32>,
    /// The audio file as the library records it, relative to the library folder.
    pub relative_path: String,
    /// Absolute-ish path to the audio file (the library's relative path joined onto its folder).
    pub path: PathBuf,
    /// `(originDatabaseUuid, originTrackId)`: where the track came from, which is the same
    /// in every library it was copied to. Not set if the library doesn't say.
    pub origin_key: Option<(String, i64)>,
    /// Streaming tracks (e.g. TIDAL) only exist on the player, never as a file here.
    pub streaming: bool,
    /// `AlbumArt.id` of the track's cover, for `Library::album_art`.
    pub album_art: Option<u32>,
    /// The star rating, 0 to 100 with 20 to the star, if the library has one, as it was
    /// when the library was opened. See `Library::rating` for what it is now.
    pub rating: Option<u32>,
    /// What SoundSwitch has for the track.
    pub lighting: Lighting,
}

/// A playlist or playlist folder. The playlists are stored depth first in display
/// order, so a flat list is enough to draw the tree.
#[derive(Debug)]
pub struct Playlist {
    pub title: String,
    pub depth: usize,
    /// Index of the parent in `Library::playlists`.
    pub parent: Option<usize>,
    pub has_children: bool,
    /// A smart playlist: its tracks are the ones that satisfy its rules.
    pub smart: bool,
    /// The `id` of a normal playlist in the database.
    pub db_id: Option<i64>,
    /// The `listUuid` of a smart playlist.
    pub uuid: Option<String>,
    /// The rules of a smart playlist, if they could be read.
    pub rules: Option<Rules>,
    /// The rules of a smart playlist as the JSON text that is stored.
    pub raw_rules: Option<String>,
    /// Indices into `Library::tracks`, in playlist order (in library order for a smart one).
    pub tracks: Vec<usize>,
}

#[derive(Debug)]
pub struct Library {
    /// The `Engine Library` folder that was opened. Track paths are relative to it,
    /// and the audio files are played from there.
    pub dir: PathBuf,
    /// The cached copy of the library folder, which holds only `Database2`.
    pub cache: PathBuf,
    pub tracks: Vec<Track>,
    /// Behind a lock because adding or editing a smart playlist changes the list.
    playlists: Mutex<Vec<Playlist>>,
    by_id: TrackIds,
    by_origin: TrackOrigins,
    /// Where earlier versions kept the origins of all libraries, to take over from.
    legacy_origins: PathBuf,
    /// Ratings set since the library was opened, by index in `tracks`.
    ratings: Mutex<HashMap<usize, u32>>,
    /// How many cues each track has (by index in `tracks`), as of now.
    cue_counts: Mutex<Vec<u32>>,
    /// The same for loops.
    loop_counts: Mutex<Vec<u32>>,
    /// Kept open to read what is not loaded with the tracks (covers, beatgrids) on demand,
    /// and to write the edits.
    connection: Mutex<Connection>,
}

/// A cover image as the file it is stored as.
#[derive(Debug, PartialEq, Eq)]
pub struct AlbumArt {
    pub data: Vec<u8>,
    pub mime_type: &'static str,
}

impl Library {
    /// Opens the library at `path`, which may be the `Engine Library` folder, the
    /// drive or folder that contains it, or its `Database2` folder. Its databases are
    /// copied to (or refreshed in) a folder under `cache_root`, which is what is read.
    pub fn open(path: &Path, cache_root: &Path) -> Result<Self, OpenError> {
        let dir = locate(path).ok_or_else(|| OpenError::NotALibrary(path.to_path_buf()))?;
        let cache = cache::sync(&dir, cache_root).map_err(OpenError::Cache)?;

        // The copy is ours, so it is opened read-write: if the library was copied in
        // the middle of a write, SQLite can roll that back using the copied journal.
        let connection = Connection::open_with_flags(
            cache.join(DATABASE),
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        Self::load(connection, dir, cache)
    }

    fn load(connection: Connection, dir: PathBuf, cache: PathBuf) -> Result<Self, OpenError> {
        Self::load_with(connection, dir, cache, true)
    }

    /// Opens the database at `database` (a copy that is to be edited and thrown away or
    /// installed somewhere) as a library of the folder `dir`. `scratch` is where it notes
    /// that it was edited. SoundSwitch is not looked at.
    pub fn open_copy(database: &Path, dir: PathBuf, scratch: PathBuf) -> Result<Self, OpenError> {
        let connection = Connection::open_with_flags(
            database,
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        Self::load_with(connection, dir, scratch, false)
    }

    fn load_with(
        connection: Connection,
        dir: PathBuf,
        cache: PathBuf,
        lighting: bool,
    ) -> Result<Self, OpenError> {
        let (tracks, by_id, by_origin) = load_tracks(&connection, &dir, lighting)?;
        let playlists = load_playlists(&connection, &tracks, &by_id, &by_origin)?;
        let cue_counts = load_counts(&connection, "quickCues", &by_id, tracks.len(), |blob| {
            QuickCues::parse(blob).map(|cues| cues.count())
        });
        let loop_counts = load_counts(&connection, "loops", &by_id, tracks.len(), |blob| {
            QuickLoops::parse(blob).map(|loops| loops.count())
        });
        Ok(Self {
            dir,
            cache,
            tracks,
            playlists: Mutex::new(playlists),
            by_id,
            by_origin,
            legacy_origins: origins::legacy_path(),
            ratings: Mutex::default(),
            cue_counts: Mutex::new(cue_counts),
            loop_counts: Mutex::new(loop_counts),
            connection: Mutex::new(connection),
        })
    }

    /// The playlists, depth first in display order. Indices into it identify a playlist
    /// until a smart playlist is added.
    pub fn playlists(&self) -> MutexGuard<'_, Vec<Playlist>> {
        self.playlists.lock().expect("playlists lock")
    }

    /// Adds a smart playlist at the top level, after the smart playlists that are
    /// already there, and returns its index in `playlists`.
    pub fn create_smart_playlist(&self, title: &str, rules: &Rules) -> Result<usize, EditError> {
        self.create_smart_playlist_in(title, None, &rules.to_json())
    }

    /// Adds a smart playlist with the rules as the JSON text Engine keeps (they are not
    /// looked at) to `parent` (an index into `playlists`; `None` for the top level), after
    /// the smart playlists that are already there. Returns its index in `playlists`.
    pub fn create_smart_playlist_in(
        &self,
        title: &str,
        parent: Option<usize>,
        rules: &str,
    ) -> Result<usize, EditError> {
        let title = title.trim();
        if title.is_empty() {
            return Err(EditError::EmptyTitle);
        }
        let parent_path = {
            let playlists = self.playlists();
            let parent_path = match parent {
                Some(parent) => {
                    let list = playlists.get(parent).ok_or(EditError::NoSuchPlaylist)?;
                    if list.smart {
                        return Err(EditError::IsSmart);
                    }
                    playlist_path(&playlists, parent)
                }
                None => String::new(),
            };
            if playlists
                .iter()
                .any(|playlist| playlist.parent == parent && playlist.title == title)
            {
                return Err(EditError::TitleTaken);
            }
            parent_path
        };
        cache::mark_edited(&self.cache).map_err(EditError::Cache)?;
        let connection = self.connection.lock().expect("database lock");
        if !has_smartlists(&connection) {
            return Err(EditError::NoSmartlists);
        }

        let uuid = new_uuid(&connection)?;

        // The new one goes after the last smart playlist, and in front of the same
        // playlist it was in front of.
        let transaction = connection.unchecked_transaction()?;
        let last: Option<(String, String)> = transaction
            .query_row(
                "SELECT listUuid, COALESCE(nextPlaylistPath, '') FROM Smartlist \
                 WHERE COALESCE(parentPlaylistPath, '') = ?1 \
                   AND COALESCE(nextListUuid, '') = '' LIMIT 1",
                [&parent_path],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let next_path = match &last {
            Some((last_uuid, next_path)) => {
                transaction.execute(
                    "UPDATE Smartlist SET nextListUuid = ?1, nextPlaylistPath = '' \
                     WHERE listUuid = ?2",
                    (&uuid, last_uuid),
                )?;
                next_path.as_str()
            }
            None => "",
        };
        transaction.execute(
            "INSERT INTO Smartlist (listUuid, title, parentPlaylistPath, nextPlaylistPath, \
                                    nextListUuid, rules, lastEditTime) \
             VALUES (?1, ?2, ?3, ?4, '', ?5, datetime('now'))",
            (&uuid, title, &parent_path, next_path, rules),
        )?;
        transaction.commit()?;

        self.reload_playlists(&connection)?;
        self.playlists()
            .iter()
            .position(|playlist| playlist.uuid.as_deref() == Some(uuid.as_str()))
            .ok_or(EditError::NoSuchPlaylist)
    }

    /// Changes the title and rules of the smart playlist at `index` in `playlists`, and
    /// returns its index afterwards (a new title can move it).
    pub fn update_smart_playlist(
        &self,
        index: usize,
        title: &str,
        rules: &Rules,
    ) -> Result<usize, EditError> {
        self.update_smart_playlist_json(index, title, &rules.to_json())
    }

    /// Like `update_smart_playlist`, with the rules as the JSON text Engine keeps.
    pub fn update_smart_playlist_json(
        &self,
        index: usize,
        title: &str,
        rules: &str,
    ) -> Result<usize, EditError> {
        let title = title.trim();
        if title.is_empty() {
            return Err(EditError::EmptyTitle);
        }
        let uuid = {
            let playlists = self.playlists();
            let playlist = playlists.get(index).ok_or(EditError::NoSuchPlaylist)?;
            let uuid = playlist.uuid.clone().ok_or(EditError::NotSmart)?;
            if playlists.iter().enumerate().any(|(other, sibling)| {
                other != index && sibling.parent == playlist.parent && sibling.title == title
            }) {
                return Err(EditError::TitleTaken);
            }
            uuid
        };
        cache::mark_edited(&self.cache).map_err(EditError::Cache)?;
        let connection = self.connection.lock().expect("database lock");
        let changed = connection.execute(
            "UPDATE Smartlist SET title = ?1, rules = ?2, lastEditTime = datetime('now') \
             WHERE listUuid = ?3",
            (title, rules, &uuid),
        )?;
        if changed == 0 {
            return Err(EditError::NoSuchPlaylist);
        }

        self.reload_playlists(&connection)?;
        self.playlists()
            .iter()
            .position(|playlist| playlist.uuid.as_deref() == Some(uuid.as_str()))
            .ok_or(EditError::NoSuchPlaylist)
    }

    /// Adds an empty playlist at the end of `parent`'s playlists (an index into
    /// `playlists`; `None` for the top level) and returns its index.
    pub fn create_playlist(&self, title: &str, parent: Option<usize>) -> Result<usize, EditError> {
        let title = title.trim();
        if title.is_empty() {
            return Err(EditError::EmptyTitle);
        }
        let parent_id = {
            let playlists = self.playlists();
            let parent_id = match parent {
                Some(parent) => {
                    let parent = playlists.get(parent).ok_or(EditError::NoSuchPlaylist)?;
                    parent.db_id.ok_or(EditError::IsSmart)?
                }
                None => 0,
            };
            if playlists
                .iter()
                .any(|playlist| playlist.parent == parent && playlist.title == title)
            {
                return Err(EditError::TitleTaken);
            }
            parent_id
        };
        cache::mark_edited(&self.cache).map_err(EditError::Cache)?;
        let connection = self.connection.lock().expect("database lock");
        // Inserted with no next playlist, which makes it the last: the database's own
        // trigger links the one that was last to it.
        connection.execute(
            "INSERT INTO Playlist (title, parentListId, isPersisted, nextListId, \
                                   lastEditTime, isExplicitlyExported) \
             VALUES (?1, ?2, 1, 0, datetime('now'), 1)",
            (title, parent_id),
        )?;
        let id = connection.last_insert_rowid();

        self.reload_playlists(&connection)?;
        self.playlists()
            .iter()
            .position(|playlist| playlist.db_id == Some(id))
            .ok_or(EditError::NoSuchPlaylist)
    }

    /// Gives the playlist at `index` in `playlists` another title. Smart playlists that
    /// are in it, or in front of it, follow it: they name it by its path.
    pub fn rename_playlist(&self, index: usize, title: &str) -> Result<(), EditError> {
        let title = title.trim();
        if title.is_empty() {
            return Err(EditError::EmptyTitle);
        }
        let (id, old_path, new_path) = {
            let playlists = self.playlists();
            let playlist = playlists.get(index).ok_or(EditError::NoSuchPlaylist)?;
            let id = playlist.db_id.ok_or(EditError::IsSmart)?;
            if playlists.iter().enumerate().any(|(other, sibling)| {
                other != index && sibling.parent == playlist.parent && sibling.title == title
            }) {
                return Err(EditError::TitleTaken);
            }
            let old_path = playlist_path(&playlists, index);
            let parent_path = playlist
                .parent
                .map_or_else(String::new, |parent| playlist_path(&playlists, parent));
            (id, old_path, format!("{parent_path}{title};"))
        };
        cache::mark_edited(&self.cache).map_err(EditError::Cache)?;
        let connection = self.connection.lock().expect("database lock");
        let transaction = connection.unchecked_transaction()?;
        transaction.execute(
            "UPDATE Playlist SET title = ?1, lastEditTime = datetime('now') WHERE id = ?2",
            (title, id),
        )?;
        if has_smartlists(&transaction) {
            let length = old_path.chars().count() as i64;
            for column in ["parentPlaylistPath", "nextPlaylistPath"] {
                transaction.execute(
                    &format!(
                        "UPDATE Smartlist SET {column} = ?1 || substr({column}, ?2) \
                         WHERE substr({column}, 1, ?3) = ?4"
                    ),
                    (&new_path, length + 1, length, &old_path),
                )?;
            }
        }
        transaction.commit()?;

        self.reload_playlists(&connection)?;
        Ok(())
    }

    /// Deletes the playlist at `index` in `playlists` with everything in it, the
    /// playlists and smart playlists below it. The tracks stay in the library.
    pub fn delete_playlist(&self, index: usize) -> Result<(), EditError> {
        let (ids, uuids, path, parent_path) = {
            let playlists = self.playlists();
            let playlist = playlists.get(index).ok_or(EditError::NoSuchPlaylist)?;
            playlist.db_id.ok_or(EditError::IsSmart)?;
            // Playlists are stored depth first: what is in it follows it.
            let end = playlists[index + 1..]
                .iter()
                .position(|inner| inner.depth <= playlist.depth)
                .map_or(playlists.len(), |offset| index + 1 + offset);
            let inside = &playlists[index..end];
            (
                inside
                    .iter()
                    .filter_map(|inner| inner.db_id)
                    .collect::<Vec<_>>(),
                inside
                    .iter()
                    .filter_map(|inner| inner.uuid.clone())
                    .collect::<Vec<_>>(),
                playlist_path(&playlists, index),
                playlist
                    .parent
                    .map_or_else(String::new, |parent| playlist_path(&playlists, parent)),
            )
        };
        cache::mark_edited(&self.cache).map_err(EditError::Cache)?;
        let connection = self.connection.lock().expect("database lock");
        let transaction = connection.unchecked_transaction()?;

        if has_smartlists(&transaction) {
            // A smart playlist that stood in front of it now stands in front of the
            // playlist after it, if there is one.
            let next: Option<String> = transaction
                .query_row(
                    "SELECT title FROM Playlist WHERE id = \
                     (SELECT nextListId FROM Playlist WHERE id = ?1)",
                    [ids[0]],
                    |row| row.get(0),
                )
                .optional()?
                .flatten();
            let next_path = next.map_or_else(String::new, |title| format!("{parent_path}{title};"));
            transaction.execute(
                "UPDATE OR IGNORE Smartlist SET nextPlaylistPath = ?1 \
                 WHERE COALESCE(parentPlaylistPath, '') = ?2 AND nextPlaylistPath = ?3",
                (next_path, &parent_path, &path),
            )?;
            for uuid in &uuids {
                transaction.execute("DELETE FROM Smartlist WHERE listUuid = ?1", [uuid])?;
            }
        }
        for id in &ids {
            transaction.execute("DELETE FROM PlaylistEntity WHERE listId = ?1", [id])?;
        }
        // The ones inside first: the database's trigger relinks the siblings of each.
        for id in ids.iter().rev() {
            transaction.execute("DELETE FROM Playlist WHERE id = ?1", [id])?;
        }
        transaction.commit()?;

        self.reload_playlists(&connection)?;
        Ok(())
    }

    /// Deletes the smart playlist at `index` in `playlists`.
    pub fn delete_smart_playlist(&self, index: usize) -> Result<(), EditError> {
        let uuid = {
            let playlists = self.playlists();
            let playlist = playlists.get(index).ok_or(EditError::NoSuchPlaylist)?;
            playlist.uuid.clone().ok_or(EditError::NotSmart)?
        };
        cache::mark_edited(&self.cache).map_err(EditError::Cache)?;
        let connection = self.connection.lock().expect("database lock");
        let transaction = connection.unchecked_transaction()?;
        let (next_uuid, next_path): (String, String) = transaction.query_row(
            "SELECT COALESCE(nextListUuid, ''), COALESCE(nextPlaylistPath, '') FROM Smartlist \
             WHERE listUuid = ?1",
            [&uuid],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        transaction.execute("DELETE FROM Smartlist WHERE listUuid = ?1", [&uuid])?;
        // The one in front of it now leads to what it led to.
        transaction.execute(
            "UPDATE Smartlist SET nextListUuid = ?1, nextPlaylistPath = ?2 WHERE nextListUuid = ?3",
            (next_uuid, next_path, &uuid),
        )?;
        transaction.commit()?;

        self.reload_playlists(&connection)?;
        Ok(())
    }

    /// Makes the tracks of the playlist at `index` in `playlists` exactly `tracks` (indices
    /// into `Library::tracks`), in that order. Tracks that are already in it keep their
    /// entries. Tracks without a recorded origin can't be listed and are left out.
    pub fn set_playlist_tracks(&self, index: usize, tracks: &[usize]) -> Result<(), EditError> {
        let list = {
            let playlists = self.playlists();
            let playlist = playlists.get(index).ok_or(EditError::NoSuchPlaylist)?;
            playlist.db_id.ok_or(EditError::IsSmart)?
        };
        // An entry names its track by the track's origin.
        let mut wanted: Vec<&(String, i64)> = Vec::new();
        for &track in tracks {
            let track = self.tracks.get(track).ok_or(EditError::NoSuchTrack)?;
            if let Some(key) = &track.origin_key
                && !wanted.contains(&key)
            {
                wanted.push(key);
            }
        }
        cache::mark_edited(&self.cache).map_err(EditError::Cache)?;
        let connection = self.connection.lock().expect("database lock");
        let transaction = connection.unchecked_transaction()?;

        let existing: Vec<(i64, String, i64)> = {
            let mut statement = transaction.prepare(
                "SELECT id, COALESCE(databaseUuid, ''), trackId FROM PlaylistEntity WHERE listId = ?1",
            )?;
            statement
                .query_map([list], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?
                .collect::<rusqlite::Result<_>>()?
        };
        // Entries of tracks that are not wanted go (the database's trigger relinks the
        // neighbours of each).
        for (id, uuid, track) in &existing {
            if !wanted.iter().any(|(u, t)| u == uuid && t == track) {
                transaction.execute("DELETE FROM PlaylistEntity WHERE id = ?1", [id])?;
            }
        }
        // The wanted ones get an entry if they have none, and then all are linked in order.
        let mut ids = Vec::with_capacity(wanted.len());
        for (uuid, track) in wanted {
            let id = match existing.iter().find(|(_, u, t)| u == uuid && t == track) {
                Some((id, _, _)) => *id,
                None => {
                    transaction.execute(
                        "INSERT INTO PlaylistEntity (listId, trackId, databaseUuid, nextEntityId, \
                                                     membershipReference) \
                         VALUES (?1, ?2, ?3, 0, 0)",
                        (list, track, uuid),
                    )?;
                    transaction.last_insert_rowid()
                }
            };
            ids.push(id);
        }
        for (position, id) in ids.iter().enumerate() {
            let next = ids.get(position + 1).copied().unwrap_or(0);
            transaction.execute(
                "UPDATE PlaylistEntity SET nextEntityId = ?1 WHERE id = ?2",
                (next, id),
            )?;
        }
        transaction.execute(
            "UPDATE Playlist SET lastEditTime = datetime('now') WHERE id = ?1",
            [list],
        )?;
        transaction.commit()?;

        self.reload_playlists(&connection)?;
        Ok(())
    }

    /// Reads the playlists again after the database changed.
    fn reload_playlists(&self, connection: &Connection) -> rusqlite::Result<()> {
        let playlists = load_playlists(connection, &self.tracks, &self.by_id, &self.by_origin)?;
        *self.playlists() = playlists;
        Ok(())
    }

    /// For each track that has a file here and says where it came from: its index in
    /// `tracks`, that origin and the file's location, as a plain path (`..` resolved without
    /// asking the file system, so it also works for a drive that is gone).
    pub fn locations(&self) -> Vec<(usize, (String, i64), String)> {
        self.tracks
            .iter()
            .enumerate()
            .filter(|(_, track)| !track.streaming)
            .filter_map(|(index, track)| {
                let key = track.origin_key.clone()?;
                Some((index, key, resolve(&track.path).display().to_string()))
            })
            .collect()
    }

    /// Where the library's own database of origins is: in the working copy, like the
    /// databases of Engine, so that syncing takes it along.
    fn origins_path(&self) -> PathBuf {
        self.cache.join(DATABASE_DIR).join(origins::FILE)
    }

    /// The saved origins (the library has none until one is saved).
    pub fn origins(&self) -> HashMap<TrackKey, String> {
        Origins::open_existing(&self.origins_path())
            .ok()
            .flatten()
            .and_then(|origins| origins.all().ok())
            .unwrap_or_default()
    }

    /// Saves origins in the working copy: with `overwrite` they replace the ones the
    /// tracks have, else only tracks that have none get one. Returns how many were new
    /// (or, with `overwrite`, different).
    pub fn save_origins(
        &self,
        entries: &[(TrackKey, String)],
        overwrite: bool,
    ) -> Result<usize, EditError> {
        let path = self.origins_path();
        let first = !path.is_file();
        if first && entries.is_empty() {
            return Ok(0); // (no file for nothing)
        }
        let origins =
            Origins::open(&path).map_err(|error| EditError::Cache(std::io::Error::other(error)))?;
        // The first time, the origins that earlier versions kept for all libraries come
        // along (those of this library's tracks), so that saving them again is not needed.
        if first
            && let Ok(Some(legacy)) = Origins::open_existing(&self.legacy_origins)
            && let Ok(all) = legacy.all()
        {
            let mine: HashSet<&TrackKey> = self
                .tracks
                .iter()
                .filter_map(|track| track.origin_key.as_ref())
                .collect();
            let inherited: Vec<(TrackKey, String)> = all
                .into_iter()
                .filter(|(key, _)| mine.contains(key))
                .collect();
            origins.save_missing(&inherited)?;
        }
        let changed = if overwrite {
            origins.replace(entries)?
        } else {
            origins.save_missing(entries)?
        };
        if changed > 0 || first {
            // Noted, so that opening the library again can't throw the origins away
            // before they reached the source.
            cache::mark_edited(&self.cache).map_err(EditError::Cache)?;
        }
        Ok(changed)
    }

    /// The star rating of the track at `index` in `tracks`, 0 to 100, as it is now.
    pub fn rating(&self, index: usize) -> Option<u32> {
        let set = self
            .ratings
            .lock()
            .expect("ratings lock")
            .get(&index)
            .copied();
        set.or_else(|| self.tracks.get(index).and_then(|track| track.rating))
    }

    /// Sets the rating (0 to 100) of the track at `index` in `tracks` in the working copy.
    /// The source only gets it when the working copy is written to it (`sync::push`).
    pub fn set_rating(&self, index: usize, rating: u32) -> Result<(), EditError> {
        let track = self.tracks.get(index).ok_or(EditError::NoSuchTrack)?;
        // Noted first: were the edit to be made and the note not, a later open could
        // replace the working copy without keeping the edit.
        cache::mark_edited(&self.cache).map_err(EditError::Cache)?;
        let connection = self.connection.lock().expect("database lock");
        let changed = connection.execute(
            "UPDATE Track SET rating = ?1 WHERE id = ?2",
            (rating, track.db_id),
        )?;
        if changed == 0 {
            return Err(EditError::NoSuchTrack);
        }
        self.ratings
            .lock()
            .expect("ratings lock")
            .insert(index, rating);
        Ok(())
    }

    /// How many cues the track at `index` in `tracks` has set, as it is now.
    pub fn cue_count(&self, index: usize) -> u32 {
        let counts = self.cue_counts.lock().expect("cue counts lock");
        counts.get(index).copied().unwrap_or(0)
    }

    /// The cues of the track at `index` in `tracks`, in slot order. None if the track has
    /// none, has not been analysed, or has them stored in a way that isn't understood.
    pub fn cues(&self, index: usize) -> rusqlite::Result<Vec<Cue>> {
        let Some(track) = self.tracks.get(index) else {
            return Ok(Vec::new());
        };
        let connection = self.connection.lock().expect("database lock");
        let Some(blobs) = read_analysis(&connection, track.db_id)? else {
            return Ok(Vec::new());
        };
        let rate = blobs.sample_rate();
        let quick = blobs.quick_cues.as_deref().and_then(QuickCues::parse);
        Ok(match (rate, quick) {
            (Some(rate), Some(quick)) => quick.cues(rate),
            _ => Vec::new(),
        })
    }

    /// Makes `cues` the cues of the track at `index` in `tracks` in the working copy (the
    /// slots that are not in it are emptied), and returns the cues as they are now. The
    /// source only gets them when the working copy is written to it (`sync::push`). The
    /// main cue point and what else is stored with the cues is left as it was.
    pub fn set_cues(&self, index: usize, cues: &[Cue]) -> Result<Vec<Cue>, EditError> {
        let track = self.tracks.get(index).ok_or(EditError::NoSuchTrack)?;
        let connection = self.connection.lock().expect("database lock");
        let blobs = read_analysis(&connection, track.db_id)?.ok_or(EditError::NoAnalysis)?;
        let rate = blobs.sample_rate().ok_or(EditError::NoAnalysis)?;
        let duration = blobs
            .track_data
            .as_deref()
            .and_then(cues::sample_count)
            .map(|samples| samples / rate);
        cues::validate(cues, duration).map_err(EditError::BadCue)?;

        let mut quick = match blobs.quick_cues.as_deref() {
            None | Some([]) => QuickCues::empty(),
            // Better to refuse than to replace what we can't read.
            Some(blob) => QuickCues::parse(blob).ok_or(EditError::UnreadableCues)?,
        };
        quick.replace(cues, rate);

        // Noted first, like a rating: see `set_rating`.
        cache::mark_edited(&self.cache).map_err(EditError::Cache)?;
        let changed = connection.execute(
            "UPDATE PerformanceData SET quickCues = ?1 WHERE trackId = ?2",
            (quick.encode(), track.db_id),
        )?;
        if changed == 0 {
            return Err(EditError::NoAnalysis);
        }
        if let Some(count) = self
            .cue_counts
            .lock()
            .expect("cue counts lock")
            .get_mut(index)
        {
            *count = quick.count() as u32;
        }
        Ok(quick.cues(rate))
    }

    /// How many loops the track at `index` in `tracks` has saved, as it is now.
    pub fn loop_count(&self, index: usize) -> u32 {
        let counts = self.loop_counts.lock().expect("loop counts lock");
        counts.get(index).copied().unwrap_or(0)
    }

    /// The saved loops of the track at `index` in `tracks`, in slot order. None if the
    /// track has none, has not been analysed, or has them stored in a way that isn't
    /// understood.
    pub fn loops(&self, index: usize) -> rusqlite::Result<Vec<Loop>> {
        let Some(track) = self.tracks.get(index) else {
            return Ok(Vec::new());
        };
        let connection = self.connection.lock().expect("database lock");
        let Some(blobs) = read_analysis(&connection, track.db_id)? else {
            return Ok(Vec::new());
        };
        let saved = blobs.loops.as_deref().and_then(QuickLoops::parse);
        Ok(match (blobs.sample_rate(), saved) {
            (Some(rate), Some(saved)) => saved.loops(rate),
            _ => Vec::new(),
        })
    }

    /// Makes `loops` the saved loops of the track at `index` in `tracks` in the working
    /// copy (the loops in other slots are removed), and returns the loops as they are now.
    /// The source only gets them when the working copy is written to it (`sync::push`).
    pub fn set_loops(&self, index: usize, loops: &[Loop]) -> Result<Vec<Loop>, EditError> {
        let track = self.tracks.get(index).ok_or(EditError::NoSuchTrack)?;
        let connection = self.connection.lock().expect("database lock");
        let blobs = read_analysis(&connection, track.db_id)?.ok_or(EditError::NoAnalysis)?;
        let rate = blobs.sample_rate().ok_or(EditError::NoAnalysis)?;
        let duration = blobs
            .track_data
            .as_deref()
            .and_then(cues::sample_count)
            .map(|samples| samples / rate);
        loops::validate(loops, duration).map_err(EditError::BadLoop)?;

        let mut saved = match blobs.loops.as_deref() {
            None | Some([]) => QuickLoops::empty(),
            // Better to refuse than to replace what we can't read.
            Some(blob) => QuickLoops::parse(blob).ok_or(EditError::UnreadableLoops)?,
        };
        saved.replace(loops, rate);

        // Noted first, like a rating: see `set_rating`.
        cache::mark_edited(&self.cache).map_err(EditError::Cache)?;
        let changed = connection.execute(
            "UPDATE PerformanceData SET loops = ?1 WHERE trackId = ?2",
            (saved.encode(), track.db_id),
        )?;
        if changed == 0 {
            return Err(EditError::NoAnalysis);
        }
        if let Some(count) = self
            .loop_counts
            .lock()
            .expect("loop counts lock")
            .get_mut(index)
        {
            *count = saved.count() as u32;
        }
        Ok(saved.loops(rate))
    }

    /// The beatgrid of the track at `index` in `tracks`, if the library has analysed one.
    pub fn beat_grid(&self, index: usize) -> rusqlite::Result<Option<Vec<Marker>>> {
        let Some(track) = self.tracks.get(index) else {
            return Ok(None);
        };
        let connection = self.connection.lock().expect("database lock");
        let blob = connection.query_row(
            "SELECT beatData FROM PerformanceData WHERE trackId = ?1",
            [track.db_id],
            |row| row.get::<_, Option<Vec<u8>>>(0),
        );
        match blob {
            Ok(blob) => Ok(blob.and_then(|blob| beatgrid::parse(&blob))),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(error) => Err(error),
        }
    }

    /// The cover image with the `AlbumArt` id `id`, if there is one on this machine.
    ///
    /// Covers are stored in one of three ways (see `docs/engine-library-format.md`):
    /// as an image in the database, as a JPEG in the library's `Artwork` folder named
    /// after the row's raw hash, or as a streaming URL, which has no image to give.
    pub fn album_art(&self, id: u32) -> rusqlite::Result<Option<AlbumArt>> {
        let (hash, image): (Value, Value) = {
            let connection = self.connection.lock().expect("database lock");
            let row = connection.query_row(
                "SELECT hash, albumArt FROM AlbumArt WHERE id = ?1",
                [id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            );
            match row {
                Ok(row) => row,
                Err(rusqlite::Error::QueryReturnedNoRows) => return Ok(None),
                Err(error) => return Err(error),
            }
        };

        let data = match (image, hash) {
            (Value::Blob(data), _) => data,
            (Value::Null, Value::Blob(hash)) => {
                let file = self
                    .dir
                    .join(ARTWORK_DIR)
                    .join(format!("{}.jpg", base64_url(&hash)));
                match std::fs::read(file) {
                    Ok(data) => data,
                    Err(_) => return Ok(None),
                }
            }
            _ => return Ok(None),
        };
        // Judge by the content, not by where it came from: a streaming row's "image"
        // is a URL, not a picture.
        Ok(image_type(&data).map(|mime_type| AlbumArt { data, mime_type }))
    }
}

fn image_type(data: &[u8]) -> Option<&'static str> {
    if data.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if data.starts_with(&[0xff, 0xd8, 0xff]) {
        Some("image/jpeg")
    } else {
        None
    }
}

/// URL-safe base64 without padding, which is how `Artwork` files are named after a hash.
fn base64_url(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = chunk
            .iter()
            .enumerate()
            .fold(0u32, |n, (i, byte)| n | u32::from(*byte) << (16 - 8 * i));
        // 1 byte gives 2 characters, 2 bytes give 3, 3 bytes give 4.
        for i in 0..=chunk.len() {
            out.push(ALPHABET[(n >> (18 - 6 * i) & 63) as usize] as char);
        }
    }
    out
}

/// Finds the `Engine Library` folder for a user-chosen path.
pub fn locate(path: &Path) -> Option<PathBuf> {
    let mut candidates = vec![path.to_path_buf(), path.join(LIBRARY_DIR)];
    if path.file_name().is_some_and(|name| name == DATABASE_DIR) {
        candidates.extend(path.parent().map(Path::to_path_buf));
    }
    candidates
        .into_iter()
        .find(|dir| dir.join(DATABASE).is_file())
}

/// `path` with its `.` and `..` parts resolved by the text alone.
fn resolve(path: &Path) -> PathBuf {
    use std::path::Component;
    let mut resolved = PathBuf::new();
    for component in path.components() {
        match component {
            Component::ParentDir => {
                resolved.pop();
            }
            Component::CurDir => {}
            other => resolved.push(other.as_os_str()),
        }
    }
    resolved
}

type TrackIds = HashMap<i64, usize>;
type TrackOrigins = HashMap<(String, i64), usize>;

fn load_tracks(
    connection: &Connection,
    dir: &Path,
    lighting: bool,
) -> rusqlite::Result<(Vec<Track>, TrackIds, TrackOrigins)> {
    // (A library from an older version may have no ratings.)
    let has_rating = connection
        .query_row(
            "SELECT 1 FROM pragma_table_info('Track') WHERE name = 'rating'",
            [],
            |_| Ok(()),
        )
        .is_ok();
    let rating = if has_rating { "rating" } else { "NULL" };
    let mut statement = connection.prepare(&format!(
        "SELECT id, title, artist, album, genre, bpmAnalyzed, bpm, length, path, \
                streamingSource, originDatabaseUuid, originTrackId, albumArtId, {rating} \
         FROM Track ORDER BY id"
    ))?;
    // SoundSwitch keeps its projects next to the library folder, on the drive.
    let drive = dir.parent().map(Path::to_path_buf);
    let soundswitch = drive.as_deref().filter(|_| lighting).map(SoundSwitch::scan);
    let mut rows = statement.query([])?;

    let mut tracks = Vec::new();
    let mut by_id = HashMap::new();
    let mut by_origin = HashMap::new();

    while let Some(row) = rows.next()? {
        let id: i64 = row.get(0)?;
        let text = |index: usize| -> rusqlite::Result<String> {
            Ok(row.get::<_, Option<String>>(index)?.unwrap_or_default())
        };
        // The numeric columns are loosely typed, so read them all as floats.
        let number = |index: usize| row.get::<_, Option<f64>>(index);

        let relative = text(8)?;
        let mut title = text(1)?;
        if title.trim().is_empty() {
            title = relative.rsplit('/').next().unwrap_or_default().to_owned();
        }
        // `bpm` is rounded and sometimes wrong; the analysed value is the real one.
        let bpm = number(5)?
            .filter(|bpm| *bpm > 0.0)
            .or(number(6)?.filter(|bpm| *bpm > 0.0));

        by_id.insert(id, tracks.len());
        let origin_key = match (row.get::<_, Option<String>>(10)?, number(11)?) {
            (Some(uuid), Some(origin_id)) if !uuid.is_empty() => Some((uuid, origin_id as i64)),
            _ => None,
        };
        if let Some(key) = &origin_key {
            by_origin.insert(key.clone(), tracks.len());
        }

        let file = dir.join(&relative);
        let lighting = match (&soundswitch, &drive) {
            (Some(soundswitch), Some(drive)) => soundswitch.lighting(drive, &file),
            _ => Lighting::default(),
        };

        tracks.push(Track {
            db_id: id,
            title,
            artist: text(2)?,
            album: text(3)?,
            genre: text(4)?,
            bpm,
            length: number(7)?
                .filter(|length| *length >= 0.0)
                .map(|length| length as u32),
            path: file,
            relative_path: relative,
            origin_key,
            streaming: row
                .get::<_, Option<String>>(9)?
                .is_some_and(|source| !source.is_empty()),
            album_art: number(12)?.filter(|id| *id > 0.0).map(|id| id as u32),
            rating: number(13)?
                .filter(|rating| *rating >= 0.0)
                .map(|rating| rating as u32),
            lighting,
        });
    }

    Ok((tracks, by_id, by_origin))
}

/// How many cues or loops each track has set, by index in the tracks: `count` says how many
/// a blob in `column` holds. A library with no analysis table, or a blob that can't be read,
/// counts as none.
fn load_counts(
    connection: &Connection,
    column: &str,
    by_id: &TrackIds,
    tracks: usize,
    count: impl Fn(&[u8]) -> Option<usize>,
) -> Vec<u32> {
    let mut counts = vec![0; tracks];
    let Ok(mut statement) = connection.prepare(&format!(
        "SELECT trackId, {column} FROM PerformanceData WHERE {column} IS NOT NULL"
    )) else {
        return counts;
    };
    let rows = statement.query_map([], |row| {
        Ok((row.get::<_, i64>(0)?, row.get::<_, Vec<u8>>(1)?))
    });
    for (id, blob) in rows.into_iter().flatten().flatten() {
        if let (Some(index), Some(found)) = (by_id.get(&id), count(&blob)) {
            counts[*index] = found as u32;
        }
    }
    counts
}

/// What a track's analysis holds that the cues and loops need.
struct Analysis {
    track_data: Option<Vec<u8>>,
    beat_data: Option<Vec<u8>>,
    quick_cues: Option<Vec<u8>>,
    loops: Option<Vec<u8>>,
}

impl Analysis {
    /// The sample rate the positions of cues and loops are in samples of.
    fn sample_rate(&self) -> Option<f64> {
        [&self.track_data, &self.beat_data]
            .into_iter()
            .flatten()
            .find_map(|blob| cues::sample_rate(blob))
    }
}

/// `None` if the track has no row of analysis.
fn read_analysis(connection: &Connection, track: i64) -> rusqlite::Result<Option<Analysis>> {
    connection
        .query_row(
            "SELECT trackData, beatData, quickCues, loops FROM PerformanceData WHERE trackId = ?1",
            [track],
            |row| {
                Ok(Analysis {
                    track_data: row.get(0)?,
                    beat_data: row.get(1)?,
                    quick_cues: row.get(2)?,
                    loops: row.get(3)?,
                })
            },
        )
        .optional()
}

fn load_playlists(
    connection: &Connection,
    library_tracks: &[Track],
    by_id: &TrackIds,
    by_origin: &TrackOrigins,
) -> rusqlite::Result<Vec<Playlist>> {
    // Tracks of each playlist. An entity names its track by the track's origin
    // database and the id it has there, which is the library's own id for most
    // tracks. Entities whose track is gone are skipped.
    let mut entities: HashMap<i64, Vec<(i64, i64)>> = HashMap::new();
    let mut entity_track: HashMap<i64, usize> = HashMap::new();
    let mut statement = connection.prepare(
        "SELECT id, listId, trackId, databaseUuid, nextEntityId FROM PlaylistEntity ORDER BY id",
    )?;
    let mut rows = statement.query([])?;
    while let Some(row) = rows.next()? {
        let id: i64 = row.get(0)?;
        let list: i64 = row.get(1)?;
        let track: i64 = row.get(2)?;
        let uuid: Option<String> = row.get(3)?;
        let next: i64 = row.get(4)?;

        let resolved = match uuid {
            Some(uuid) if !uuid.is_empty() => by_origin.get(&(uuid, track)),
            _ => by_id.get(&track),
        };
        if let Some(&index) = resolved {
            entity_track.insert(id, index);
        }
        entities.entry(list).or_default().push((id, next));
    }

    let mut rows_by_parent: HashMap<i64, Vec<(i64, i64)>> = HashMap::new();
    let mut titles: HashMap<i64, String> = HashMap::new();
    let mut statement = connection
        .prepare("SELECT id, title, parentListId, nextListId FROM Playlist ORDER BY id")?;
    let mut rows = statement.query([])?;
    while let Some(row) = rows.next()? {
        let id: i64 = row.get(0)?;
        let parent: i64 = row.get(2)?;
        let next: i64 = row.get(3)?;
        titles.insert(id, row.get::<_, Option<String>>(1)?.unwrap_or_default());
        rows_by_parent.entry(parent).or_default().push((id, next));
    }

    let mut smart_by_parent: HashMap<String, Vec<SmartRow>> = HashMap::new();
    for row in load_smartlists(connection)? {
        smart_by_parent
            .entry(row.parent.clone())
            .or_default()
            .push(row);
    }

    let mut playlists: Vec<Playlist> = Vec::new();
    // Depth first, each level in its stored sibling order. Playlists hang off the
    // root, whose id is 0. A playlist is told apart from its siblings by its path,
    // its parents' titles and its own, each followed by `;`, which is how smart
    // playlists name their parent.
    let root = ordered_children(
        rows_by_parent.get(&0).map(Vec::as_slice).unwrap_or(&[]),
        &titles,
        smart_by_parent.remove("").unwrap_or_default(),
    );
    let mut stack: Vec<(Node, usize, Option<usize>, String)> = root
        .into_iter()
        .rev()
        .map(|node| (node, 0, None, String::new()))
        .collect();

    while let Some((node, depth, parent, parent_path)) = stack.pop() {
        let (playlist, children, path) = match node {
            Node::List(id) => {
                let title = titles.remove(&id).unwrap_or_default();
                let path = format!("{parent_path}{title};");
                let children = ordered_children(
                    rows_by_parent.get(&id).map(Vec::as_slice).unwrap_or(&[]),
                    &titles,
                    smart_by_parent.remove(&path).unwrap_or_default(),
                );
                let tracks = entities
                    .get(&id)
                    .map(|list| {
                        chain_order(list)
                            .into_iter()
                            .filter_map(|entity| entity_track.get(&entity).copied())
                            .collect()
                    })
                    .unwrap_or_default();
                let playlist = Playlist {
                    title,
                    depth,
                    parent,
                    has_children: !children.is_empty(),
                    smart: false,
                    db_id: Some(id),
                    uuid: None,
                    rules: None,
                    raw_rules: None,
                    tracks,
                };
                (playlist, children, path)
            }
            Node::Smart(row) => {
                let rules = Rules::parse(&row.rules);
                let tracks = library_tracks
                    .iter()
                    .enumerate()
                    .filter(|(_, track)| rules.as_ref().is_some_and(|rules| rules.matches(track)))
                    .map(|(index, _)| index)
                    .collect();
                let playlist = Playlist {
                    title: row.title,
                    depth,
                    parent,
                    has_children: false,
                    smart: true,
                    db_id: None,
                    uuid: Some(row.uuid),
                    rules,
                    raw_rules: Some(row.rules),
                    tracks,
                };
                (playlist, Vec::new(), String::new())
            }
        };

        let index = playlists.len();
        playlists.push(playlist);
        stack.extend(
            children
                .into_iter()
                .rev()
                .map(|child| (child, depth + 1, Some(index), path.clone())),
        );
    }

    Ok(playlists)
}

/// A row of the `Smartlist` table.
struct SmartRow {
    uuid: String,
    title: String,
    /// The playlist it is in, as the titles from the top, each followed by `;`. Empty
    /// at the top level.
    parent: String,
    /// The smart playlist after this one, if it is in the same place.
    next_uuid: String,
    /// The playlist after this one, if one follows it instead, as a path like `parent`.
    next_path: String,
    rules: String,
}

/// A new random (version 4) UUID in the usual 8-4-4-4-12 form.
pub fn new_uuid(connection: &Connection) -> rusqlite::Result<String> {
    let hex: String =
        connection.query_row("SELECT lower(hex(randomblob(16)))", [], |row| row.get(0))?;
    let variant = ['8', '9', 'a', 'b'][usize::from(hex.as_bytes()[16] % 4)];
    Ok(format!(
        "{}-{}-4{}-{variant}{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[13..16],
        &hex[17..20],
        &hex[20..32]
    ))
}

/// The titles from the top down to the playlist at `index`, each followed by `;`: how
/// smart playlists name where they are.
fn playlist_path(playlists: &[Playlist], index: usize) -> String {
    let mut titles = Vec::new();
    let mut at = Some(index);
    while let Some(index) = at {
        titles.push(playlists[index].title.as_str());
        at = playlists[index].parent;
    }
    titles
        .iter()
        .rev()
        .map(|title| format!("{title};"))
        .collect()
}

fn has_smartlists(connection: &Connection) -> bool {
    connection
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'Smartlist'",
            [],
            |_| Ok(()),
        )
        .is_ok()
}

/// The smart playlists of the library. (A library from an older version has none.)
fn load_smartlists(connection: &Connection) -> rusqlite::Result<Vec<SmartRow>> {
    if !has_smartlists(connection) {
        return Ok(Vec::new());
    }
    let mut statement = connection.prepare(
        "SELECT listUuid, title, parentPlaylistPath, nextListUuid, nextPlaylistPath, rules \
         FROM Smartlist ORDER BY rowid",
    )?;
    let mut rows = statement.query([])?;
    let mut smartlists = Vec::new();
    while let Some(row) = rows.next()? {
        let text = |index: usize| -> rusqlite::Result<String> {
            Ok(row.get::<_, Option<String>>(index)?.unwrap_or_default())
        };
        smartlists.push(SmartRow {
            uuid: text(0)?,
            title: text(1)?,
            parent: text(2)?,
            next_uuid: text(3)?,
            next_path: text(4)?,
            rules: text(5)?,
        });
    }
    Ok(smartlists)
}

/// A playlist or a smart playlist, to be placed in the tree.
enum Node {
    /// A row of `Playlist`.
    List(i64),
    Smart(SmartRow),
}

/// The children of one playlist (or of the top level) in display order. The playlists
/// keep their stored order. The smart playlists, which are ordered among themselves by
/// their own links, stand before the playlist their last one points at, or after all of
/// them if it points at none.
fn ordered_children(
    lists: &[(i64, i64)],
    titles: &HashMap<i64, String>,
    smart: Vec<SmartRow>,
) -> Vec<Node> {
    let mut nodes: Vec<Node> = chain_order(lists).into_iter().map(Node::List).collect();
    if smart.is_empty() {
        return nodes;
    }

    // `chain_order` wants ids that are not 0: use the position plus one.
    let position: HashMap<&str, i64> = smart
        .iter()
        .enumerate()
        .map(|(index, row)| (row.uuid.as_str(), index as i64 + 1))
        .collect();
    let links: Vec<(i64, i64)> = smart
        .iter()
        .enumerate()
        .map(|(index, row)| {
            (
                index as i64 + 1,
                position.get(row.next_uuid.as_str()).copied().unwrap_or(0),
            )
        })
        .collect();
    let order = chain_order(&links);

    let before = order
        .last()
        .and_then(|&last| {
            smart[last as usize - 1]
                .next_path
                .split(';')
                .rfind(|s| !s.is_empty())
        })
        .map(str::to_owned);
    let at = before
        .and_then(|title| {
            nodes
                .iter()
                .position(|node| matches!(node, Node::List(id) if titles.get(id) == Some(&title)))
        })
        .unwrap_or(nodes.len());

    let mut smart: Vec<Option<SmartRow>> = smart.into_iter().map(Some).collect();
    let placed: Vec<Node> = order
        .into_iter()
        .filter_map(|id| smart[id as usize - 1].take().map(Node::Smart))
        .collect();
    nodes.splice(at..at, placed);
    nodes
}

/// Orders `(id, next)` pairs that form a singly linked list: the head is the item
/// nobody points to and `next` is 0 or unknown at the end. This is how Engine
/// stores the order of sibling playlists and of the tracks in a playlist.
/// Broken data (several heads, a cycle) still yields every item exactly once.
fn chain_order(items: &[(i64, i64)]) -> Vec<i64> {
    let ids: HashSet<i64> = items.iter().map(|(id, _)| *id).collect();
    let next: HashMap<i64, i64> = items.iter().copied().collect();
    let pointed_to: HashSet<i64> = items
        .iter()
        .map(|(_, next)| *next)
        .filter(|next| ids.contains(next))
        .collect();

    let heads = items
        .iter()
        .map(|(id, _)| *id)
        .filter(|id| !pointed_to.contains(id));
    let leftovers = items.iter().map(|(id, _)| *id);

    let mut seen = HashSet::new();
    let mut order = Vec::with_capacity(items.len());
    for start in heads.chain(leftovers) {
        let mut current = Some(start);
        while let Some(id) = current {
            if !seen.insert(id) {
                break;
            }
            order.push(id);
            current = next.get(&id).copied().filter(|next| ids.contains(next));
        }
    }
    order
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = "CREATE TABLE Track (id INTEGER PRIMARY KEY, title TEXT, artist TEXT, album TEXT,
                    genre TEXT, bpmAnalyzed REAL, bpm INTEGER, length INTEGER, path TEXT,
                    streamingSource TEXT, originDatabaseUuid TEXT, originTrackId INTEGER, albumArtId INTEGER);
                 CREATE TABLE AlbumArt (id INTEGER PRIMARY KEY, hash, albumArt);
                 CREATE TABLE Playlist (id INTEGER PRIMARY KEY, title TEXT, parentListId INTEGER,
                    nextListId INTEGER);
                 CREATE TABLE PlaylistEntity (id INTEGER PRIMARY KEY, listId INTEGER,
                    trackId INTEGER, databaseUuid TEXT, nextEntityId INTEGER);
                 INSERT INTO Track VALUES
                    (1, 'One', 'A', 'X', 'House', 124.0, 124, 200, '../Music/one.flac', NULL, 'db', 1, 1),
                    (2, NULL, NULL, NULL, NULL, NULL, 90, NULL, '../Music/two.mp3', NULL, 'db', 2, 2),
                    (3, 'Three', 'C', 'Z', 'Techno', 0.0, 0, 61, '../tidal/three.mp4', 'TIDAL', 'other', 7, 3);
                 -- Covers: an image in the database, a raw hash (the image is a file in
                 -- Artwork/), a streaming URL, and a hash with no file.
                 INSERT INTO AlbumArt VALUES
                    (1, 'abc123', x'89504e470d0a1a0a0001'),
                    (2, x'fbff0011223344556677889900aabbccddeeff00', NULL),
                    (3, 'URI:12ab', CAST('image://streaming/https%3A%2F%2Fexample.com%2Fa.jpg' AS BLOB)),
                    (4, x'0000000000000000000000000000000000000000', NULL);
                 -- Roots in order: Beta (2) -> Alpha (1). Alpha holds Child (3).
                 INSERT INTO Playlist VALUES (1, 'Alpha', 0, 0), (2, 'Beta', 0, 1), (3, 'Child', 1, 0);
                 -- Beta lists tracks 3, 1, 2 (3 by its origin id 7).
                 INSERT INTO PlaylistEntity VALUES
                    (10, 2, 1, 'db', 11), (11, 2, 2, 'db', 0), (12, 2, 7, 'other', 10),
                    (13, 2, 99, 'db', 0);";

    fn database() -> Connection {
        let connection = Connection::open_in_memory().unwrap();
        connection.execute_batch(FIXTURE).unwrap();
        connection
    }

    #[test]
    fn a_rating_is_written_to_the_working_copy() {
        let cache =
            std::env::temp_dir().join(format!("open-engine-lib-rating-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&cache);
        std::fs::create_dir_all(&cache).unwrap();

        let connection = database();
        connection
            .execute_batch(
                "ALTER TABLE Track ADD COLUMN rating INTEGER;
                 UPDATE Track SET rating = 40 WHERE id = 1;",
            )
            .unwrap();
        let library = Library::load(connection, PathBuf::from("/lib"), cache.clone()).unwrap();
        assert_eq!(library.rating(0), Some(40));
        assert_eq!(library.rating(1), None);

        library.set_rating(0, 100).unwrap();
        library.set_rating(1, 60).unwrap();
        library.set_rating(1, 0).unwrap();
        assert_eq!(library.rating(0), Some(100));
        assert_eq!(library.rating(1), Some(0), "no stars is a rating of 0");
        assert_eq!(library.rating(2), None, "the others are as they were");

        let stored: Vec<Option<u32>> = {
            let connection = library.connection.lock().unwrap();
            let mut statement = connection
                .prepare("SELECT rating FROM Track ORDER BY id")
                .unwrap();
            statement
                .query_map([], |row| row.get(0))
                .unwrap()
                .map(Result::unwrap)
                .collect()
        };
        assert_eq!(stored, [Some(100), Some(0), None]);
        assert!(
            cache.join("edited").exists(),
            "the edit is noted for the cache"
        );

        assert!(matches!(
            library.set_rating(3, 20),
            Err(EditError::NoSuchTrack)
        ));
        let _ = std::fs::remove_dir_all(&cache);
    }

    #[test]
    fn chain_order_follows_next_links() {
        assert_eq!(chain_order(&[(1, 0), (2, 1), (3, 2)]), [3, 2, 1]);
        assert_eq!(chain_order(&[]), Vec::<i64>::new());
    }

    #[test]
    fn chain_order_survives_broken_data() {
        // A cycle with no head, a dangling next and two heads.
        assert_eq!(chain_order(&[(1, 2), (2, 1)]), [1, 2]);
        assert_eq!(chain_order(&[(1, 9), (2, 0)]), [1, 2]);
        let mut both = chain_order(&[(1, 0), (2, 0)]);
        both.sort();
        assert_eq!(both, [1, 2]);
    }

    #[test]
    fn locations_are_the_files_with_their_origin() {
        let library = Library::load(
            database(),
            PathBuf::from("/lib/Engine Library"),
            PathBuf::from("/cache"),
        )
        .unwrap();
        assert_eq!(
            library.locations(),
            [
                (0, ("db".to_string(), 1), "/lib/Music/one.flac".to_string()),
                (1, ("db".to_string(), 2), "/lib/Music/two.mp3".to_string()),
            ],
            "the streaming track has no file here"
        );
    }

    #[test]
    fn loads_tracks_with_fallbacks() {
        let library =
            Library::load(database(), PathBuf::from("/lib"), PathBuf::from("/cache")).unwrap();

        assert_eq!(library.tracks.len(), 3);
        let one = &library.tracks[0];
        assert_eq!(
            (one.title.as_str(), one.bpm, one.length),
            ("One", Some(124.0), Some(200))
        );
        assert_eq!(one.path, Path::new("/lib/../Music/one.flac"));
        assert!(!one.streaming);

        let two = &library.tracks[1];
        assert_eq!(
            two.title, "two.mp3",
            "an untitled track is named after its file"
        );
        assert_eq!(two.bpm, Some(90.0), "falls back to the rounded bpm");
        assert_eq!(two.length, None);

        let three = &library.tracks[2];
        assert_eq!(three.bpm, None, "a bpm of 0 means unknown");
        assert!(three.streaming);

        let covers: Vec<_> = library.tracks.iter().map(|t| t.album_art).collect();
        assert_eq!(covers, [Some(1), Some(2), Some(3)]);
    }

    #[test]
    fn orders_playlists_depth_first_by_sibling_links() {
        let library =
            Library::load(database(), PathBuf::from("/lib"), PathBuf::from("/cache")).unwrap();

        let playlists = library.playlists();
        let shape: Vec<_> = playlists
            .iter()
            .map(|p| (p.title.as_str(), p.depth, p.parent, p.has_children))
            .collect();
        assert_eq!(
            shape,
            [
                ("Beta", 0, None, false),
                ("Alpha", 0, None, true),
                ("Child", 1, Some(1), false),
            ]
        );
    }

    #[test]
    fn places_smart_playlists_and_fills_them_from_their_rules() {
        let connection = database();
        // Top level: Smart A -> Smart B stand before Alpha. Alpha holds Smart C, after Child.
        // Smart D names a parent that does not exist and is left out.
        connection
            .execute_batch(
                "CREATE TABLE Smartlist (listUuid TEXT PRIMARY KEY, title TEXT,
                    parentPlaylistPath TEXT, nextPlaylistPath TEXT, nextListUuid TEXT, rules TEXT);
                 INSERT INTO Smartlist VALUES
                    ('b', 'Smart B', '', 'Alpha;', '', '{\"match\":\"all\",\"rules\":[{\"col\":\"genre\",\"con\":\"LIKE\",\"param\":\"''%tech%''\"}]}'),
                    ('a', 'Smart A', '', '', 'b', '{\"match\":\"all\",\"rules\":[{\"col\":\"genre\",\"con\":\"LIKE\",\"param\":\"''%house%''\"}]}'),
                    ('c', 'Smart C', 'Alpha;', '', '', 'broken'),
                    ('d', 'Smart D', 'Nowhere;', '', '', '{}');",
            )
            .unwrap();
        let library =
            Library::load(connection, PathBuf::from("/lib"), PathBuf::from("/cache")).unwrap();

        let playlists = library.playlists();
        let shape: Vec<_> = playlists
            .iter()
            .map(|p| (p.title.as_str(), p.depth, p.smart, p.has_children))
            .collect();
        assert_eq!(
            shape,
            [
                ("Beta", 0, false, false),
                ("Smart A", 0, true, false),
                ("Smart B", 0, true, false),
                ("Alpha", 0, false, true),
                ("Child", 1, false, false),
                ("Smart C", 1, true, false),
            ]
        );
        drop(playlists);
        assert_eq!(library.playlists()[1].tracks, [0], "genre House");
        assert_eq!(library.playlists()[2].tracks, [2], "genre Techno");
        assert!(
            library.playlists()[5].tracks.is_empty(),
            "unreadable rules match nothing"
        );
        assert_eq!(library.playlists()[5].parent, Some(3));
    }

    fn editable_library(name: &str) -> (Library, PathBuf) {
        let cache =
            std::env::temp_dir().join(format!("open-engine-lib-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&cache);
        std::fs::create_dir_all(&cache).unwrap();
        let connection = database();
        connection
            .execute_batch(
                "CREATE TABLE Smartlist (listUuid TEXT NOT NULL PRIMARY KEY, title TEXT,
                    parentPlaylistPath TEXT, nextPlaylistPath TEXT, nextListUuid TEXT, rules TEXT,
                    lastEditTime DATETIME,
                    UNIQUE (title, parentPlaylistPath),
                    UNIQUE (parentPlaylistPath, nextPlaylistPath, nextListUuid));",
            )
            .unwrap();
        let library = Library::load(connection, PathBuf::from("/lib"), cache.clone()).unwrap();
        (library, cache)
    }

    fn rules(genre: &str) -> Rules {
        Rules::from_parts(true, vec![("genre".into(), "LIKE".into(), genre.into())]).unwrap()
    }

    fn titles(library: &Library) -> Vec<String> {
        library
            .playlists()
            .iter()
            .map(|p| p.title.clone())
            .collect()
    }

    #[test]
    fn adds_smart_playlists_in_front_of_the_playlist_the_chain_ends_at() {
        let (library, cache) = editable_library("smart-create");

        // The first one has nothing to follow, so it stands after the playlists.
        let first = library
            .create_smart_playlist("  House  ", &rules("%house%"))
            .unwrap();
        assert_eq!(titles(&library), ["Beta", "Alpha", "Child", "House"]);
        assert_eq!(first, 3);
        assert_eq!(library.playlists()[3].tracks, [0]);
        assert!(library.playlists()[3].smart);

        // Move the end of the chain in front of Alpha, as Engine does for its own lists.
        {
            let connection = library.connection.lock().unwrap();
            connection
                .execute("UPDATE Smartlist SET nextPlaylistPath = 'Alpha;'", [])
                .unwrap();
        }
        library
            .reload_playlists(&library.connection.lock().unwrap())
            .unwrap();
        assert_eq!(titles(&library), ["Beta", "House", "Alpha", "Child"]);

        // The second follows the first and stays in front of Alpha.
        let second = library
            .create_smart_playlist("Techno", &rules("%techno%"))
            .unwrap();
        assert_eq!(
            titles(&library),
            ["Beta", "House", "Techno", "Alpha", "Child"]
        );
        assert_eq!(second, 2);
        assert_eq!(library.playlists()[2].tracks, [2]);

        // It is stored as Engine stores rules, and the edit is noted for the cache.
        let stored: String = library
            .connection
            .lock()
            .unwrap()
            .query_row(
                "SELECT rules FROM Smartlist WHERE title = 'Techno'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            stored,
            r#"{"match":"all","rules":[{"col":"genre","con":"LIKE","param":"'%techno%'","v":"2.20.0"}],"rv":2}"#
        );
        assert!(cache.join("edited").exists());

        assert!(matches!(
            library.create_smart_playlist("Techno", &rules("x")),
            Err(EditError::TitleTaken)
        ));
        assert!(matches!(
            library.create_smart_playlist("Beta", &rules("x")),
            Err(EditError::TitleTaken)
        ));
        assert!(matches!(
            library.create_smart_playlist("  ", &rules("x")),
            Err(EditError::EmptyTitle)
        ));
        let _ = std::fs::remove_dir_all(&cache);
    }

    #[test]
    fn changes_the_title_and_rules_of_a_smart_playlist() {
        let (library, cache) = editable_library("smart-update");
        let index = library
            .create_smart_playlist("House", &rules("%house%"))
            .unwrap();
        library
            .create_smart_playlist("Other", &rules("%x%"))
            .unwrap();
        assert_eq!(library.playlists()[index].tracks, [0]);

        let index = library
            .update_smart_playlist(index, "Tech", &rules("%techno%"))
            .unwrap();
        assert_eq!(library.playlists()[index].title, "Tech");
        assert_eq!(library.playlists()[index].tracks, [2]);
        let parts: Vec<_> = library.playlists()[index]
            .rules
            .as_ref()
            .unwrap()
            .parts()
            .map(|(c, o, p)| (c.to_string(), o.to_string(), p))
            .collect();
        assert_eq!(parts, [("genre".into(), "LIKE".into(), "%techno%".into())]);

        // Its own title is fine; another list's, a playlist's and an empty one are not.
        library
            .update_smart_playlist(index, "Tech", &rules("%a%"))
            .unwrap();
        assert!(matches!(
            library.update_smart_playlist(index, "Other", &rules("x")),
            Err(EditError::TitleTaken)
        ));
        assert!(matches!(
            library.update_smart_playlist(index, "Beta", &rules("x")),
            Err(EditError::TitleTaken)
        ));
        // A playlist that is not smart, and one that is not there.
        assert!(matches!(
            library.update_smart_playlist(0, "x", &rules("x")),
            Err(EditError::NotSmart)
        ));
        assert!(matches!(
            library.update_smart_playlist(99, "x", &rules("x")),
            Err(EditError::NoSuchPlaylist)
        ));
        let _ = std::fs::remove_dir_all(&cache);
    }

    /// Like `editable_library`, with the `Playlist` table, its triggers and its views as
    /// Engine creates them: Beta -> Alpha at the top, Alpha holds Child.
    fn engine_library(name: &str) -> (Library, PathBuf) {
        let (library, cache) = editable_library(name);
        {
            let connection = library.connection.lock().unwrap();
            connection
                .execute_batch(
                    "DROP TABLE Playlist;
                     CREATE TABLE Playlist (id INTEGER PRIMARY KEY AUTOINCREMENT, title TEXT,
                        parentListId INTEGER, isPersisted BOOLEAN, nextListId INTEGER,
                        lastEditTime DATETIME, isExplicitlyExported BOOLEAN,
                        CONSTRAINT C_NAME_UNIQUE_FOR_PARENT UNIQUE (title, parentListId),
                        CONSTRAINT C_NEXT_LIST_ID_UNIQUE_FOR_PARENT UNIQUE (parentListId, nextListId));
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
                     CREATE VIEW PlaylistAllParent AS
                        WITH FindAllParent AS (
                            SELECT id, parentListId FROM Playlist
                            UNION ALL
                            SELECT r.id, p.parentListId FROM Playlist p
                            INNER JOIN FindAllParent r ON r.parentListId = p.id)
                        SELECT * FROM FindAllParent;
                     CREATE TRIGGER trigger_after_insert_isPersist AFTER INSERT ON Playlist
                        WHEN new.isPersisted = 1 BEGIN
                        UPDATE Playlist SET isPersisted = 1
                        WHERE id IN (SELECT parentListId FROM PlaylistAllParent WHERE id = new.id);
                     END;
                     INSERT INTO Playlist (id, title, parentListId, isPersisted, nextListId) VALUES
                        (1, 'Alpha', 0, 1, 0), (2, 'Beta', 0, 1, 1), (3, 'Child', 1, 1, 0);
                     -- Smart playlists: in front of Alpha, in Alpha, in Child.
                     INSERT INTO Smartlist VALUES
                        ('s1', 'Before', '', 'Alpha;', '', '{}', NULL),
                        ('s2', 'In Alpha', 'Alpha;', '', '', '{}', NULL),
                        ('s3', 'In Child', 'Alpha;Child;', '', '', '{}', NULL);",
                )
                .unwrap();
            library.reload_playlists(&connection).unwrap();
        }
        (library, cache)
    }

    fn smartlist_paths(library: &Library) -> Vec<(String, String, String)> {
        let connection = library.connection.lock().unwrap();
        let mut statement = connection
            .prepare(
                "SELECT title, parentPlaylistPath, nextPlaylistPath FROM Smartlist ORDER BY title",
            )
            .unwrap();
        statement
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
            .unwrap()
            .map(Result::unwrap)
            .collect()
    }

    #[test]
    fn adds_a_playlist_at_the_end_of_its_parent() {
        let (library, cache) = engine_library("playlist-create");
        assert_eq!(
            titles(&library),
            ["Beta", "Before", "Alpha", "Child", "In Child", "In Alpha"]
        );

        let top = library.create_playlist("  New  ", None).unwrap();
        assert_eq!(library.playlists()[top].title, "New");
        assert_eq!(library.playlists()[top].depth, 0);
        assert!(!library.playlists()[top].smart);
        assert!(library.playlists()[top].tracks.is_empty());
        assert_eq!(
            titles(&library),
            [
                "Beta", "Before", "Alpha", "Child", "In Child", "In Alpha", "New"
            ],
            "after the last one"
        );

        let alpha = library
            .playlists()
            .iter()
            .position(|p| p.title == "Alpha")
            .unwrap();
        let inner = library.create_playlist("Inner", Some(alpha)).unwrap();
        assert_eq!(library.playlists()[inner].parent, Some(alpha));
        assert_eq!(library.playlists()[inner].depth, 1);
        let alpha_children: Vec<_> = library
            .playlists()
            .iter()
            .filter(|p| p.parent == Some(alpha))
            .map(|p| p.title.clone())
            .collect();
        assert_eq!(alpha_children, ["Child", "Inner", "In Alpha"]);

        // How Engine flags the ones it makes, and the edit is noted.
        let (persisted, exported): (bool, bool) = library
            .connection
            .lock()
            .unwrap()
            .query_row(
                "SELECT isPersisted, isExplicitlyExported FROM Playlist WHERE title = 'New'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert!(persisted && exported);
        assert!(cache.join("edited").exists());

        assert!(matches!(
            library.create_playlist("New", None),
            Err(EditError::TitleTaken)
        ));
        assert!(matches!(
            library.create_playlist("Child", Some(alpha)),
            Err(EditError::TitleTaken)
        ));
        assert!(matches!(
            library.create_playlist(" ", None),
            Err(EditError::EmptyTitle)
        ));
        assert!(matches!(
            library.create_playlist("x", Some(99)),
            Err(EditError::NoSuchPlaylist)
        ));
        let smart = library.playlists().iter().position(|p| p.smart).unwrap();
        assert!(matches!(
            library.create_playlist("x", Some(smart)),
            Err(EditError::IsSmart)
        ));
        let _ = std::fs::remove_dir_all(&cache);
    }

    #[test]
    fn renaming_a_playlist_keeps_the_smart_playlists_that_name_it() {
        let (library, cache) = engine_library("playlist-rename");
        let alpha = library
            .playlists()
            .iter()
            .position(|p| p.title == "Alpha")
            .unwrap();
        library.rename_playlist(alpha, "Omega").unwrap();

        assert_eq!(
            titles(&library),
            ["Beta", "Before", "Omega", "Child", "In Child", "In Alpha"],
            "it stays where it was"
        );
        assert_eq!(
            smartlist_paths(&library),
            [
                ("Before".into(), "".into(), "Omega;".into()),
                ("In Alpha".into(), "Omega;".into(), "".into()),
                ("In Child".into(), "Omega;Child;".into(), "".into()),
            ]
        );
        // Still found where they belong.
        let playlists = library.playlists();
        assert_eq!(playlists[1].title, "Before", "still in front of it");
        assert_eq!(playlists[5].parent, Some(2));
        drop(playlists);

        assert!(matches!(
            library.rename_playlist(alpha, "Beta"),
            Err(EditError::TitleTaken)
        ));
        library.rename_playlist(alpha, "Omega").unwrap();
        assert!(matches!(
            library.rename_playlist(alpha, " "),
            Err(EditError::EmptyTitle)
        ));
        assert!(matches!(
            library.rename_playlist(99, "x"),
            Err(EditError::NoSuchPlaylist)
        ));
        assert!(matches!(
            library.rename_playlist(1, "x"),
            Err(EditError::IsSmart)
        ));
        let _ = std::fs::remove_dir_all(&cache);
    }

    #[test]
    fn deleting_a_playlist_takes_what_is_in_it_with_it() {
        let (library, cache) = engine_library("playlist-delete");
        let alpha = library
            .playlists()
            .iter()
            .position(|p| p.title == "Alpha")
            .unwrap();
        library.delete_playlist(alpha).unwrap();

        // Alpha, Child and the smart playlists in them are gone; Before is not in them.
        assert_eq!(titles(&library), ["Beta", "Before"]);
        assert_eq!(
            smartlist_paths(&library),
            [("Before".into(), "".into(), "".into())],
            "nothing follows it any more"
        );
        let remaining: i64 = library
            .connection
            .lock()
            .unwrap()
            .query_row("SELECT count(*) FROM Playlist", [], |row| row.get(0))
            .unwrap();
        assert_eq!(remaining, 1);

        // Beta had tracks: they leave with it, and the tracks themselves stay.
        library.delete_playlist(0).unwrap();
        assert_eq!(titles(&library), ["Before"]);
        let (entities, tracks): (i64, i64) = library
            .connection
            .lock()
            .unwrap()
            .query_row(
                "SELECT (SELECT count(*) FROM PlaylistEntity), (SELECT count(*) FROM Track)",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!((entities, tracks), (0, 3));

        assert!(matches!(
            library.delete_playlist(0),
            Err(EditError::IsSmart)
        ));
        assert!(matches!(
            library.delete_playlist(5),
            Err(EditError::NoSuchPlaylist)
        ));
        let _ = std::fs::remove_dir_all(&cache);
    }

    #[test]
    fn deleting_a_middle_playlist_relinks_its_neighbours() {
        let (library, cache) = engine_library("playlist-delete-middle");
        library.create_playlist("Gamma", None).unwrap();
        // Roots are now Beta -> Alpha -> Gamma. Take Alpha out.
        let alpha = library
            .playlists()
            .iter()
            .position(|p| p.title == "Alpha")
            .unwrap();
        library.delete_playlist(alpha).unwrap();
        assert_eq!(titles(&library), ["Beta", "Before", "Gamma"]);
        assert_eq!(
            smartlist_paths(&library)[0],
            ("Before".into(), "".into(), "Gamma;".into()),
            "now in front of the one after Alpha"
        );
        library.create_playlist("Delta", None).unwrap();
        assert_eq!(titles(&library), ["Beta", "Before", "Gamma", "Delta"]);
        let _ = std::fs::remove_dir_all(&cache);
    }

    #[test]
    fn a_playlists_tracks_are_set_in_order_keeping_the_entries_it_has() {
        let (library, cache) = engine_library("playlist-tracks");
        library
            .connection
            .lock()
            .unwrap()
            .execute(
                "ALTER TABLE PlaylistEntity ADD COLUMN membershipReference INTEGER",
                [],
            )
            .unwrap();
        let beta = library
            .playlists()
            .iter()
            .position(|p| p.title == "Beta")
            .unwrap();
        assert_eq!(library.playlists()[beta].tracks, [2, 0, 1]);
        let entry_of_first: i64 = library
            .connection
            .lock()
            .unwrap()
            .query_row(
                "SELECT id FROM PlaylistEntity WHERE listId = 2 AND trackId = 1",
                [],
                |row| row.get(0),
            )
            .unwrap();

        // One track goes, the others change places.
        library.set_playlist_tracks(beta, &[1, 0]).unwrap();
        assert_eq!(library.playlists()[beta].tracks, [1, 0]);
        let kept: i64 = library
            .connection
            .lock()
            .unwrap()
            .query_row(
                "SELECT id FROM PlaylistEntity WHERE listId = 2 AND trackId = 1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(kept, entry_of_first, "its entry is the one it had");

        // One comes back as a new entry, and an empty list empties it.
        library.set_playlist_tracks(beta, &[0, 2, 0]).unwrap();
        assert_eq!(library.playlists()[beta].tracks, [0, 2], "each track once");
        library.set_playlist_tracks(beta, &[]).unwrap();
        assert!(library.playlists()[beta].tracks.is_empty());
        assert!(matches!(
            library.set_playlist_tracks(beta, &[9]),
            Err(EditError::NoSuchTrack)
        ));
        assert!(matches!(
            library.set_playlist_tracks(99, &[]),
            Err(EditError::NoSuchPlaylist)
        ));
        let _ = std::fs::remove_dir_all(&cache);
    }

    #[test]
    fn deleting_a_smart_playlist_closes_the_gap_in_the_chain() {
        let (library, cache) = editable_library("smart-delete");
        for name in ["One", "Two", "Three"] {
            library.create_smart_playlist(name, &rules("%x%")).unwrap();
        }
        let two = library
            .playlists()
            .iter()
            .position(|p| p.title == "Two")
            .unwrap();
        library.delete_smart_playlist(two).unwrap();
        let smart: Vec<_> = library
            .playlists()
            .iter()
            .filter(|p| p.smart)
            .map(|p| p.title.clone())
            .collect();
        assert_eq!(smart, ["One", "Three"]);

        // And the chain still takes a new one at its end.
        library
            .create_smart_playlist("Four", &rules("%x%"))
            .unwrap();
        let smart: Vec<_> = library
            .playlists()
            .iter()
            .filter(|p| p.smart)
            .map(|p| p.title.clone())
            .collect();
        assert_eq!(smart, ["One", "Three", "Four"]);
        assert!(matches!(
            library.delete_smart_playlist(0),
            Err(EditError::NotSmart)
        ));
        let _ = std::fs::remove_dir_all(&cache);
    }

    #[test]
    fn origins_are_saved_in_the_working_copy_and_take_over_the_old_ones() {
        let cache = std::env::temp_dir().join(format!(
            "open-engine-lib-library-origins-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&cache);
        std::fs::create_dir_all(cache.join(DATABASE_DIR)).unwrap();
        let mut library = Library::load(database(), PathBuf::from("/lib"), cache.clone()).unwrap();
        let key = |id: i64| ("db".to_string(), id);

        // What earlier versions saved: for a track of this library, and for another.
        let legacy = cache.join("legacy.db");
        Origins::open(&legacy)
            .unwrap()
            .save_missing(&[
                (key(1), "/old/one.flac".into()),
                (("elsewhere".to_string(), 9), "/old/nine.flac".into()),
            ])
            .unwrap();
        library.legacy_origins = legacy;

        // Nothing to save makes no file; saving makes it in the working copy, noted as edited.
        assert_eq!(library.save_origins(&[], false).unwrap(), 0);
        assert!(!cache.join(DATABASE_DIR).join(origins::FILE).exists());
        assert!(library.origins().is_empty());
        let new = library
            .save_origins(
                &[
                    (key(1), "/pc/one.flac".into()),
                    (key(2), "/pc/two.flac".into()),
                ],
                false,
            )
            .unwrap();
        assert_eq!(new, 1, "the old origin of one is kept, two is new");
        let saved = library.origins();
        assert_eq!(
            saved.len(),
            2,
            "the other library's origin did not come along"
        );
        assert_eq!(saved[&key(1)], "/old/one.flac");
        assert_eq!(saved[&key(2)], "/pc/two.flac");
        assert!(cache.join("edited").exists());

        // Replacing is for syncing from another library.
        assert_eq!(
            library
                .save_origins(&[(key(1), "/pc/one.flac".into())], true)
                .unwrap(),
            1
        );
        assert_eq!(library.origins()[&key(1)], "/pc/one.flac");
        let _ = std::fs::remove_dir_all(&cache);
    }

    /// A `trackData` blob that starts the way Engine's does: the sample rate and the
    /// number of samples.
    fn track_data(rate: f64, samples: i64) -> Vec<u8> {
        use std::io::Write;
        let mut data = rate.to_be_bytes().to_vec();
        data.extend(samples.to_be_bytes());
        let mut encoder =
            flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(&data).unwrap();
        let mut blob = (data.len() as u32).to_be_bytes().to_vec();
        blob.extend(encoder.finish().unwrap());
        blob
    }

    fn cue(slot: usize, label: &str, position_seconds: f64) -> Cue {
        Cue {
            slot,
            label: label.to_string(),
            position_seconds,
            color: 0xfff4_d338,
        }
    }

    #[test]
    fn cues_are_counted_read_and_written_in_the_working_copy() {
        let cache =
            std::env::temp_dir().join(format!("open-engine-lib-cues-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&cache);
        std::fs::create_dir_all(&cache).unwrap();

        // Track 1 has a cue, track 2 has analysis but no cue blob, track 3 has none at all.
        let connection = database();
        connection
            .execute_batch(
                "CREATE TABLE PerformanceData (trackId INTEGER PRIMARY KEY, trackData BLOB,
                    overviewWaveFormData BLOB, beatData BLOB, quickCues BLOB, loops BLOB);",
            )
            .unwrap();
        let mut with_cue = QuickCues::empty();
        with_cue.replace(&[cue(2, "Drop", 10.0)], 44_100.0);
        connection
            .execute(
                "INSERT INTO PerformanceData (trackId, trackData, quickCues) VALUES
                    (1, ?1, ?2), (2, ?1, NULL)",
                (track_data(44_100.0, 44_100 * 200), with_cue.encode()),
            )
            .unwrap();
        let library = Library::load(connection, PathBuf::from("/lib"), cache.clone()).unwrap();

        assert_eq!(
            (0..3)
                .map(|index| library.cue_count(index))
                .collect::<Vec<_>>(),
            [1, 0, 0]
        );
        let cues = library.cues(0).unwrap();
        assert_eq!(
            (cues.len(), cues[0].slot, cues[0].label.as_str()),
            (1, 2, "Drop")
        );
        assert_eq!(cues[0].position_seconds, 10.0);
        assert!(library.cues(1).unwrap().is_empty());
        assert!(library.cues(2).unwrap().is_empty());

        // Changing the cue of track 1 and adding two to track 2, which had no blob.
        let after = library
            .set_cues(0, &[cue(0, "In", 1.5), cue(2, "Drop", 12.0)])
            .unwrap();
        assert_eq!(after.iter().map(|cue| cue.slot).collect::<Vec<_>>(), [0, 2]);
        assert_eq!(library.cues(0).unwrap(), after);
        library
            .set_cues(1, &[cue(7, "", 0.0), cue(6, "Out", 200.0)])
            .unwrap();
        assert_eq!(library.cue_count(0), 2);
        assert_eq!(library.cue_count(1), 2);
        assert!(
            cache.join("edited").exists(),
            "the edit is noted for the cache"
        );

        // The slots that are not sent are emptied.
        assert!(library.set_cues(0, &[]).unwrap().is_empty());
        assert_eq!(library.cue_count(0), 0);
        assert!(library.cues(0).unwrap().is_empty());

        // What is not valid, or can't hold cues, changes nothing.
        assert!(matches!(
            library.set_cues(1, &[cue(8, "", 1.0)]),
            Err(EditError::BadCue(_))
        ));
        assert!(matches!(
            library.set_cues(1, &[cue(0, "", 200.5)]),
            Err(EditError::BadCue(_)),
        ));
        assert!(matches!(
            library.set_cues(2, &[cue(0, "", 1.0)]),
            Err(EditError::NoAnalysis)
        ));
        assert!(matches!(
            library.set_cues(5, &[]),
            Err(EditError::NoSuchTrack)
        ));
        assert_eq!(library.cue_count(1), 2);

        // A blob that isn't understood is not written over.
        library
            .connection
            .lock()
            .unwrap()
            .execute(
                "UPDATE PerformanceData SET quickCues = x'0000000401' WHERE trackId = 2",
                [],
            )
            .unwrap();
        assert!(matches!(
            library.set_cues(1, &[cue(0, "", 1.0)]),
            Err(EditError::UnreadableCues)
        ));
        let _ = std::fs::remove_dir_all(&cache);
    }

    fn saved_loop(slot: usize, label: &str, start: f64, end: f64) -> Loop {
        Loop {
            slot,
            label: label.to_string(),
            start_seconds: start,
            end_seconds: end,
            color: 0xffef_8130,
        }
    }

    #[test]
    fn loops_are_counted_read_and_written_in_the_working_copy() {
        let cache =
            std::env::temp_dir().join(format!("open-engine-lib-loops-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&cache);
        std::fs::create_dir_all(&cache).unwrap();

        // Track 1 has a loop, track 2 has analysis but no loops blob, track 3 has none at all.
        let connection = database();
        connection
            .execute_batch(
                "CREATE TABLE PerformanceData (trackId INTEGER PRIMARY KEY, trackData BLOB,
                    overviewWaveFormData BLOB, beatData BLOB, quickCues BLOB, loops BLOB);",
            )
            .unwrap();
        let mut with_loop = QuickLoops::empty();
        with_loop.replace(&[saved_loop(2, "Loop 3", 10.0, 12.0)], 44_100.0);
        connection
            .execute(
                "INSERT INTO PerformanceData (trackId, trackData, loops) VALUES
                    (1, ?1, ?2), (2, ?1, NULL)",
                (track_data(44_100.0, 44_100 * 200), with_loop.encode()),
            )
            .unwrap();
        let library = Library::load(connection, PathBuf::from("/lib"), cache.clone()).unwrap();

        assert_eq!(
            (0..3)
                .map(|index| library.loop_count(index))
                .collect::<Vec<_>>(),
            [1, 0, 0]
        );
        let loops = library.loops(0).unwrap();
        assert_eq!(
            (loops.len(), loops[0].slot, loops[0].label.as_str()),
            (1, 2, "Loop 3")
        );
        assert_eq!((loops[0].start_seconds, loops[0].end_seconds), (10.0, 12.0));
        assert!(library.loops(1).unwrap().is_empty());
        assert!(library.loops(2).unwrap().is_empty());

        let after = library
            .set_loops(
                0,
                &[
                    saved_loop(0, "In", 1.5, 3.0),
                    saved_loop(2, "Drop", 12.0, 20.0),
                ],
            )
            .unwrap();
        assert_eq!(
            after.iter().map(|saved| saved.slot).collect::<Vec<_>>(),
            [0, 2]
        );
        assert_eq!(library.loops(0).unwrap(), after);
        library
            .set_loops(
                1,
                &[
                    saved_loop(7, "", 0.0, 1.0),
                    saved_loop(6, "Out", 100.0, 200.0),
                ],
            )
            .unwrap();
        assert_eq!(library.loop_count(0), 2);
        assert_eq!(library.loop_count(1), 2);
        assert!(
            cache.join("edited").exists(),
            "the edit is noted for the cache"
        );

        // The cues of the tracks are not touched by writing loops.
        assert_eq!(library.cue_count(0), 0);

        assert!(library.set_loops(0, &[]).unwrap().is_empty());
        assert_eq!(library.loop_count(0), 0);

        // What is not valid, or can't hold loops, changes nothing.
        for bad in [
            saved_loop(8, "", 1.0, 2.0),
            saved_loop(0, "", 2.0, 2.0),
            saved_loop(0, "", 3.0, 2.0),
            saved_loop(0, "", 199.0, 200.5),
        ] {
            assert!(matches!(
                library.set_loops(1, &[bad]),
                Err(EditError::BadLoop(_))
            ));
        }
        assert!(matches!(
            library.set_loops(2, &[saved_loop(0, "", 1.0, 2.0)]),
            Err(EditError::NoAnalysis)
        ));
        assert!(matches!(
            library.set_loops(5, &[]),
            Err(EditError::NoSuchTrack)
        ));
        assert_eq!(library.loop_count(1), 2);

        // A blob that isn't understood is not written over.
        library
            .connection
            .lock()
            .unwrap()
            .execute(
                "UPDATE PerformanceData SET loops = x'0400000000000000' WHERE trackId = 2",
                [],
            )
            .unwrap();
        assert!(matches!(
            library.set_loops(1, &[saved_loop(0, "", 1.0, 2.0)]),
            Err(EditError::UnreadableLoops)
        ));
        let _ = std::fs::remove_dir_all(&cache);
    }

    #[test]
    fn a_library_without_smart_playlists_cannot_get_one() {
        let cache =
            std::env::temp_dir().join(format!("open-engine-lib-no-smart-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&cache);
        std::fs::create_dir_all(&cache).unwrap();
        let library = Library::load(database(), PathBuf::from("/lib"), cache.clone()).unwrap();
        assert!(matches!(
            library.create_smart_playlist("x", &rules("x")),
            Err(EditError::NoSmartlists)
        ));
        let _ = std::fs::remove_dir_all(&cache);
    }

    #[test]
    fn orders_playlist_tracks_and_resolves_origin_ids() {
        let library =
            Library::load(database(), PathBuf::from("/lib"), PathBuf::from("/cache")).unwrap();

        // Entity 12 -> 10 -> 11, and the entity for the missing track 99 is dropped.
        // Track 3 is found through its origin ('other', 7), not its own id.
        assert_eq!(library.playlists()[0].tracks, [2, 0, 1]);
        assert!(library.playlists()[1].tracks.is_empty());
    }

    #[test]
    fn locates_the_library_folder() {
        let root =
            std::env::temp_dir().join(format!("open-engine-lib-test-{}", std::process::id()));
        let library = root.join(LIBRARY_DIR);
        std::fs::create_dir_all(library.join("Database2")).unwrap();
        std::fs::write(library.join(DATABASE), b"").unwrap();

        assert_eq!(locate(&root), Some(library.clone()));
        assert_eq!(locate(&library), Some(library.clone()));
        assert_eq!(locate(&library.join("Database2")), Some(library.clone()));
        assert_eq!(locate(&root.join("nowhere")), None);

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn opens_a_copy_in_the_cache_and_leaves_the_source_alone() {
        let root =
            std::env::temp_dir().join(format!("open-engine-lib-open-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let library = root.join(LIBRARY_DIR);
        std::fs::create_dir_all(library.join(DATABASE_DIR)).unwrap();
        Connection::open(library.join(DATABASE))
            .unwrap()
            .execute_batch(FIXTURE)
            .unwrap();
        let before = std::fs::read(library.join(DATABASE)).unwrap();

        let opened = Library::open(&root, &root.join("cache")).unwrap();

        assert_eq!(opened.dir, library);
        assert!(opened.cache.starts_with(root.join("cache")));
        assert!(opened.cache.join(DATABASE).is_file(), "the copy exists");
        assert_eq!(opened.tracks.len(), 3);
        // Audio is played from where it is, not from the cache.
        assert_eq!(opened.tracks[0].path, library.join("../Music/one.flac"));
        assert_eq!(std::fs::read(library.join(DATABASE)).unwrap(), before);

        assert!(matches!(
            Library::open(&root.join("nowhere"), &root.join("cache")),
            Err(OpenError::NotALibrary(_))
        ));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn encodes_base64_without_padding_in_the_url_alphabet() {
        assert_eq!(base64_url(b""), "");
        assert_eq!(base64_url(b"f"), "Zg");
        assert_eq!(base64_url(b"fo"), "Zm8");
        assert_eq!(base64_url(b"foobar"), "Zm9vYmFy");
        // The two characters that differ from standard base64.
        assert_eq!(base64_url(&[0xfb, 0xff, 0xbf]), "-_-_");
    }

    #[test]
    fn recognises_images_by_their_content() {
        assert_eq!(image_type(b"\x89PNG\r\n\x1a\n...."), Some("image/png"));
        assert_eq!(image_type(&[0xff, 0xd8, 0xff, 0xe0]), Some("image/jpeg"));
        assert_eq!(image_type(b"image://streaming/x"), None);
        assert_eq!(image_type(b""), None);
    }

    #[test]
    fn finds_covers_in_the_database_and_the_artwork_folder() {
        let root = std::env::temp_dir().join(format!("open-engine-lib-art-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let library = root.join(LIBRARY_DIR);
        std::fs::create_dir_all(library.join(DATABASE_DIR)).unwrap();
        std::fs::create_dir_all(library.join(ARTWORK_DIR)).unwrap();
        Connection::open(library.join(DATABASE))
            .unwrap()
            .execute_batch(FIXTURE)
            .unwrap();
        // The file for cover 2 is named after its hash.
        let hash = [
            0xfb, 0xff, 0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0x00, 0xaa,
            0xbb, 0xcc, 0xdd, 0xee, 0xff, 0x00,
        ];
        std::fs::write(
            library
                .join(ARTWORK_DIR)
                .join(format!("{}.jpg", base64_url(&hash))),
            [0xff, 0xd8, 0xff, 0xe0, 1, 2, 3],
        )
        .unwrap();

        let opened = Library::open(&root, &root.join("cache")).unwrap();

        let in_database = opened.album_art(1).unwrap().unwrap();
        assert_eq!(in_database.mime_type, "image/png");
        assert_eq!(
            in_database.data,
            [0x89, b'P', b'N', b'G', 13, 10, 26, 10, 0, 1]
        );

        let on_disk = opened.album_art(2).unwrap().unwrap();
        assert_eq!(on_disk.mime_type, "image/jpeg");
        assert_eq!(on_disk.data, [0xff, 0xd8, 0xff, 0xe0, 1, 2, 3]);

        assert_eq!(
            opened.album_art(3).unwrap(),
            None,
            "a streaming url is not an image"
        );
        assert_eq!(opened.album_art(4).unwrap(), None, "the file is missing");
        assert_eq!(opened.album_art(99).unwrap(), None, "no such cover");
        std::fs::remove_dir_all(root).unwrap();
    }
}
