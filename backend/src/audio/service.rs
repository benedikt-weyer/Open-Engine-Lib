//! The gRPC services of the audio process, which only the library process talks to:
//! playback and the analysis of audio files.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use open_engine_api::API_VERSION;
use open_engine_api::v1 as api;
use open_engine_api::v1::audio_service_server::AudioService;
use open_engine_api::v1::backend_service_server::BackendService;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status};

use crate::analysis::{self, Stage};
use crate::engine::{Command, Engine, ErrorKind, QueueItem, State};
use crate::midi;
use crate::player::Start;
use crate::waveform::{self, AnalyzeError, Gains, Peaks, Split};

/// How many MIDI messages wait for the library process before the ones of a busy control
/// (a jog wheel) are dropped.
const MIDI_EVENTS_BUFFERED: usize = 256;

/// How many progress messages wait for the library process.
const ANALYSIS_EVENTS_BUFFERED: usize = 64;

/// Waveforms are kept for the tracks used last, so skipping back to a track is instant.
const WAVEFORMS_KEPT: usize = 8;

#[derive(Default)]
struct WaveformCache {
    /// Paths, oldest first.
    order: VecDeque<WaveformKey>,
    waveforms: HashMap<WaveformKey, Arc<Peaks>>,
}

/// A waveform depends on where the bands were split, not just on the file.
type WaveformKey = (PathBuf, Split);

impl WaveformCache {
    fn insert(&mut self, key: WaveformKey, waveform: Arc<Peaks>) {
        if self.waveforms.insert(key.clone(), waveform).is_none() {
            self.order.push_back(key);
        }
        while self.order.len() > WAVEFORMS_KEPT {
            if let Some(oldest) = self.order.pop_front() {
                self.waveforms.remove(&oldest);
            }
        }
    }
}

struct Shared {
    waveforms: Mutex<WaveformCache>,
    engine: Engine,
}

#[derive(Clone)]
pub struct Service(Arc<Shared>);

impl Service {
    pub fn new(engine: Engine) -> Self {
        Self(Arc::new(Shared {
            waveforms: Mutex::new(WaveformCache::default()),
            engine,
        }))
    }

    async fn command(&self, command: Command) -> Result<Response<api::PlaybackStatus>, Status> {
        Ok(Response::new(status_message(
            self.0.engine.send(command).await,
        )))
    }
}

/// The file to read: the one at `path`, else the one at `fallback` (a track's saved origin),
/// else `path` again, so that the error is about it.
fn resolve(path: &str, fallback: &str) -> PathBuf {
    let path = PathBuf::from(path);
    if !fallback.is_empty() && !path.is_file() {
        let fallback = Path::new(fallback);
        if fallback.is_file() {
            return fallback.to_path_buf();
        }
    }
    path
}

#[tonic::async_trait]
impl BackendService for Service {
    async fn get_info(
        &self,
        _: Request<api::GetInfoRequest>,
    ) -> Result<Response<api::GetInfoResponse>, Status> {
        Ok(Response::new(api::GetInfoResponse {
            backend_version: env!("CARGO_PKG_VERSION").to_owned(),
            supported_api_versions: vec![API_VERSION],
        }))
    }
}

#[tonic::async_trait]
impl AudioService for Service {
    async fn play(
        &self,
        request: Request<api::AudioPlayRequest>,
    ) -> Result<Response<api::PlaybackStatus>, Status> {
        let request = request.into_inner();
        let queue: Vec<QueueItem> = request
            .queue
            .into_iter()
            .map(|entry| QueueItem {
                track_id: entry.track_id,
                path: PathBuf::from(entry.path),
                fallback: (!entry.fallback_path.is_empty())
                    .then(|| PathBuf::from(entry.fallback_path)),
                streaming: entry.streaming,
            })
            .collect();

        let position = request.position as usize;
        if position >= queue.len() {
            return Err(Status::invalid_argument(format!(
                "position {position} is outside the queue of {} tracks",
                queue.len()
            )));
        }

        let start = Start {
            paused: request.start_paused,
            at: Duration::from_millis(request.start_position_ms),
        };
        self.command(Command::Play {
            queue,
            position,
            start,
        })
        .await
    }

    async fn toggle_pause(
        &self,
        _: Request<api::TogglePauseRequest>,
    ) -> Result<Response<api::PlaybackStatus>, Status> {
        self.command(Command::TogglePause).await
    }

    async fn stop(
        &self,
        _: Request<api::StopRequest>,
    ) -> Result<Response<api::PlaybackStatus>, Status> {
        self.command(Command::Stop).await
    }

    async fn next(
        &self,
        _: Request<api::NextRequest>,
    ) -> Result<Response<api::PlaybackStatus>, Status> {
        self.command(Command::Next).await
    }

    async fn previous(
        &self,
        _: Request<api::PreviousRequest>,
    ) -> Result<Response<api::PlaybackStatus>, Status> {
        self.command(Command::Previous).await
    }

    async fn seek(
        &self,
        request: Request<api::SeekRequest>,
    ) -> Result<Response<api::PlaybackStatus>, Status> {
        let to = Duration::from_millis(request.into_inner().position_ms);
        self.command(Command::Seek(to)).await
    }

    async fn begin_scratch(
        &self,
        _: Request<api::BeginScratchRequest>,
    ) -> Result<Response<api::PlaybackStatus>, Status> {
        // Decoding the stretch to scratch takes a moment, and the engine's thread does it,
        // so the answer says whether it worked.
        self.command(Command::ScratchBegin).await
    }

    async fn move_scratch(
        &self,
        request: Request<api::MoveScratchRequest>,
    ) -> Result<Response<api::PlaybackStatus>, Status> {
        self.command(Command::ScratchMove(request.into_inner().delta_seconds))
            .await
    }

    async fn end_scratch(
        &self,
        _: Request<api::EndScratchRequest>,
    ) -> Result<Response<api::PlaybackStatus>, Status> {
        self.command(Command::ScratchEnd).await
    }

    async fn get_status(
        &self,
        _: Request<api::GetStatusRequest>,
    ) -> Result<Response<api::PlaybackStatus>, Status> {
        // Asked of the engine, not read from what it published last: that can be stale.
        self.command(Command::Poll).await
    }

    type WatchMidiStream = ReceiverStream<Result<api::MidiEvent, Status>>;

    async fn watch_midi(
        &self,
        _: Request<api::WatchMidiRequest>,
    ) -> Result<Response<Self::WatchMidiStream>, Status> {
        let (events, receiver) = mpsc::channel(MIDI_EVENTS_BUFFERED);
        // The thread ends when the call is dropped, which drops the receiver.
        std::thread::Builder::new()
            .name("midi".into())
            .spawn(move || midi::listen(events))
            .map_err(|error| Status::internal(format!("could not listen to MIDI: {error}")))?;
        Ok(Response::new(ReceiverStream::new(receiver)))
    }

    type AnalyzeTrackStream = ReceiverStream<Result<api::AudioAnalyzeTrackEvent, Status>>;

    async fn analyze_track(
        &self,
        request: Request<api::AudioAnalyzeTrackRequest>,
    ) -> Result<Response<Self::AnalyzeTrackStream>, Status> {
        let request = request.into_inner();
        let path = resolve(&request.path, &request.fallback_path);
        let (events, receiver) = mpsc::channel(ANALYSIS_EVENTS_BUFFERED);
        // Decoding and the network take seconds to minutes: a thread of their own, which
        // stops when the call is dropped (its sends fail, and the progress goes nowhere).
        std::thread::Builder::new()
            .name("analysis".into())
            .spawn(move || {
                let progress = |stage: Stage, fraction: f32| {
                    let stage = match stage {
                        Stage::Decoding => api::AudioAnalysisStage::Decoding,
                        Stage::Beats => api::AudioAnalysisStage::Beats,
                        Stage::Key => api::AudioAnalysisStage::Key,
                    };
                    // A full buffer only drops a step of the bar.
                    let _ = events.try_send(Ok(api::AudioAnalyzeTrackEvent {
                        event: Some(api::audio_analyze_track_event::Event::Progress(
                            api::AudioAnalysisProgress {
                                stage: stage.into(),
                                fraction,
                            },
                        )),
                    }));
                };
                let last = match analysis::analyze(&path, &analysis::model_dirs(), &progress) {
                    Ok(found) => Ok(api::AudioAnalyzeTrackEvent {
                        event: Some(api::audio_analyze_track_event::Event::Found(
                            api::AudioFound {
                                beats: found.beats,
                                downbeats: found.downbeats,
                                sample_rate: found.sample_rate,
                                frames: found.frames,
                                key: found.key,
                            },
                        )),
                    }),
                    Err(analysis::Error::NotFound) => Err(Status::not_found(format!(
                        "{} does not exist",
                        path.display()
                    ))),
                    Err(analysis::Error::NoModels(searched)) => {
                        Err(Status::failed_precondition(no_models_message(&searched)))
                    }
                    Err(analysis::Error::Unavailable) => Err(Status::failed_precondition(
                        "this build of Open Engine Lib has no track analysis",
                    )),
                    Err(analysis::Error::Failed(error)) => Err(Status::internal(error)),
                };
                // (Sent after the last progress, even if the buffer was full for a moment.)
                let _ = events.blocking_send(last);
            })
            .map_err(|error| Status::internal(format!("could not start the analysis: {error}")))?;
        Ok(Response::new(ReceiverStream::new(receiver)))
    }

    async fn close(
        &self,
        _: Request<api::CloseRequest>,
    ) -> Result<Response<api::PlaybackStatus>, Status> {
        self.command(Command::Close).await
    }

    async fn analyze_waveform(
        &self,
        request: Request<api::AnalyzeWaveformRequest>,
    ) -> Result<Response<api::AnalyzeWaveformResponse>, Status> {
        let request = request.into_inner();
        let split = Split {
            low_mid_hz: request.low_mid_hz.unwrap_or(waveform::DEFAULT_LOW_MID_HZ),
            mid_high_hz: request.mid_high_hz.unwrap_or(waveform::DEFAULT_MID_HIGH_HZ),
        };
        split.validate().map_err(Status::invalid_argument)?;
        let defaults = Gains::default();
        let gains = Gains {
            low: request.gain_low.unwrap_or(defaults.low),
            mid: request.gain_mid.unwrap_or(defaults.mid),
            high: request.gain_high.unwrap_or(defaults.high),
        };
        gains.validate().map_err(Status::invalid_argument)?;
        let path = resolve(&request.path, &request.fallback_path);

        let shared = Arc::clone(&self.0);
        // Decoding a whole track takes a moment and must not block the async threads.
        let peaks = tokio::task::spawn_blocking(move || {
            let cached = shared
                .waveforms
                .lock()
                .expect("waveform lock")
                .waveforms
                .get(&(path.clone(), split))
                .cloned();
            match cached {
                Some(waveform) => Ok(waveform),
                None => {
                    let waveform = Arc::new(waveform::analyze_file(&path, split).map_err(
                        |error| match error {
                            AnalyzeError::NotFound => {
                                Status::not_found(format!("{} does not exist", path.display()))
                            }
                            AnalyzeError::Failed(error) => Status::internal(error),
                        },
                    )?);
                    shared
                        .waveforms
                        .lock()
                        .expect("waveform lock")
                        .insert((path, split), Arc::clone(&waveform));
                    Ok::<_, Status>(waveform)
                }
            }
        })
        .await
        .map_err(|error| Status::internal(error.to_string()))??;

        Ok(Response::new(api::AnalyzeWaveformResponse {
            duration_seconds: peaks.duration_seconds,
            columns_per_second: waveform::COLUMNS_PER_SECOND,
            columns: peaks.scaled(gains).iter().flatten().copied().collect(),
        }))
    }
}

/// What is said when the files of the beat network are not installed.
fn no_models_message(searched: &[PathBuf]) -> String {
    let folders: Vec<String> = searched
        .iter()
        .map(|folder| folder.display().to_string())
        .collect();
    format!(
        "the models of Beat This! ({}, and {} or {}) were not found in: {}",
        analysis::MEL_MODEL,
        analysis::BEAT_MODELS[0],
        analysis::BEAT_MODELS[1],
        folders.join(", ")
    )
}

fn status_message(status: crate::engine::Status) -> api::PlaybackStatus {
    api::PlaybackStatus {
        state: match status.state {
            State::Stopped => api::PlaybackState::Stopped,
            State::Playing => api::PlaybackState::Playing,
            State::Paused => api::PlaybackState::Paused,
        }
        .into(),
        queue_position: status.queue_position,
        track_id: status.track_id,
        position_ms: status.position.as_millis() as u64,
        error: status.error.map(|error| api::PlaybackError {
            kind: match error.kind {
                ErrorKind::NotFound => api::PlaybackErrorKind::NotFound,
                ErrorKind::Streaming => api::PlaybackErrorKind::Streaming,
                ErrorKind::Failed => api::PlaybackErrorKind::Failed,
            }
            .into(),
            track_id: error.track_id,
            detail: error.detail,
        }),
        scratching: status.scratching,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_file_missing_at_its_path_is_read_from_the_fallback() {
        let folder =
            std::env::temp_dir().join(format!("open-engine-audio-resolve-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&folder);
        std::fs::create_dir_all(&folder).unwrap();
        let (location, origin) = (folder.join("location.flac"), folder.join("origin.flac"));
        let (location_text, origin_text) =
            (location.display().to_string(), origin.display().to_string());

        // Neither file is there: the error is about the path.
        assert_eq!(resolve(&location_text, &origin_text), location);
        assert_eq!(resolve(&location_text, ""), location);
        // Only the fallback is there, then both, then only the path.
        std::fs::write(&origin, b"o").unwrap();
        assert_eq!(resolve(&location_text, &origin_text), origin);
        assert_eq!(resolve(&location_text, ""), location);
        std::fs::write(&location, b"l").unwrap();
        assert_eq!(resolve(&location_text, &origin_text), location);
        std::fs::remove_file(&origin).unwrap();
        assert_eq!(resolve(&location_text, &origin_text), location);
        let _ = std::fs::remove_dir_all(&folder);
    }
}
