//! The UI's copy of the open library, as the backend described it.

use std::collections::HashMap;

#[derive(Debug)]
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
}

/// A playlist or playlist folder. Playlists are stored depth first in display order,
/// so a flat list is enough to draw the tree.
#[derive(Debug)]
pub struct Playlist {
    pub title: String,
    pub depth: usize,
    pub has_children: bool,
    /// Indices into `Library::tracks`, in playlist order.
    pub tracks: Vec<usize>,
}

#[derive(Debug)]
pub struct Library {
    /// The `Engine Library` folder that was opened.
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
            path,
            cache_path,
            tracks,
            playlists,
            index_of_id,
        }
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
            tracks: Vec::new(),
        }
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
