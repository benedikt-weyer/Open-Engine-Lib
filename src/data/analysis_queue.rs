//! The tracks that wait to be analysed. The backend analyses one track at a time (and
//! refuses a second request while one runs), so a list of tracks is analysed by asking for
//! the next one when the answer to the last has come. The queue is the UI's: it remembers
//! which library the tracks are in, so that it never asks the backend to analyse a track
//! of a library that is no longer open.

use std::collections::VecDeque;

/// A track to analyse: the backend's id for it and its title.
pub type Entry = (u32, String);

/// Tracks of one library, analysed one after the other.
#[derive(Debug, Clone, PartialEq)]
pub struct Queue {
    /// The library the tracks are in (as the backend names it).
    pub library: String,
    /// Analyse tracks that have an analysis too, replacing it.
    pub overwrite: bool,
    waiting: VecDeque<Entry>,
    total: u32,
    started: u32,
    done: u32,
    failed: u32,
    /// The first track that failed and why.
    first_failure: Option<(String, String)>,
}

/// How a queue ended.
#[derive(Debug, Clone, PartialEq)]
pub struct Summary {
    pub total: u32,
    pub done: u32,
    pub failed: u32,
    /// The title of the first track that failed and the reason.
    pub first_failure: Option<(String, String)>,
}

impl Queue {
    pub fn new(library: String, tracks: Vec<Entry>, overwrite: bool) -> Self {
        Self {
            library,
            overwrite,
            total: tracks.len() as u32,
            waiting: tracks.into(),
            started: 0,
            done: 0,
            failed: 0,
            first_failure: None,
        }
    }

    /// Takes the next track to analyse.
    pub fn next(&mut self) -> Option<Entry> {
        let next = self.waiting.pop_front()?;
        self.started += 1;
        Some(next)
    }

    /// Whether tracks wait for their turn.
    pub fn has_next(&self) -> bool {
        !self.waiting.is_empty()
    }

    /// The track that was last taken: which of how many.
    pub fn position(&self) -> (u32, u32) {
        (self.started, self.total)
    }

    /// Notes how the analysis of the track that was last taken went.
    pub fn finished(&mut self, title: &str, error: Option<&str>) {
        match error {
            None => self.done += 1,
            Some(error) => {
                self.failed += 1;
                self.first_failure
                    .get_or_insert_with(|| (title.to_owned(), error.to_owned()));
            }
        }
    }

    /// The tracks that wait are not analysed after all (the one that is being analysed is).
    pub fn stop(&mut self) {
        self.waiting.clear();
    }

    pub fn summary(&self) -> Summary {
        Summary {
            total: self.total,
            done: self.done,
            failed: self.failed,
            first_failure: self.first_failure.clone(),
        }
    }
}

/// What to do when the library that was imported into has been opened.
#[derive(Debug, Clone, PartialEq)]
pub struct AfterOpen {
    /// The library, as the backend names it once it is open.
    pub library: String,
    /// The title of the playlist to show.
    pub playlist: String,
    /// The paths (as the library records them) of the new tracks.
    pub added: Vec<String>,
    /// Analyse the new tracks.
    pub analyse: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn queue(count: u32) -> Queue {
        Queue::new(
            "/lib".into(),
            (0..count).map(|id| (id, format!("Track {id}"))).collect(),
            false,
        )
    }

    #[test]
    fn the_tracks_come_one_after_the_other_in_order() {
        let mut queue = queue(3);
        assert_eq!(queue.position(), (0, 3));
        assert_eq!(queue.next(), Some((0, "Track 0".into())));
        assert_eq!(queue.position(), (1, 3));
        assert!(queue.has_next());
        assert_eq!(queue.next().map(|entry| entry.0), Some(1));
        assert_eq!(queue.next().map(|entry| entry.0), Some(2));
        assert!(!queue.has_next());
        assert_eq!(queue.next(), None);
    }

    #[test]
    fn the_summary_counts_what_worked_and_remembers_the_first_failure() {
        let mut queue = queue(4);
        queue.next();
        queue.finished("Track 0", None);
        queue.next();
        queue.finished("Track 1", Some("no models"));
        queue.next();
        queue.finished("Track 2", Some("no file"));
        assert_eq!(
            queue.summary(),
            Summary {
                total: 4,
                done: 1,
                failed: 2,
                first_failure: Some(("Track 1".into(), "no models".into())),
            }
        );
    }

    #[test]
    fn stopping_leaves_the_track_that_runs_and_drops_the_rest() {
        let mut queue = queue(5);
        queue.next();
        queue.stop();
        assert!(!queue.has_next());
        queue.finished("Track 0", None);
        assert_eq!(queue.summary().done, 1);
        assert_eq!(queue.summary().total, 5);
    }
}
