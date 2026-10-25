//! The order the files of a folder of music are put in when it is imported as a playlist:
//! by name, where the digits in a name count either as text ("strict": 1, 10, 11, 2) or as
//! numbers ("natural": 1, 2, 10, 11).
//!
//! In both a folder's own files come before the folders in it, as in the scan of a folder
//! (`folder.rs`), and capitals do not matter. Strict is the order the scan itself uses.

use std::cmp::Ordering;
use std::iter::Peekable;
use std::path::Path;
use std::str::Chars;

use open_engine_api::v1 as api;

use crate::library::{Playlist, Track};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum NameOrder {
    #[default]
    Strict,
    Natural,
}

impl NameOrder {
    pub fn from_api(order: i32) -> Self {
        match api::NameOrder::try_from(order) {
            Ok(api::NameOrder::Natural) => Self::Natural,
            _ => Self::Strict,
        }
    }
}

/// How two names compare. Names that are the same for the order (`a02` and `a2` are, in
/// the natural one) fall back to the strict comparison so the result is always the same.
pub fn compare(a: &str, b: &str, order: NameOrder) -> Ordering {
    let (a, b) = (a.to_lowercase(), b.to_lowercase());
    match order {
        NameOrder::Strict => a.cmp(&b),
        NameOrder::Natural => natural(&a, &b).then_with(|| a.cmp(&b)),
    }
}

fn natural(a: &str, b: &str) -> Ordering {
    let (mut a, mut b) = (a.chars().peekable(), b.chars().peekable());
    loop {
        match (a.peek().copied(), b.peek().copied()) {
            (None, None) => return Ordering::Equal,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(x), Some(y)) if x.is_ascii_digit() && y.is_ascii_digit() => {
                let ordering = number(digits(&mut a), digits(&mut b));
                if ordering != Ordering::Equal {
                    return ordering;
                }
            }
            (Some(x), Some(y)) => {
                if x != y {
                    return x.cmp(&y);
                }
                a.next();
                b.next();
            }
        }
    }
}

/// The run of digits at the front, without the zeros before the number.
fn digits(chars: &mut Peekable<Chars<'_>>) -> String {
    let mut run = String::new();
    while let Some(digit) = chars.next_if(char::is_ascii_digit) {
        run.push(digit);
    }
    run.trim_start_matches('0').to_owned()
}

/// Numbers of any length: the longer one is the bigger (there are no zeros before them).
fn number(a: String, b: String) -> Ordering {
    a.len().cmp(&b.len()).then_with(|| a.cmp(&b))
}

/// The tracks `wanted` (indices into `tracks`) in the order of their paths in the folder.
pub fn ordered(tracks: &[Track], wanted: &[usize], order: NameOrder) -> Vec<usize> {
    let mut wanted = wanted.to_vec();
    wanted.sort_by(|&a, &b| {
        compare_paths(
            Path::new(&tracks[a].relative_path),
            Path::new(&tracks[b].relative_path),
            order,
        )
    });
    wanted
}

/// Level by level: at each a file comes before a folder, then they go by name.
fn compare_paths(a: &Path, b: &Path, order: NameOrder) -> Ordering {
    let names = |path: &Path| -> Vec<String> {
        path.components()
            .map(|part| part.as_os_str().to_string_lossy().into_owned())
            .collect()
    };
    let (a, b) = (names(a), names(b));
    for (level, (x, y)) in a.iter().zip(&b).enumerate() {
        let (x_is_folder, y_is_folder) = (level + 1 < a.len(), level + 1 < b.len());
        let ordering = x_is_folder
            .cmp(&y_is_folder)
            .then_with(|| compare(x, y, order));
        if ordering != Ordering::Equal {
            return ordering;
        }
    }
    a.len().cmp(&b.len())
}

/// The path of the folder `index` of the playlists below the folder that was scanned, with
/// a `/` at its end (empty for no folder at all), to take from its tracks' paths.
pub fn folder_prefix(playlists: &[Playlist], index: usize) -> String {
    let mut names = Vec::new();
    let mut at = Some(index);
    while let Some(playlist) = at.and_then(|at| playlists.get(at)) {
        names.push(playlist.title.as_str());
        at = playlist.parent;
    }
    names.reverse();
    names.iter().map(|name| format!("{name}/")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sorted(names: &[&str], order: NameOrder) -> Vec<String> {
        let mut names: Vec<String> = names.iter().map(|name| (*name).to_owned()).collect();
        names.sort_by(|a, b| compare(a, b, order));
        names
    }

    const NAMES: [&str; 6] = [
        "10. Ten.mp3",
        "2. Two.mp3",
        "1. One.mp3",
        "11. Eleven.mp3",
        "3. Three.mp3",
        "01. Intro.mp3",
    ];

    #[test]
    fn strict_goes_by_the_characters_and_natural_by_the_numbers() {
        assert_eq!(
            sorted(&NAMES, NameOrder::Strict),
            [
                "01. Intro.mp3",
                "1. One.mp3",
                "10. Ten.mp3",
                "11. Eleven.mp3",
                "2. Two.mp3",
                "3. Three.mp3"
            ]
        );
        assert_eq!(
            sorted(&NAMES, NameOrder::Natural),
            [
                "01. Intro.mp3",
                "1. One.mp3",
                "2. Two.mp3",
                "3. Three.mp3",
                "10. Ten.mp3",
                "11. Eleven.mp3"
            ]
        );
    }

    #[test]
    fn natural_numbers_can_be_any_length_and_capitals_do_not_matter() {
        let big = "t99999999999999999999999.mp3";
        let bigger = "t100000000000000000000000.mp3";
        assert_eq!(compare(big, bigger, NameOrder::Natural), Ordering::Less);
        assert_eq!(compare(bigger, big, NameOrder::Strict), Ordering::Less);
        assert_eq!(
            sorted(&["b2", "A10", "a9"], NameOrder::Natural),
            ["a9", "A10", "b2"]
        );
        // The same number written with zeros before it: stable, not equal.
        assert_ne!(compare("a02", "a2", NameOrder::Natural), Ordering::Equal);
    }

    #[test]
    fn a_folders_files_come_before_the_folders_in_it() {
        let paths = [
            "a/1.mp3", "10.mp3", "2.mp3", "a/10.mp3", "a/2.mp3", "A2/x.mp3",
        ];
        let mut paths: Vec<&Path> = paths.iter().map(Path::new).collect();
        paths.sort_by(|a, b| compare_paths(a, b, NameOrder::Natural));
        let paths: Vec<_> = paths.iter().map(|path| path.to_str().unwrap()).collect();
        assert_eq!(
            paths,
            [
                "2.mp3", "10.mp3", "a/1.mp3", "a/2.mp3", "a/10.mp3", "A2/x.mp3"
            ]
        );
    }
}
