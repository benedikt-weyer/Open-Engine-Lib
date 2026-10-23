//! "Repair library": puts a library as earlier versions left it into the shape this version
//! keeps it in. Earlier versions wrote two things into the `Database2` folder of the library,
//! next to Engine's own databases, which a player may take for damage:
//!
//! - `open-engine-lib-backup`, a folder with the databases a sync replaced. This version keeps
//!   its backups in the cache folder of the computer, so the folder is taken away.
//! - `open-engine-lib.db`, the saved origins of tracks. This version keeps them in an
//!   `open-engine` folder beside the `Engine Library` folder (see `origins`), so the file is
//!   moved there.
//!
//! Nothing else in the library is touched, and no database is opened or changed.

use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::cache::DATABASE_DIR;
use crate::library;
use crate::origins;
use crate::sync::same_content;

/// The folder of backups that earlier versions made inside `Database2`.
pub const OLD_BACKUP_DIR: &str = "open-engine-lib-backup";

#[derive(Debug)]
pub enum RepairError {
    /// There is no Engine library at or just below the path.
    NotALibrary(PathBuf),
    Io(io::Error),
}

impl fmt::Display for RepairError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RepairError::NotALibrary(path) => write!(
                f,
                "{} does not contain an Engine library (Database2/m.db)",
                path.display()
            ),
            RepairError::Io(error) => write!(f, "{error}"),
        }
    }
}

impl From<io::Error> for RepairError {
    fn from(error: io::Error) -> Self {
        RepairError::Io(error)
    }
}

/// What `repair` did.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Repaired {
    /// The library's `Engine Library` folder.
    pub library: PathBuf,
    /// The origins were moved out of `Database2`.
    pub origins_moved: bool,
    /// The origins in `Database2` were left where they are, because the `open-engine` folder
    /// has others that differ: neither can be told to be the right ones.
    pub origins_conflict: bool,
    /// The folder of old backups was taken away.
    pub backup_removed: bool,
}

/// Repairs the library at `path` (its `Engine Library` folder, the folder that contains it, or
/// its `Database2` folder).
pub fn repair(path: &Path) -> Result<Repaired, RepairError> {
    let library = library::locate(path).ok_or_else(|| RepairError::NotALibrary(path.into()))?;
    let mut repaired = Repaired {
        library: library.clone(),
        ..Repaired::default()
    };

    let (old, new) = (
        origins::old_source_file(&library),
        origins::source_file(&library),
    );
    if old.is_file() {
        if !new.is_file() {
            move_file(&old, &new)?;
            repaired.origins_moved = true;
        } else if same_content(&old, &new) {
            // Already there; this is only a second copy.
            fs::remove_file(&old)?;
            repaired.origins_moved = true;
        } else {
            repaired.origins_conflict = true;
        }
    }

    let backup = library.join(DATABASE_DIR).join(OLD_BACKUP_DIR);
    if backup.is_dir() {
        fs::remove_dir_all(&backup)?;
        repaired.backup_removed = true;
    }
    Ok(repaired)
}

/// Moves `from` to `to` (making the folder it goes in), by renaming it, or by copying and
/// taking away if that is not possible. A file is never left half written at `to`.
fn move_file(from: &Path, to: &Path) -> io::Result<()> {
    if let Some(folder) = to.parent() {
        fs::create_dir_all(folder)?;
    }
    if fs::rename(from, to).is_ok() {
        return Ok(());
    }
    let temporary = to.with_file_name(format!(".{}.moving", origins::FILE));
    let result = fs::copy(from, &temporary).and_then(|_| fs::rename(&temporary, to));
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
        return result;
    }
    fs::remove_file(from)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(name: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "open-engine-lib-repair-{name}-{}",
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

    /// A drive as earlier versions left it: a library with the origins and a backup folder in
    /// `Database2`.
    fn old_drive(root: &Path) -> PathBuf {
        let library = root.join("Engine Library");
        let databases = library.join(DATABASE_DIR);
        fs::create_dir_all(databases.join(OLD_BACKUP_DIR)).unwrap();
        fs::write(databases.join("m.db"), b"main").unwrap();
        fs::write(databases.join(origins::FILE), b"origins").unwrap();
        fs::write(databases.join(OLD_BACKUP_DIR).join("m.db"), b"older").unwrap();
        fs::write(root.join("song.flac"), b"audio").unwrap();
        library
    }

    #[test]
    fn the_origins_are_moved_beside_the_library_and_the_backups_are_taken_away() {
        let temp = TempDir::new("old");
        let library = old_drive(&temp.0);

        let done = repair(&temp.0).unwrap();
        assert_eq!(
            done,
            Repaired {
                library: library.clone(),
                origins_moved: true,
                origins_conflict: false,
                backup_removed: true,
            }
        );
        let databases = library.join(DATABASE_DIR);
        assert_eq!(
            fs::read(temp.0.join("open-engine").join(origins::FILE)).unwrap(),
            b"origins"
        );
        assert!(!databases.join(origins::FILE).exists());
        assert!(!databases.join(OLD_BACKUP_DIR).exists());
        // Nothing else is touched.
        assert_eq!(fs::read(databases.join("m.db")).unwrap(), b"main");
        assert_eq!(fs::read(temp.0.join("song.flac")).unwrap(), b"audio");

        // A library that is in order is left alone.
        assert_eq!(
            repair(&library).unwrap(),
            Repaired {
                library,
                ..Repaired::default()
            }
        );
    }

    #[test]
    fn origins_that_are_at_both_places_are_only_taken_away_when_the_same() {
        let temp = TempDir::new("both");
        let library = old_drive(&temp.0);
        let new = temp.0.join("open-engine").join(origins::FILE);
        fs::create_dir_all(new.parent().unwrap()).unwrap();
        fs::write(&new, b"other origins").unwrap();

        let done = repair(&library).unwrap();
        assert!(done.origins_conflict && !done.origins_moved && done.backup_removed);
        assert_eq!(fs::read(&new).unwrap(), b"other origins");
        assert!(library.join(DATABASE_DIR).join(origins::FILE).is_file());

        fs::write(&new, b"origins").unwrap();
        let done = repair(&library).unwrap();
        assert!(done.origins_moved && !done.origins_conflict);
        assert!(!library.join(DATABASE_DIR).join(origins::FILE).exists());
    }

    #[test]
    fn something_that_is_not_a_library_is_refused() {
        let temp = TempDir::new("none");
        assert!(matches!(repair(&temp.0), Err(RepairError::NotALibrary(_))));
    }
}
