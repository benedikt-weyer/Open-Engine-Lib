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
    /// Whether the track that was last taken is analysed even if it has an analysis, replacing
    /// it. (Each track of the queue has its own: tracks can be added later, in another mode.)
    pub overwrite: bool,
    /// The tracks that wait, each with whether it replaces an analysis.
    waiting: VecDeque<(Entry, bool)>,
    /// The track that was last taken, until its analysis is over.
    running: Option<u32>,
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
            waiting: tracks.into_iter().map(|track| (track, overwrite)).collect(),
            running: None,
            started: 0,
            done: 0,
            failed: 0,
            first_failure: None,
        }
    }

    /// Takes the next track to analyse (`overwrite` says how).
    pub fn next(&mut self) -> Option<Entry> {
        let (next, overwrite) = self.waiting.pop_front()?;
        self.started += 1;
        self.overwrite = overwrite;
        self.running = Some(next.0);
        Some(next)
    }

    /// Whether the track is being analysed or waits for its turn.
    pub fn contains(&self, track: u32) -> bool {
        self.running == Some(track) || self.waiting.iter().any(|((id, _), _)| *id == track)
    }

    /// Puts tracks at the end of the queue, even while it runs. A track that is in the queue
    /// already (being analysed or waiting) is not put in again, but one that waits to be
    /// analysed without replacing its analysis is made to replace it if `overwrite` says so.
    /// Returns how many tracks were added.
    pub fn add(&mut self, tracks: Vec<Entry>, overwrite: bool) -> usize {
        let mut added = 0;
        for track in tracks {
            if self.running == Some(track.0) {
                continue;
            }
            match self.waiting.iter_mut().find(|((id, _), _)| *id == track.0) {
                Some((_, replaces)) => *replaces |= overwrite,
                None => {
                    self.waiting.push_back((track, overwrite));
                    added += 1;
                }
            }
        }
        self.total += added as u32;
        added
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
        self.running = None;
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
    fn tracks_can_be_added_while_it_runs_but_none_twice() {
        let mut queue = queue(3);
        queue.next();
        // Track 0 runs, 1 and 2 wait. 0, 1 and 2 are in; 3 and 4 are new, one of them twice.
        assert!(queue.contains(0) && queue.contains(2) && !queue.contains(3));
        let entry = |id: u32| (id, format!("Track {id}"));
        assert_eq!(
            queue.add(
                vec![entry(0), entry(1), entry(3), entry(3), entry(4)],
                false
            ),
            2
        );
        assert_eq!(queue.position(), (1, 5), "the total grows");
        let order: Vec<_> = std::iter::from_fn(|| queue.next().map(|entry| entry.0)).collect();
        assert_eq!(order, [1, 2, 3, 4]);

        // Once its analysis is over, a track can be put in again.
        assert!(queue.contains(4));
        queue.finished("Track 4", None);
        assert!(!queue.contains(4));
        assert_eq!(queue.add(vec![entry(4)], false), 1);
        assert_eq!(queue.next().map(|entry| entry.0), Some(4));
    }

    #[test]
    fn every_track_keeps_the_mode_it_was_asked_for_in() {
        let mut queue = queue(2);
        let entry = |id: u32| (id, format!("Track {id}"));
        // 5 is new and replaces; 1 waits already and is made to replace too.
        assert_eq!(queue.add(vec![entry(5), entry(1)], true), 1);
        let modes: Vec<_> =
            std::iter::from_fn(|| queue.next().map(|entry| (entry.0, queue.overwrite))).collect();
        assert_eq!(modes, [(0, false), (1, true), (5, true)]);
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
