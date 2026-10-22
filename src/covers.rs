//! Cover images the UI has fetched from the backend.
//!
//! Only the covers that are on screen are fetched, and only a limited number are
//! kept, because a library has thousands of them.

use std::collections::hash_map::Entry;
use std::collections::{HashMap, VecDeque};

use iced::widget::image::Handle;

/// How many covers are kept. Far more than fit on screen, so scrolling back a little
/// finds them again, but few enough to bound memory.
const MAX_KEPT: usize = 300;

enum Cover {
    Loading,
    Ready(Handle),
    /// The backend has no image for it, or asking failed. Not asked again.
    Missing,
}

#[derive(Default)]
pub struct Covers {
    covers: HashMap<u32, Cover>,
    /// Ids in the order they were fetched, oldest first, for eviction.
    order: VecDeque<u32>,
    /// Changes whenever the library does, so late answers about the old library's
    /// covers (whose ids mean other covers now) can be told apart and dropped.
    generation: u32,
}

impl Covers {
    pub fn get(&self, id: u32) -> Option<&Handle> {
        match self.covers.get(&id) {
            Some(Cover::Ready(handle)) => Some(handle),
            _ => None,
        }
    }

    pub fn generation(&self) -> u32 {
        self.generation
    }

    /// Forgets everything, e.g. when another library is opened.
    pub fn clear(&mut self) {
        self.covers.clear();
        self.order.clear();
        self.generation += 1;
    }

    /// Of the wanted covers, the ones nobody has asked for yet; they count as being
    /// loaded from now on.
    pub fn claim_unrequested(&mut self, wanted: impl IntoIterator<Item = u32>) -> Vec<u32> {
        let mut fetch = Vec::new();
        for id in wanted {
            if let Entry::Vacant(slot) = self.covers.entry(id) {
                slot.insert(Cover::Loading);
                fetch.push(id);
            }
        }
        fetch
    }

    /// Stores the answer for `id`: `None` if there is no image. Answers for an earlier
    /// library (`generation` is not the current one) are ignored.
    pub fn set(&mut self, generation: u32, id: u32, image: Option<Vec<u8>>) {
        if generation != self.generation {
            return;
        }
        self.covers.insert(
            id,
            match image {
                Some(bytes) => Cover::Ready(Handle::from_bytes(bytes)),
                None => Cover::Missing,
            },
        );
        self.order.push_back(id);

        while self.order.len() > MAX_KEPT {
            if let Some(oldest) = self.order.pop_front() {
                self.covers.remove(&oldest);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn asks_for_each_cover_once() {
        let mut covers = Covers::default();
        assert_eq!(covers.claim_unrequested([3, 1, 3, 2]), [3, 1, 2]);
        assert_eq!(
            covers.claim_unrequested([1, 4]),
            [4],
            "1 is already being loaded"
        );

        covers.set(covers.generation(), 1, Some(vec![1, 2, 3]));
        covers.set(covers.generation(), 4, None);
        assert!(covers.get(1).is_some());
        assert!(covers.get(4).is_none(), "no image");
        assert!(
            covers.claim_unrequested([1, 4]).is_empty(),
            "answered, even if empty"
        );
    }

    #[test]
    fn drops_answers_for_a_closed_library() {
        let mut covers = Covers::default();
        let before = covers.generation();
        covers.claim_unrequested([1]);
        covers.clear();

        covers.set(before, 1, Some(vec![9]));
        assert!(covers.get(1).is_none());
        assert_eq!(
            covers.claim_unrequested([1]),
            [1],
            "the new library asks again"
        );
    }

    #[test]
    fn keeps_a_bounded_number_and_forgets_the_oldest() {
        let mut covers = Covers::default();
        let generation = covers.generation();
        for id in 0..(MAX_KEPT as u32 + 5) {
            covers.set(generation, id, Some(vec![0]));
        }
        assert!(covers.get(0).is_none() && covers.get(4).is_none());
        assert!(covers.get(5).is_some());
        assert!(covers.get(MAX_KEPT as u32 + 4).is_some());
        assert_eq!(
            covers.claim_unrequested([0]),
            [0],
            "an evicted cover can be fetched again"
        );
    }
}
