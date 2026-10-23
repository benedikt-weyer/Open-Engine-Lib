//! The working copy of a library's databases.
//!
//! The backend never opens the databases in the library folder itself, which may be
//! on a removable drive and may be in use by Engine DJ or a player. It copies them
//! into a cache folder and reads from the copy. The audio files (and artwork, stems
//! and overview data) are far too big to copy and stay where they are.
//!
//! The copy is reused as long as the source databases keep the same size and
//! modification time, so reopening an unchanged library is instant. A file whose
//! modification time differs but whose content is what was copied counts as unchanged
//! too: the time a file system reports is not always what it was set to (network and
//! FUSE mounts such as rclone's show the old one for a while after a file was replaced).

use std::fs::{self, File};
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

/// The folder inside an Engine library that holds the databases.
pub const DATABASE_DIR: &str = "Database2";
/// Lists the files the copy was made from; written last, so a copy that was cut
/// short is never mistaken for a complete one.
const MANIFEST: &str = "manifest";
/// Present while the working copy holds edits that were not written to the source yet.
pub const EDITED: &str = "edited";
/// Where a working copy with such edits goes when the source changed and it is replaced.
pub const UNSYNCED: &str = "unsynced-edits";

/// Where cached libraries live: `$OPEN_ENGINE_LIB_CACHE`, else the user cache dir.
pub fn default_root() -> PathBuf {
    std::env::var_os("OPEN_ENGINE_LIB_CACHE")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            dirs::cache_dir()
                .unwrap_or_else(std::env::temp_dir)
                .join("open-engine-lib")
                .join("libraries")
        })
}

/// Makes sure the cache under `cache_root` holds an up to date copy of the databases
/// of the library at `source`, and returns the cache's library folder (the one that
/// contains `Database2`).
pub fn sync(source: &Path, cache_root: &Path) -> io::Result<PathBuf> {
    let cache = cache_root.join(cache_name(source));
    let mut files = database_files(&source.join(DATABASE_DIR))?;

    let up_to_date = files
        .iter()
        .all(|file| cache.join(DATABASE_DIR).join(&file.name).is_file())
        && compare(&source.join(DATABASE_DIR), &cache, &files).is_ok_and(|found| found.unchanged());
    if up_to_date {
        return Ok(cache);
    }

    // Start over: drop the manifest first, then every old file, so nothing stale
    // survives (e.g. a journal the source no longer has).
    match fs::remove_file(cache.join(MANIFEST)) {
        Err(error) if error.kind() != io::ErrorKind::NotFound => return Err(error),
        _ => {}
    }
    if cache.join(EDITED).exists() {
        keep_unsynced_edits(&cache)?;
    }
    match fs::remove_dir_all(cache.join(DATABASE_DIR)) {
        Err(error) if error.kind() != io::ErrorKind::NotFound => return Err(error),
        _ => {}
    }
    fs::create_dir_all(cache.join(DATABASE_DIR))?;
    for file in &files {
        fs::copy(
            source.join(DATABASE_DIR).join(&file.name),
            cache.join(DATABASE_DIR).join(&file.name),
        )?;
    }
    // (Of the copy, which is what was read from the source.)
    for file in &mut files {
        file.checksum = Some(checksum(&cache.join(DATABASE_DIR).join(&file.name))?);
    }
    fs::write(cache.join(MANIFEST), render_manifest(&files))?;
    Ok(cache)
}

/// Moves the working copy's databases, which hold edits that never reached the source, to
/// `unsynced-edits` in the cache before they are replaced. (Only the latest is kept.)
pub fn keep_unsynced_edits(cache: &Path) -> io::Result<()> {
    let kept = cache.join(UNSYNCED);
    match fs::remove_dir_all(&kept) {
        Err(error) if error.kind() != io::ErrorKind::NotFound => return Err(error),
        _ => {}
    }
    fs::create_dir_all(&kept)?;
    match fs::rename(cache.join(DATABASE_DIR), kept.join(DATABASE_DIR)) {
        Err(error) if error.kind() != io::ErrorKind::NotFound => return Err(error),
        _ => {}
    }
    fs::remove_file(cache.join(EDITED))
}

/// Notes that the working copy in `cache` was changed and the source does not have the
/// change, so that opening the library again can't throw the change away unnoticed.
pub fn mark_edited(cache: &Path) -> io::Result<()> {
    fs::write(cache.join(EDITED), "")
}

/// Whether the databases at `source` are no longer as they were when the working copy in
/// `cache` (a folder `sync` returned) was made from them: something else changed them.
pub fn source_changed(source: &Path, cache: &Path) -> io::Result<bool> {
    let files = database_files(&source.join(DATABASE_DIR))?;
    Ok(!compare(&source.join(DATABASE_DIR), cache, &files).is_ok_and(|found| found.unchanged()))
}

/// Notes that the working copy in `cache` matches the databases at `source` as they are
/// now, e.g. after the working copy was written to the source.
pub fn record_source(source: &Path, cache: &Path) -> io::Result<()> {
    let mut files = database_files(&source.join(DATABASE_DIR))?;
    for file in &mut files {
        // The working copy is the source's content now, and is read faster than the source.
        let copy = cache.join(DATABASE_DIR).join(&file.name);
        let path = if copy.is_file() {
            copy
        } else {
            source.join(DATABASE_DIR).join(&file.name)
        };
        file.checksum = Some(checksum(&path)?);
    }
    fs::write(cache.join(MANIFEST), render_manifest(&files))?;
    match fs::remove_file(cache.join(EDITED)) {
        Err(error) if error.kind() != io::ErrorKind::NotFound => Err(error),
        _ => Ok(()),
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct SourceFile {
    pub name: String,
    size: u64,
    /// Nanoseconds since the Unix epoch.
    modified: u128,
    /// The CRC-32 of the content, once it is known (it is only worked out for what is
    /// recorded, and a manifest from an older version does not have it).
    checksum: Option<u32>,
}

/// The database files to copy: `*.db`, plus the journal/WAL files SQLite needs to
/// recover a database that was in the middle of a write. Empty side files carry
/// nothing and are skipped.
pub fn database_files(dir: &Path) -> io::Result<Vec<SourceFile>> {
    let mut files = Vec::new();
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let metadata = entry.metadata()?;
        let Ok(name) = entry.file_name().into_string() else {
            continue;
        };
        let is_database = name.ends_with(".db");
        let is_side_file = [".db-journal", ".db-wal", ".db-shm"]
            .iter()
            .any(|suffix| name.ends_with(suffix));
        if !metadata.is_file() || !(is_database || (is_side_file && metadata.len() > 0)) {
            continue;
        }
        files.push(SourceFile {
            name,
            size: metadata.len(),
            modified: metadata
                .modified()?
                .duration_since(UNIX_EPOCH)
                .map_or(0, |since| since.as_nanos()),
            checksum: None,
        });
    }
    files.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(files)
}

fn render_manifest(files: &[SourceFile]) -> String {
    files
        .iter()
        .map(|file| match file.checksum {
            Some(checksum) => format!(
                "{}\t{}\t{}\t{checksum:08x}\n",
                file.name, file.size, file.modified
            ),
            None => format!("{}\t{}\t{}\n", file.name, file.size, file.modified),
        })
        .collect()
}

/// What `render_manifest` wrote, or `None` if it is not that.
fn parse_manifest(text: &str) -> Option<Vec<SourceFile>> {
    text.lines()
        .map(|line| {
            let mut parts = line.split('\t');
            Some(SourceFile {
                name: parts.next()?.to_string(),
                size: parts.next()?.parse().ok()?,
                modified: parts.next()?.parse().ok()?,
                checksum: match parts.next() {
                    Some(checksum) => Some(u32::from_str_radix(checksum, 16).ok()?),
                    None => None,
                },
            })
        })
        .collect()
}

/// The CRC-32 of a file's content.
fn checksum(path: &Path) -> io::Result<u32> {
    let mut file = File::open(path)?;
    let mut hasher = crc32fast::Hasher::new();
    let mut buffer = vec![0u8; 1 << 20];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            return Ok(hasher.finalize());
        }
        hasher.update(&buffer[..read]);
    }
}

/// How the databases at `dir` (listed as `files`) are compared with what the working copy
/// in `cache` was made from.
enum Comparison {
    /// The same files with the same sizes and times.
    Same,
    /// The same files with the same sizes and content, but some modification time is not
    /// what was recorded. The manifest has been brought up to date.
    SameContent,
    Different,
}

impl Comparison {
    fn unchanged(&self) -> bool {
        !matches!(self, Comparison::Different)
    }
}

fn compare(dir: &Path, cache: &Path, files: &[SourceFile]) -> io::Result<Comparison> {
    let Some(recorded) = parse_manifest(&fs::read_to_string(cache.join(MANIFEST))?) else {
        return Ok(Comparison::Different);
    };
    let same_files = recorded.len() == files.len()
        && recorded
            .iter()
            .zip(files)
            .all(|(before, now)| before.name == now.name && before.size == now.size);
    if !same_files {
        return Ok(Comparison::Different);
    }
    // The content is only read for a file whose time differs, and only if what was
    // copied is known: the time alone says too little where it can't be trusted.
    let mut touched = false;
    let mut now_recorded = Vec::new();
    for (before, now) in recorded.iter().zip(files) {
        if before.modified != now.modified {
            match before.checksum {
                Some(known) if checksum(&dir.join(&now.name))? == known => touched = true,
                _ => return Ok(Comparison::Different),
            }
        }
        now_recorded.push(SourceFile {
            name: now.name.clone(),
            size: now.size,
            modified: now.modified,
            checksum: before.checksum,
        });
    }
    if !touched {
        return Ok(Comparison::Same);
    }
    // So that the content is not read again at the next look. (Failing to say so only
    // means that it is.)
    let _ = fs::write(cache.join(MANIFEST), render_manifest(&now_recorded));
    Ok(Comparison::SameContent)
}

/// A folder name for the cache of the library at `source`: its name for humans and
/// a hash of its full path so that two libraries with the same name don't collide.
fn cache_name(source: &Path) -> String {
    let full = fs::canonicalize(source).unwrap_or_else(|_| source.to_path_buf());
    let name: String = full
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();

    // FNV-1a, because the name has to be the same in every run and version.
    let hash = full
        .to_string_lossy()
        .bytes()
        .fold(0xcbf29ce484222325u64, |hash, byte| {
            (hash ^ u64::from(byte)).wrapping_mul(0x100000001b3)
        });
    format!("{name}-{hash:016x}")
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(name: &str) -> Self {
            let path =
                std::env::temp_dir().join(format!("open-engine-lib-{name}-{}", std::process::id()));
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

    fn source_library(root: &Path) -> PathBuf {
        let library = root.join("Engine Library");
        fs::create_dir_all(library.join(DATABASE_DIR)).unwrap();
        fs::write(library.join("Database2/m.db"), b"main").unwrap();
        fs::write(library.join("Database2/hm.db"), b"history").unwrap();
        fs::write(library.join("Database2/m.db-journal"), b"").unwrap();
        fs::write(library.join("Database2/notes.txt"), b"not a database").unwrap();
        // Big data next to the databases must not be copied.
        fs::create_dir_all(library.join("Stems")).unwrap();
        fs::write(library.join("Stems/1.stems"), b"huge").unwrap();
        library
    }

    #[test]
    fn copies_only_the_databases() {
        let temp = TempDir::new("copy");
        let source = source_library(&temp.0);
        let cache = sync(&source, &temp.0.join("cache")).unwrap();

        assert_eq!(fs::read(cache.join("Database2/m.db")).unwrap(), b"main");
        assert_eq!(fs::read(cache.join("Database2/hm.db")).unwrap(), b"history");
        assert!(
            !cache.join("Database2/m.db-journal").exists(),
            "empty journal"
        );
        assert!(!cache.join("Database2/notes.txt").exists());
        assert!(!cache.join("Stems").exists());
        // The source is untouched.
        assert_eq!(fs::read(source.join("Database2/m.db")).unwrap(), b"main");
        assert!(source.join("Stems/1.stems").exists());
    }

    #[test]
    fn keeps_a_journal_that_has_content() {
        let temp = TempDir::new("journal");
        let source = source_library(&temp.0);
        fs::write(source.join("Database2/m.db-journal"), b"hot").unwrap();

        let cache = sync(&source, &temp.0.join("cache")).unwrap();
        assert_eq!(
            fs::read(cache.join("Database2/m.db-journal")).unwrap(),
            b"hot"
        );
    }

    #[test]
    fn reuses_an_unchanged_copy_and_refreshes_a_changed_one() {
        let temp = TempDir::new("reuse");
        let source = source_library(&temp.0);
        let root = temp.0.join("cache");
        let cache = sync(&source, &root).unwrap();

        // Marking the copy shows whether the next sync leaves it alone.
        fs::write(cache.join("Database2/m.db"), b"marked").unwrap();
        assert_eq!(sync(&source, &root).unwrap(), cache);
        assert_eq!(fs::read(cache.join("Database2/m.db")).unwrap(), b"marked");

        // A changed source database (here: another size) is copied again, and a
        // file the source no longer has disappears from the cache.
        fs::write(source.join("Database2/m.db"), b"main, edited").unwrap();
        fs::remove_file(source.join("Database2/hm.db")).unwrap();
        sync(&source, &root).unwrap();
        assert_eq!(
            fs::read(cache.join("Database2/m.db")).unwrap(),
            b"main, edited"
        );
        assert!(!cache.join("Database2/hm.db").exists());
    }

    #[test]
    fn notices_when_the_source_is_changed_by_someone_else() {
        let temp = TempDir::new("outside");
        let source = source_library(&temp.0);
        let cache = sync(&source, &temp.0.join("cache")).unwrap();
        assert!(!source_changed(&source, &cache).unwrap());

        // Another program changes a database (a different size is enough to tell).
        fs::write(source.join("Database2/m.db"), b"main, edited elsewhere").unwrap();
        assert!(source_changed(&source, &cache).unwrap());

        // After the working copy has been written to the source, they match again.
        record_source(&source, &cache).unwrap();
        assert!(!source_changed(&source, &cache).unwrap());
    }

    /// Sets the modification time of `path` to `seconds` after the epoch.
    fn set_modified(path: &Path, seconds: u64) {
        fs::OpenOptions::new()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(UNIX_EPOCH + std::time::Duration::from_secs(seconds))
            .unwrap();
    }

    #[test]
    fn a_time_that_is_not_to_be_trusted_does_not_make_the_source_look_changed() {
        let temp = TempDir::new("touched");
        let source = source_library(&temp.0);
        let root = temp.0.join("cache");
        let cache = sync(&source, &root).unwrap();
        let recorded = fs::read_to_string(cache.join(MANIFEST)).unwrap();
        assert!(recorded.lines().all(|line| line.split('\t').count() == 4));

        // The file system says another time for a file that is the same (as a mounted
        // network share can after a file was replaced): the copy is still the source.
        set_modified(&source.join("Database2/m.db"), 1_000_000);
        assert!(!source_changed(&source, &cache).unwrap());
        // That was noted, so the next look does not read the file again.
        assert_ne!(fs::read_to_string(cache.join(MANIFEST)).unwrap(), recorded);
        assert!(!source_changed(&source, &cache).unwrap());

        // Opening the library keeps the copy, with what was done to it.
        fs::write(cache.join("Database2/m.db"), b"marked").unwrap();
        set_modified(&source.join("Database2/m.db"), 2_000_000);
        assert_eq!(sync(&source, &root).unwrap(), cache);
        assert_eq!(fs::read(cache.join("Database2/m.db")).unwrap(), b"marked");

        // But another time with other content (here the size is the same) is a change.
        fs::write(source.join("Database2/m.db"), b"MAIN").unwrap();
        set_modified(&source.join("Database2/m.db"), 3_000_000);
        assert_eq!(
            fs::metadata(source.join("Database2/m.db")).unwrap().len(),
            4
        );
        assert!(source_changed(&source, &cache).unwrap());
    }

    #[test]
    fn a_manifest_without_checksums_trusts_the_time_alone() {
        let temp = TempDir::new("old-manifest");
        let source = source_library(&temp.0);
        let cache = sync(&source, &temp.0.join("cache")).unwrap();
        // What earlier versions wrote.
        let old: String = fs::read_to_string(cache.join(MANIFEST))
            .unwrap()
            .lines()
            .map(|line| format!("{}\n", line.rsplit_once('\t').unwrap().0))
            .collect();
        fs::write(cache.join(MANIFEST), old).unwrap();
        assert!(!source_changed(&source, &cache).unwrap());

        set_modified(&source.join("Database2/m.db"), 1_000_000);
        assert!(source_changed(&source, &cache).unwrap());
        // Recording the source brings in the checksums.
        record_source(&source, &cache).unwrap();
        set_modified(&source.join("Database2/m.db"), 2_000_000);
        assert!(!source_changed(&source, &cache).unwrap());
    }

    #[test]
    fn edits_not_yet_written_to_the_source_are_kept_when_the_source_changes() {
        let temp = TempDir::new("edited");
        let source = source_library(&temp.0);
        let root = temp.0.join("cache");
        let cache = sync(&source, &root).unwrap();

        // Edit the working copy. As long as the source stays as it is, it is reused.
        fs::write(cache.join("Database2/m.db"), b"main, rated").unwrap();
        mark_edited(&cache).unwrap();
        sync(&source, &root).unwrap();
        assert_eq!(
            fs::read(cache.join("Database2/m.db")).unwrap(),
            b"main, rated"
        );

        // Once the source changes it is replaced, but what was edited is not thrown away.
        fs::write(source.join("Database2/m.db"), b"main, edited elsewhere").unwrap();
        sync(&source, &root).unwrap();
        assert_eq!(
            fs::read(cache.join("Database2/m.db")).unwrap(),
            b"main, edited elsewhere"
        );
        assert_eq!(
            fs::read(cache.join("unsynced-edits/Database2/m.db")).unwrap(),
            b"main, rated"
        );
        assert!(!cache.join(EDITED).exists());

        // An unedited copy that is replaced leaves nothing behind, and writing the working
        // copy to the source clears the mark.
        fs::remove_dir_all(cache.join(UNSYNCED)).unwrap();
        fs::write(source.join("Database2/m.db"), b"changed again").unwrap();
        sync(&source, &root).unwrap();
        assert!(!cache.join(UNSYNCED).exists());
        mark_edited(&cache).unwrap();
        record_source(&source, &cache).unwrap();
        assert!(!cache.join(EDITED).exists());
    }

    #[test]
    fn libraries_with_the_same_name_get_different_caches() {
        let temp = TempDir::new("names");
        let first = source_library(&temp.0.join("a"));
        let second = source_library(&temp.0.join("b"));
        assert_ne!(cache_name(&first), cache_name(&second));
        assert_eq!(cache_name(&first), cache_name(&first));
        assert!(cache_name(&first).starts_with("Engine_Library-"));
    }

    #[test]
    fn a_missing_database_folder_is_an_error() {
        let temp = TempDir::new("missing");
        assert!(sync(&temp.0.join("nowhere"), &temp.0.join("cache")).is_err());
    }
}
