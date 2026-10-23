//! What SoundSwitch has for the tracks of a library: whether it analysed a track and
//! whether lighting was rendered for it.
//!
//! SoundSwitch keeps its projects on the drive, in `<drive>/SoundSwitch/<name>.ssproj`
//! folders. Its format is not documented, so this is worked out from what such folders
//! hold, and is as far as it was possible to check:
//!
//! - `SoundSwitchTrackMap.bin` lists every track SoundSwitch knows by the path of its
//!   audio file as Windows saw it (`F:\Music\a.flac`), as UTF-16. For a track that was
//!   analysed, a little after the path comes a marker (`04 00 00 00 01`) and the 16 bytes
//!   of a GUID, little-endian.
//! - That GUID names the track's data in the folder, `{GUID}.ssfile`, which exists for
//!   every analysed track.
//! - Next to it, `{GUID}.ssa` exists for most of them. These are small and look compressed
//!   or encrypted. They are taken to be the rendered lighting ("autoscript"): they come in
//!   the same moment as the `.ssfile` of a track that was given lighting, and a project
//!   whose tracks were only analysed has none. The contents were not read, so this is an
//!   inference from the names and the counts.
//!
//! A track is matched to the library's by the path of its file relative to the drive.

use std::collections::HashMap;
use std::fs;
use std::path::{Component, Path, PathBuf};

use open_engine_api::v1::SoundSwitchState;

const FOLDER: &str = "SoundSwitch";
const PROJECT_SUFFIX: &str = ".ssproj";
const TRACK_MAP: &str = "SoundSwitchTrackMap.bin";
/// Marks a GUID in the track map: a length of 4 and a flag byte.
const GUID_MARKER: [u8; 5] = [4, 0, 0, 0, 1];
/// How far after a path to look for the marker of its GUID, in bytes. (It is 29 to 45 in
/// the maps seen.)
const GUID_WITHIN: usize = 128;
/// Longer than any path is.
const MAX_PATH_BYTES: usize = 4096;

/// What SoundSwitch has for one track.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Lighting {
    pub state: SoundSwitchState,
    /// The projects that have lighting for the track.
    pub projects: Vec<String>,
}

/// What the SoundSwitch projects of a drive have for each track, by the track's path
/// relative to the drive.
#[derive(Debug, Default)]
pub struct SoundSwitch {
    tracks: HashMap<String, Lighting>,
}

impl SoundSwitch {
    /// Reads the projects in `<drive>/SoundSwitch`. A drive without any, or a project that
    /// can't be read, simply has nothing to say about its tracks.
    pub fn scan(drive: &Path) -> Self {
        let mut tracks: HashMap<String, Lighting> = HashMap::new();
        let Ok(entries) = fs::read_dir(drive.join(FOLDER)) else {
            return Self::default();
        };
        let mut projects: Vec<PathBuf> = entries
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| {
                path.is_dir()
                    && path
                        .file_name()
                        .is_some_and(|name| name.to_string_lossy().ends_with(PROJECT_SUFFIX))
            })
            .collect();
        projects.sort();

        for project in projects {
            let name = project
                .file_name()
                .map(|name| {
                    name.to_string_lossy()
                        .trim_end_matches(PROJECT_SUFFIX)
                        .to_owned()
                })
                .unwrap_or_default();
            let Ok(map) = fs::read(project.join(TRACK_MAP)) else {
                continue;
            };
            for (path, guid) in parse_track_map(&map) {
                let file = |extension: &str| project.join(format!("{{{guid}}}.{extension}"));
                if !file("ssfile").is_file() {
                    continue; // a GUID that is not a track's data
                }
                let entry = tracks.entry(relative_key(&path)).or_default();
                if file("ssa").is_file() {
                    entry.state = SoundSwitchState::Rendered;
                    if !entry.projects.contains(&name) {
                        entry.projects.push(name.clone());
                    }
                } else if entry.state == SoundSwitchState::Unspecified {
                    entry.state = SoundSwitchState::Analysed;
                }
            }
        }
        Self { tracks }
    }

    /// What SoundSwitch has for the track whose file is `file`, given the drive's folder.
    pub fn lighting(&self, drive: &Path, file: &Path) -> Lighting {
        let relative = normalize(file)
            .strip_prefix(normalize(drive))
            .map(|relative| relative.to_string_lossy().replace('\\', "/").to_lowercase())
            .ok();
        relative
            .and_then(|key| self.tracks.get(&key))
            .cloned()
            .unwrap_or_default()
    }
}

/// The path that SoundSwitch recorded as a key: without the drive letter, with `/`, with
/// `.` and `..` worked out (a path through `D:\Engine Library\..\Music` is `Music`), and
/// in lower case, as Windows paths don't tell upper from lower case.
fn relative_key(windows_path: &str) -> String {
    let mut parts: Vec<&str> = Vec::new();
    let without_drive = windows_path.get(2..).unwrap_or_default();
    for part in without_drive.split(['\\', '/']) {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            part => parts.push(part),
        }
    }
    parts.join("/").to_lowercase()
}

/// `path` with `.` and `..` worked out, without asking the file system.
fn normalize(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::ParentDir => {
                normalized.pop();
            }
            Component::CurDir => {}
            other => normalized.push(other.as_os_str()),
        }
    }
    normalized
}

/// The paths in a track map, each with the GUID that follows it, if one does.
fn parse_track_map(map: &[u8]) -> Vec<(String, String)> {
    let mut found = Vec::new();
    let mut at = 0;
    while at + 6 <= map.len() {
        // A path starts with a drive letter, a colon and a backslash, in UTF-16.
        let start = &map[at..at + 6];
        let is_path = start[0].is_ascii_alphabetic() && start[1..] == [0, b':', 0, b'\\', 0];
        if !is_path {
            at += 1;
            continue;
        }

        // It runs to the first 16-bit zero (on an even offset from its start).
        let mut end = at;
        while end + 1 < map.len() && end - at < MAX_PATH_BYTES && map[end..end + 2] != [0, 0] {
            end += 2;
        }
        let units: Vec<u16> = map[at..end]
            .as_chunks::<2>()
            .0
            .iter()
            .map(|pair| u16::from_le_bytes(*pair))
            .collect();
        let path = String::from_utf16_lossy(&units);

        // Then, soon after, the GUID of the track's data.
        let window = &map[end..(end + GUID_WITHIN).min(map.len())];
        if let Some(marker) = window
            .windows(GUID_MARKER.len())
            .position(|w| w == GUID_MARKER)
        {
            let guid_at = marker + GUID_MARKER.len();
            if let Some(bytes) = window.get(guid_at..guid_at + 16) {
                found.push((path, guid_text(bytes)));
            }
        }
        at = end.max(at + 1);
    }
    found
}

/// A GUID stored the Windows way (the first three fields little-endian) as the upper-case
/// text its file is named with.
fn guid_text(bytes: &[u8]) -> String {
    format!(
        "{:08X}-{:04X}-{:04X}-{:02X}{:02X}-{}",
        u32::from_le_bytes(bytes[0..4].try_into().unwrap()),
        u16::from_le_bytes(bytes[4..6].try_into().unwrap()),
        u16::from_le_bytes(bytes[6..8].try_into().unwrap()),
        bytes[8],
        bytes[9],
        bytes[10..16]
            .iter()
            .map(|b| format!("{b:02X}"))
            .collect::<String>(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(name: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "open-engine-lib-soundswitch-{name}-{}",
                std::process::id()
            ));
            let _ = fs::remove_dir_all(&path);
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    const FIRST: &str = "4C5F1B0F-59AB-4715-BECC-9C498727C9DD";
    const SECOND: &str = "C0A44700-0FB3-407C-8764-B976C5259816";
    const THIRD: &str = "856AB892-8411-4443-8A36-87E237E9E659";

    fn guid_bytes(text: &str) -> [u8; 16] {
        let hex: String = text.chars().filter(|c| *c != '-').collect();
        let b: Vec<u8> = (0..16)
            .map(|i| u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).unwrap())
            .collect();
        // The first three fields are little-endian.
        [
            b[3], b[2], b[1], b[0], b[5], b[4], b[7], b[6], b[8], b[9], b[10], b[11], b[12], b[13],
            b[14], b[15],
        ]
    }

    /// A record as the track map has it: text around the path, the path, some bytes that
    /// are not understood, and the marker and GUID. (`guid` is left out for a track that
    /// was not analysed.)
    fn record(path: &str, guid: Option<&str>) -> Vec<u8> {
        let mut record = vec![1, 0, 0, 0];
        for text in ["Title", "Artist"] {
            record.extend((text.len() as u32 + 1).to_le_bytes());
            record.extend(text.encode_utf16().flat_map(u16::to_le_bytes));
            record.extend([0, 0, 1, 0, 0, 0]);
        }
        record.extend((path.encode_utf16().count() as u32 + 1).to_le_bytes());
        record.extend(path.encode_utf16().flat_map(u16::to_le_bytes));
        record.extend([0, 0]);
        record.extend([
            0x80, 0, 0, 0, 0xf8, 0x18, 3, 0, 1, 2, 0, 0, 0, 1, 1, 0, 0, 0, 0x40, 0xe4,
        ]);
        if let Some(guid) = guid {
            record.extend(GUID_MARKER);
            record.extend(guid_bytes(guid));
        }
        record.extend([1, 0, 0, 0]);
        record
    }

    /// A drive with SoundSwitch projects: `files` are the `.ssfile` and `.ssa` that exist.
    fn project(drive: &Path, name: &str, records: &[Vec<u8>], files: &[&str]) {
        let dir = drive.join(FOLDER).join(format!("{name}.ssproj"));
        fs::create_dir_all(&dir).unwrap();
        let mut map = vec![0xaa, 0xaa, 0x09, 0x55, 3, 0, 0, 0];
        for record in records {
            map.extend(record);
        }
        fs::write(dir.join(TRACK_MAP), map).unwrap();
        for file in files {
            fs::write(dir.join(file), b"x").unwrap();
        }
    }

    #[test]
    fn writes_guids_the_windows_way() {
        assert_eq!(guid_text(&guid_bytes(FIRST)), FIRST);
        assert_eq!(guid_text(&guid_bytes(SECOND)), SECOND);
    }

    #[test]
    fn finds_the_paths_in_a_track_map_with_the_guid_after_each() {
        let mut map = record(r"C:\Temp\Demo.mp4", Some(FIRST));
        map.extend(record(r"F:\Music\Sven Väth\a.flac", Some(SECOND)));
        map.extend(record(r"F:\Music\b.flac", None));
        let found = parse_track_map(&map);
        // A track with no GUID after its path (SoundSwitch has no data for it) gives nothing.
        assert_eq!(found.len(), 2);
        assert_eq!(found[0], (r"C:\Temp\Demo.mp4".to_owned(), FIRST.to_owned()));
        assert_eq!(
            found[1],
            (r"F:\Music\Sven Väth\a.flac".to_owned(), SECOND.to_owned())
        );
    }

    #[test]
    fn paths_are_compared_as_windows_does_without_the_drive() {
        assert_eq!(
            relative_key(r"F:\tidal-download\Techno\A.m4a"),
            "tidal-download/techno/a.m4a"
        );
        assert_eq!(
            relative_key(r"D:\Engine Library\..\tidal-download\Techno\A.m4a"),
            "tidal-download/techno/a.m4a",
            "a path through the library folder"
        );
        assert_eq!(relative_key(r"F:\a\.\b\..\c.flac"), "a/c.flac");
    }

    #[test]
    fn tells_lighting_from_a_track_that_was_only_analysed() {
        let temp = TempDir::new("states");
        let drive = &temp.0;
        project(
            drive,
            "Venue",
            &[
                record(r"F:\Music\lit.flac", Some(FIRST)),
                record(r"D:\Engine Library\..\Music\Sub\Plain.flac", Some(SECOND)),
                record(r"F:\Music\unknown.flac", None),
                // A GUID that names no data of the project is not a track's.
                record(r"F:\Music\stray.flac", Some(THIRD)),
            ],
            &[
                &format!("{{{FIRST}}}.ssfile"),
                &format!("{{{FIRST}}}.ssa"),
                &format!("{{{SECOND}}}.ssfile"),
            ],
        );

        let soundswitch = SoundSwitch::scan(drive);
        let state = |file: &str| soundswitch.lighting(drive, &drive.join(file));
        let lit = state("Music/lit.flac");
        assert_eq!(
            (lit.state, lit.projects),
            (SoundSwitchState::Rendered, vec!["Venue".to_owned()])
        );
        // Upper and lower case don't matter, and the Engine library's own `..` is no problem.
        assert_eq!(
            state("music/SUB/plain.FLAC").state,
            SoundSwitchState::Analysed
        );
        assert_eq!(
            state("Music/unknown.flac").state,
            SoundSwitchState::Unspecified
        );
        assert_eq!(
            state("Music/stray.flac").state,
            SoundSwitchState::Unspecified
        );
        assert_eq!(state("Music/never-heard-of.flac"), Lighting::default());
        // A file outside the drive can't be matched.
        assert_eq!(
            soundswitch.lighting(drive, Path::new("/elsewhere/lit.flac")),
            Lighting::default()
        );
    }

    #[test]
    fn lighting_in_any_project_counts_and_the_projects_are_named() {
        let temp = TempDir::new("projects");
        let drive = &temp.0;
        let track = record(r"F:\Music\a.flac", Some(FIRST));
        // In one project the track is only analysed, in the other it has lighting.
        project(
            drive,
            "Analysed only",
            std::slice::from_ref(&track),
            &[&format!("{{{FIRST}}}.ssfile")],
        );
        project(
            drive,
            "Lit",
            &[track],
            &[&format!("{{{FIRST}}}.ssfile"), &format!("{{{FIRST}}}.ssa")],
        );

        let lighting = SoundSwitch::scan(drive).lighting(drive, &drive.join("Music/a.flac"));
        assert_eq!(lighting.state, SoundSwitchState::Rendered);
        assert_eq!(lighting.projects, ["Lit"]);
    }

    #[test]
    fn a_drive_without_soundswitch_or_with_a_broken_map_has_nothing_to_say() {
        let temp = TempDir::new("none");
        let none = SoundSwitch::scan(&temp.0);
        assert_eq!(
            none.lighting(&temp.0, &temp.0.join("a.flac")),
            Lighting::default()
        );

        // A map that is cut off in the middle of a path or garbage must not panic.
        let drive = TempDir::new("broken");
        let full = record(r"F:\Music\a.flac", Some(FIRST));
        project(&drive.0, "Cut", &[full[..full.len() / 2].to_vec()], &[]);
        project(
            &drive.0,
            "Garbage",
            &[vec![0xff; 5000], b"F\0:\0\\\0".to_vec()],
            &[],
        );
        let broken = SoundSwitch::scan(&drive.0);
        assert_eq!(
            broken.lighting(&drive.0, &drive.0.join("Music/a.flac")),
            Lighting::default()
        );
    }
}
