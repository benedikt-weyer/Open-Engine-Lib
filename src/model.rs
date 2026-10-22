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

    pub fn track_by_id(&self, id: u32) -> Option<&Track> {
        self.index_of_id.get(&id).map(|&index| &self.tracks[index])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
