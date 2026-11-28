//! The jobs that wait for their turn: analyses and stem generations of tracks. The backend
//! runs one job at a time (alongside edits, syncs and the rest), so the UI asks for the next
//! one when the answer to the last has come. The queue is the UI's: it remembers which
//! library the tracks are in, and each track by a reference that outlives the track's id
//! (Engine's origin of the track, else its path), so that a track is found again after the
//! library was read again, and one that is gone is left out.

use crate::model::{Library, Track};

/// What is done with a track.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// Find its beats and key.
    Analyse,
    /// Separate it into stems and write Engine's stems file.
    Stems,
}

/// A track, as it is found again in the library even when its id changed: by Engine's
/// identity of it (its origin) if it has one, else by its path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrackRef {
    pub origin: Option<(String, i64)>,
    pub path: String,
}

impl TrackRef {
    pub fn of(track: &Track) -> Self {
        Self {
            origin: track.origin_key.clone(),
            path: track.path.clone(),
        }
    }

    /// The track in `library`, if it is there.
    pub fn find<'a>(&self, library: &'a Library) -> Option<&'a Track> {
        match &self.origin {
            Some(origin) => library
                .tracks
                .iter()
                .find(|track| track.origin_key.as_ref() == Some(origin)),
            None => library.tracks.iter().find(|track| track.path == self.path),
        }
    }
}

/// A job: what to do with which track.
#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    pub kind: Kind,
    pub track: TrackRef,
    pub title: String,
    /// Replace what the track has (its analysis, its stems file).
    pub overwrite: bool,
}

impl Entry {
    pub fn new(kind: Kind, track: &Track, overwrite: bool) -> Self {
        Self {
            kind,
            track: TrackRef::of(track),
            title: if track.title.is_empty() {
                track.path.clone()
            } else {
                track.title.clone()
            },
            overwrite,
        }
    }

    pub fn analyse(track: &Track, overwrite: bool) -> Self {
        Self::new(Kind::Analyse, track, overwrite)
    }
}

/// Where a job is.
#[derive(Debug, Clone, PartialEq)]
pub enum Status {
    Waiting,
    Running,
    Done,
    /// It failed, or its track was no longer in the library when its turn came: why.
    Failed(String),
    /// Taken out of the queue before its turn, or stopped while it ran.
    Stopped,
}

/// A job and where it is.
#[derive(Debug, Clone, PartialEq)]
pub struct Item {
    pub entry: Entry,
    pub status: Status,
}

/// The jobs of one library, run one after the other.
#[derive(Debug, Clone, PartialEq)]
pub struct Queue {
    /// The library the tracks are in (as the backend names it).
    pub library: String,
    /// All jobs since the queue was made, in order, with where each is.
    items: Vec<Item>,
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
    pub fn new(library: String, entries: Vec<Entry>) -> Self {
        let mut queue = Self {
            library,
            items: Vec::new(),
        };
        queue.add(entries);
        queue
    }

    /// Takes the next job to run.
    pub fn next(&mut self) -> Option<Entry> {
        let item = self
            .items
            .iter_mut()
            .find(|item| item.status == Status::Waiting)?;
        item.status = Status::Running;
        Some(item.entry.clone())
    }

    /// The job that runs, if one does.
    pub fn running(&self) -> Option<&Entry> {
        self.items
            .iter()
            .find(|item| item.status == Status::Running)
            .map(|item| &item.entry)
    }

    /// Whether the job (that kind, that track) runs or waits for its turn.
    pub fn contains(&self, kind: Kind, track: &TrackRef) -> bool {
        self.items.iter().any(|item| {
            matches!(item.status, Status::Waiting | Status::Running)
                && item.entry.kind == kind
                && &item.entry.track == track
        })
    }

    /// Puts jobs at the end of the queue, even while it runs. One that runs or waits already
    /// is not put in again, but one that waits without replacing is made to replace if the
    /// new one says so. Returns how many were added.
    pub fn add(&mut self, entries: Vec<Entry>) -> usize {
        let mut added = 0;
        for entry in entries {
            let same = |item: &Item| {
                item.entry.kind == entry.kind
                    && item.entry.track == entry.track
                    && matches!(item.status, Status::Waiting | Status::Running)
            };
            match self.items.iter_mut().find(|item| same(item)) {
                Some(item) => {
                    if item.status == Status::Waiting {
                        item.entry.overwrite |= entry.overwrite;
                    }
                }
                None => {
                    self.items.push(Item {
                        entry,
                        status: Status::Waiting,
                    });
                    added += 1;
                }
            }
        }
        added
    }

    /// Whether jobs wait for their turn.
    pub fn has_next(&self) -> bool {
        self.items.iter().any(|item| item.status == Status::Waiting)
    }

    /// The job that was last taken: which of how many (the ones taken out count too).
    pub fn position(&self) -> (u32, u32) {
        let started = self
            .items
            .iter()
            .filter(|item| item.status != Status::Waiting)
            .count() as u32;
        (started, self.items.len() as u32)
    }

    /// Notes how the job that runs went.
    pub fn finished(&mut self, error: Option<&str>) {
        if let Some(item) = self
            .items
            .iter_mut()
            .find(|item| item.status == Status::Running)
        {
            item.status = match error {
                None => Status::Done,
                Some(error) => Status::Failed(error.to_owned()),
            };
        }
    }

    /// Takes the waiting job at `index` of `items` out.
    pub fn remove(&mut self, index: usize) {
        if let Some(item) = self.items.get_mut(index)
            && item.status == Status::Waiting
        {
            item.status = Status::Stopped;
        }
    }

    /// The jobs that wait are not run after all (the one that runs goes on).
    pub fn stop(&mut self) {
        for item in &mut self.items {
            if item.status == Status::Waiting {
                item.status = Status::Stopped;
            }
        }
    }

    /// The jobs that wait and the one that runs are not done after all: the one that runs
    /// counts as stopped (its answer is not waited for).
    pub fn stop_all(&mut self) {
        self.stop();
        for item in &mut self.items {
            if item.status == Status::Running {
                item.status = Status::Stopped;
            }
        }
    }

    /// How many jobs wait or run.
    pub fn pending(&self) -> usize {
        self.items
            .iter()
            .filter(|item| matches!(item.status, Status::Waiting | Status::Running))
            .count()
    }

    pub fn items(&self) -> &[Item] {
        &self.items
    }

    pub fn summary(&self) -> Summary {
        let mut summary = Summary {
            total: 0,
            done: 0,
            failed: 0,
            first_failure: None,
        };
        for item in &self.items {
            match &item.status {
                Status::Done => summary.done += 1,
                Status::Failed(error) => {
                    summary.failed += 1;
                    summary
                        .first_failure
                        .get_or_insert_with(|| (item.entry.title.clone(), error.clone()));
                }
                Status::Waiting | Status::Running | Status::Stopped => {}
            }
            summary.total += 1;
        }
        summary
    }
}

/// What to do when the library that was imported into has been opened.
#[derive(Debug, Clone, PartialEq)]
pub struct AfterOpen {
    /// The library, as the backend names it once it is open.
    pub library: String,
    /// The playlist to show.
    pub playlist: String,
    /// The paths (as the library records them) of the new tracks.
    pub added: Vec<String>,
    /// Analyse the new tracks.
    pub analyse: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(id: u32) -> Entry {
        Entry {
            kind: Kind::Analyse,
            track: TrackRef {
                origin: Some(("db".into(), i64::from(id))),
                path: format!("{id}.flac"),
            },
            title: format!("Track {id}"),
            overwrite: false,
        }
    }

    fn queue(count: u32) -> Queue {
        Queue::new("/lib".into(), (0..count).map(entry).collect())
    }

    fn title(entry: Option<Entry>) -> Option<String> {
        entry.map(|entry| entry.title)
    }

    #[test]
    fn the_jobs_come_one_after_the_other_in_order() {
        let mut queue = queue(3);
        assert_eq!(queue.position(), (0, 3));
        assert_eq!(title(queue.next()), Some("Track 0".into()));
        assert_eq!(queue.position(), (1, 3));
        assert_eq!(
            queue.running().map(|entry| entry.title.as_str()),
            Some("Track 0")
        );
        assert!(queue.has_next());
        queue.finished(None);
        assert_eq!(title(queue.next()), Some("Track 1".into()));
        queue.finished(None);
        assert_eq!(title(queue.next()), Some("Track 2".into()));
        queue.finished(None);
        assert!(!queue.has_next());
        assert_eq!(queue.next(), None);
    }

    #[test]
    fn jobs_can_be_added_while_it_runs_but_none_twice() {
        let mut queue = queue(3);
        queue.next();
        let track = |id: u32| entry(id).track;
        assert!(queue.contains(Kind::Analyse, &track(0)));
        assert!(!queue.contains(Kind::Analyse, &track(3)));
        assert!(
            !queue.contains(Kind::Stems, &track(0)),
            "another kind of job is another job"
        );
        assert_eq!(
            queue.add(vec![entry(0), entry(1), entry(3), entry(3), entry(4)]),
            2
        );
        assert_eq!(queue.position(), (1, 5), "the total grows");
        // Stems of a track that is analysed are a job of their own.
        let stems = Entry {
            kind: Kind::Stems,
            ..entry(1)
        };
        assert_eq!(queue.add(vec![stems]), 1);

        // Once its job is over, a track can be put in again.
        queue.finished(None);
        assert!(!queue.contains(Kind::Analyse, &track(0)));
        assert_eq!(queue.add(vec![entry(0)]), 1);
    }

    #[test]
    fn a_waiting_job_takes_the_replacing_mode_of_the_same_job_added_later() {
        let mut queue = queue(2);
        let replacing = Entry {
            overwrite: true,
            ..entry(1)
        };
        assert_eq!(queue.add(vec![replacing]), 0);
        queue.next();
        assert!(queue.next().unwrap().overwrite);
    }

    #[test]
    fn the_summary_counts_what_worked_and_remembers_the_first_failure() {
        let mut queue = queue(5);
        queue.next();
        queue.finished(None);
        queue.next();
        queue.finished(Some("no models"));
        queue.next();
        queue.finished(Some("gone"));
        queue.next();
        queue.finished(Some("no file"));
        queue.remove(4);
        assert_eq!(
            queue.summary(),
            Summary {
                total: 5,
                done: 1,
                failed: 3,
                first_failure: Some(("Track 1".into(), "no models".into())),
            }
        );
        assert_eq!(queue.items()[2].status, Status::Failed("gone".into()));
        assert_eq!(queue.items()[4].status, Status::Stopped);
    }

    #[test]
    fn stopping_leaves_the_job_that_runs_unless_all_are_stopped() {
        let mut queue = queue(5);
        queue.next();
        queue.stop();
        assert!(!queue.has_next());
        assert_eq!(queue.pending(), 1);
        queue.finished(None);
        assert_eq!(queue.summary().done, 1);
        assert_eq!(queue.summary().total, 5, "of all that were queued");

        let mut queue = self::queue(3);
        queue.next();
        queue.stop_all();
        assert_eq!(queue.pending(), 0);
        assert!(queue.running().is_none());
    }

    #[test]
    fn only_a_waiting_job_can_be_taken_out() {
        let mut queue = queue(3);
        queue.next();
        queue.remove(0);
        assert_eq!(queue.items()[0].status, Status::Running);
        queue.remove(1);
        assert_eq!(queue.items()[1].status, Status::Stopped);
        queue.finished(None);
        assert_eq!(title(queue.next()), Some("Track 2".into()), "1 is skipped");
    }
}
