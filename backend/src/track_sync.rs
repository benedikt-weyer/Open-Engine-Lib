//! The track part of "sync to other library" (see `mirror`): the stars of the open
//! library's tracks, where their files are, and the files themselves.
//!
//! Everything is done on the copy of the other library's `m.db` that `mirror` works on,
//! so a diff (which runs this without copying any file) lists what a push does. The
//! copied files go to an `Engine-Tracks` folder beside the other library's
//! `Engine Library` folder, in an `Artist/Album` structure, with file and folder names
//! that exFAT (the file system of most DJ sticks) accepts.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use open_engine_api::v1 as api;
use rusqlite::{Connection, OpenFlags};

use crate::library::{ARTWORK_DIR, Library, STEMS_DIR, base64_url, stems_file_name};
use crate::mirror::MirrorError;
use crate::progress::{Phase, Progress};

/// The folder the tracks are copied to, next to the `Engine Library` folder.
pub const TRACKS_DIR: &str = "Engine-Tracks";
/// What a track without an artist or an album is filed under.
const UNKNOWN_ARTIST: &str = "Unknown Artist";
const UNKNOWN_ALBUM: &str = "Unknown Album";
/// exFAT allows names up to 255 UTF-16 code units.
const MAX_NAME_UNITS: usize = 255;

/// What to do with the tracks the two libraries have in common.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Options {
    /// The other library's star ratings become those of the open library.
    pub stars: bool,
    /// The other library's track paths become the open library's, as they are written.
    pub locations: bool,
    /// The audio files are copied into `Engine-Tracks` and the paths lead there (instead of
    /// `locations`). Tracks the other library lacks are added to it.
    pub copy_tracks: bool,
    /// The `.stems` file of each copied track, if it has one, is copied to the `Stems`
    /// folder of the other library. Only together with `copy_tracks`.
    pub copy_stems: bool,
}

impl Options {
    pub fn any(self) -> bool {
        self.stars || self.locations || self.copy_tracks
    }
}

/// What the track part did, or in a diff would do.
#[derive(Debug, Default)]
pub struct Report {
    /// Tracks of the open library that were added to the other one (indices into its
    /// tracks).
    pub added: Vec<usize>,
    /// Tracks the other library has whose stars or path changed.
    pub changed: Vec<api::TrackChange>,
    /// Files that are not in `Engine-Tracks` yet.
    pub files_to_copy: u32,
    pub bytes_to_copy: u64,
    /// Files of the open library that could not be found, so they are not copied.
    pub files_missing: u32,
    /// Tracks whose new path another track of the other library already has.
    pub location_conflicts: u32,
    /// The files that were copied, so that a failure later on can take them away again.
    pub copied: Vec<PathBuf>,
}

/// `name` as a file or folder name that exFAT accepts: the characters it forbids
/// (`\ / : * ? " < > |` and control characters) are replaced by `_`, and so is a trailing
/// dot or space, and a name that would be empty or `.` or `..` is made `_`. Names that
/// are too long are cut.
pub fn exfat_name(name: &str) -> String {
    let mut cleaned: String = name
        .chars()
        .map(|c| {
            if c.is_control() || "\\/:*?\"<>|".contains(c) {
                '_'
            } else {
                c
            }
        })
        .collect();
    // Windows (which reads these sticks too) cannot have a name end in a dot or a space.
    let kept = cleaned.trim_end_matches(['.', ' ']).len();
    let trailing = cleaned.len() - kept;
    cleaned.truncate(kept);
    cleaned.push_str(&"_".repeat(trailing));
    truncate_units(&cleaned, MAX_NAME_UNITS)
}

/// Like `exfat_name`, for a file name: what is cut off a long one is not the extension.
pub fn exfat_file_name(name: &str) -> String {
    let (stem, extension) = split_extension(name);
    let extension = exfat_name(extension);
    let extension = if extension.is_empty() {
        extension
    } else {
        format!(".{extension}")
    };
    let room = MAX_NAME_UNITS.saturating_sub(extension.encode_utf16().count());
    let stem = truncate_units(&exfat_name(stem), room.max(1));
    format!("{stem}{extension}")
}

/// A name's stem and its extension (without the dot): the part after the last dot, if it
/// is short and there is something before the dot.
fn split_extension(name: &str) -> (&str, &str) {
    match name.rfind('.') {
        Some(dot) if dot > 0 && name.len() - dot - 1 <= 10 => (&name[..dot], &name[dot + 1..]),
        _ => (name, ""),
    }
}

/// At most `units` UTF-16 code units of `text`, cut between characters.
fn truncate_units(text: &str, units: usize) -> String {
    let mut used = 0;
    let mut kept = String::new();
    for c in text.chars() {
        used += c.len_utf16();
        if used > units {
            break;
        }
        kept.push(c);
    }
    kept
}

/// Where a track's file goes.
struct Destination {
    /// As the track's path in the other library, relative to its `Engine Library` folder.
    relative: String,
    absolute: PathBuf,
    size: u64,
    /// A file of the same size is already there (from an earlier sync).
    present: bool,
}

/// Where each track of `source` that has a file goes: `Engine-Tracks/Artist/Album/File`,
/// beside `target_dir`. Names that exFAT treats as the same (it ignores case) get a
/// number: the first track keeps the plain name.
fn destinations(
    source: &Library,
    target_dir: &Path,
    report: &mut Report,
) -> HashMap<usize, Destination> {
    let root = target_dir.parent().unwrap_or(target_dir).join(TRACKS_DIR);
    let mut claimed: HashSet<String> = HashSet::new();
    let mut destinations = HashMap::new();

    for (index, track) in source.tracks.iter().enumerate() {
        if track.streaming {
            continue;
        }
        let Ok(size) = fs::metadata(&track.path).and_then(|m| {
            if m.is_file() {
                Ok(m.len())
            } else {
                Err(std::io::ErrorKind::NotFound.into())
            }
        }) else {
            report.files_missing += 1;
            continue;
        };

        let folder = |name: &str, unknown: &str| {
            let name = name.trim();
            exfat_name(if name.is_empty() { unknown } else { name })
        };
        let artist = folder(&track.artist, UNKNOWN_ARTIST);
        let album = folder(&track.album, UNKNOWN_ALBUM);
        let original = track
            .path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        let file = exfat_file_name(&original);
        let (stem, extension) = split_extension(&file);

        for attempt in 1u32.. {
            let name = match (attempt, extension.is_empty()) {
                (1, _) => file.clone(),
                (n, true) => numbered(stem, n, MAX_NAME_UNITS),
                (n, false) => {
                    let room = MAX_NAME_UNITS - extension.encode_utf16().count() - 1;
                    format!("{}.{extension}", numbered(stem, n, room))
                }
            };
            let relative = format!("../{TRACKS_DIR}/{artist}/{album}/{name}");
            if claimed.contains(&relative.to_lowercase()) {
                continue;
            }
            let absolute = root.join(&artist).join(&album).join(&name);
            let present = match fs::metadata(&absolute) {
                Ok(existing) if existing.is_file() && existing.len() == size => true,
                // Another file is there: it is not this track's.
                Ok(_) => continue,
                Err(_) => false,
            };
            claimed.insert(relative.to_lowercase());
            destinations.insert(
                index,
                Destination {
                    relative,
                    absolute,
                    size,
                    present,
                },
            );
            break;
        }
    }
    destinations
}

/// `stem (n)`, with the stem cut so that the whole is at most `units` long.
fn numbered(stem: &str, n: u32, units: usize) -> String {
    let suffix = format!(" ({n})");
    let room = units.saturating_sub(suffix.encode_utf16().count());
    format!("{}{suffix}", truncate_units(stem, room.max(1)))
}

/// The file name of a track path as written in a library (`../a/b.flac` is `b.flac`).
fn base_name(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// Stars as a number of stars, for the lists.
fn stars(rating: Option<u32>) -> String {
    (rating.map_or(0, |rating| ((rating + 10) / 20).min(5))).to_string()
}

/// Copies `from` to `to` (making its folders) so that `to` is never half written.
fn copy_file(from: &Path, to: &Path, progress: &Progress) -> std::io::Result<()> {
    if let Some(folder) = to.parent() {
        fs::create_dir_all(folder)?;
    }
    let temporary = to.with_file_name(format!(
        ".copying-{}",
        to.file_name().map_or(0, |name| name.len())
    ));
    let result = progress
        .copy(from, &temporary, true)
        .and_then(|_| fs::rename(&temporary, to));
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

/// Copies the file `from` to `to` unless something is at `to` already (a cover is named
/// after its hash and stems after their track, so what is there is the same file, and
/// another one is not ours to replace). With `copy_files` false it is only counted.
fn place_extra(
    from: &Path,
    to: &Path,
    copy_files: bool,
    progress: &Progress,
    report: &mut Report,
) -> Result<(), MirrorError> {
    let Ok(size) = fs::metadata(from).and_then(|m| {
        if m.is_file() {
            Ok(m.len())
        } else {
            Err(std::io::ErrorKind::NotFound.into())
        }
    }) else {
        return Ok(());
    };
    if fs::symlink_metadata(to).is_ok() {
        return Ok(());
    }
    report.files_to_copy += 1;
    report.bytes_to_copy += size;
    if copy_files {
        progress.grow(size, 1);
        copy_file(from, to, progress)?;
        report.copied.push(to.to_path_buf());
    }
    Ok(())
}

/// The name of the `Stems` file of `track`: its origin, which every library the track was
/// copied to has the same, so the file is found under the same name there.
fn stems_name(track: &crate::library::Track) -> Option<String> {
    let (uuid, id) = track.origin_key.as_ref()?;
    Some(stems_file_name(uuid, *id))
}

fn columns(connection: &Connection, table: &str, schema: &str) -> rusqlite::Result<Vec<String>> {
    let mut statement = connection.prepare(&format!(
        "SELECT name FROM pragma_table_info('{table}', '{schema}')"
    ))?;
    statement.query_map([], |row| row.get(0))?.collect()
}

/// Carries out `options` on the database copy at `database`, which belongs to the library
/// `probe` (the same copy, opened), for the tracks `matched` says the libraries share.
/// With `copy_files` false nothing is written but the database copy, so that a diff can
/// say what would happen.
pub fn sync(
    source: &Library,
    probe: &Library,
    matched: &[Option<usize>],
    database: &Path,
    options: Options,
    copy_files: bool,
    progress: &Progress,
) -> Result<Report, MirrorError> {
    let mut report = Report::default();
    let destinations = if options.copy_tracks {
        destinations(source, &probe.dir, &mut report)
    } else {
        HashMap::new()
    };
    if copy_files {
        // What there is to copy is known now; the copying is done track by track below.
        let (files, bytes) = destinations
            .values()
            .filter(|destination| !destination.present)
            .fold((0u32, 0u64), |(files, bytes), d| {
                (files + 1, bytes + d.size)
            });
        progress.phase(Phase::CopyingTracks, bytes, files);
    }

    let mut connection = Connection::open_with_flags(
        database,
        OpenFlags::SQLITE_OPEN_READ_WRITE
            | OpenFlags::SQLITE_OPEN_URI
            | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    connection.execute(
        "ATTACH DATABASE ?1 AS src",
        [crate::sync::source_uri(
            &source.cache.join(crate::cache::DATABASE_DIR).join("m.db"),
        )],
    )?;
    let own = columns(&connection, "Track", "main")?;
    let theirs = columns(&connection, "Track", "src")?;
    let has_filename = own.iter().any(|name| name == "filename");
    // The columns a new track gets as the open library's track has them.
    let copied: Vec<String> = own
        .iter()
        .filter(|name| theirs.contains(name))
        .filter(|name| !["id", "path", "filename", "albumArtId"].contains(&name.as_str()))
        .cloned()
        .collect();
    let performance_own = columns(&connection, "PerformanceData", "main")?;
    let performance_theirs = columns(&connection, "PerformanceData", "src")?;
    let performance: Vec<String> = performance_own
        .iter()
        .filter(|name| performance_theirs.contains(name) && name.as_str() != "trackId")
        .cloned()
        .collect();

    let transaction = connection.transaction()?;
    let mut covers: HashMap<u32, Option<i64>> = HashMap::new();

    // Whether another track of the other library has `path`.
    let taken = |transaction: &rusqlite::Transaction<'_>, path: &str, except: i64| {
        transaction
            .query_row(
                "SELECT 1 FROM main.Track WHERE path = ?1 AND id <> ?2",
                (path, except),
                |_| Ok(()),
            )
            .is_ok()
    };
    let reference = |track: &crate::library::Track| api::TrackRef {
        title: track.title.clone(),
        artist: track.artist.clone(),
        path: track.relative_path.clone(),
    };

    for (index, track) in source.tracks.iter().enumerate() {
        let destination = destinations.get(&index);
        // Where the track's path should lead.
        let wanted_path: Option<String> = if options.copy_tracks {
            destination.map(|destination| destination.relative.clone())
        } else if options.locations && !track.streaming {
            // The track's path is relative to its own library's folder; the other library
            // needs the way to the same file from its folder.
            Some(crate::save::relative_path(&probe.dir, &track.path))
        } else {
            None
        };

        match matched[index] {
            Some(other_index) => {
                let other = &probe.tracks[other_index];
                let mut fields = Vec::new();

                if options.stars {
                    let new = source.rating(index);
                    if new.unwrap_or(0) != other.rating.unwrap_or(0) {
                        transaction.execute(
                            "UPDATE main.Track SET rating = ?1 WHERE id = ?2",
                            (new, other.db_id),
                        )?;
                        fields.push(api::FieldChange {
                            field: "stars".into(),
                            source_value: stars(other.rating),
                            working_value: stars(new),
                        });
                    }
                }

                let mut copy_it = false;
                if let Some(path) = wanted_path {
                    if taken(&transaction, &path, other.db_id) {
                        report.location_conflicts += 1;
                    } else {
                        copy_it = true;
                        if path != other.relative_path {
                            if has_filename {
                                transaction.execute(
                                    "UPDATE main.Track SET path = ?1, filename = ?2 WHERE id = ?3",
                                    (&path, base_name(&path), other.db_id),
                                )?;
                            } else {
                                transaction.execute(
                                    "UPDATE main.Track SET path = ?1 WHERE id = ?2",
                                    (&path, other.db_id),
                                )?;
                            }
                            fields.push(api::FieldChange {
                                field: "path".into(),
                                source_value: other.relative_path.clone(),
                                working_value: path,
                            });
                        }
                    }
                }
                if !fields.is_empty() {
                    report.changed.push(api::TrackChange {
                        track: Some(reference(track)),
                        fields,
                    });
                }
                if let (true, Some(destination)) = (copy_it, destination) {
                    place(destination, track, copy_files, progress, &mut report)?;
                    if options.copy_stems {
                        place_stems(source, probe, track, copy_files, progress, &mut report)?;
                    }
                }
            }
            None => {
                // Only a track whose file will be there can be added.
                let (true, Some(destination)) = (options.copy_tracks, destination) else {
                    continue;
                };
                if taken(&transaction, &destination.relative, 0) {
                    report.location_conflicts += 1;
                    continue;
                }
                let art = match track.album_art {
                    Some(art) => match covers.get(&art) {
                        Some(known) => *known,
                        None => {
                            let id = copy_cover(&transaction, art)?;
                            // A cover that is a file comes along, small as it is.
                            if let Some(name) = cover_file(&transaction, art)? {
                                place_extra(
                                    &source.dir.join(ARTWORK_DIR).join(&name),
                                    &probe.dir.join(ARTWORK_DIR).join(&name),
                                    copy_files,
                                    progress,
                                    &mut report,
                                )?;
                            }
                            covers.insert(art, id);
                            id
                        }
                    },
                    None => None,
                };

                let mut names = copied.clone();
                let mut values = copied.clone();
                names.extend(["path".into(), "albumArtId".into()]);
                values.extend(["?1".into(), "?2".into()]);
                if has_filename {
                    names.push("filename".into());
                    values.push("?3".into());
                }
                let sql = format!(
                    "INSERT INTO main.Track ({}) SELECT {} FROM src.Track WHERE id = ?4",
                    names.join(", "),
                    values.join(", ")
                );
                transaction.execute(
                    &sql,
                    (
                        &destination.relative,
                        art,
                        base_name(&destination.relative),
                        track.db_id,
                    ),
                )?;
                let new_id = transaction.last_insert_rowid();
                if !performance.is_empty() {
                    let assignments: Vec<String> = performance
                        .iter()
                        .map(|name| {
                            format!(
                                "{name} = (SELECT {name} FROM src.PerformanceData WHERE trackId = ?1)"
                            )
                        })
                        .collect();
                    transaction.execute(
                        &format!(
                            "UPDATE main.PerformanceData SET {} WHERE trackId = ?2",
                            assignments.join(", ")
                        ),
                        (track.db_id, new_id),
                    )?;
                }
                report.added.push(index);
                place(destination, track, copy_files, progress, &mut report)?;
                if options.copy_stems {
                    place_stems(source, probe, track, copy_files, progress, &mut report)?;
                }
            }
        }
    }
    transaction.commit()?;
    Ok(report)
}

/// Notes that the stems of `track` are to go to the other library, and copies them.
fn place_stems(
    source: &Library,
    probe: &Library,
    track: &crate::library::Track,
    copy_files: bool,
    progress: &Progress,
    report: &mut Report,
) -> Result<(), MirrorError> {
    let Some(name) = stems_name(track) else {
        return Ok(());
    };
    place_extra(
        &source.dir.join(STEMS_DIR).join(&name),
        &probe.dir.join(STEMS_DIR).join(&name),
        copy_files,
        progress,
        report,
    )
}

/// Notes that the file of `track` is to go to `destination`, and copies it if it is not
/// there and `copy_files` says to.
fn place(
    destination: &Destination,
    track: &crate::library::Track,
    copy_files: bool,
    progress: &Progress,
    report: &mut Report,
) -> Result<(), MirrorError> {
    if destination.present {
        return Ok(());
    }
    report.files_to_copy += 1;
    report.bytes_to_copy += destination.size;
    if copy_files {
        copy_file(&track.path, &destination.absolute, progress)?;
        report.copied.push(destination.absolute.clone());
    }
    Ok(())
}

/// The name of the file in `Artwork` that the cover `art` of the open library is, if it is
/// kept there (a hash and no image in the row) and not in the database.
fn cover_file(
    transaction: &rusqlite::Transaction<'_>,
    art: u32,
) -> rusqlite::Result<Option<String>> {
    use rusqlite::OptionalExtension;
    use rusqlite::types::Value;
    let row: Option<(Value, Value)> = transaction
        .query_row(
            "SELECT hash, albumArt FROM src.AlbumArt WHERE id = ?1",
            [art],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    Ok(match row {
        Some((Value::Blob(hash), Value::Null)) => Some(format!("{}.jpg", base64_url(&hash))),
        _ => None,
    })
}

/// Gives the other library the cover `art` of the open library has, unless it has the
/// same one (by its hash). Returns the cover's id there.
fn copy_cover(transaction: &rusqlite::Transaction<'_>, art: u32) -> rusqlite::Result<Option<i64>> {
    use rusqlite::OptionalExtension;
    let hash: Option<rusqlite::types::Value> = transaction
        .query_row(
            "SELECT hash FROM src.AlbumArt WHERE id = ?1",
            [art],
            |row| row.get(0),
        )
        .optional()?;
    let Some(hash) = hash else {
        return Ok(None);
    };
    let existing: Option<i64> = transaction
        .query_row(
            "SELECT id FROM main.AlbumArt WHERE hash IS ?1 LIMIT 1",
            [&hash],
            |row| row.get(0),
        )
        .optional()?;
    if existing.is_some() {
        return Ok(existing);
    }
    transaction.execute(
        "INSERT INTO main.AlbumArt (hash, albumArt) SELECT hash, albumArt FROM src.AlbumArt WHERE id = ?1",
        [art],
    )?;
    Ok(Some(transaction.last_insert_rowid()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exfat_forbidden_characters_are_replaced() {
        assert_eq!(exfat_name("AC/DC"), "AC_DC");
        assert_eq!(exfat_name(r#"a\b:c*d?e"f<g>h|i"#), "a_b_c_d_e_f_g_h_i");
        assert_eq!(exfat_name("tab\there\u{1}"), "tab_here_");
        assert_eq!(exfat_name("Who? What!"), "Who_ What!");
        assert_eq!(exfat_name("Ünïcode – ok 音楽"), "Ünïcode – ok 音楽");
    }

    #[test]
    fn names_that_exfat_cannot_hold_are_made_usable() {
        assert_eq!(exfat_name("name."), "name_");
        assert_eq!(exfat_name("name "), "name_");
        assert_eq!(exfat_name(""), "");
        assert_eq!(exfat_name("."), "_");
        assert_eq!(exfat_name(".."), "__");
        assert_eq!(exfat_name("   "), "___");
        let long = "x".repeat(400);
        assert_eq!(exfat_name(&long).chars().count(), 255);
        // 音 is one UTF-16 unit, 😀 two: the limit is in units.
        let emoji = "😀".repeat(200);
        assert_eq!(exfat_name(&emoji).encode_utf16().count(), 254);
    }

    #[test]
    fn a_long_file_name_keeps_its_extension() {
        let name = format!("{}.flac", "y".repeat(400));
        let cleaned = exfat_file_name(&name);
        assert_eq!(cleaned.chars().count(), 255);
        assert!(cleaned.ends_with("y.flac"));
        assert_eq!(exfat_file_name("01 What?.mp3"), "01 What_.mp3");
        assert_eq!(exfat_file_name("no extension"), "no extension");
        assert_eq!(exfat_file_name("a.b.c: d.mp3"), "a.b.c_ d.mp3");
        assert_eq!(exfat_file_name(".hidden"), ".hidden");
        assert_eq!(numbered("song", 2, 255), "song (2)");
        assert_eq!(numbered(&"z".repeat(300), 12, 20).chars().count(), 20);
    }
}
