//! Separating a track into four stems with BS-RoFormer (an ONNX export of ZFTurbo's 4-stem
//! MUSDB18-HQ checkpoint, run with ONNX Runtime): drums, bass, other and vocals, of which
//! "other" is the melody of Engine's stems.
//!
//! The export leaves the STFT out of the model: it takes the spectrogram of 8 seconds of
//! stereo audio and gives back a masked spectrogram per stem. So this does what `torch.stft`
//! and `torch.istft` do (n_fft 2048, hop 441, a periodic Hann window, centred with reflected
//! padding), cuts the track into chunks that overlap by half, and adds the chunks' stems up
//! again with a window that fades each in and out.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use ort::session::Session;
use ort::value::Tensor;
use realfft::num_complex::Complex32;
use realfft::{ComplexToReal, RealFftPlanner, RealToComplex};

/// The model's file, looked for in the models folders (see `analysis::model_dirs`).
pub const MODEL_FILE: &str = "bs_roformer_4stem_rhythm_fp16.onnx";
/// The sample rate the model was trained at.
pub const SAMPLE_RATE: u32 = 44_100;
/// The model's STFT.
const N_FFT: usize = 2048;
const HOP: usize = 441;
const BINS: usize = N_FFT / 2 + 1;
/// The model takes 8 seconds at a time: 801 frames of the STFT.
pub const CHUNK: usize = 352_800;
const FRAMES: usize = CHUNK / HOP + 1;
/// How far one chunk starts after the one before: they overlap by half.
const STEP: usize = CHUNK / 2;
/// How long the fade in and out of each chunk's window is.
const FADE: usize = CHUNK / 10;
/// The stems the model gives, in its order.
pub const MODEL_STEMS: usize = 4;
pub const DRUMS: usize = 0;
pub const BASS: usize = 1;
pub const OTHER: usize = 2;
pub const VOCALS: usize = 3;

/// Where ONNX Runtime's library is: `$OPEN_ENGINE_LIB_ONNXRUNTIME`, else the one the build
/// was told of (the Nix package names its own), else `libonnxruntime.so` on the library path.
fn onnxruntime() -> PathBuf {
    std::env::var_os("OPEN_ENGINE_LIB_ONNXRUNTIME")
        .map(PathBuf::from)
        .or_else(|| option_env!("OPEN_ENGINE_LIB_ONNXRUNTIME").map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from("libonnxruntime.so"))
}

/// ONNX Runtime, loaded once per process.
fn runtime() -> Result<&'static ort::environment::Environment, String> {
    static RUNTIME: std::sync::OnceLock<Result<ort::environment::Environment, String>> =
        std::sync::OnceLock::new();
    RUNTIME
        .get_or_init(|| {
            let path = onnxruntime();
            ort::init_from(&path)
                .map_err(|error| error.to_string())
                .and_then(|builder| builder.build().map_err(|error| error.to_string()))
                .map_err(|error| {
                    format!(
                        "ONNX Runtime is needed to separate stems, and {} could not be loaded: \
                         {error}",
                        path.display()
                    )
                })
        })
        .as_ref()
        .map_err(Clone::clone)
}

/// The periodic Hann window of `torch.hann_window(2048)`.
fn hann() -> Vec<f32> {
    (0..N_FFT)
        .map(|n| {
            let phase = 2.0 * std::f64::consts::PI * n as f64 / N_FFT as f64;
            (0.5 - 0.5 * phase.cos()) as f32
        })
        .collect()
}

/// The STFT and its inverse for one chunk, with their FFT plans and buffers.
struct Stft {
    window: Vec<f32>,
    forward: Arc<dyn RealToComplex<f32>>,
    inverse: Arc<dyn ComplexToReal<f32>>,
    /// Where the inverse is divided by: the sum of the squared windows at each sample.
    envelope: Vec<f32>,
}

impl Stft {
    fn new() -> Self {
        let mut planner = RealFftPlanner::<f32>::new();
        let window = hann();
        let mut envelope = vec![0.0f32; N_FFT + HOP * (FRAMES - 1)];
        for frame in 0..FRAMES {
            for (n, w) in window.iter().enumerate() {
                envelope[frame * HOP + n] += w * w;
            }
        }
        Self {
            forward: planner.plan_fft_forward(N_FFT),
            inverse: planner.plan_fft_inverse(N_FFT),
            window,
            envelope,
        }
    }

    /// The spectrogram of one channel of a chunk (`CHUNK` samples) as `torch.stft` with
    /// `center=True` makes it: real and imaginary parts, `[bin][frame]`.
    fn forward(&self, samples: &[f32], real: &mut [f32], imag: &mut [f32]) {
        debug_assert_eq!(samples.len(), CHUNK);
        let pad = N_FFT / 2;
        // Reflected at both ends, as torch pads.
        let at = |index: isize| -> f32 {
            let len = CHUNK as isize;
            let mut index = index - pad as isize;
            if index < 0 {
                index = -index;
            }
            if index >= len {
                index = 2 * (len - 1) - index;
            }
            samples[index as usize]
        };
        let mut frame = vec![0.0f32; N_FFT];
        let mut spectrum = vec![Complex32::default(); BINS];
        for t in 0..FRAMES {
            for (n, value) in frame.iter_mut().enumerate() {
                *value = at((t * HOP + n) as isize) * self.window[n];
            }
            self.forward
                .process(&mut frame, &mut spectrum)
                .expect("buffers of the planned size");
            for (bin, value) in spectrum.iter().enumerate() {
                real[bin * FRAMES + t] = value.re;
                imag[bin * FRAMES + t] = value.im;
            }
        }
    }

    /// The inverse, as `torch.istft` with `center=True` and `length=CHUNK`, added into `out`
    /// with `weight` for each sample.
    fn inverse(&self, real: &[f32], imag: &[f32], out: &mut [f32], weight: &[f32]) {
        let mut signal = vec![0.0f32; self.envelope.len()];
        let mut spectrum = vec![Complex32::default(); BINS];
        let mut frame = vec![0.0f32; N_FFT];
        for t in 0..FRAMES {
            for (bin, value) in spectrum.iter_mut().enumerate() {
                *value = Complex32::new(real[bin * FRAMES + t], imag[bin * FRAMES + t]);
            }
            // (The inverse of a real signal has no imaginary part at either end.)
            spectrum[0].im = 0.0;
            spectrum[BINS - 1].im = 0.0;
            self.inverse
                .process(&mut spectrum, &mut frame)
                .expect("buffers of the planned size");
            for (n, value) in frame.iter().enumerate() {
                signal[t * HOP + n] += value / N_FFT as f32 * self.window[n];
            }
        }
        let pad = N_FFT / 2;
        for (index, sample) in out.iter_mut().enumerate().take(CHUNK) {
            let envelope = self.envelope[index + pad];
            if envelope > 1e-11 {
                *sample += signal[index + pad] / envelope * weight[index];
            }
        }
    }
}

/// How much of each sample of a chunk counts: faded in and out over `FADE`, except at the
/// start of the first chunk and the end of the last, so nothing at the ends is lost.
fn chunk_weight(first: bool, last: bool) -> Vec<f32> {
    (0..CHUNK)
        .map(|n| {
            let fade_in = if first {
                1.0
            } else {
                ((n as f32 + 1.0) / FADE as f32).min(1.0)
            };
            let fade_out = if last {
                1.0
            } else {
                ((CHUNK - n) as f32 / FADE as f32).min(1.0)
            };
            fade_in * fade_out
        })
        .collect()
}

/// What a separation is doing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    Decoding,
    Separating,
    Writing,
}

/// The model's file in the first of `dirs` that has it.
pub fn find_model(dirs: &[PathBuf]) -> Option<PathBuf> {
    dirs.iter()
        .map(|dir| dir.join(MODEL_FILE))
        .find(|path| path.is_file())
}

/// Why stems could not be made.
#[derive(Debug)]
pub enum SeparateError {
    /// The track's file is not there.
    NotFound(PathBuf),
    /// The model or ONNX Runtime is not installed (the message says where they are looked
    /// for).
    Missing(String),
    Failed(String),
}

/// Decodes the track at `path` into stereo at 44.1 kHz, with ffmpeg.
fn decode(path: &Path) -> Result<[Vec<f32>; 2], SeparateError> {
    if !path.is_file() {
        return Err(SeparateError::NotFound(path.to_path_buf()));
    }
    let output = std::process::Command::new(crate::stems::ffmpeg())
        .args(["-nostdin", "-v", "error", "-i"])
        .arg(path)
        .args([
            "-map",
            "0:a:0",
            "-f",
            "f32le",
            "-acodec",
            "pcm_f32le",
            "-ac",
            "2",
            "-ar",
        ])
        .arg(SAMPLE_RATE.to_string())
        .arg("pipe:1")
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|error| SeparateError::Failed(format!("could not start ffmpeg: {error}")))?;
    if !output.status.success() {
        return Err(SeparateError::Failed(format!(
            "ffmpeg could not decode {}: {}",
            path.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    let samples: Vec<f32> = output
        .stdout
        .as_chunks::<4>()
        .0
        .iter()
        .map(|sample| f32::from_le_bytes(*sample))
        .collect();
    let (left, right) = samples
        .as_chunks::<2>()
        .0
        .iter()
        .map(|frame| (frame[0], frame[1]))
        .unzip();
    Ok([left, right])
}

/// Separates the track at `track` into stems and writes them to `out` as Engine's stems file,
/// encrypted with `key`. `progress` hears the stage and how far the whole job is, 0 to 1:
/// decoding is the first 5%, separating up to 95%, writing the rest; while separating, it
/// says whether to go on (false stops it, with `CANCELLED`).
pub fn generate(
    track: &Path,
    model_dirs: &[PathBuf],
    key: &crate::stems::Key,
    out: &Path,
    progress: &dyn Fn(Stage, f32) -> bool,
) -> Result<(), SeparateError> {
    let Some(model) = find_model(model_dirs) else {
        let dirs: Vec<String> = model_dirs
            .iter()
            .map(|dir| dir.display().to_string())
            .collect();
        return Err(SeparateError::Missing(format!(
            "the stems model {MODEL_FILE} is not installed; it is looked for in {}",
            dirs.join(", ")
        )));
    };
    let _ = progress(Stage::Decoding, 0.0);
    let mut separator = Separator::load(&model).map_err(SeparateError::Missing)?;
    let [left, right] = decode(track)?;
    if left.is_empty() {
        return Err(SeparateError::Failed(format!(
            "{} has no audio",
            track.display()
        )));
    }
    let _ = progress(Stage::Separating, 0.05);
    let mut separated = separator
        .separate(&left, &right, &|done| {
            progress(Stage::Separating, 0.05 + 0.9 * done)
        })
        .map_err(SeparateError::Failed)?;
    let _ = progress(Stage::Writing, 0.95);
    // Engine's order: vocals, bass, drums, melody (the model's "other").
    let take =
        |stems: &mut [[Vec<f32>; 2]; MODEL_STEMS], stem: usize| std::mem::take(&mut stems[stem]);
    let engine = [
        take(&mut separated, VOCALS),
        take(&mut separated, BASS),
        take(&mut separated, DRUMS),
        take(&mut separated, OTHER),
    ];
    crate::stems::write_stems(&engine, key, out).map_err(SeparateError::Failed)?;
    let _ = progress(Stage::Writing, 1.0);
    Ok(())
}

/// What a separation that nobody waits for any more fails with.
pub const CANCELLED: &str = "the separation was stopped";

/// The model, ready to run.
pub struct Separator {
    session: Session,
    stft: Stft,
}

impl Separator {
    pub fn load(model: &Path) -> Result<Self, String> {
        let runtime = runtime()?;
        let threads = std::thread::available_parallelism().map_or(4, usize::from);
        // On an NVIDIA GPU if ONNX Runtime was built for it (the Nix package's `cuda`), which
        // is many times as fast; else, and if it can't be used, on the CPU.
        let session = Session::builder(runtime)
            .map_err(|error| error.to_string())?
            .with_execution_providers([ort::ep::CUDA::default().build()])
            .map_err(|error| error.to_string())?
            .with_intra_threads(threads)
            .map_err(|error| error.to_string())?
            .commit_from_file(model)
            .map_err(|error| format!("could not load {}: {error}", model.display()))?;
        Ok(Self {
            session,
            stft: Stft::new(),
        })
    }

    /// The four stems of one chunk of stereo audio (`left` and `right`, `CHUNK` samples
    /// each), added into `stems[stem][channel]` with `weight`.
    fn chunk(
        &mut self,
        channels: [&[f32]; 2],
        weight: &[f32],
        stems: &mut [[Vec<f32>; 2]; MODEL_STEMS],
        at: usize,
    ) -> Result<(), String> {
        let plane = BINS * FRAMES;
        let mut real = vec![0.0f32; 2 * plane];
        let mut imag = vec![0.0f32; 2 * plane];
        for (channel, samples) in channels.iter().enumerate() {
            let range = channel * plane..(channel + 1) * plane;
            self.stft
                .forward(samples, &mut real[range.clone()], &mut imag[range]);
        }
        let shape = [1usize, 2, BINS, FRAMES];
        let real = Tensor::from_array((shape, real)).map_err(|error| error.to_string())?;
        let imag = Tensor::from_array((shape, imag)).map_err(|error| error.to_string())?;
        let outputs = self
            .session
            .run(ort::inputs!["spec_real" => real, "spec_imag" => imag])
            .map_err(|error| format!("the separation failed: {error}"))?;
        let (_, out_real) = outputs["out_spec_real"]
            .try_extract_tensor::<f32>()
            .map_err(|error| error.to_string())?;
        let (_, out_imag) = outputs["out_spec_imag"]
            .try_extract_tensor::<f32>()
            .map_err(|error| error.to_string())?;
        for (stem, channels) in stems.iter_mut().enumerate() {
            for (channel, out) in channels.iter_mut().enumerate() {
                let start = (stem * 2 + channel) * plane;
                self.stft.inverse(
                    &out_real[start..start + plane],
                    &out_imag[start..start + plane],
                    &mut out[at..at + CHUNK],
                    weight,
                );
            }
        }
        Ok(())
    }

    /// Separates a stereo track (`left` and `right` at 44.1 kHz) into the model's four stems,
    /// `[stem][channel]`, each as long as the track. `progress` hears how far it is, 0 to 1.
    pub fn separate(
        &mut self,
        left: &[f32],
        right: &[f32],
        progress: &dyn Fn(f32) -> bool,
    ) -> Result<[[Vec<f32>; 2]; MODEL_STEMS], String> {
        let len = left.len();
        // Room before the track so its start is in the middle of a chunk, and after it to fill
        // the last chunk.
        let before = CHUNK - STEP;
        let chunks = (before + len).saturating_sub(CHUNK).div_ceil(STEP) + 1;
        let padded = (chunks - 1) * STEP + CHUNK;
        let pad = |samples: &[f32]| {
            let mut out = vec![0.0f32; padded];
            out[before..before + samples.len()].copy_from_slice(samples);
            out
        };
        let (left, right) = (pad(left), pad(right));
        let mut stems: [[Vec<f32>; 2]; MODEL_STEMS] =
            std::array::from_fn(|_| [vec![0.0f32; padded], vec![0.0f32; padded]]);
        let mut total = vec![0.0f32; padded];
        for index in 0..chunks {
            let at = index * STEP;
            let weight = chunk_weight(index == 0, index + 1 == chunks);
            self.chunk(
                [&left[at..at + CHUNK], &right[at..at + CHUNK]],
                &weight,
                &mut stems,
                at,
            )?;
            for (sum, w) in total[at..at + CHUNK].iter_mut().zip(&weight) {
                *sum += w;
            }
            // (Asked whether to go on: nobody may be waiting any more.)
            if !progress((index + 1) as f32 / chunks as f32) {
                return Err(CANCELLED.to_owned());
            }
        }
        Ok(stems.map(|channels| {
            channels.map(|samples| {
                samples[before..before + len]
                    .iter()
                    .zip(&total[before..before + len])
                    .map(|(sample, weight)| if *weight > 0.0 { sample / weight } else { 0.0 })
                    .collect()
            })
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_stft_and_its_inverse_give_the_chunk_back() {
        let stft = Stft::new();
        let samples: Vec<f32> = (0..CHUNK)
            .map(|n| (n as f32 * 0.013).sin() * 0.5 + (n as f32 * 0.0007).cos() * 0.2)
            .collect();
        let plane = BINS * FRAMES;
        let (mut real, mut imag) = (vec![0.0; plane], vec![0.0; plane]);
        stft.forward(&samples, &mut real, &mut imag);
        let mut out = vec![0.0; CHUNK];
        stft.inverse(&real, &imag, &mut out, &vec![1.0; CHUNK]);
        let worst = samples
            .iter()
            .zip(&out)
            .map(|(a, b)| (a - b).abs())
            .fold(0.0f32, f32::max);
        assert!(worst < 1e-4, "{worst}");
    }

    #[test]
    fn the_chunks_cover_the_track_and_their_weights_add_up() {
        // (Without the model: what the chunking does with the weights.)
        for len in [1, CHUNK / 3, CHUNK, CHUNK + 1, 5 * CHUNK + 1234] {
            let before = CHUNK - STEP;
            let chunks = (before + len).saturating_sub(CHUNK).div_ceil(STEP) + 1;
            let padded = (chunks - 1) * STEP + CHUNK;
            assert!(padded >= before + len, "{len}");
            let mut total = vec![0.0f32; padded];
            for index in 0..chunks {
                let weight = chunk_weight(index == 0, index + 1 == chunks);
                for (sum, w) in total[index * STEP..index * STEP + CHUNK]
                    .iter_mut()
                    .zip(&weight)
                {
                    *sum += w;
                }
            }
            assert!(
                total[before..before + len].iter().all(|w| *w > 0.5),
                "{len}"
            );
        }
    }

    /// Times one chunk of the real model: `OPEN_ENGINE_LIB_MODELS` says where it is.
    #[test]
    #[ignore = "needs the model and ONNX Runtime"]
    fn one_chunk_of_the_real_model() {
        let dir = PathBuf::from(std::env::var("OPEN_ENGINE_LIB_MODELS").expect("set it"));
        let started = std::time::Instant::now();
        let mut separator = Separator::load(&dir.join(MODEL_FILE)).unwrap();
        eprintln!("loaded in {:?}", started.elapsed());
        let left: Vec<f32> = (0..CHUNK).map(|n| (n as f32 * 0.05).sin() * 0.3).collect();
        let started = std::time::Instant::now();
        let stems = separator.separate(&left, &left, &|_| true).unwrap();
        eprintln!("2 chunks in {:?}", started.elapsed());
        assert_eq!(stems[0][0].len(), CHUNK);
    }

    /// Makes the stems of a real track with the real key, all from the environment:
    /// `OPEN_ENGINE_LIB_MODELS`, `OPEN_ENGINE_LIB_STEMS_KEY`, `OPEN_ENGINE_LIB_TRACK` and
    /// where to write them, `OPEN_ENGINE_LIB_STEMS_OUT`.
    #[test]
    #[ignore = "needs the model, ONNX Runtime, Engine's key and a track"]
    fn generates_the_stems_of_a_real_track() {
        let var = |name: &str| std::env::var(name).unwrap_or_else(|_| panic!("set {name}"));
        let key = open_engine_api::stems_key::parse(&var("OPEN_ENGINE_LIB_STEMS_KEY")).unwrap();
        let started = std::time::Instant::now();
        generate(
            Path::new(&var("OPEN_ENGINE_LIB_TRACK")),
            &[PathBuf::from(var("OPEN_ENGINE_LIB_MODELS"))],
            &key,
            Path::new(&var("OPEN_ENGINE_LIB_STEMS_OUT")),
            &|stage, fraction| {
                eprintln!("{stage:?} {:.0}%", fraction * 100.0);
                true
            },
        )
        .map_err(|error| format!("{error:?}"))
        .unwrap();
        eprintln!("made in {:?}", started.elapsed());
    }
}
