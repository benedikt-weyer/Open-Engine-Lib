//! The libraries opened recently, remembered between runs.
//!
//! This is state, not configuration, so it lives in its own file in the user state
//! dir (`<state dir>/open-engine-lib/recent-libraries.toml`) instead of in the
//! layered config, which can be read-only.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

const APP_DIR: &str = "open-engine-lib";
const FILE: &str = "recent-libraries.toml";
/// The list keeps this many libraries; opening another drops the oldest.
const MAX_ENTRIES: usize = 10;
/// The folder every Engine library is called, which says nothing about which one it is.
const LIBRARY_DIR: &str = "Engine Library";

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(default)]
struct Saved {
    libraries: Vec<PathBuf>,
}

/// The recently opened libraries, most recent first.
#[derive(Debug)]
pub struct Recents {
    libraries: Vec<PathBuf>,
    file: PathBuf,
}

impl Recents {
    pub fn discover() -> Self {
        let file = dirs::state_dir()
            .or_else(dirs::data_local_dir)
            .unwrap_or_default()
            .join(APP_DIR)
            .join(FILE);
        Self::load(file)
    }

    /// Reads the list from `file`. A missing or unreadable file is an empty list: it
    /// is only a convenience, so it must never stop the app from starting.
    pub fn load(file: PathBuf) -> Self {
        let libraries = fs::read_to_string(&file)
            .ok()
            .and_then(|text| toml::from_str::<Saved>(&text).ok())
            .map(|saved| saved.libraries)
            .unwrap_or_default();
        Self { libraries, file }
    }

    pub fn paths(&self) -> &[PathBuf] {
        &self.libraries
    }

    /// Puts `path` at the top of the list, without duplicates, and saves.
    pub fn add(&mut self, path: PathBuf) -> io::Result<()> {
        self.libraries.retain(|existing| *existing != path);
        self.libraries.insert(0, path);
        self.libraries.truncate(MAX_ENTRIES);
        self.save()
    }

    pub fn forget(&mut self, path: &Path) -> io::Result<()> {
        self.libraries.retain(|existing| existing != path);
        self.save()
    }

    fn save(&self) -> io::Result<()> {
        if let Some(parent) = self.file.parent() {
            fs::create_dir_all(parent)?;
        }
        let text = toml::to_string(&Saved {
            libraries: self.libraries.clone(),
        })
        .map_err(io::Error::other)?;

        // Write beside the file and move it into place, so a crash can't leave half a list.
        let temporary = self.file.with_extension("toml.tmp");
        fs::write(&temporary, text)?;
        fs::rename(&temporary, &self.file)
    }
}

/// A short name for a library in a list. Libraries are all called `Engine Library`,
/// so the folder that holds it (usually the drive's name) says more.
pub fn display_name(path: &Path) -> String {
    let name = |path: &Path| {
        path.file_name()
            .map(|name| name.to_string_lossy().into_owned())
    };
    match name(path) {
        Some(own) if own == LIBRARY_DIR => path.parent().and_then(name).unwrap_or(own),
        Some(own) => own,
        None => path.display().to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TempFile(PathBuf);

    impl TempFile {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "open-engine-lib-recent-{name}-{}",
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

    fn paths(recents: &Recents) -> Vec<&str> {
        recents
            .paths()
            .iter()
            .map(|p| p.to_str().unwrap())
            .collect()
    }

    #[test]
    fn most_recent_first_without_duplicates() {
        let file = TempFile::new("order");
        let mut recents = Recents::load(file.0.clone());
        recents.add("/a".into()).unwrap();
        recents.add("/b".into()).unwrap();
        recents.add("/c".into()).unwrap();
        recents.add("/a".into()).unwrap();
        assert_eq!(paths(&recents), ["/a", "/c", "/b"]);
    }

    #[test]
    fn keeps_only_the_latest_entries() {
        let file = TempFile::new("cap");
        let mut recents = Recents::load(file.0.clone());
        for index in 0..MAX_ENTRIES + 3 {
            recents.add(format!("/library{index}").into()).unwrap();
        }
        assert_eq!(recents.paths().len(), MAX_ENTRIES);
        assert_eq!(paths(&recents)[0], format!("/library{}", MAX_ENTRIES + 2));
        assert!(!paths(&recents).contains(&"/library0"));
    }

    #[test]
    fn forgets_a_library() {
        let file = TempFile::new("forget");
        let mut recents = Recents::load(file.0.clone());
        recents.add("/a".into()).unwrap();
        recents.add("/b".into()).unwrap();
        recents.forget(Path::new("/a")).unwrap();
        recents.forget(Path::new("/never-there")).unwrap();
        assert_eq!(paths(&recents), ["/b"]);
    }

    #[test]
    fn is_remembered_between_runs() {
        let file = TempFile::new("persist");
        let mut recents = Recents::load(file.0.clone());
        recents.add("/media/DRIVE/Engine Library".into()).unwrap();
        recents.add("/home/me/Music/Engine Library".into()).unwrap();

        let again = Recents::load(file.0.clone());
        assert_eq!(
            paths(&again),
            [
                "/home/me/Music/Engine Library",
                "/media/DRIVE/Engine Library"
            ]
        );
    }

    #[test]
    fn a_missing_or_corrupt_file_is_an_empty_list() {
        let file = TempFile::new("corrupt");
        assert!(Recents::load(file.0.clone()).paths().is_empty());

        fs::create_dir_all(file.0.parent().unwrap()).unwrap();
        fs::write(&file.0, "libraries = [[[ not toml").unwrap();
        let mut recents = Recents::load(file.0.clone());
        assert!(recents.paths().is_empty());
        // And it can be written over.
        recents.add("/a".into()).unwrap();
        assert_eq!(paths(&Recents::load(file.0.clone())), ["/a"]);
    }

    #[test]
    fn names_a_library_by_its_drive() {
        assert_eq!(
            display_name(Path::new("/run/media/me/DJ-MAIN/Engine Library")),
            "DJ-MAIN"
        );
        assert_eq!(display_name(Path::new("/home/me/Sets")), "Sets");
        assert_eq!(display_name(Path::new("/Engine Library")), "Engine Library");
    }
}
