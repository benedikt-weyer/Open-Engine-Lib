//! What a long operation (a sync, the scan of a folder of music) has done so far, for the UI to ask about while it runs.
//!
//! The operation updates a `Progress` as it goes, in steps small enough to look smooth
//! (files are copied in chunks, not in one call), and `GetProgress` hands out a snapshot.

use std::fs::File;
use std::io::{self, Read, Write};
use std::path::Path;
use std::sync::{Arc, Mutex};

/// How much of a file is copied between two updates.
const CHUNK: usize = 1 << 20;

/// What the operation is busy with.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Phase {
    #[default]
    Idle,
    /// Making the copy of the other library's databases to work on.
    Preparing,
    /// Copying the audio files.
    CopyingTracks,
    /// Changing the copy: tracks, playlists, origins.
    Updating,
    /// Putting the databases in place.
    Writing,
    /// Copying the source's databases into the working copy.
    Pulling,
    /// Looking for the audio files of a folder (counted in `done_files`, no total).
    Scanning,
    /// Reading the tags and length of the files found.
    ReadingTags,
    /// Analysing a track (`done_bytes` of `total_bytes` is how far, in thousandths):
    /// decoding it, finding its beats, finding its key.
    AnalysingDecoding,
    AnalysingBeats,
    AnalysingKey,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Snapshot {
    pub running: bool,
    pub phase: Phase,
    /// In the current phase: bytes copied and bytes to copy (0 if the phase copies none).
    pub done_bytes: u64,
    pub total_bytes: u64,
    pub done_files: u32,
    pub total_files: u32,
    /// The file being copied.
    pub current: String,
}

#[derive(Clone, Default)]
pub struct Progress(Arc<Mutex<Snapshot>>);

impl Progress {
    fn update(&self, change: impl FnOnce(&mut Snapshot)) {
        change(&mut self.0.lock().expect("progress lock"));
    }

    pub fn snapshot(&self) -> Snapshot {
        self.0.lock().expect("progress lock").clone()
    }

    /// An operation starts. It is over when the returned guard is dropped, however it ends.
    pub fn begin(&self) -> Running {
        self.update(|state| {
            *state = Snapshot {
                running: true,
                ..Snapshot::default()
            };
        });
        Running(self.clone())
    }

    /// Like `begin`, but `None` if an operation is running already (which this one would
    /// confuse the progress of).
    pub fn try_begin(&self) -> Option<Running> {
        let mut state = self.0.lock().expect("progress lock");
        if state.running {
            return None;
        }
        *state = Snapshot {
            running: true,
            ..Snapshot::default()
        };
        Some(Running(self.clone()))
    }

    /// An analysis is in `phase` and has got this far, in thousandths of the whole.
    pub fn analysing(&self, phase: Phase, thousandths: u64, track: &str) {
        self.update(|state| {
            state.phase = phase;
            state.done_bytes = thousandths;
            state.total_bytes = 1000;
            state.current = track.to_owned();
        });
    }

    /// The next phase starts, with this much to copy.
    pub fn phase(&self, phase: Phase, total_bytes: u64, total_files: u32) {
        self.update(|state| {
            state.phase = phase;
            state.done_bytes = 0;
            state.total_bytes = total_bytes;
            state.done_files = 0;
            state.total_files = total_files;
            state.current.clear();
        });
    }

    /// More to do in the current phase than was said at its start (a file found on the way).
    pub fn grow(&self, bytes: u64, files: u32) {
        self.update(|state| {
            state.total_bytes += bytes;
            state.total_files += files;
        });
    }

    fn bytes(&self, copied: u64) {
        self.update(|state| state.done_bytes += copied);
    }

    fn started(&self, name: &str) {
        self.update(|state| state.current = name.to_owned());
    }

    pub fn file_done(&self) {
        self.update(|state| state.done_files += 1);
    }

    /// Copies `from` to `to` like `std::fs::copy`, telling the progress how it goes. With
    /// `counted` the file counts as one of the phase's files when it is done.
    pub fn copy(&self, from: &Path, to: &Path, counted: bool) -> io::Result<u64> {
        let name = from
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        self.started(&name);
        let mut input = File::open(from)?;
        let mut output = File::create(to)?;
        let mut buffer = vec![0u8; CHUNK];
        let mut total = 0;
        loop {
            let read = input.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            output.write_all(&buffer[..read])?;
            total += read as u64;
            self.bytes(read as u64);
        }
        output.flush()?;
        if counted {
            self.file_done();
        }
        Ok(total)
    }
}

/// Marks an operation as running until it is dropped.
pub struct Running(Progress);

impl Drop for Running {
    fn drop(&mut self) {
        self.0.update(|state| *state = Snapshot::default());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_one_operation_runs_at_a_time() {
        let progress = Progress::default();
        let first = progress.try_begin().expect("nothing is running");
        assert!(progress.try_begin().is_none(), "one is");
        progress.analysing(Phase::AnalysingBeats, 400, "Song");
        let seen = progress.snapshot();
        assert_eq!(
            (
                seen.phase,
                seen.done_bytes,
                seen.total_bytes,
                seen.current.as_str()
            ),
            (Phase::AnalysingBeats, 400, 1000, "Song")
        );
        drop(first);
        assert!(progress.try_begin().is_some(), "free again");
    }

    #[test]
    fn a_copy_is_counted_in_chunks_and_an_operation_ends_with_its_guard() {
        let folder =
            std::env::temp_dir().join(format!("open-engine-lib-progress-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&folder);
        std::fs::create_dir_all(&folder).unwrap();
        let from = folder.join("big.bin");
        let data = vec![7u8; CHUNK * 2 + 123];
        std::fs::write(&from, &data).unwrap();

        let progress = Progress::default();
        assert_eq!(progress.snapshot(), Snapshot::default());
        let running = progress.begin();
        assert!(progress.snapshot().running);

        progress.phase(Phase::CopyingTracks, data.len() as u64, 2);
        let copied = progress.copy(&from, &folder.join("one.bin"), true).unwrap();
        assert_eq!(copied, data.len() as u64);
        let seen = progress.snapshot();
        assert_eq!(
            (
                seen.phase,
                seen.done_bytes,
                seen.total_bytes,
                seen.done_files,
                seen.total_files
            ),
            (
                Phase::CopyingTracks,
                data.len() as u64,
                data.len() as u64,
                1,
                2
            )
        );
        assert_eq!(seen.current, "big.bin");
        assert_eq!(std::fs::read(folder.join("one.bin")).unwrap(), data);

        // A file that is not one of the phase's is copied without counting as one.
        progress
            .copy(&from, &folder.join("two.bin"), false)
            .unwrap();
        assert_eq!(progress.snapshot().done_files, 1);

        // A new phase starts from nothing.
        progress.phase(Phase::Writing, 10, 0);
        assert_eq!(progress.snapshot().done_bytes, 0);

        drop(running);
        assert_eq!(progress.snapshot(), Snapshot::default());
        let _ = std::fs::remove_dir_all(&folder);
    }
}
