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

use rusqlite::{Connection, OpenFlags};

use crate::cache::{self, DATABASE_DIR};

const DATABASE: &str = "Database2/m.db";
const LIBRARY_DIR: &str = "Engine Library";

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

impl From<rusqlite::Error> for OpenError {
    fn from(error: rusqlite::Error) -> Self {
        OpenError::Database(error)
    }
}

#[derive(Debug, Clone)]
pub struct Track {
    pub title: String,
    pub artist: String,
    pub album: String,
    pub genre: String,
    pub bpm: Option<f64>,
    /// Length in whole seconds.
    pub length: Option<u32>,
    /// Absolute-ish path to the audio file (the library's relative path joined onto its folder).
    pub path: PathBuf,
    /// Streaming tracks (e.g. TIDAL) only exist on the player, never as a file here.
    pub streaming: bool,
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
    /// Indices into `Library::tracks`, in playlist order.
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
    pub playlists: Vec<Playlist>,
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
        Self::load(&connection, dir, cache)
    }

    fn load(connection: &Connection, dir: PathBuf, cache: PathBuf) -> Result<Self, OpenError> {
        let (tracks, by_id, by_origin) = load_tracks(connection, &dir)?;
        let playlists = load_playlists(connection, &by_id, &by_origin)?;
        Ok(Self {
            dir,
            cache,
            tracks,
            playlists,
        })
    }
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
    let mut statement = connection.prepare(
        "SELECT id, title, artist, album, genre, bpmAnalyzed, bpm, length, path, \
                streamingSource, originDatabaseUuid, originTrackId \
         FROM Track ORDER BY id",
    )?;
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

        tracks.push(Track {
            title,
            artist: text(2)?,
            album: text(3)?,
            genre: text(4)?,
            bpm,
            length: number(7)?
                .filter(|length| *length >= 0.0)
                .map(|length| length as u32),
            path: dir.join(&relative),
            streaming: row
                .get::<_, Option<String>>(9)?
                .is_some_and(|source| !source.is_empty()),
        });
    }

    Ok((tracks, by_id, by_origin))
}

fn load_playlists(
    connection: &Connection,
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

    let mut playlists = Vec::new();
    // Depth first, each level in its stored sibling order. Playlists hang off the
    // root, whose id is 0.
    let mut stack: Vec<(i64, usize, Option<usize>)> =
        chain_order(rows_by_parent.get(&0).map(Vec::as_slice).unwrap_or(&[]))
            .into_iter()
            .rev()
            .map(|id| (id, 0, None))
            .collect();

    while let Some((id, depth, parent)) = stack.pop() {
        let children = rows_by_parent.get(&id).map(Vec::as_slice).unwrap_or(&[]);
        let tracks = entities
            .get(&id)
            .map(|list| {
                chain_order(list)
                    .into_iter()
                    .filter_map(|entity| entity_track.get(&entity).copied())
                    .collect()
            })
            .unwrap_or_default();

        let index = playlists.len();
        playlists.push(Playlist {
            title: titles.remove(&id).unwrap_or_default(),
            depth,
            parent,
            has_children: !children.is_empty(),
            tracks,
        });
        stack.extend(
            chain_order(children)
                .into_iter()
                .rev()
                .map(|child| (child, depth + 1, Some(index))),
        );
    }

    Ok(playlists)
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
                    streamingSource TEXT, originDatabaseUuid TEXT, originTrackId INTEGER);
                 CREATE TABLE Playlist (id INTEGER PRIMARY KEY, title TEXT, parentListId INTEGER,
                    nextListId INTEGER);
                 CREATE TABLE PlaylistEntity (id INTEGER PRIMARY KEY, listId INTEGER,
                    trackId INTEGER, databaseUuid TEXT, nextEntityId INTEGER);
                 INSERT INTO Track VALUES
                    (1, 'One', 'A', 'X', 'House', 124.0, 124, 200, '../Music/one.flac', NULL, 'db', 1),
                    (2, NULL, NULL, NULL, NULL, NULL, 90, NULL, '../Music/two.mp3', NULL, 'db', 2),
                    (3, 'Three', 'C', 'Z', 'Techno', 0.0, 0, 61, '../tidal/three.mp4', 'TIDAL', 'other', 7);
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
            Library::load(&database(), PathBuf::from("/lib"), PathBuf::from("/cache")).unwrap();

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
    }

    #[test]
    fn orders_playlists_depth_first_by_sibling_links() {
        let library =
            Library::load(&database(), PathBuf::from("/lib"), PathBuf::from("/cache")).unwrap();

        let shape: Vec<_> = library
            .playlists
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
    fn orders_playlist_tracks_and_resolves_origin_ids() {
        let library =
            Library::load(&database(), PathBuf::from("/lib"), PathBuf::from("/cache")).unwrap();

        // Entity 12 -> 10 -> 11, and the entity for the missing track 99 is dropped.
        // Track 3 is found through its origin ('other', 7), not its own id.
        assert_eq!(library.playlists[0].tracks, [2, 0, 1]);
        assert!(library.playlists[1].tracks.is_empty());
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
}
