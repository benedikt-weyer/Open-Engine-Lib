//! The analysis of a track: its beats and downbeats (Beat This!, a neural network, through
//! the `beat-this` crate) and its key (libKeyFinder). The audio is decoded once, and the
//! mono mix goes to both.
//!
//! The network's weights are files, not part of the executable: `mel_spectrogram.onnx` and
//! `beat_this.onnx` (or the smaller `beat_this_small.onnx`), looked for in the places
//! `model_dirs` lists. Without the `analysis` feature nothing here can run, and `analyze`
//! says so.

use std::path::{Path, PathBuf};

/// The front end of the network, which turns audio into a spectrogram.
pub const MEL_MODEL: &str = "mel_spectrogram.onnx";
/// The network that finds beats, the accurate one first.
pub const BEAT_MODELS: [&str; 2] = ["beat_this.onnx", "beat_this_small.onnx"];
/// The folder inside a data folder that holds the models.
const MODELS_FOLDER: &str = "models";
/// The folder that Open Engine Lib keeps its data in, inside a data folder.
const APP_FOLDER: &str = "open-engine-lib";

/// Where in the analysis it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    /// Decoding the file, while the network's front end listens to it.
    Decoding,
    /// The network finds the beats.
    Beats,
    /// libKeyFinder finds the key.
    Key,
}

/// Where each stage starts and ends in the share of the whole analysis (0 to 1). The network
/// takes most of the time.
fn span(stage: Stage) -> (f32, f32) {
    match stage {
        Stage::Decoding => (0.0, 0.3),
        Stage::Beats => (0.3, 0.85),
        Stage::Key => (0.85, 1.0),
    }
}

#[derive(Debug)]
pub enum Error {
    /// There is no file there.
    NotFound,
    /// The files of the network were not found in any of these folders.
    NoModels(Vec<PathBuf>),
    /// Decoding or analysing failed.
    Failed(String),
    /// This build has no analysis.
    #[cfg_attr(feature = "analysis", allow(dead_code))]
    Unavailable,
}

impl Error {
    #[cfg(feature = "analysis")]
    fn failed(error: impl std::fmt::Display) -> Self {
        Self::Failed(error.to_string())
    }
}

/// What the analysis found.
#[derive(Debug, Clone, PartialEq)]
pub struct Found {
    /// The time of every beat, in seconds.
    pub beats: Vec<f64>,
    /// The time of the beats that start a bar.
    pub downbeats: Vec<f64>,
    /// The sample rate of the file.
    pub sample_rate: u32,
    /// How many samples (per channel) the file has.
    pub frames: u64,
    /// libKeyFinder's key, 0 to 23 (see `track_analysis::key_name`); `None` if it heard none (silence).
    pub key: Option<u32>,
}

/// The folders to look for the models in, best first: `$OPEN_ENGINE_LIB_MODELS`, `models`
/// next to the executable, the `share` folder of the install the executable is in, and the
/// user's and the system's data folders.
pub fn model_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(dir) = std::env::var_os("OPEN_ENGINE_LIB_MODELS") {
        dirs.push(PathBuf::from(dir));
    }
    if let Some(exe) = std::env::current_exe().ok().and_then(|exe| {
        // (The path may be a link into a store: look next to the file itself as well.)
        exe.canonicalize().ok().or(Some(exe))
    }) && let Some(bin) = exe.parent()
    {
        dirs.push(bin.join(MODELS_FOLDER));
        if let Some(prefix) = bin.parent() {
            dirs.push(prefix.join("share").join(APP_FOLDER).join(MODELS_FOLDER));
        }
    }
    if let Some(data) = dirs::data_dir() {
        dirs.push(data.join(APP_FOLDER).join(MODELS_FOLDER));
    }
    if let Some(system) = std::env::var_os("XDG_DATA_DIRS") {
        dirs.extend(
            std::env::split_paths(&system).map(|dir| dir.join(APP_FOLDER).join(MODELS_FOLDER)),
        );
    }
    dirs
}

/// The front end and the beat network to use, from the first of `dirs` that has the front
/// end and a beat network.
pub fn find_models(dirs: &[PathBuf]) -> Option<(PathBuf, PathBuf)> {
    dirs.iter().find_map(|dir| {
        let mel = dir.join(MEL_MODEL);
        let beat = BEAT_MODELS
            .iter()
            .map(|name| dir.join(name))
            .find(|path| path.is_file())?;
        mel.is_file().then_some((mel, beat))
    })
}

/// Analyses the file at `path`. `progress` is told the stage and how far the whole analysis
/// has got (0 to 1, only ever up); it is called from several threads.
#[cfg(feature = "analysis")]
pub fn analyze(
    path: &Path,
    models: &[PathBuf],
    progress: &(dyn Fn(Stage, f32) + Sync),
) -> Result<Found, Error> {
    use std::fs::File;
    use std::io;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::{Duration, Instant};

    use beat_this::{BeatThis, RtenRuntime};
    use libkeyfinder_sys::{AudioData, KeyFinder, KeyFinderKey};
    use rodio::{Decoder, Source};

    /// Mono samples handed to the network at a time.
    const CHUNK: usize = 1 << 16;

    let (mel, beat) = find_models(models).ok_or_else(|| Error::NoModels(models.to_vec()))?;
    let file = File::open(path).map_err(|error| match error.kind() {
        io::ErrorKind::NotFound => Error::NotFound,
        _ => Error::failed(error),
    })?;
    let source = Decoder::try_from(file).map_err(Error::failed)?;
    let channels = usize::from(source.channels().get());
    let sample_rate = source.sample_rate().get();
    // (Not every format says how long it is; then the bar of this stage waits.)
    let expected_frames = source
        .total_duration()
        .map(|duration| duration.as_secs_f64() * f64::from(sample_rate));

    let report = |stage: Stage, within: f32| {
        let (from, to) = span(stage);
        progress(stage, from + (to - from) * within.clamp(0.0, 1.0));
    };
    report(Stage::Decoding, 0.0);

    let mut tracker = BeatThis::new(&RtenRuntime, &mel, &beat).map_err(Error::failed)?;
    let mut stream = tracker.stream(sample_rate).map_err(Error::failed)?;

    // Decoded once: the network listens as the samples come, libKeyFinder gets them all after.
    let mut mono = Vec::<f32>::new();
    let mut chunk = Vec::<f32>::with_capacity(CHUNK);
    let mut frame_sum = 0.0;
    let mut in_frame = 0;
    let mut frames: u64 = 0;
    for sample in source {
        frame_sum += sample;
        in_frame += 1;
        if in_frame < channels {
            continue;
        }
        chunk.push(frame_sum / channels as f32);
        (frame_sum, in_frame) = (0.0, 0);
        frames += 1;
        if chunk.len() == CHUNK {
            stream.push(&chunk).map_err(Error::failed)?;
            mono.extend_from_slice(&chunk);
            chunk.clear();
            if let Some(expected) = expected_frames {
                report(Stage::Decoding, (frames as f64 / expected) as f32);
            }
        }
    }
    if !chunk.is_empty() {
        stream.push(&chunk).map_err(Error::failed)?;
        mono.extend_from_slice(&chunk);
    }
    if frames == 0 {
        return Err(Error::Failed("the file has no audio".into()));
    }
    report(Stage::Decoding, 1.0);

    // The network is one call that says nothing while it works, so the bar creeps towards
    // the end of its share, slower the longer it takes, and never gets there before it is done.
    let seconds = frames as f64 / f64::from(sample_rate);
    let expected = Duration::from_secs_f64((seconds / 10.0).max(5.0));
    let done = AtomicBool::new(false);
    let analysis = std::thread::scope(|scope| {
        scope.spawn(|| {
            let started = Instant::now();
            while !done.load(Ordering::Relaxed) {
                let share = 1.0 - (-started.elapsed().as_secs_f32() / expected.as_secs_f32()).exp();
                report(Stage::Beats, share);
                std::thread::sleep(Duration::from_millis(200));
            }
        });
        let analysis = stream.finish();
        done.store(true, Ordering::Relaxed);
        analysis
    })
    .map_err(Error::failed)?;
    report(Stage::Beats, 1.0);

    report(Stage::Key, 0.0);
    let mut audio = AudioData::new();
    audio.set_frame_rate(sample_rate);
    audio.set_channels(1);
    audio.extend(mono);
    let key = match KeyFinder::new().key_of_audio(&audio) {
        KeyFinderKey::Silence => None,
        key => Some(key as u32),
    };
    report(Stage::Key, 1.0);

    Ok(Found {
        beats: analysis.beats.iter().map(|&time| f64::from(time)).collect(),
        downbeats: analysis
            .downbeats
            .iter()
            .map(|&time| f64::from(time))
            .collect(),
        sample_rate,
        frames,
        key,
    })
}

#[cfg(not(feature = "analysis"))]
pub fn analyze(_: &Path, _: &[PathBuf], _: &(dyn Fn(Stage, f32) + Sync)) -> Result<Found, Error> {
    Err(Error::Unavailable)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_first_folder_that_has_the_models_wins() {
        let root =
            std::env::temp_dir().join(format!("open-engine-audio-models-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let (empty, partial, small, full) = (
            root.join("empty"),
            root.join("partial"),
            root.join("small"),
            root.join("full"),
        );
        for dir in [&empty, &partial, &small, &full] {
            std::fs::create_dir_all(dir).unwrap();
        }
        // A beat network without the front end is no use.
        std::fs::write(partial.join(BEAT_MODELS[0]), b"x").unwrap();
        std::fs::write(small.join(MEL_MODEL), b"x").unwrap();
        std::fs::write(small.join(BEAT_MODELS[1]), b"x").unwrap();
        std::fs::write(full.join(MEL_MODEL), b"x").unwrap();
        std::fs::write(full.join(BEAT_MODELS[0]), b"x").unwrap();
        std::fs::write(full.join(BEAT_MODELS[1]), b"x").unwrap();

        assert!(find_models(&[empty.clone(), partial.clone()]).is_none());
        let (mel, beat) =
            find_models(&[empty.clone(), partial, small.clone(), full.clone()]).unwrap();
        assert_eq!(
            (mel, beat),
            (small.join(MEL_MODEL), small.join(BEAT_MODELS[1]))
        );
        // The accurate network is preferred where both are.
        let (_, beat) = find_models(std::slice::from_ref(&full)).unwrap();
        assert_eq!(beat, full.join(BEAT_MODELS[0]));
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Needs the models (`OPEN_ENGINE_LIB_MODELS`) and a file (`ANALYZE_FILE`, several
    /// separated by newlines): `cargo test -p open-engine-backend analysing -- --ignored --nocapture`.
    #[test]
    #[ignore = "needs the models and audio files"]
    fn analysing_a_real_file() {
        let files = std::env::var("ANALYZE_FILE").expect("ANALYZE_FILE");
        for file in files.lines() {
            let started = std::time::Instant::now();
            let found = analyze(Path::new(file), &model_dirs(), &|_, _| {}).unwrap();
            if std::env::var_os("ANALYZE_BEATS").is_some() {
                for pair in found.beats.windows(2) {
                    println!("beat {:.4} {:.4}", pair[0], pair[1] - pair[0]);
                }
            }
            println!(
                "{file}\t{}\t{} beats\t{} downbeats\t{:?}\t{:.1}s",
                found.sample_rate,
                found.beats.len(),
                found.downbeats.len(),
                found.key,
                started.elapsed().as_secs_f32()
            );
        }
    }
}
