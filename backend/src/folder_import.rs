//! Importing a folder of music into an Engine library ("Import as playlist…"): the audio
//! files below a folder of the open folder library become tracks of the library, and a
//! playlist there holds them.
//!
//! The library is never edited in place, as for `mirror`: its `m.db` is copied to a scratch
//! folder, the tracks and the playlist are written to the copy, and only then does the copy
//! replace the original, after the original was kept in a backup folder. The audio files
//! (if they are to be copied) go to an `Engine-Tracks` folder beside the library first, and
//! are taken away again if anything after that fails. The new tracks are not analysed:
//! their `PerformanceData` is the empty row the database's own trigger makes, which is how
//! Engine itself holds a track it has not analysed yet, and `AnalyzeTrack` can analyse them
//! once the library is open.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use open_engine_api::v1 as api;
use rusqlite::types::Value;
use rusqlite::{Connection, OpenFlags};

use crate::cache::DATABASE_DIR;
use crate::library::{self, ARTWORK_DIR, EditError, Library, base64_url};
use crate::mirror::{
    Databases, MAIN_DATABASE, MirrorError, Place, copy_databases, databases_size, install,
};
use crate::origins;
use crate::progress::{Phase, Progress};
use crate::save::relative_path;
use crate::sync::is_busy;
use crate::track_sync::{self, base_name, copy_file};

/// The folder in the open folder's scratch folder where the work is done.
const SCRATCH_DIR: &str = "folder-import";
/// Where the databases that an import replaces are kept, in the open folder's scratch
/// folder (the library, which may be a player's stick, gets no backup folder).
const BACKUP_DIR: &str = "before-folder-import";

/// What to import and where to.
pub struct Request<'a> {
    /// The `Engine Library` folder, the folder or drive that contains it, or its `Database2`.
    pub target: &'a Path,
    /// Index of the folder in the open library's playlists.
    pub playlist: usize,
    /// The title of the playlist the tracks go in.
    pub title: &'a str,
    /// Copy the audio files into `Engine-Tracks` beside the library.
    pub copy_files: bool,
}

/// What the import does to the copy of the library's database.
#[derive(Default)]
struct Outcome {
    /// The paths (as the library records them) of the tracks that are new to it.
    added: Vec<String>,
    /// The tracks the library had already.
    existing: u32,
    files_missing: u32,
    /// The audio files that were copied, so that a failure later on can take them away again.
    copied: Vec<PathBuf>,
    /// The cover files that were written to `Artwork`, likewise.
    covers_written: Vec<PathBuf>,
    /// Each track of the folder that is in the library now (new or not), as an index into the
    /// folder's tracks and the path the library has for it, in the folder's order.
    placed: Vec<(usize, String)>,
}

impl Outcome {
    /// Every file the import made, to take away again if it fails.
    fn written(&self) -> Vec<PathBuf> {
        self.copied
            .iter()
            .chain(&self.covers_written)
            .cloned()
            .collect()
    }
}

/// Adds the tracks of the playlist `request.playlist` of the folder library `open` to the
/// Engine library at `request.target` and puts them in a playlist there. `scratch_root` is
/// where the copy of the database is made (and the old one kept), `progress` hears how far
/// the work is.
pub fn import(
    open: &Library,
    request: &Request<'_>,
    progress: &Progress,
) -> Result<api::ImportFolderResponse, MirrorError> {
    let wanted: Vec<usize> = {
        let playlists = open.playlists();
        let playlist = playlists
            .get(request.playlist)
            .ok_or(MirrorError::Edit(EditError::NoSuchPlaylist))?;
        playlist.tracks.clone()
    };
    if wanted.is_empty() {
        return Err(MirrorError::NothingToImport);
    }
    let title = request.title.trim();
    if title.is_empty() {
        return Err(MirrorError::Edit(EditError::EmptyTitle));
    }

    let dir = library::locate(request.target)
        .ok_or_else(|| MirrorError::NotALibrary(request.target.into()))?;
    let databases = dir.join(DATABASE_DIR);
    if is_busy(&databases) {
        return Err(MirrorError::Busy);
    }

    // The copy that is worked on.
    let base = open.cache.join(SCRATCH_DIR);
    let _ = fs::remove_dir_all(&base);
    let scratch = base.join("target");
    let from = Databases::of_library(&dir);
    progress.phase(Phase::Preparing, databases_size(&from), 0);
    let result = match copy_databases(&from, &scratch, progress) {
        Ok(()) => work(open, &wanted, request, title, &dir, &scratch, progress),
        Err(error) => Err((error, Vec::new())),
    };
    let (outcome, existed, changed) = match result {
        Ok(done) => done,
        Err((error, copied)) => {
            remove_files(&copied);
            let _ = fs::remove_dir_all(&base);
            return Err(error);
        }
    };

    let mut backup_path = String::new();
    if !changed.is_empty() {
        let place = Place::existing_library(&dir, open.cache.join(BACKUP_DIR));
        match install(&place, &scratch, &changed, progress) {
            Ok(backup) => backup_path = backup.display().to_string(),
            Err(error) => {
                remove_files(&outcome.written());
                let _ = fs::remove_dir_all(&base);
                return Err(error);
            }
        }
    }
    let _ = fs::remove_dir_all(&base);
    Ok(api::ImportFolderResponse {
        target_path: dir.display().to_string(),
        playlist_title: title.to_owned(),
        playlist_existed: existed,
        tracks_added: outcome.added.len() as u32,
        tracks_existing: outcome.existing,
        files_missing: outcome.files_missing,
        files_copied: outcome.copied.len() as u32,
        covers_added: outcome.covers_written.len() as u32,
        backup_path,
        added_paths: outcome.added,
    })
}

/// The work on the copy at `scratch`: the tracks, then the playlist and the origins.
/// Returns what was done, whether the playlist was there already and the names of the
/// databases that changed; or what went wrong and the files that were copied by then.
#[allow(clippy::type_complexity)]
fn work(
    open: &Library,
    wanted: &[usize],
    request: &Request<'_>,
    title: &str,
    dir: &Path,
    scratch: &Path,
    progress: &Progress,
) -> Result<(Outcome, bool, Vec<&'static str>), (MirrorError, Vec<PathBuf>)> {
    let database = scratch.join(DATABASE_DIR).join(MAIN_DATABASE);
    let mut outcome = Outcome::default();
    let tracks = add_tracks(
        open,
        wanted,
        request.copy_files,
        dir,
        &database,
        progress,
        &mut outcome,
    );
    let copied = Outcome::written;
    if let Err(error) = tracks {
        return Err((error, copied(&outcome)));
    }
    progress.phase(Phase::Updating, 0, 0);
    match playlist_and_origins(open, title, dir, scratch, &database, &outcome) {
        Ok((existed, changed)) => Ok((outcome, existed, changed)),
        Err(error) => Err((error, copied(&outcome))),
    }
}

/// What the table `Track` of the library needs a new row to have.
struct Column {
    name: String,
    /// The declared type, as written.
    declared: String,
    /// The column can't be left out: it can't be NULL and has no default.
    required: bool,
}

fn columns(connection: &Connection) -> rusqlite::Result<Vec<Column>> {
    let mut statement = connection.prepare(
        "SELECT name, type, \"notnull\", dflt_value IS NOT NULL, pk FROM pragma_table_info('Track')",
    )?;
    statement
        .query_map([], |row| {
            let (not_null, has_default, key): (i64, bool, i64) =
                (row.get(2)?, row.get(3)?, row.get(4)?);
            Ok(Column {
                name: row.get(0)?,
                declared: row.get::<_, Option<String>>(1)?.unwrap_or_default(),
                required: not_null != 0 && !has_default && key == 0,
            })
        })?
        .collect()
}

/// What a column that has to have a value gets when nothing here says what it is: the value
/// most of the library's tracks have (the kind of a track, say), else zero or the empty text.
fn usual_value(connection: &Connection, column: &Column) -> rusqlite::Result<Value> {
    let name = column.name.replace('"', "\"\"");
    let usual: Option<Value> = connection
        .query_row(
            &format!(
                "SELECT \"{name}\" FROM Track WHERE \"{name}\" IS NOT NULL \
                 GROUP BY \"{name}\" ORDER BY COUNT(*) DESC LIMIT 1"
            ),
            [],
            |row| row.get(0),
        )
        .ok();
    Ok(usual.unwrap_or_else(|| {
        let declared = column.declared.to_uppercase();
        if ["INT", "NUM", "REAL", "BOOL", "FLOAT", "DOUBLE"]
            .iter()
            .any(|kind| declared.contains(kind))
        {
            Value::Integer(0)
        } else {
            Value::Text(String::new())
        }
    }))
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs() as i64)
}

/// Adds a row to `Track` in the copy at `database` for each of the tracks `wanted` of the
/// folder `open` that the library lacks, and copies their files if asked to.
fn add_tracks(
    open: &Library,
    wanted: &[usize],
    copy_files: bool,
    dir: &Path,
    database: &Path,
    progress: &Progress,
    outcome: &mut Outcome,
) -> Result<(), MirrorError> {
    let mut report = track_sync::Report::default();
    let destinations = if copy_files {
        track_sync::destinations(
            wanted.iter().map(|&index| (index, &open.tracks[index])),
            dir,
            &mut report,
        )
    } else {
        Default::default()
    };
    outcome.files_missing = report.files_missing;
    if copy_files {
        let (files, bytes) = destinations
            .values()
            .filter(|destination| !destination.present)
            .fold((0u32, 0u64), |(files, bytes), destination| {
                (files + 1, bytes + destination.size)
            });
        progress.phase(Phase::CopyingTracks, bytes, files);
    }

    let mut connection = Connection::open_with_flags(
        database,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    let columns = columns(&connection)?;
    let transaction = connection.transaction()?;
    let mut known: HashSet<String> = {
        let mut statement = transaction.prepare("SELECT path FROM Track")?;
        statement
            .query_map([], |row| row.get::<_, Option<String>>(0))?
            .flatten()
            .flatten()
            .collect()
    };
    // What a column that has to have a value gets, looked up once.
    let mut usual: Vec<Option<Value>> = Vec::with_capacity(columns.len());
    for column in &columns {
        usual.push(if column.required {
            Some(usual_value(&transaction, column)?)
        } else {
            None
        });
    }

    let mut covers: HashMap<[u8; 20], Option<i64>> = HashMap::new();
    let mut seen = HashSet::new();
    for &index in wanted {
        let track = &open.tracks[index];
        // Where the track's path leads, and the file that goes there.
        let (relative, destination) = if copy_files {
            match destinations.get(&index) {
                Some(destination) => (destination.relative.clone(), Some(destination)),
                // (Its file was not found, which `destinations` counted.)
                None => continue,
            }
        } else {
            if !track.path.is_file() {
                outcome.files_missing += 1;
                continue;
            }
            (relative_path(dir, &track.path), None)
        };
        if !seen.insert(relative.clone()) {
            continue;
        }
        outcome.placed.push((index, relative.clone()));
        if known.contains(&relative) {
            outcome.existing += 1;
            continue;
        }

        let size = fs::metadata(&track.path).map_or(0, |metadata| metadata.len());
        let extension = track
            .path
            .extension()
            .map(|extension| extension.to_string_lossy().to_lowercase())
            .unwrap_or_default();
        let stamp = now();
        let art = cover(&transaction, &track.path, dir, &mut covers, outcome)?;
        let text = |value: &str| Value::Text(value.to_owned());
        let mut names = Vec::new();
        let mut values: Vec<Value> = Vec::new();
        for (column, usual) in columns.iter().zip(&usual) {
            // `id` is the row's own; the origin, the performance data and the edit time are
            // filled in by the database's triggers when the row is there.
            let value = match column.name.as_str() {
                "path" => text(&relative),
                "filename" => text(base_name(&relative)),
                "title" => text(&track.title),
                "artist" => text(&track.artist),
                "album" => text(&track.album),
                "genre" => text(&track.genre),
                "length" => match track.length {
                    Some(length) => Value::Integer(i64::from(length)),
                    None => continue,
                },
                "fileType" => text(&extension),
                "fileBytes" => Value::Integer(size as i64),
                // Not analysed (see the module's comment): no key, no stars.
                "key" => Value::Integer(-1),
                "rating" | "isAnalyzed" | "isPlayed" => Value::Integer(0),
                "isAvailable" => Value::Integer(1),
                "albumArtId" => match art {
                    Some(art) => Value::Integer(art),
                    None => continue,
                },
                "dateCreated" | "dateAdded" | "lastEditTime" => Value::Integer(stamp),
                _ => match usual {
                    Some(value) => value.clone(),
                    None => continue,
                },
            };
            if column.name == "id" {
                continue;
            }
            names.push(format!("\"{}\"", column.name.replace('"', "\"\"")));
            values.push(value);
        }
        let marks: Vec<String> = (1..=names.len()).map(|n| format!("?{n}")).collect();
        transaction.execute(
            &format!(
                "INSERT INTO Track ({}) VALUES ({})",
                names.join(", "),
                marks.join(", ")
            ),
            rusqlite::params_from_iter(values),
        )?;

        if let Some(destination) = destination
            && !destination.present
        {
            copy_file(&track.path, &destination.absolute, progress)?;
            outcome.copied.push(destination.absolute.clone());
        }
        known.insert(relative.clone());
        outcome.added.push(relative);
    }
    transaction.commit()?;
    Ok(())
}

/// The size of the covers Engine keeps in `Artwork`: they are thumbnails.
const COVER_SIDE: u32 = 256;

/// The picture embedded in the audio file at `path`: the front cover if there is one, else
/// the first picture.
fn embedded_picture(path: &Path) -> Option<Vec<u8>> {
    use lofty::config::ParseOptions;
    use lofty::picture::PictureType;
    use lofty::prelude::*;
    use lofty::probe::Probe;

    let tagged = Probe::open(path)
        .and_then(|probe| {
            probe
                .options(ParseOptions::new().read_cover_art(true))
                .read()
        })
        .ok()?;
    let pictures: Vec<_> = tagged
        .tags()
        .iter()
        .flat_map(|tag| tag.pictures())
        .collect();
    let picture = pictures
        .iter()
        .find(|picture| picture.pic_type() == PictureType::CoverFront)
        .or(pictures.first())?;
    Some(picture.data().to_vec())
}

/// `picture` as the thumbnail Engine keeps: a 256 by 256 JPEG (the picture is scaled to
/// cover the square and the rest is cut off).
fn thumbnail(picture: &[u8]) -> Option<Vec<u8>> {
    use image::codecs::jpeg::JpegEncoder;
    use image::imageops::FilterType;

    let decoded = image::load_from_memory(picture).ok()?;
    let square = decoded
        .resize_to_fill(COVER_SIDE, COVER_SIDE, FilterType::Lanczos3)
        .to_rgb8();
    let mut jpeg = Vec::new();
    JpegEncoder::new_with_quality(&mut jpeg, 90)
        .encode_image(&square)
        .ok()?;
    Some(jpeg)
}

/// The `AlbumArt` row of the cover embedded in the file at `path`, made if the library has
/// none for it (its thumbnail goes to `Artwork`, named after the hash of the embedded
/// picture, as Engine does it). `None` if the file has no picture or it can't be read.
/// `known` remembers the covers of this import (an album's tracks share one).
fn cover(
    transaction: &rusqlite::Transaction<'_>,
    path: &Path,
    dir: &Path,
    known: &mut HashMap<[u8; 20], Option<i64>>,
    outcome: &mut Outcome,
) -> Result<Option<i64>, MirrorError> {
    use rusqlite::OptionalExtension;
    use sha1::{Digest, Sha1};

    let Some(picture) = embedded_picture(path) else {
        return Ok(None);
    };
    let hash: [u8; 20] = Sha1::digest(&picture).into();
    if let Some(art) = known.get(&hash) {
        return Ok(*art);
    }
    let existing: Option<i64> = transaction
        .query_row(
            "SELECT id FROM AlbumArt WHERE hash IS ?1 LIMIT 1",
            [&hash[..]],
            |row| row.get(0),
        )
        .optional()?;
    let art = match existing {
        Some(art) => Some(art),
        None => {
            let file = dir
                .join(ARTWORK_DIR)
                .join(format!("{}.jpg", base64_url(&hash)));
            let present = file.is_file();
            let jpeg = if present { None } else { thumbnail(&picture) };
            // (A picture that can't be decoded is no cover; nothing is left half done.)
            if present || jpeg.is_some() {
                if let Some(jpeg) = jpeg {
                    fs::create_dir_all(dir.join(ARTWORK_DIR))?;
                    fs::write(&file, jpeg)?;
                    outcome.covers_written.push(file);
                }
                transaction.execute(
                    "INSERT INTO AlbumArt (hash, albumArt) VALUES (?1, NULL)",
                    [&hash[..]],
                )?;
                Some(transaction.last_insert_rowid())
            } else {
                None
            }
        }
    };
    known.insert(hash, art);
    Ok(art)
}

/// Puts the tracks of the folder in a playlist of the copy (made if there is none of that
/// title; one that is there gets the tracks added to its end) and saves where the new
/// tracks' files came from. Returns whether the playlist was there and which databases
/// changed.
fn playlist_and_origins(
    open: &Library,
    title: &str,
    dir: &Path,
    scratch: &Path,
    database: &Path,
    outcome: &Outcome,
) -> Result<(bool, Vec<&'static str>), MirrorError> {
    let copy = Library::open_copy(database, dir.to_path_buf(), scratch.to_path_buf())?;
    let by_path: HashMap<&str, usize> = copy
        .tracks
        .iter()
        .enumerate()
        .map(|(index, track)| (track.relative_path.as_str(), index))
        .collect();
    // The tracks of the folder as the library has them now, in the folder's order.
    let mut folder: Vec<usize> = Vec::new();
    for (_, relative) in &outcome.placed {
        if let Some(&there) = by_path.get(relative.as_str())
            && !folder.contains(&there)
        {
            folder.push(there);
        }
    }

    let existing = copy.playlists().iter().position(|playlist| {
        playlist.parent.is_none() && !playlist.smart && playlist.title == title
    });
    let (list, existed) = match existing {
        Some(list) => (list, true),
        None => (copy.create_playlist(title, None)?, false),
    };
    let current = copy.playlists()[list].tracks.clone();
    let mut desired = current.clone();
    desired.extend(folder.iter().filter(|track| !current.contains(track)));
    if desired != current {
        copy.set_playlist_tracks(list, &desired)?;
    }

    // Where the new tracks' files came from: their origin, which the track keeps in every
    // library it is copied to.
    let entries: Vec<(origins::TrackKey, String)> = outcome
        .placed
        .iter()
        .filter(|(_, relative)| outcome.added.contains(relative))
        .filter_map(|(source, relative)| {
            let key = copy.tracks[*by_path.get(relative.as_str())?]
                .origin_key
                .clone()?;
            Some((key, open.tracks[*source].path.display().to_string()))
        })
        .collect();
    let origins_changed = copy.write_origins(&entries)? > 0;

    let mut changed = Vec::new();
    if !outcome.added.is_empty() || !existed || desired != current {
        changed.push(MAIN_DATABASE);
    }
    if origins_changed {
        changed.push(origins::FILE);
    }
    Ok((existed, changed))
}

fn remove_files(files: &[PathBuf]) {
    for file in files {
        let _ = fs::remove_file(file);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(name: &str) -> Self {
            let path = std::env::temp_dir()
                .join(format!("open-engine-import-{name}-{}", std::process::id()));
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

    /// A mono 8 kHz WAV of one second of silence.
    fn wav(path: &Path) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let samples = 8_000u32;
        let mut file = b"RIFF".to_vec();
        file.extend((36 + samples * 2).to_le_bytes());
        file.extend(b"WAVEfmt ");
        file.extend(16u32.to_le_bytes());
        file.extend(1u16.to_le_bytes());
        file.extend(1u16.to_le_bytes());
        file.extend(8_000u32.to_le_bytes());
        file.extend((8_000u32 * 2).to_le_bytes());
        file.extend(2u16.to_le_bytes());
        file.extend(16u16.to_le_bytes());
        file.extend(b"data");
        file.extend((samples * 2).to_le_bytes());
        file.extend(vec![0u8; samples as usize * 2]);
        fs::write(path, file).unwrap();
    }

    /// The parts of an Engine library's schema that matter here, with the columns that
    /// have to have a value and the triggers that fill in the origin and make the row of
    /// performance data.
    const SCHEMA: &str = "
        CREATE TABLE Information (id INTEGER PRIMARY KEY, uuid TEXT);
        INSERT INTO Information VALUES (1, 'new-library');
        CREATE TABLE Track (id INTEGER PRIMARY KEY AUTOINCREMENT, title TEXT, artist TEXT,
            album TEXT, genre TEXT, length INTEGER, path TEXT, filename TEXT, fileType TEXT,
            fileBytes INTEGER, key INTEGER, rating INTEGER, isAnalyzed NUMERIC NOT NULL,
            isAvailable NUMERIC, dateCreated INTEGER, dateAdded INTEGER, lastEditTime INTEGER,
            trackType INTEGER NOT NULL, isExternalTrack NUMERIC NOT NULL DEFAULT 0,
            uri TEXT NOT NULL, streamingSource TEXT, bpmAnalyzed REAL, bpm INTEGER,
            originDatabaseUuid TEXT, originTrackId INTEGER, albumArtId INTEGER,
            UNIQUE (originDatabaseUuid, originTrackId), UNIQUE (path));
        CREATE TRIGGER trigger_after_insert_Track_fix_origin AFTER INSERT ON Track
        WHEN NEW.originTrackId IS NULL BEGIN
            UPDATE Track SET originTrackId = NEW.id,
                originDatabaseUuid = (SELECT uuid FROM Information) WHERE id = NEW.id;
        END;
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
        CREATE TABLE PlaylistEntity (id INTEGER PRIMARY KEY AUTOINCREMENT, listId INTEGER,
            trackId INTEGER, databaseUuid TEXT, nextEntityId INTEGER, membershipReference INTEGER,
            UNIQUE (listId, databaseUuid, trackId));
        CREATE TABLE Smartlist (listUuid TEXT NOT NULL PRIMARY KEY, title TEXT,
            parentPlaylistPath TEXT, nextPlaylistPath TEXT, nextListUuid TEXT, rules TEXT,
            lastEditTime DATETIME);";

    /// An Engine library on a stick at `root/stick`, with one track that has a path
    /// (`known`, if any) and a kind that is not 0. Returns its `Engine Library` folder.
    fn stick(root: &Path, known: Option<&str>) -> PathBuf {
        let library = root.join("stick").join("Engine Library");
        fs::create_dir_all(library.join(DATABASE_DIR)).unwrap();
        let connection = Connection::open(library.join(DATABASE_DIR).join(MAIN_DATABASE)).unwrap();
        connection.execute_batch(SCHEMA).unwrap();
        if let Some(path) = known {
            connection
                .execute(
                    "INSERT INTO Track (title, path, isAnalyzed, trackType, uri, \
                                        originDatabaseUuid, originTrackId) \
                     VALUES ('Known', ?1, 1, 1, '', 'old-library', 7)",
                    [path],
                )
                .unwrap();
        }
        library
    }

    /// The folder `root/pc/Music` with `Set` (a, b and `deep/c`) and `Other` (d), opened as
    /// a library, and the index of the playlist of `Set`.
    fn folder(root: &Path) -> (Library, usize) {
        for file in ["Set/a.wav", "Set/b.wav", "Set/deep/c.wav", "Other/d.wav"] {
            wav(&root.join("pc/Music").join(file));
        }
        let library = Library::open_folder(
            &root.join("pc/Music"),
            root.join("scratch"),
            &Progress::default(),
        )
        .unwrap();
        let set = library
            .playlists()
            .iter()
            .position(|playlist| playlist.title == "Set")
            .unwrap();
        (library, set)
    }

    fn rows(library: &Path, sql: &str) -> Vec<Vec<Value>> {
        let connection = Connection::open(library.join(DATABASE_DIR).join(MAIN_DATABASE)).unwrap();
        let mut statement = connection.prepare(sql).unwrap();
        let columns = statement.column_count();
        statement
            .query_map([], |row| (0..columns).map(|at| row.get(at)).collect())
            .unwrap()
            .map(Result::unwrap)
            .collect()
    }

    fn text(value: &str) -> Value {
        Value::Text(value.to_owned())
    }

    #[test]
    fn a_folder_becomes_tracks_that_are_not_analysed_and_a_playlist() {
        let temp = TempDir::new("plain");
        let (open, set) = folder(&temp.0);
        // The library has `a` already, by the path it would get.
        let library = stick(&temp.0, None);
        let known = relative_path(&library, &temp.0.join("pc/Music/Set/a.wav"));
        let connection = Connection::open(library.join(DATABASE_DIR).join(MAIN_DATABASE)).unwrap();
        connection
            .execute(
                "INSERT INTO Track (title, path, isAnalyzed, trackType, uri) \
                 VALUES ('a', ?1, 1, 3, '')",
                [&known],
            )
            .unwrap();
        drop(connection);

        let done = import(
            &open,
            &Request {
                target: &temp.0.join("stick"),
                playlist: set,
                title: "Set",
                copy_files: false,
            },
            &Progress::default(),
        )
        .unwrap();
        assert_eq!(
            (done.tracks_added, done.tracks_existing, done.files_copied),
            (2, 1, 0)
        );
        assert!(!done.playlist_existed);
        assert!(!done.backup_path.is_empty());
        assert_eq!(done.added_paths.len(), 2);

        // The new tracks are the files, with what the tags and the file say; not analysed,
        // and the columns that have to have a value got what the library's tracks have.
        let added = rows(
            &library,
            "SELECT title, path, fileType, isAnalyzed, key, trackType, length \
             FROM Track WHERE id > 1 ORDER BY id",
        );
        assert_eq!(added.len(), 2);
        assert_eq!(added[0][0], text("b"));
        assert_eq!(added[0][1], text(&done.added_paths[0]));
        assert_eq!(added[0][2], text("wav"));
        assert_eq!(
            added[0][3..],
            [
                Value::Integer(0),
                Value::Integer(-1),
                Value::Integer(3),
                Value::Integer(1)
            ]
        );
        // (A row of performance data with nothing in it, from the database's trigger.)
        assert_eq!(
            rows(
                &library,
                "SELECT COUNT(*) FROM PerformanceData WHERE trackData IS NULL"
            )[0][0],
            Value::Integer(3)
        );

        // The playlist holds the three tracks of the folder, the one the library had too.
        let reopened = Library::open_copy(
            &library.join(DATABASE_DIR).join(MAIN_DATABASE),
            library.clone(),
            temp.0.join("check"),
        )
        .unwrap();
        let playlists = reopened.playlists();
        assert_eq!(playlists.len(), 1);
        assert_eq!(playlists[0].title, "Set");
        let titles: Vec<&str> = playlists[0]
            .tracks
            .iter()
            .map(|&track| reopened.tracks[track].title.as_str())
            .collect();
        assert_eq!(titles, ["a", "b", "c"]);
        drop(playlists);

        // Where the new files came from is saved as their origin.
        let origins = origins::Origins::open_existing(&origins::source_file(&library))
            .unwrap()
            .expect("the origins were saved")
            .all()
            .unwrap();
        let sources: HashSet<&String> = origins.values().collect();
        assert_eq!(sources.len(), 2);
        assert!(
            sources
                .iter()
                .all(|source| source.ends_with("b.wav") || source.ends_with("c.wav"))
        );

        // The backup has the library as it was; the folder was not touched.
        assert!(
            temp.0
                .join("scratch")
                .join(BACKUP_DIR)
                .join(MAIN_DATABASE)
                .is_file()
        );
        assert!(temp.0.join("pc/Music/Set/b.wav").is_file());
    }

    #[test]
    fn the_files_can_be_copied_and_importing_again_changes_nothing() {
        let temp = TempDir::new("copy");
        let (open, set) = folder(&temp.0);
        let library = stick(&temp.0, None);
        let request = Request {
            target: &library,
            playlist: set,
            title: "Warm-up",
            copy_files: true,
        };
        let done = import(&open, &request, &Progress::default()).unwrap();
        assert_eq!((done.tracks_added, done.files_copied), (3, 3));
        // `Artist/Album/File` in `Engine-Tracks` beside the library folder.
        let copied = temp
            .0
            .join("stick/Engine-Tracks/Unknown Artist/Unknown Album/a.wav");
        assert!(copied.is_file());
        assert!(
            done.added_paths
                .contains(&"../Engine-Tracks/Unknown Artist/Unknown Album/a.wav".to_owned())
        );
        let paths = rows(&library, "SELECT path FROM Track ORDER BY id");
        assert_eq!(paths.len(), 3);

        // Again: the tracks are there, the playlist is too, so nothing is written.
        let again = import(&open, &request, &Progress::default()).unwrap();
        assert_eq!(
            (
                again.tracks_added,
                again.tracks_existing,
                again.files_copied
            ),
            (0, 3, 0)
        );
        assert!(again.playlist_existed);
        assert!(again.backup_path.is_empty());
        assert_eq!(
            rows(&library, "SELECT COUNT(*) FROM Track")[0][0],
            Value::Integer(3)
        );
        assert_eq!(
            rows(&library, "SELECT COUNT(*) FROM PlaylistEntity")[0][0],
            Value::Integer(3)
        );
    }

    #[test]
    fn a_folder_without_music_a_missing_library_and_a_busy_one_are_refused() {
        let temp = TempDir::new("refused");
        let (open, set) = folder(&temp.0);
        let progress = Progress::default();
        let ask = |target: &Path, playlist: usize| {
            import(
                &open,
                &Request {
                    target,
                    playlist,
                    title: "Set",
                    copy_files: false,
                },
                &progress,
            )
        };
        assert!(matches!(
            ask(&temp.0.join("nowhere"), set),
            Err(MirrorError::NotALibrary(_))
        ));
        let library = stick(&temp.0, None);
        assert!(matches!(
            ask(&library, 999),
            Err(MirrorError::Edit(EditError::NoSuchPlaylist))
        ));
        // A journal that is not empty: another program is writing.
        fs::write(library.join(DATABASE_DIR).join("m.db-journal"), b"x").unwrap();
        assert!(matches!(ask(&library, set), Err(MirrorError::Busy)));
    }

    #[test]
    fn a_failure_leaves_no_copied_files_and_the_library_as_it_was() {
        let temp = TempDir::new("failure");
        let (_, _) = folder(&temp.0);
        embed(&temp.0.join("pc/Music/Set/a.wav"), png());
        let open = Library::open_folder(
            &temp.0.join("pc/Music"),
            temp.0.join("scratch"),
            &Progress::default(),
        )
        .unwrap();
        let set = open
            .playlists()
            .iter()
            .position(|playlist| playlist.title == "Set")
            .unwrap();
        let library = stick(&temp.0, None);
        // A playlist with the title that a smart playlist has: the playlist can't be made,
        // which is found after the files were copied.
        let connection = Connection::open(library.join(DATABASE_DIR).join(MAIN_DATABASE)).unwrap();
        connection
            .execute(
                "INSERT INTO Smartlist VALUES ('s1', 'Taken', '', '', '', '{\"match\":\"all\"}', NULL)",
                [],
            )
            .unwrap();
        drop(connection);
        let before = fs::read(library.join(DATABASE_DIR).join(MAIN_DATABASE)).unwrap();
        let result = import(
            &open,
            &Request {
                target: &library,
                playlist: set,
                title: "Taken",
                copy_files: true,
            },
            &Progress::default(),
        );
        assert!(matches!(
            result,
            Err(MirrorError::Edit(EditError::TitleTaken))
        ));
        assert!(
            !temp
                .0
                .join("stick/Engine-Tracks/Unknown Artist/Unknown Album/a.wav")
                .exists()
        );
        assert_eq!(
            fs::read(library.join(DATABASE_DIR).join(MAIN_DATABASE)).unwrap(),
            before
        );
    }

    /// A 300 by 200 PNG.
    fn png() -> Vec<u8> {
        let mut png = Vec::new();
        image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(
            300,
            200,
            image::Rgb([200, 30, 30]),
        ))
        .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
        .unwrap();
        png
    }

    /// Gives the file the picture as its front cover.
    fn embed(path: &Path, picture: Vec<u8>) {
        use lofty::config::WriteOptions;
        use lofty::picture::{MimeType, Picture, PictureType};
        use lofty::prelude::*;
        use lofty::tag::{Tag, TagType};
        let mut tag = Tag::new(TagType::Id3v2);
        tag.push_picture(
            Picture::unchecked(picture)
                .pic_type(PictureType::CoverFront)
                .mime_type(MimeType::Png)
                .build(),
        );
        tag.save_to_path(path, WriteOptions::default()).unwrap();
    }

    #[test]
    fn embedded_covers_become_thumbnails_in_artwork_that_tracks_of_an_album_share() {
        use sha1::{Digest, Sha1};
        let temp = TempDir::new("covers");
        let (_, _) = folder(&temp.0); // (the files)
        // A 300 by 200 PNG on a and b, nothing on c.
        let mut png = Vec::new();
        image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(
            300,
            200,
            image::Rgb([200, 30, 30]),
        ))
        .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
        .unwrap();
        embed(&temp.0.join("pc/Music/Set/a.wav"), png.clone());
        embed(&temp.0.join("pc/Music/Set/b.wav"), png.clone());
        // (Opened after the pictures are in, as the user would have it.)
        let open = Library::open_folder(
            &temp.0.join("pc/Music"),
            temp.0.join("scratch"),
            &Progress::default(),
        )
        .unwrap();
        let set = open
            .playlists()
            .iter()
            .position(|playlist| playlist.title == "Set")
            .unwrap();
        let library = stick(&temp.0, None);

        let done = import(
            &open,
            &Request {
                target: &library,
                playlist: set,
                title: "Set",
                copy_files: false,
            },
            &Progress::default(),
        )
        .unwrap();
        assert_eq!(
            (done.tracks_added, done.covers_added),
            (3, 1),
            "one picture for the two"
        );

        // The file is named after the hash of the embedded picture and is a 256 by 256 JPEG.
        let hash: [u8; 20] = Sha1::digest(&png).into();
        let file = library
            .join(ARTWORK_DIR)
            .join(format!("{}.jpg", library::base64_url(&hash)));
        let thumbnail = image::open(&file).unwrap();
        assert_eq!((thumbnail.width(), thumbnail.height()), (256, 256));
        assert_eq!(
            image::guess_format(&fs::read(&file).unwrap()).unwrap(),
            image::ImageFormat::Jpeg
        );

        // One row of the library holds the hash (raw, no image); a and b point at it.
        assert_eq!(
            rows(&library, "SELECT COUNT(*), COUNT(albumArt) FROM AlbumArt"),
            [[Value::Integer(1), Value::Integer(0)]]
        );
        assert_eq!(
            rows(&library, "SELECT hash FROM AlbumArt")[0][0],
            Value::Blob(hash.to_vec())
        );
        let art = rows(&library, "SELECT title, albumArtId FROM Track ORDER BY id");
        assert_eq!(art[0][1], art[1][1]);
        assert_ne!(art[0][1], Value::Null);
        assert_eq!(art[2], [text("c"), Value::Null]);

        // The library gives the cover back, as the player shows it.
        let reopened = Library::open_copy(
            &library.join(DATABASE_DIR).join(MAIN_DATABASE),
            library.clone(),
            temp.0.join("check"),
        )
        .unwrap();
        let id = reopened.tracks[0].album_art.expect("a has a cover");
        let cover = reopened.album_art(id).unwrap().expect("the cover is found");
        assert_eq!(cover.mime_type, "image/jpeg");

        // Importing again makes no second cover.
        let again = import(
            &open,
            &Request {
                target: &library,
                playlist: set,
                title: "Set",
                copy_files: false,
            },
            &Progress::default(),
        )
        .unwrap();
        assert_eq!(again.covers_added, 0);
    }
}
