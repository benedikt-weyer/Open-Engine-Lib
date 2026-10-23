//! The folders of music the user added to the "Local music" section, remembered between
//! runs.
//!
//! This is state, not configuration, so it lives in its own file in the user state dir
//! (`<state dir>/open-engine-lib/local-folders.toml`).

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

const APP_DIR: &str = "open-engine-lib";
const FILE: &str = "local-folders.toml";

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(default)]
struct Saved {
    folders: Vec<PathBuf>,
}

/// The folders, in the order they were added.
#[derive(Debug)]
pub struct LocalFolders {
    folders: Vec<PathBuf>,
    file: PathBuf,
}

impl LocalFolders {
    pub fn discover() -> Self {
        let file = dirs::state_dir()
            .or_else(dirs::data_local_dir)
            .unwrap_or_default()
            .join(APP_DIR)
            .join(FILE);
        Self::load(file)
    }

    /// Reads the list from `file`. A missing or unreadable file is an empty list: it is
    /// only a convenience, so it must never stop the app from starting.
    pub fn load(file: PathBuf) -> Self {
        let folders = fs::read_to_string(&file)
            .ok()
            .and_then(|text| toml::from_str::<Saved>(&text).ok())
            .map(|saved| saved.folders)
            .unwrap_or_default();
        Self { folders, file }
    }

    pub fn paths(&self) -> &[PathBuf] {
        &self.folders
    }

    pub fn contains(&self, path: &Path) -> bool {
        self.folders.iter().any(|folder| folder == path)
    }

    /// Adds `path` at the end of the list if it is not in it, and saves.
    pub fn add(&mut self, path: PathBuf) -> io::Result<()> {
        if self.contains(&path) {
            return Ok(());
        }
        self.folders.push(path);
        self.save()
    }

    pub fn remove(&mut self, path: &Path) -> io::Result<()> {
        self.folders.retain(|folder| folder != path);
        self.save()
    }

    fn save(&self) -> io::Result<()> {
        if let Some(parent) = self.file.parent() {
            fs::create_dir_all(parent)?;
        }
        let text = toml::to_string(&Saved {
            folders: self.folders.clone(),
        })
        .map_err(io::Error::other)?;

        // Write beside the file and move it into place, so a crash can't leave half a list.
        let temporary = self.file.with_extension("toml.tmp");
        fs::write(&temporary, text)?;
        fs::rename(&temporary, &self.file)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TempFile(PathBuf);

    impl TempFile {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "open-engine-lib-local-folders-{name}-{}",
                std::process::id()
            ));
            let _ = fs::remove_dir_all(&dir);
            Self(dir.join("state").join(FILE))
        }
    }

    impl Drop for TempFile {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(self.0.parent().unwrap().parent().unwrap());
        }
    }

    #[test]
    fn folders_are_kept_in_the_order_they_were_added_without_duplicates() {
        let file = TempFile::new("add");
        let mut folders = LocalFolders::load(file.0.clone());
        assert!(folders.paths().is_empty());

        folders.add("/music/b".into()).unwrap();
        folders.add("/music/a".into()).unwrap();
        folders.add("/music/b".into()).unwrap();
        assert_eq!(
            folders.paths(),
            [PathBuf::from("/music/b"), PathBuf::from("/music/a")]
        );
        assert!(folders.contains(Path::new("/music/a")));
        assert!(!folders.contains(Path::new("/music/c")));

        // And they are there after a restart.
        let again = LocalFolders::load(file.0.clone());
        assert_eq!(again.paths(), folders.paths());
    }

    #[test]
    fn a_folder_can_be_removed() {
        let file = TempFile::new("remove");
        let mut folders = LocalFolders::load(file.0.clone());
        folders.add("/music/a".into()).unwrap();
        folders.add("/music/b".into()).unwrap();
        folders.remove(Path::new("/music/a")).unwrap();
        folders.remove(Path::new("/music/never")).unwrap();
        assert_eq!(
            LocalFolders::load(file.0.clone()).paths(),
            [PathBuf::from("/music/b")]
        );
    }

    #[test]
    fn a_broken_file_is_an_empty_list() {
        let file = TempFile::new("broken");
        fs::create_dir_all(file.0.parent().unwrap()).unwrap();
        fs::write(&file.0, "folders = 12 [").unwrap();
        assert!(LocalFolders::load(file.0.clone()).paths().is_empty());
    }
}
