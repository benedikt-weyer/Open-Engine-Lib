//! Whether the paths shown in the track list (the location of a file and its saved
//! origin) can be reached. Only the paths of the rows in view are checked, off the UI
//! thread, because asking about a drive that is gone or struggling can take a while.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::Path;

/// How many answers are kept. Far more than are on screen, so scrolling back a little
/// finds them again, but few enough to bound memory.
const MAX_KEPT: usize = 40_000;

#[derive(Default)]
pub struct Reachability {
    known: HashMap<String, bool>,
    /// Being checked now.
    pending: HashSet<String>,
    /// The known paths in the order they were answered, oldest first, for eviction.
    order: VecDeque<String>,
    /// Changes whenever the answers are thrown away, so a late answer, which may be
    /// about a drive that was since connected or removed, can be told apart and dropped.
    generation: u32,
}

impl Reachability {
    /// Whether `path` could be reached when it was checked; `None` if it was not
    /// checked (yet).
    pub fn get(&self, path: &str) -> Option<bool> {
        self.known.get(path).copied()
    }

    pub fn generation(&self) -> u32 {
        self.generation
    }

    /// Forgets everything, e.g. when another library is opened or a drive comes or goes.
    pub fn clear(&mut self) {
        self.known.clear();
        self.pending.clear();
        self.order.clear();
        self.generation += 1;
    }

    /// Of the wanted paths, the ones nobody has asked about yet; they count as being
    /// checked from now on.
    pub fn claim_unrequested<'a>(
        &mut self,
        wanted: impl IntoIterator<Item = &'a str>,
    ) -> Vec<String> {
        let mut check = Vec::new();
        for path in wanted {
            if !path.is_empty()
                && !self.known.contains_key(path)
                && self.pending.insert(path.to_owned())
            {
                check.push(path.to_owned());
            }
        }
        check
    }

    /// Stores the answers. Answers from before the last `clear` (`generation` is not the
    /// current one) are ignored.
    pub fn set(&mut self, generation: u32, answers: Vec<(String, bool)>) {
        if generation != self.generation {
            return;
        }
        for (path, reachable) in answers {
            self.pending.remove(&path);
            if self.known.insert(path.clone(), reachable).is_none() {
                self.order.push_back(path);
            }
        }
        while self.order.len() > MAX_KEPT {
            if let Some(oldest) = self.order.pop_front() {
                self.known.remove(&oldest);
            }
        }
    }
}

/// Checks whether each path exists. Done on a thread of its own, one path after the
/// other: a path on a drive that hangs holds up only this.
pub async fn check(paths: Vec<String>) -> Vec<(String, bool)> {
    tokio::task::spawn_blocking(move || {
        paths
            .into_iter()
            .map(|path| {
                let reachable = Path::new(&path).exists();
                (path, reachable)
            })
            .collect()
    })
    .await
    .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_path_is_checked_once_and_answers_from_before_a_clear_are_dropped() {
        let mut reachability = Reachability::default();
        let first = reachability.claim_unrequested(["/a", "/b", "", "/a"]);
        assert_eq!(first, ["/a", "/b"], "no empty path, none twice");
        assert!(
            reachability.claim_unrequested(["/a"]).is_empty(),
            "already being checked"
        );
        assert_eq!(reachability.get("/a"), None);

        let generation = reachability.generation();
        reachability.set(generation, vec![("/a".into(), true), ("/b".into(), false)]);
        assert_eq!(reachability.get("/a"), Some(true));
        assert_eq!(reachability.get("/b"), Some(false));
        assert!(reachability.claim_unrequested(["/a", "/b"]).is_empty());

        // A drive came or went: everything is asked again, and a late answer is ignored.
        reachability.claim_unrequested(["/c"]);
        reachability.clear();
        reachability.set(generation, vec![("/c".into(), true)]);
        assert_eq!(reachability.get("/c"), None);
        assert_eq!(reachability.claim_unrequested(["/a", "/c"]), ["/a", "/c"]);
    }

    #[test]
    fn only_a_bounded_number_of_answers_is_kept() {
        let mut reachability = Reachability::default();
        let generation = reachability.generation();
        let answers = (0..MAX_KEPT + 10)
            .map(|index| (format!("/music/{index}"), true))
            .collect();
        reachability.set(generation, answers);
        assert_eq!(
            reachability.get("/music/0"),
            None,
            "the oldest is forgotten"
        );
        assert_eq!(
            reachability.get(&format!("/music/{}", MAX_KEPT + 9)),
            Some(true)
        );
    }

    #[test]
    fn existing_and_missing_paths_are_told_apart() {
        let here = std::env::current_dir().unwrap().display().to_string();
        let missing = "/nowhere/at/all.flac".to_string();
        let answers = futures_lite_block(check(vec![here.clone(), missing.clone()]));
        assert_eq!(answers, [(here, true), (missing, false)]);
    }

    /// Runs a future to its end on a small runtime of its own.
    fn futures_lite_block<T>(future: impl std::future::Future<Output = T>) -> T {
        tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
            .block_on(future)
    }
}
