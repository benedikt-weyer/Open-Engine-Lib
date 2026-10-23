//! The columns of the track list and how wide the user made them.
//!
//! The widths are a preference the user sets by dragging, so they are remembered in
//! their own file in the user state dir (`<state dir>/open-engine-lib/track-columns.toml`).
//! The last column, the file's location, is not listed: it takes what the others leave.

use std::fs;
use std::io;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

const APP_DIR: &str = "open-engine-lib";
const FILE: &str = "track-columns.toml";
/// A column can't be made narrower than this (pixels), or the handle would be lost.
pub const MIN_WIDTH: f32 = 40.0;
/// Nor wider than this.
pub const MAX_WIDTH: f32 = 1200.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Column {
    Title,
    Artist,
    Album,
    Genre,
    Bpm,
    Length,
    Rating,
    Cues,
    Loops,
    Lighting,
    Stems,
    Origin,
}

impl Column {
    /// Whether the column is about what an Engine library has (ratings, cues, loops,
    /// SoundSwitch lighting, where a track came from), so a plain folder of music has none.
    pub fn is_engine_only(self) -> bool {
        matches!(
            self,
            Column::Rating
                | Column::Cues
                | Column::Loops
                | Column::Lighting
                | Column::Stems
                | Column::Origin
        )
    }

    /// How many there are.
    pub const COUNT: usize = 12;
    /// In the order they are shown.
    pub const ALL: [Column; Column::COUNT] = [
        Column::Title,
        Column::Artist,
        Column::Album,
        Column::Genre,
        Column::Bpm,
        Column::Length,
        Column::Rating,
        Column::Cues,
        Column::Loops,
        Column::Lighting,
        Column::Stems,
        Column::Origin,
    ];
}

/// The width in pixels of each column that can be resized.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ColumnWidths {
    title: f32,
    artist: f32,
    album: f32,
    genre: f32,
    bpm: f32,
    length: f32,
    rating: f32,
    cues: f32,
    loops: f32,
    lighting: f32,
    stems: f32,
    origin: f32,
}

impl Default for ColumnWidths {
    fn default() -> Self {
        Self {
            title: 280.0,
            artist: 200.0,
            album: 200.0,
            genre: 120.0,
            bpm: 64.0,
            length: 64.0,
            rating: 80.0,
            cues: 56.0,
            loops: 56.0,
            lighting: 72.0,
            stems: 64.0,
            origin: 260.0,
        }
    }
}

impl ColumnWidths {
    pub fn get(&self, column: Column) -> f32 {
        match column {
            Column::Title => self.title,
            Column::Artist => self.artist,
            Column::Album => self.album,
            Column::Genre => self.genre,
            Column::Bpm => self.bpm,
            Column::Length => self.length,
            Column::Rating => self.rating,
            Column::Cues => self.cues,
            Column::Loops => self.loops,
            Column::Lighting => self.lighting,
            Column::Stems => self.stems,
            Column::Origin => self.origin,
        }
    }

    /// Sets a width, kept between `MIN_WIDTH` and `MAX_WIDTH`.
    pub fn set(&mut self, column: Column, width: f32) {
        let width = width.clamp(MIN_WIDTH, MAX_WIDTH);
        match column {
            Column::Title => self.title = width,
            Column::Artist => self.artist = width,
            Column::Album => self.album = width,
            Column::Genre => self.genre = width,
            Column::Bpm => self.bpm = width,
            Column::Length => self.length = width,
            Column::Rating => self.rating = width,
            Column::Cues => self.cues = width,
            Column::Loops => self.loops = width,
            Column::Lighting => self.lighting = width,
            Column::Stems => self.stems = width,
            Column::Origin => self.origin = width,
        }
    }

    /// Replaces widths that make no sense (a hand-edited file) by the defaults.
    fn sanitized(mut self) -> Self {
        let defaults = Self::default();
        for column in Column::ALL {
            let width = self.get(column);
            if width.is_finite() {
                self.set(column, width);
            } else {
                self.set(column, defaults.get(column));
            }
        }
        self
    }
}

/// The widths and the file they are saved in.
pub struct ColumnStore {
    file: PathBuf,
    pub widths: ColumnWidths,
}

impl ColumnStore {
    pub fn discover() -> Self {
        let file = dirs::state_dir()
            .or_else(dirs::data_local_dir)
            .unwrap_or_default()
            .join(APP_DIR)
            .join(FILE);
        Self::load(file)
    }

    /// Reads the file. A missing or unreadable one gives the default widths: they are
    /// only a convenience, so they must never stop the app from starting.
    pub fn load(file: PathBuf) -> Self {
        let widths = fs::read_to_string(&file)
            .ok()
            .and_then(|text| toml::from_str::<ColumnWidths>(&text).ok())
            .map(ColumnWidths::sanitized)
            .unwrap_or_default();
        Self { file, widths }
    }

    pub fn save(&self) -> io::Result<()> {
        if let Some(parent) = self.file.parent() {
            fs::create_dir_all(parent)?;
        }
        let text = toml::to_string(&self.widths).map_err(io::Error::other)?;
        // Write beside the file and move it into place, so a crash can't leave half of it.
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
                "open-engine-lib-columns-{name}-{}",
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
    fn the_columns_about_an_engine_library_are_told_apart() {
        let engine_only: Vec<Column> = Column::ALL
            .into_iter()
            .filter(|column| column.is_engine_only())
            .collect();
        assert_eq!(
            engine_only,
            [
                Column::Rating,
                Column::Cues,
                Column::Loops,
                Column::Lighting,
                Column::Stems,
                Column::Origin
            ]
        );
    }

    #[test]
    fn a_width_can_be_set_within_limits() {
        let mut widths = ColumnWidths::default();
        widths.set(Column::Artist, 333.0);
        assert_eq!(widths.get(Column::Artist), 333.0);
        assert_eq!(
            widths.get(Column::Title),
            ColumnWidths::default().get(Column::Title)
        );

        widths.set(Column::Artist, 1.0);
        assert_eq!(
            widths.get(Column::Artist),
            MIN_WIDTH,
            "not narrower than a handle"
        );
        widths.set(Column::Artist, 99_999.0);
        assert_eq!(widths.get(Column::Artist), MAX_WIDTH);
    }

    #[test]
    fn widths_are_remembered_between_runs() {
        let file = TempFile::new("persist");
        let mut store = ColumnStore::load(file.0.clone());
        assert_eq!(store.widths, ColumnWidths::default());

        store.widths.set(Column::Title, 410.0);
        store.widths.set(Column::Bpm, 90.0);
        store.save().unwrap();

        let again = ColumnStore::load(file.0.clone());
        assert_eq!(again.widths.get(Column::Title), 410.0);
        assert_eq!(again.widths.get(Column::Bpm), 90.0);
        assert_eq!(
            again.widths.get(Column::Genre),
            ColumnWidths::default().get(Column::Genre)
        );
    }

    #[test]
    fn a_bad_file_gives_usable_widths() {
        let file = TempFile::new("bad");
        fs::create_dir_all(file.0.parent().unwrap()).unwrap();

        fs::write(&file.0, "title = [[[ not toml").unwrap();
        assert_eq!(
            ColumnStore::load(file.0.clone()).widths,
            ColumnWidths::default()
        );

        // Hand-edited nonsense is pulled into range (or replaced) rather than trusted.
        fs::write(&file.0, "title = 2.0\nartist = 9999999.0\nalbum = nan\n").unwrap();
        let widths = ColumnStore::load(file.0.clone()).widths;
        assert_eq!(widths.get(Column::Title), MIN_WIDTH);
        assert_eq!(widths.get(Column::Artist), MAX_WIDTH);
        assert_eq!(
            widths.get(Column::Album),
            ColumnWidths::default().get(Column::Album)
        );
    }
}
