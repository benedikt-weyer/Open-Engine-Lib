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
    /// Ratings set since the library was opened, by index in `tracks`.
    ratings: Mutex<HashMap<usize, u32>>,
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
        let (tracks, by_id, by_origin) = load_tracks(&connection, &dir)?;
        let playlists = load_playlists(&connection, &tracks, &by_id, &by_origin)?;
        Ok(Self {
            dir,
            cache,
            tracks,
            playlists: Mutex::new(playlists),
            by_id,
            by_origin,
            ratings: Mutex::default(),
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
        let title = title.trim();
        if title.is_empty() {
            return Err(EditError::EmptyTitle);
        }
        if self
            .playlists()
            .iter()
            .any(|playlist| playlist.parent.is_none() && playlist.title == title)
        {
            return Err(EditError::TitleTaken);
        }
        cache::mark_edited(&self.cache).map_err(EditError::Cache)?;
        let connection = self.connection.lock().expect("database lock");
        if !has_smartlists(&connection) {
            return Err(EditError::NoSmartlists);
        }

        let uuid: String =
            connection.query_row("SELECT lower(hex(randomblob(16)))", [], |row| row.get(0))?;
        // A version 4 UUID in the usual 8-4-4-4-12 form.
        let variant = ['8', '9', 'a', 'b'][usize::from(uuid.as_bytes()[16] % 4)];
        let uuid = format!(
            "{}-{}-4{}-{variant}{}-{}",
            &uuid[0..8],
            &uuid[8..12],
            &uuid[13..16],
            &uuid[17..20],
            &uuid[20..32]
        );

        // The new one goes after the last smart playlist, and in front of the same
        // playlist it was in front of.
        let transaction = connection.unchecked_transaction()?;
        let last: Option<(String, String)> = transaction
            .query_row(
                "SELECT listUuid, COALESCE(nextPlaylistPath, '') FROM Smartlist \
                 WHERE COALESCE(parentPlaylistPath, '') = '' \
                   AND COALESCE(nextListUuid, '') = '' LIMIT 1",
                [],
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
             VALUES (?1, ?2, '', ?3, '', ?4, datetime('now'))",
            (&uuid, title, next_path, rules.to_json()),
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
            (title, rules.to_json(), &uuid),
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

    /// Reads the playlists again after the database changed.
    fn reload_playlists(&self, connection: &Connection) -> rusqlite::Result<()> {
        let playlists = load_playlists(connection, &self.tracks, &self.by_id, &self.by_origin)?;
        *self.playlists() = playlists;
        Ok(())
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
fn locate(path: &Path) -> Option<PathBuf> {
    let mut candidates = vec![path.to_path_buf(), path.join(LIBRARY_DIR)];
    if path.file_name().is_some_and(|name| name == DATABASE_DIR) {
        candidates.extend(path.parent().map(Path::to_path_buf));
    }
    candidates
        .into_iter()
        .find(|dir| dir.join(DATABASE).is_file())
}

type TrackIds = HashMap<i64, usize>;
type TrackOrigins = HashMap<(String, i64), usize>;

fn load_tracks(
    connection: &Connection,
    dir: &Path,
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
    let soundswitch = drive.as_deref().map(SoundSwitch::scan);
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
        if let (Some(uuid), Some(origin_id)) = (row.get::<_, Option<String>>(10)?, number(11)?) {
            by_origin.insert((uuid, origin_id as i64), tracks.len());
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
