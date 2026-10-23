//! The UI's copy of the open library, as the backend described it.

use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};

use open_engine_api::v1::SmartRules;

#[derive(Debug, Clone)]
pub struct Track {
    /// The backend's id for the track, used to ask it to play the track.
    pub id: u32,
    pub title: String,
    pub artist: String,
    pub album: String,
    pub genre: String,
    pub bpm: Option<f64>,
    /// Length in whole seconds.
    pub length: Option<u32>,
    /// Streaming tracks have no file on this machine, so they can't be played.
    pub streaming: bool,
    /// The cover's id for the backend. Tracks of one album share it.
    pub album_art: Option<u32>,
    /// The audio file as the library records it, relative to the library folder. Unlike
    /// `id` it is the same every time the library is opened.
    pub path: String,
    /// Where the audio file is on this machine: `path` resolved against the library's
    /// folder. Empty for streaming tracks, which have no file here.
    pub location: String,
    /// The star rating, 0 to 100 with 20 to the star, if the library has one.
    pub rating: Option<u32>,
    /// What SoundSwitch has for the track.
    pub lighting: Lighting,
    /// The SoundSwitch projects that have lighting for the track.
    pub lighting_projects: Vec<String>,
    /// The track has stems rendered in the library.
    pub has_stems: bool,
    /// How many cues the track had set when the library was fetched (the browser knows
    /// what it is now).
    pub cue_count: u32,
    /// The same for saved loops.
    pub loop_count: u32,
    /// Where the file was when its origin was saved, in Open Engine Lib's own database.
    pub origin: Option<String>,
}

/// Whether SoundSwitch, the lighting software, has lighting for a track.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Lighting {
    /// Not in SoundSwitch, or there is no SoundSwitch data to say.
    #[default]
    None,
    /// SoundSwitch analysed the track but rendered no lighting for it.
    Analysed,
    Rendered,
}

impl Track {
    /// The rating as 0 to 5 stars. A rating that is not a multiple of 20 (libraries that
    /// were imported can have any) is rounded to the nearest star.
    pub fn stars(&self) -> u8 {
        stars_of(self.rating)
    }
}

/// A rating as 0 to 5 stars, rounded to the nearest star.
pub fn stars_of(rating: Option<u32>) -> u8 {
    rating.map_or(0, |rating| ((rating + 10) / 20).min(5) as u8)
}

/// Where a track's audio file is: the path the library records, relative to the
/// `Engine Library` folder (so usually starting with `..`), resolved to a plain path.
/// Resolved by the path alone, without asking the file system, so it also works for a
/// library on a drive that is gone.
pub fn location(library_dir: &str, recorded: &str) -> String {
    let mut resolved = PathBuf::new();
    for component in Path::new(library_dir).join(recorded).components() {
        match component {
            Component::ParentDir => {
                // Above the root there is nothing to go up to.
                resolved.pop();
            }
            Component::CurDir => {}
            other => resolved.push(other.as_os_str()),
        }
    }
    resolved.display().to_string()
}

/// A playlist or playlist folder. Playlists are stored depth first in display order,
/// so a flat list is enough to draw the tree.
#[derive(Debug, Clone)]
pub struct Playlist {
    pub title: String,
    pub depth: usize,
    pub has_children: bool,
    /// A smart playlist: its tracks are the ones that satisfy its rules.
    pub is_smart: bool,
    /// What a smart playlist holds, if the backend could read it.
    pub rules: Option<SmartRules>,
    /// Indices into `Library::tracks`, in playlist order.
    pub tracks: Vec<usize>,
}

/// What a library is made from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Kind {
    /// An Engine DJ library: its databases, with ratings, cues, playlists to edit and so on.
    #[default]
    Engine,
    /// A plain folder of music files, whose folders are the playlists. It has nothing but
    /// what the files say, and nothing to sync.
    Folder,
}

#[derive(Debug)]
pub struct Library {
    pub kind: Kind,
    /// The `Engine Library` folder that was opened (or the folder of music).
    pub path: String,
    /// The backend's cached copy of the library's databases, which is what it reads.
    pub cache_path: String,
    pub tracks: Vec<Track>,
    pub playlists: Vec<Playlist>,
    index_of_id: HashMap<u32, usize>,
}

impl Library {
    pub fn new(
        path: String,
        cache_path: String,
        tracks: Vec<Track>,
        playlists: Vec<Playlist>,
    ) -> Self {
        let index_of_id = tracks
            .iter()
            .enumerate()
            .map(|(index, track)| (track.id, index))
            .collect();
        Self {
            kind: Kind::Engine,
            path,
            cache_path,
            tracks,
            playlists,
            index_of_id,
        }
    }

    /// The same library, of another kind.
    pub fn of_kind(mut self, kind: Kind) -> Self {
        self.kind = kind;
        self
    }

    /// Whether this is an Engine library, which is what has ratings, cues and loops to
    /// show and edit, playlists to change, and a source to sync with.
    pub fn is_engine(&self) -> bool {
        self.kind == Kind::Engine
    }

    /// The playlists from the top-level one down to `index`, as indices into `playlists`.
    pub fn playlist_chain(&self, index: usize) -> Vec<usize> {
        // Playlists are stored depth first, so the parent is the nearest one before
        // `index` that is one level up, and so on up the tree.
        let mut chain = vec![index];
        let mut depth = self.playlists[index].depth;
        for earlier in (0..index).rev() {
            if depth == 0 {
                break;
            }
            if self.playlists[earlier].depth == depth - 1 {
                chain.push(earlier);
                depth -= 1;
            }
        }
        chain.reverse();
        chain
    }

    /// The playlist at `index` and everything in it, as a range of `playlists`. (They
    /// are stored depth first, so what is in a playlist follows it.)
    pub fn playlist_span(&self, index: usize) -> std::ops::Range<usize> {
        let depth = self.playlists[index].depth;
        let end = self.playlists[index + 1..]
            .iter()
            .position(|inner| inner.depth <= depth)
            .map_or(self.playlists.len(), |offset| index + 1 + offset);
        index..end
    }

    /// The titles from the top-level playlist down to `index`: where to find it again.
    pub fn playlist_path(&self, index: usize) -> Vec<String> {
        self.playlist_chain(index)
            .into_iter()
            .map(|playlist| self.playlists[playlist].title.clone())
            .collect()
    }

    /// The playlist with the given titles from the top down, see `playlist_path`.
    pub fn find_playlist(&self, path: &[String]) -> Option<usize> {
        if path.is_empty() {
            return None;
        }
        (0..self.playlists.len()).find(|&index| {
            let chain = self.playlist_chain(index);
            chain.len() == path.len()
                && chain
                    .iter()
                    .zip(path)
                    .all(|(&playlist, title)| self.playlists[playlist].title == *title)
        })
    }

    /// The same library with other playlists, as after a smart playlist was added.
    pub fn with_playlists(&self, playlists: Vec<Playlist>) -> Self {
        Self::new(
            self.path.clone(),
            self.cache_path.clone(),
            self.tracks.clone(),
            playlists,
        )
    }

    /// Index into `tracks` of the track with the backend's `id`.
    pub fn track_index(&self, id: u32) -> Option<usize> {
        self.index_of_id.get(&id).copied()
    }

    pub fn track_by_id(&self, id: u32) -> Option<&Track> {
        self.index_of_id.get(&id).map(|&index| &self.tracks[index])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn playlist(title: &str, depth: usize) -> Playlist {
        Playlist {
            title: title.into(),
            depth,
            has_children: false,
            is_smart: false,
            rules: None,
            tracks: Vec::new(),
        }
    }

    #[test]
    fn a_playlist_and_what_is_in_it_are_a_range() {
        let library = Library::new(
            "/lib".into(),
            "/cache".into(),
            Vec::new(),
            vec![
                playlist("Pop", 0),
                playlist("House", 0),
                playlist("Deep", 1),
                playlist("Dark", 2),
                playlist("Tech", 1),
                playlist("Techno", 0),
            ],
        );
        assert_eq!(library.playlist_span(0), 0..1);
        assert_eq!(library.playlist_span(1), 1..5);
        assert_eq!(library.playlist_span(2), 2..4);
        assert_eq!(library.playlist_span(4), 4..5);
        assert_eq!(library.playlist_span(5), 5..6);
    }

    #[test]
    fn playlists_are_found_again_by_their_titles() {
        // Pop, House/{Deep, Tech/Dark}, Techno/Dark
        let library = Library::new(
            "/lib".into(),
            "/cache".into(),
            Vec::new(),
            vec![
                playlist("Pop", 0),
                playlist("House", 0),
                playlist("Deep", 1),
                playlist("Tech", 1),
                playlist("Dark", 2),
                playlist("Techno", 0),
                playlist("Dark", 1),
            ],
        );

        assert_eq!(library.playlist_chain(4), [1, 3, 4]);
        assert_eq!(library.playlist_path(4), ["House", "Tech", "Dark"]);
        assert_eq!(library.playlist_path(5), ["Techno"]);

        // The same title under another parent is a different playlist.
        let titles =
            |path: &[&str]| -> Vec<String> { path.iter().map(|t| t.to_string()).collect() };
        assert_eq!(
            library.find_playlist(&titles(&["House", "Tech", "Dark"])),
            Some(4)
        );
        assert_eq!(library.find_playlist(&titles(&["Techno", "Dark"])), Some(6));
        assert_eq!(library.find_playlist(&titles(&["Dark"])), None);
        assert_eq!(library.find_playlist(&titles(&["House", "Gone"])), None);
        assert_eq!(
            library.find_playlist(&[]),
            None,
            "no title means all tracks"
        );
    }

    #[test]
    fn a_rating_is_shown_as_stars() {
        let stars = |rating| {
            let mut track = Track {
                rating,
                ..sample_track()
            };
            track.rating = rating;
            track.stars()
        };
        assert_eq!(stars(None), 0);
        assert_eq!(stars(Some(0)), 0);
        assert_eq!(stars(Some(20)), 1);
        assert_eq!(stars(Some(60)), 3);
        assert_eq!(stars(Some(100)), 5);
        // Ratings from elsewhere are rounded to the nearest star, and never go past five.
        assert_eq!(
            [
                stars(Some(9)),
                stars(Some(10)),
                stars(Some(29)),
                stars(Some(30))
            ],
            [0, 1, 1, 2]
        );
        assert_eq!(stars(Some(255)), 5);
    }

    fn sample_track() -> Track {
        Track {
            id: 1,
            title: String::new(),
            artist: String::new(),
            album: String::new(),
            genre: String::new(),
            bpm: None,
            length: None,
            streaming: false,
            album_art: None,
            path: String::new(),
            location: String::new(),
            rating: None,
            lighting: Lighting::None,
            lighting_projects: Vec::new(),
            has_stems: false,
            cue_count: 0,
            loop_count: 0,
            origin: None,
        }
    }

    #[test]
    fn a_tracks_location_is_its_path_resolved_against_the_library() {
        let library = "/run/media/me/DRIVE/Engine Library";
        // The usual case: the music sits next to the library folder.
        assert_eq!(
            location(library, "../House/a.flac"),
            "/run/media/me/DRIVE/House/a.flac"
        );
        assert_eq!(
            location(library, "../../Music/x/y.mp3"),
            "/run/media/me/Music/x/y.mp3"
        );
        // Inside the library folder, with a redundant ./ and ..
        assert_eq!(
            location(library, "./Stems/../a.flac"),
            "/run/media/me/DRIVE/Engine Library/a.flac"
        );
        // An absolute path is just that, and too many .. stop at the top.
        assert_eq!(location(library, "/srv/music/a.flac"), "/srv/music/a.flac");
        assert_eq!(location("/lib", "../../../a.flac"), "/a.flac");
    }

    #[test]
    fn finds_tracks_by_backend_id() {
        let track = |id| Track {
            id,
            title: format!("track {id}"),
            artist: String::new(),
            album: String::new(),
            genre: String::new(),
            bpm: None,
            length: None,
            streaming: false,
            album_art: None,
            path: format!("../Music/{id}.flac"),
            location: String::new(),
            rating: None,
            lighting: Lighting::None,
            lighting_projects: Vec::new(),
            has_stems: false,
            cue_count: 0,
            loop_count: 0,
            origin: None,
        };
        let library = Library::new(
            "/lib".into(),
            "/cache".into(),
            vec![track(7), track(3)],
            Vec::new(),
        );

        assert_eq!(
            library.track_by_id(7).map(|t| t.title.as_str()),
            Some("track 7")
        );
        assert!(library.track_by_id(4).is_none());
    }
}
