//! A plain folder of music as a library: the audio files below it are the tracks, and its
//! folders are the playlists (a folder holds the tracks of all the folders below it too).
//! There is no Engine library behind it, so there is nothing but what the files say: the
//! tags give the title, artist, album, genre and BPM, and the file the length.
//!
//! Only files the player can play are tracks. Hidden files and folders (a name starting
//! with a dot, such as the `._` files macOS leaves on a drive) are skipped, and so are links
//! to folders, which could lead back to where they came from.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use lofty::config::ParseOptions;
use lofty::prelude::*;
use lofty::probe::Probe;

use crate::library::{Playlist, Track};
use crate::progress::{Phase, Progress};
use crate::soundswitch::Lighting;

/// What the player can decode (see the `rodio` features in `Cargo.toml`), by file extension.
const EXTENSIONS: [&str; 6] = ["mp3", "flac", "wav", "ogg", "m4a", "mp4"];
/// Folders nested deeper than this are not looked into.
const MAX_DEPTH: usize = 32;

pub struct Scanned {
    pub tracks: Vec<Track>,
    pub playlists: Vec<Playlist>,
}

/// A folder: its audio files, then the folders in it, in the order they are listed in.
struct Node {
    name: String,
    files: Vec<PathBuf>,
    folders: Vec<Node>,
    /// The tracks in it and below it are `start..end` of the tracks, which are numbered
    /// folder by folder, so that they are one stretch.
    start: usize,
    end: usize,
}

/// Whether `path` is a file the player can play.
pub fn is_audio(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            EXTENSIONS
                .iter()
                .any(|known| known.eq_ignore_ascii_case(extension))
        })
}

/// Finds the music below `dir`. Fails only if `dir` itself can't be read. `progress` is
/// told how many files are found, then how many of them have been read.
pub fn scan(dir: &Path, progress: &Progress) -> io::Result<Scanned> {
    progress.phase(Phase::Scanning, 0, 0);
    let mut root = walk(dir, String::new(), 0, progress)?;
    let mut files = Vec::new();
    number(&mut root, &mut files);

    progress.phase(Phase::ReadingTags, 0, files.len() as u32);
    let metadata = read_all(&files, progress);
    let tracks = files
        .into_iter()
        .zip(metadata)
        .enumerate()
        .map(|(index, (file, meta))| track(index, dir, file, meta))
        .collect();

    let mut playlists = Vec::new();
    list(&root, 0, None, &mut playlists);
    Ok(Scanned { tracks, playlists })
}

fn walk(dir: &Path, name: String, depth: usize, progress: &Progress) -> io::Result<Node> {
    let mut entries = fs::read_dir(dir)?
        .filter_map(Result::ok)
        .filter(|entry| !entry.file_name().to_string_lossy().starts_with('.'))
        .collect::<Vec<_>>();
    entries.sort_by_key(|entry| entry.file_name().to_string_lossy().to_lowercase());

    let mut node = Node {
        name,
        files: Vec::new(),
        folders: Vec::new(),
        start: 0,
        end: 0,
    };
    for entry in entries {
        let path = entry.path();
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        if kind.is_dir() {
            if depth < MAX_DEPTH {
                // (A folder that can't be read is left out rather than failing the rest.)
                if let Ok(child) = walk(
                    &path,
                    entry.file_name().to_string_lossy().into_owned(),
                    depth + 1,
                    progress,
                ) {
                    node.folders.push(child);
                }
            }
        } else if is_audio(&path) && (kind.is_file() || path.is_file()) {
            node.files.push(path);
            progress.file_done();
        }
    }
    Ok(node)
}

/// Puts the files in order (a folder's own first, then those of the folders in it) and
/// notes for each folder which stretch of them it holds.
fn number(node: &mut Node, files: &mut Vec<PathBuf>) {
    node.start = files.len();
    files.extend(node.files.iter().cloned());
    for folder in &mut node.folders {
        number(folder, files);
    }
    node.end = files.len();
}

/// The playlists for the folders in `node`, depth first, leaving out those with no music.
fn list(node: &Node, depth: usize, parent: Option<usize>, playlists: &mut Vec<Playlist>) {
    for folder in node
        .folders
        .iter()
        .filter(|folder| folder.end > folder.start)
    {
        let index = playlists.len();
        playlists.push(Playlist {
            title: folder.name.clone(),
            depth,
            parent,
            has_children: folder.folders.iter().any(|child| child.end > child.start),
            smart: false,
            db_id: None,
            uuid: None,
            rules: None,
            raw_rules: None,
            tracks: (folder.start..folder.end).collect(),
        });
        list(folder, depth + 1, Some(index), playlists);
    }
}

#[derive(Default)]
struct Meta {
    title: String,
    artist: String,
    album: String,
    genre: String,
    bpm: Option<f64>,
    seconds: Option<u32>,
}

/// The tags and length of every file, read on as many threads as there are cores: the
/// files are many, and each is a read from the disk. Each one read is told to `progress`.
fn read_all(files: &[PathBuf], progress: &Progress) -> Vec<Meta> {
    let threads = std::thread::available_parallelism().map_or(1, |threads| threads.get());
    let chunk = files.len().div_ceil(threads).max(1);
    std::thread::scope(|scope| {
        let workers: Vec<_> = files
            .chunks(chunk)
            .map(|files| {
                scope.spawn(|| {
                    files
                        .iter()
                        .map(|file| {
                            let meta = read(file);
                            progress.file_done();
                            meta
                        })
                        .collect::<Vec<_>>()
                })
            })
            .collect();
        workers
            .into_iter()
            .flat_map(|worker| worker.join().unwrap_or_default())
            .collect()
    })
}

/// What the file says about itself. A file that can't be read has nothing to say, but is
/// still a track.
fn read(path: &Path) -> Meta {
    let options = ParseOptions::new().read_cover_art(false);
    let Ok(tagged) = Probe::open(path).and_then(|probe| probe.options(options).read()) else {
        return Meta::default();
    };
    let mut meta = Meta {
        seconds: Some(tagged.properties().duration().as_secs_f64().round() as u32),
        ..Meta::default()
    };
    if let Some(tag) = tagged.primary_tag().or_else(|| tagged.first_tag()) {
        let text = |value: Option<std::borrow::Cow<'_, str>>| {
            value
                .map(|value| value.trim().to_owned())
                .unwrap_or_default()
        };
        meta.title = text(tag.title());
        meta.artist = text(tag.artist());
        meta.album = text(tag.album());
        meta.genre = text(tag.genre());
        meta.bpm = [ItemKey::Bpm, ItemKey::IntegerBpm]
            .into_iter()
            .filter_map(|key| tag.get_string(key))
            .find_map(|bpm| bpm.trim().replace(',', ".").parse::<f64>().ok())
            .filter(|bpm| bpm.is_finite() && *bpm > 0.0);
    }
    meta
}

fn track(index: usize, dir: &Path, file: PathBuf, meta: Meta) -> Track {
    let relative = file.strip_prefix(dir).unwrap_or(&file);
    let title = if meta.title.is_empty() {
        file.file_stem()
            .map(|stem| stem.to_string_lossy().into_owned())
            .unwrap_or_default()
    } else {
        meta.title
    };
    Track {
        db_id: index as i64,
        title,
        artist: meta.artist,
        album: meta.album,
        genre: meta.genre,
        bpm: meta.bpm,
        length: meta.seconds,
        relative_path: relative.to_string_lossy().into_owned(),
        path: file.clone(),
        origin_key: None,
        streaming: false,
        album_art: None,
        rating: None,
        lighting: Lighting::default(),
    }
}

#[cfg(test)]
mod tests {
    use lofty::config::WriteOptions;
    use lofty::tag::{Tag, TagType};

    use super::*;

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(name: &str) -> Self {
            let path = std::env::temp_dir()
                .join(format!("open-engine-folder-{name}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&path);
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn file(&self, relative: &str) -> PathBuf {
            let path = self.0.join(relative);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            path
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    /// A mono 8 kHz WAV of `seconds` of silence.
    fn wav(path: &Path, seconds: u32) {
        let samples = 8_000 * seconds;
        let mut file = b"RIFF".to_vec();
        file.extend((36 + samples * 2).to_le_bytes());
        file.extend(b"WAVEfmt ");
        file.extend(16u32.to_le_bytes());
        file.extend(1u16.to_le_bytes()); // PCM
        file.extend(1u16.to_le_bytes()); // mono
        file.extend(8_000u32.to_le_bytes());
        file.extend((8_000u32 * 2).to_le_bytes());
        file.extend(2u16.to_le_bytes());
        file.extend(16u16.to_le_bytes());
        file.extend(b"data");
        file.extend((samples * 2).to_le_bytes());
        file.extend(vec![0u8; samples as usize * 2]);
        fs::write(path, file).unwrap();
    }

    fn titles(scanned: &Scanned, indices: &[usize]) -> Vec<String> {
        indices
            .iter()
            .map(|&index| scanned.tracks[index].title.clone())
            .collect()
    }

    #[test]
    fn music_in_folders_becomes_tracks_and_a_tree_of_playlists() {
        let temp = TempDir::new("tree");
        for relative in [
            "b.wav",
            "A.wav",
            "House/one.wav",
            "House/Deep/two.wav",
            "House/Deep/Dub/three.wav",
            "Techno/four.WAV",
            // Not music, hidden, or a folder with no music in it.
            "House/cover.jpg",
            "House/.hidden.wav",
            ".Trash/gone.wav",
            "Empty/notes.txt",
            "Empty/Deeper/readme.md",
        ] {
            wav(&temp.file(relative), 1);
        }

        let scanned = scan(&temp.0, &Progress::default()).unwrap();
        // A folder's own files come first, then the folders in it, all by name.
        assert_eq!(
            scanned
                .tracks
                .iter()
                .map(|track| track.relative_path.as_str())
                .collect::<Vec<_>>(),
            [
                "A.wav",
                "b.wav",
                "House/one.wav",
                "House/Deep/two.wav",
                "House/Deep/Dub/three.wav",
                "Techno/four.WAV"
            ]
        );
        assert_eq!(scanned.tracks[2].path, temp.0.join("House/one.wav"));
        assert_eq!(scanned.tracks[2].title, "one", "no tag, so the file name");
        assert_eq!(scanned.tracks[2].length, Some(1));
        assert!(!scanned.tracks[2].streaming);

        let shape: Vec<_> = scanned
            .playlists
            .iter()
            .map(|playlist| {
                (
                    playlist.title.as_str(),
                    playlist.depth,
                    playlist.parent,
                    playlist.has_children,
                    titles(&scanned, &playlist.tracks),
                )
            })
            .collect();
        assert_eq!(
            shape,
            [
                (
                    "House",
                    0,
                    None,
                    true,
                    vec!["one".into(), "two".into(), "three".into()]
                ),
                ("Deep", 1, Some(0), true, vec!["two".into(), "three".into()]),
                ("Dub", 2, Some(1), false, vec!["three".into()]),
                ("Techno", 0, None, false, vec!["four".into()]),
            ],
            "folders without music are left out"
        );
    }

    #[test]
    fn tags_give_the_title_artist_album_genre_and_tempo() {
        let temp = TempDir::new("tags");
        let path = temp.file("song.wav");
        wav(&path, 3);
        let mut tag = Tag::new(TagType::Id3v2);
        tag.set_title("Real Title".into());
        tag.set_artist("Someone".into());
        tag.set_album("The Album".into());
        tag.set_genre("House".into());
        tag.insert_text(ItemKey::IntegerBpm, "126".into());
        tag.save_to_path(&path, WriteOptions::default()).unwrap();

        let scanned = scan(&temp.0, &Progress::default()).unwrap();
        let track = &scanned.tracks[0];
        assert_eq!(
            (
                track.title.as_str(),
                track.artist.as_str(),
                track.album.as_str(),
                track.genre.as_str()
            ),
            ("Real Title", "Someone", "The Album", "House")
        );
        assert_eq!(track.bpm, Some(126.0));
        assert_eq!(track.length, Some(3));
    }

    #[test]
    fn a_file_that_can_not_be_read_is_still_a_track() {
        let temp = TempDir::new("broken");
        fs::write(temp.file("broken.mp3"), b"not audio").unwrap();
        let scanned = scan(&temp.0, &Progress::default()).unwrap();
        assert_eq!(scanned.tracks.len(), 1);
        assert_eq!(scanned.tracks[0].title, "broken");
        assert_eq!(scanned.tracks[0].length, None);
    }

    #[test]
    fn a_folder_without_music_has_no_tracks_and_a_missing_one_is_an_error() {
        let temp = TempDir::new("none");
        let scanned = scan(&temp.0, &Progress::default()).unwrap();
        assert!(scanned.tracks.is_empty() && scanned.playlists.is_empty());
        assert!(scan(&temp.0.join("missing"), &Progress::default()).is_err());
    }

    #[test]
    fn the_scan_tells_how_many_files_it_found_and_read() {
        let temp = TempDir::new("progress");
        for relative in ["a.wav", "House/b.wav", "House/Deep/c.wav", "notes.txt"] {
            wav(&temp.file(relative), 1);
        }
        let progress = Progress::default();
        let running = progress.begin();
        scan(&temp.0, &progress).unwrap();
        let seen = progress.snapshot();
        assert_eq!(
            (seen.running, seen.phase, seen.done_files, seen.total_files),
            (true, Phase::ReadingTags, 3, 3),
            "every file found was read, and the text file is not one"
        );
        drop(running);
    }

    #[test]
    fn only_what_the_player_can_decode_counts_as_audio() {
        for name in ["a.mp3", "a.FLAC", "a.Wav", "a.ogg", "a.m4a", "a.mp4"] {
            assert!(is_audio(Path::new(name)), "{name}");
        }
        for name in ["a.txt", "a.jpg", "a", "mp3", "a.aiff"] {
            assert!(!is_audio(Path::new(name)), "{name}");
        }
    }
}
