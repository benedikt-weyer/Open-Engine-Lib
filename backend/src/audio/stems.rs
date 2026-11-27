//! Playing Engine's `.stems` files: one MP4 with a single 8-channel AAC stream, four stereo
//! pairs (the stems), whose samples Engine encrypts (AES-128 in ECB mode, each sample padded
//! to whole blocks with PKCS#7; see docs/engine-library-format.md). The key is the user's to
//! give: Open Engine Lib does not come with it.
//!
//! [`decrypt`] turns a stems file into a plain MP4, and [`StemsSource`] plays that with
//! ffmpeg (no Rust decoder takes an AAC stream of 8 channels laid out by a program config
//! element), mixing the four pairs into stereo at the loudness [`Gains`] says, which can
//! change while it plays.

use std::fs;
use std::io::{self, Read};
use std::num::NonZero;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::Duration;

use aes::Aes128;
use aes::cipher::{BlockDecrypt, BlockEncrypt, KeyInit, generic_array::GenericArray};
use rodio::Source;
use rodio::source::SeekError;

/// How many stems a file has: four stereo pairs.
pub const STEMS: usize = 4;
/// The channels of a stems file.
const CHANNELS: usize = STEMS * 2;
/// The sample rate Engine writes stems at.
pub const SAMPLE_RATE: u32 = 44_100;
/// How many frames (of all eight channels) ffmpeg's output is handed on in.
const CHUNK_FRAMES: usize = 2048;
/// How many chunks are decoded ahead of what is heard (about 1.5 seconds).
const CHUNKS_AHEAD: usize = 32;
/// A sample whose padding is wrong is one the key did not decrypt. A few could be chance;
/// more than this share of them means the key does not fit the file.
const BAD_PADDING_SHARE: usize = 100;

pub type Key = [u8; 16];

/// Where ffmpeg is: `$OPEN_ENGINE_LIB_FFMPEG`, else the one the build was told of (the Nix
/// package names its own), else `ffmpeg` on the `PATH`.
pub fn ffmpeg() -> PathBuf {
    std::env::var_os("OPEN_ENGINE_LIB_FFMPEG")
        .map(PathBuf::from)
        .or_else(|| option_env!("OPEN_ENGINE_LIB_FFMPEG").map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from("ffmpeg"))
}

// ---------------------------------------------------------------------------------------
// Decrypting

/// A box of an MP4 file: its type and where its header starts, its content starts and it
/// ends.
#[derive(Debug, Clone, Copy)]
struct Mp4Box {
    kind: [u8; 4],
    start: usize,
    body: usize,
    end: usize,
}

fn read_u32(data: &[u8], at: usize) -> Result<u32, String> {
    data.get(at..at + 4)
        .map(|bytes| u32::from_be_bytes(bytes.try_into().expect("4 bytes")))
        .ok_or_else(|| "the stems file is cut short".to_owned())
}

fn read_u64(data: &[u8], at: usize) -> Result<u64, String> {
    data.get(at..at + 8)
        .map(|bytes| u64::from_be_bytes(bytes.try_into().expect("8 bytes")))
        .ok_or_else(|| "the stems file is cut short".to_owned())
}

/// The boxes between `start` and `end`.
fn boxes(data: &[u8], mut start: usize, end: usize) -> Result<Vec<Mp4Box>, String> {
    let mut found = Vec::new();
    while start + 8 <= end {
        let size = read_u32(data, start)? as usize;
        let kind: [u8; 4] = data[start + 4..start + 8].try_into().expect("4 bytes");
        let (body, size) = match size {
            0 => (start + 8, end - start),
            1 => (start + 16, read_u64(data, start + 8)? as usize),
            size => (start + 8, size),
        };
        let box_end = start
            .checked_add(size)
            .filter(|&box_end| box_end <= end && body <= box_end);
        let Some(box_end) = box_end else {
            return Err("the stems file is not a valid MP4 file".to_owned());
        };
        found.push(Mp4Box {
            kind,
            start,
            body,
            end: box_end,
        });
        start = box_end;
    }
    Ok(found)
}

fn child(data: &[u8], parent: Mp4Box, kind: &[u8; 4]) -> Result<Option<Mp4Box>, String> {
    Ok(boxes(data, parent.body, parent.end)?
        .into_iter()
        .find(|found| &found.kind == kind))
}

fn need(found: Option<Mp4Box>, what: &str) -> Result<Mp4Box, String> {
    found.ok_or_else(|| format!("the stems file has no {what} box"))
}

/// Where the sample table of the file's (first) track says its samples are.
struct Samples {
    /// The byte size of each sample, in order, and where its size is written (`stsz`).
    sizes: Vec<usize>,
    sizes_at: usize,
    /// Where each chunk starts and how many samples it has, and where its start is written
    /// (`stco`, or `co64` with 8 bytes for each).
    chunks: Vec<(usize, usize)>,
    chunks_at: usize,
    wide_offsets: bool,
}

fn samples(data: &[u8], moov: Mp4Box) -> Result<Samples, String> {
    let trak = need(child(data, moov, b"trak")?, "trak")?;
    let mdia = need(child(data, trak, b"mdia")?, "mdia")?;
    let minf = need(child(data, mdia, b"minf")?, "minf")?;
    let stbl = need(child(data, minf, b"stbl")?, "stbl")?;

    let stsz = need(child(data, stbl, b"stsz")?, "stsz")?;
    if read_u32(data, stsz.body + 4)? != 0 {
        return Err(
            "the samples of the stems file all have one size, which Engine does not write"
                .to_owned(),
        );
    }
    let count = read_u32(data, stsz.body + 8)? as usize;
    let sizes_at = stsz.body + 12;
    let sizes = (0..count)
        .map(|index| read_u32(data, sizes_at + index * 4).map(|size| size as usize))
        .collect::<Result<Vec<_>, _>>()?;

    let (offsets, wide_offsets) = match child(data, stbl, b"stco")? {
        Some(stco) => (stco, false),
        None => (need(child(data, stbl, b"co64")?, "stco")?, true),
    };
    let chunk_count = read_u32(data, offsets.body + 4)? as usize;
    let chunks_at = offsets.body + 8;
    let starts = (0..chunk_count)
        .map(|index| {
            if wide_offsets {
                read_u64(data, chunks_at + index * 8).map(|at| at as usize)
            } else {
                read_u32(data, chunks_at + index * 4).map(|at| at as usize)
            }
        })
        .collect::<Result<Vec<_>, _>>()?;

    // Runs of chunks with the same number of samples: (first chunk, counting from 1, samples).
    let stsc = need(child(data, stbl, b"stsc")?, "stsc")?;
    let runs = (0..read_u32(data, stsc.body + 4)? as usize)
        .map(|index| {
            let at = stsc.body + 8 + index * 12;
            Ok((
                read_u32(data, at)? as usize,
                read_u32(data, at + 4)? as usize,
            ))
        })
        .collect::<Result<Vec<_>, String>>()?;
    let mut chunks = Vec::with_capacity(starts.len());
    let mut left = sizes.len();
    for (index, start) in starts.into_iter().enumerate() {
        let per_chunk = runs
            .iter()
            .take_while(|(first, _)| *first <= index + 1)
            .last()
            .map_or(0, |(_, samples)| *samples)
            .min(left);
        left -= per_chunk;
        chunks.push((start, per_chunk));
    }
    if left != 0 {
        return Err("the sample table of the stems file does not add up".to_owned());
    }

    Ok(Samples {
        sizes,
        sizes_at,
        chunks,
        chunks_at,
        wide_offsets,
    })
}

/// The sample without its PKCS#7 padding, or `None` if the padding is not right.
fn unpad(sample: &[u8]) -> Option<&[u8]> {
    let padding = usize::from(*sample.last()?);
    if padding == 0 || padding > 16 || padding > sample.len() {
        return None;
    }
    let (audio, pad) = sample.split_at(sample.len() - padding);
    pad.iter()
        .all(|&byte| usize::from(byte) == padding)
        .then_some(audio)
}

/// The MP4 `data` with every sample of its track replaced by what `each` makes of it, and the
/// sizes and starts of the samples changed to match. The samples are written in order, one
/// chunk after the other, and what comes after the audio moves by as much as the audio grew
/// or shrank.
fn rewrite_samples(
    data: &[u8],
    mut each: impl FnMut(&[u8]) -> Result<Vec<u8>, String>,
) -> Result<Vec<u8>, String> {
    let top = boxes(data, 0, data.len())?;
    let mdat = need(
        top.iter().copied().find(|found| &found.kind == b"mdat"),
        "mdat",
    )?;
    let moov = need(
        top.iter().copied().find(|found| &found.kind == b"moov"),
        "moov",
    )?;
    let table = samples(data, moov)?;

    let mut audio = Vec::with_capacity(mdat.end - mdat.body);
    let mut sizes = Vec::with_capacity(table.sizes.len());
    let mut starts = Vec::with_capacity(table.chunks.len());
    let mut sample = 0;
    for &(start, count) in &table.chunks {
        starts.push(audio.len());
        let mut at = start;
        for &size in &table.sizes[sample..sample + count] {
            let bytes = data
                .get(at..at + size)
                .filter(|_| at >= mdat.body && at + size <= mdat.end)
                .ok_or("a sample of the stems file is outside its audio")?;
            let changed = each(bytes)?;
            sizes.push(changed.len());
            audio.extend_from_slice(&changed);
            at += size;
        }
        sample += count;
    }

    let header = 8;
    let new_mdat_len = header + audio.len();
    let moved = |at: usize| -> usize {
        if at >= mdat.end {
            at - (mdat.end - mdat.start) + new_mdat_len
        } else {
            at
        }
    };
    let audio_start = mdat.start + header;
    let mut out = Vec::with_capacity(data.len() + 16 * sizes.len());
    out.extend_from_slice(&data[..mdat.start]);
    out.extend_from_slice(
        &u32::try_from(new_mdat_len)
            .map_err(|_| "the stems file is too big")?
            .to_be_bytes(),
    );
    out.extend_from_slice(b"mdat");
    out.extend_from_slice(&audio);
    out.extend_from_slice(&data[mdat.end..]);

    for (index, size) in sizes.iter().enumerate() {
        let at = moved(table.sizes_at + index * 4);
        out[at..at + 4].copy_from_slice(&(*size as u32).to_be_bytes());
    }
    for (index, start) in starts.iter().enumerate() {
        let start = audio_start + start;
        if table.wide_offsets {
            let at = moved(table.chunks_at + index * 8);
            out[at..at + 8].copy_from_slice(&(start as u64).to_be_bytes());
        } else {
            let at = moved(table.chunks_at + index * 4);
            let start = u32::try_from(start).map_err(|_| "the stems file is too big")?;
            out[at..at + 4].copy_from_slice(&start.to_be_bytes());
        }
    }
    Ok(out)
}

/// The stems file `data` with its samples decrypted with `key`: a plain MP4 that ffmpeg
/// plays. Fails if it is not a stems file or the key does not fit it.
pub fn decrypt(data: &[u8], key: &Key) -> Result<Vec<u8>, String> {
    let cipher = Aes128::new(GenericArray::from_slice(key));
    let (mut bad, mut count) = (0, 0);
    let plain = rewrite_samples(data, |encrypted| {
        if encrypted.len() % 16 != 0 {
            return Err("the stems file is not encrypted the way Engine encrypts it".to_owned());
        }
        let mut blocks = encrypted.to_vec();
        for block in blocks.as_chunks_mut::<16>().0 {
            cipher.decrypt_block(GenericArray::from_mut_slice(block));
        }
        count += 1;
        match unpad(&blocks) {
            Some(audio) => Ok(audio.to_vec()),
            None => {
                bad += 1;
                Ok(blocks)
            }
        }
    })?;
    if bad * BAD_PADDING_SHARE > count.max(1) {
        return Err("the stems key does not fit this file".to_owned());
    }
    Ok(plain)
}

/// The plain MP4 `data` with its samples encrypted as Engine encrypts its stems: each padded
/// to whole blocks (PKCS#7) and encrypted with `key` in ECB mode.
pub fn encrypt(data: &[u8], key: &Key) -> Result<Vec<u8>, String> {
    let cipher = Aes128::new(GenericArray::from_slice(key));
    rewrite_samples(data, |sample| {
        let padding = 16 - sample.len() % 16;
        let mut blocks = sample.to_vec();
        blocks.extend(std::iter::repeat_n(padding as u8, padding));
        for block in blocks.as_chunks_mut::<16>().0 {
            cipher.encrypt_block(GenericArray::from_mut_slice(block));
        }
        Ok(blocks)
    })
}

/// Writes `data` to `path` beside it first and renames it there, so that the file is never
/// half written.
fn write_whole(path: &Path, data: &[u8]) -> Result<(), String> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).map_err(|error| format!("{}: {error}", dir.display()))?;
    }
    let partial = path.with_extension("partial");
    fs::write(&partial, data).map_err(|error| format!("{}: {error}", partial.display()))?;
    fs::rename(&partial, path).map_err(|error| format!("{}: {error}", path.display()))
}

/// Where ffmpeg 4.4 is, which Engine encodes its stems with, and whose AAC encoder writes the
/// same channel layout (CPE and SCE in front, a CPE at the side, CPE and SCE at the back):
/// `$OPEN_ENGINE_LIB_FFMPEG4`, else the one the build was told of, else `ffmpeg` on the
/// `PATH` (whose layout may differ).
fn ffmpeg4() -> PathBuf {
    std::env::var_os("OPEN_ENGINE_LIB_FFMPEG4")
        .map(PathBuf::from)
        .or_else(|| option_env!("OPEN_ENGINE_LIB_FFMPEG4").map(PathBuf::from))
        .unwrap_or_else(ffmpeg)
}

/// Writes four stereo stems (`[stem][channel]`, 44.1 kHz, in Engine's order: vocals, bass,
/// drums, melody) to `out` as Engine writes a stems file: 8-channel AAC-LC at 640 kbit/s
/// (ffmpeg's octagonal layout, which is how the pairs are laid out), every sample
/// encrypted with `key`.
pub fn write_stems(stems: &[[Vec<f32>; 2]; STEMS], key: &Key, out: &Path) -> Result<(), String> {
    use std::io::Write;

    let frames = stems[0][0].len();
    let dir = out.parent().unwrap_or(Path::new("."));
    fs::create_dir_all(dir).map_err(|error| format!("{}: {error}", dir.display()))?;
    let plain = out.with_extension("plain.m4a");
    let mut process = Command::new(ffmpeg4())
        .args(["-nostdin", "-v", "error", "-y", "-f", "f32le", "-ar"])
        .arg(SAMPLE_RATE.to_string())
        .args(["-ac", "8", "-channel_layout", "octagonal", "-i", "pipe:0"])
        .args(["-c:a", "aac", "-b:a", "640k"])
        .arg(&plain)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| match error.kind() {
            io::ErrorKind::NotFound => {
                "ffmpeg is needed to write stems, and it was not found".to_owned()
            }
            _ => format!("could not start ffmpeg: {error}"),
        })?;
    let mut input = process.stdin.take().expect("stdin is piped");
    // (Fed from a thread, so that ffmpeg's messages can't fill up and hold it.)
    let feeding = {
        let mut bytes = Vec::with_capacity(frames * CHANNELS * 4);
        for frame in 0..frames {
            for stem in stems {
                for channel in stem {
                    bytes.extend_from_slice(&channel[frame].to_le_bytes());
                }
            }
        }
        thread::spawn(move || input.write_all(&bytes))
    };
    let output = process
        .wait_with_output()
        .map_err(|error| format!("ffmpeg: {error}"))?;
    let fed = feeding.join().expect("the feeding thread does not panic");
    if !output.status.success() || fed.is_err() {
        let _ = fs::remove_file(&plain);
        return Err(format!(
            "ffmpeg could not encode the stems: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let data = fs::read(&plain).map_err(|error| format!("{}: {error}", plain.display()));
    let _ = fs::remove_file(&plain);
    write_whole(out, &encrypt(&data?, key)?)
}

/// Decrypts the stems file at `stems` into `out` (see [`decrypt`]). Written beside it and
/// renamed, so that a decoder still reading the old file keeps reading that one.
pub fn decrypt_file(stems: &Path, key: &Key, out: &Path) -> Result<(), String> {
    let data = fs::read(stems).map_err(|error| format!("{}: {error}", stems.display()))?;
    write_whole(out, &decrypt(&data, key)?)
}

// ---------------------------------------------------------------------------------------
// Playing

/// How loud each stem is (0 silent, 1 as separated), shared between the engine, which
/// changes it, and the source, which reads it for every frame.
#[derive(Debug, Clone)]
pub struct Gains(Arc<[AtomicU32; STEMS]>);

impl Default for Gains {
    fn default() -> Self {
        Self(Arc::new(std::array::from_fn(|_| {
            AtomicU32::new(1.0f32.to_bits())
        })))
    }
}

impl Gains {
    pub fn set(&self, gains: [f32; STEMS]) {
        for (gain, value) in self.0.iter().zip(gains) {
            gain.store(value.to_bits(), Ordering::Relaxed);
        }
    }

    fn get(&self) -> [f32; STEMS] {
        std::array::from_fn(|index| f32::from_bits(self.0[index].load(Ordering::Relaxed)))
    }
}

/// One frame of the eight channels, mixed into left and right.
fn mix(frame: &[f32], gains: &[f32; STEMS]) -> (f32, f32) {
    let mut left = 0.0;
    let mut right = 0.0;
    for (pair, gain) in frame.as_chunks::<2>().0.iter().zip(gains) {
        left += pair[0] * gain;
        right += pair[1] * gain;
    }
    (left, right)
}

/// An ffmpeg decoding the file from a position, and the chunks of its output.
struct Decoder {
    process: Child,
    chunks: Receiver<Vec<f32>>,
}

impl Decoder {
    fn start(file: &Path, at: Duration) -> Result<Self, String> {
        let mut process = Command::new(ffmpeg())
            .args(["-nostdin", "-v", "quiet", "-ss"])
            .arg(format!("{:.3}", at.as_secs_f64()))
            .arg("-i")
            .arg(file)
            .args(["-map", "0:a:0", "-f", "f32le", "-acodec", "pcm_f32le"])
            .args([
                "-ac",
                &CHANNELS.to_string(),
                "-ar",
                &SAMPLE_RATE.to_string(),
            ])
            .arg("pipe:1")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|error| match error.kind() {
                io::ErrorKind::NotFound => {
                    "ffmpeg is needed to play stems, and it was not found".to_owned()
                }
                _ => format!("could not start ffmpeg: {error}"),
            })?;
        let mut output = process.stdout.take().expect("stdout is piped");
        let (sender, chunks) = mpsc::sync_channel(CHUNKS_AHEAD);
        // Ends when ffmpeg has written all (or is killed) or the source is gone.
        thread::Builder::new()
            .name("stems".into())
            .spawn(move || {
                let mut bytes = vec![0; CHUNK_FRAMES * CHANNELS * 4];
                loop {
                    let mut filled = 0;
                    while filled < bytes.len() {
                        match output.read(&mut bytes[filled..]) {
                            Ok(0) => break,
                            Ok(read) => filled += read,
                            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                            Err(_) => break,
                        }
                    }
                    let whole = filled - filled % (CHANNELS * 4);
                    let chunk: Vec<f32> = bytes[..whole]
                        .as_chunks::<4>()
                        .0
                        .iter()
                        .map(|sample| f32::from_le_bytes(*sample))
                        .collect();
                    if (!chunk.is_empty() && sender.send(chunk).is_err()) || filled < bytes.len() {
                        return;
                    }
                }
            })
            .map_err(|error| format!("could not start decoding the stems: {error}"))?;
        Ok(Self { process, chunks })
    }
}

impl Drop for Decoder {
    fn drop(&mut self) {
        let _ = self.process.kill();
        let _ = self.process.wait();
    }
}

/// Plays a decrypted stems file in stereo, its stems as loud as [`Gains`] says.
pub struct StemsSource {
    file: PathBuf,
    decoder: Decoder,
    gains: Gains,
    /// The chunk being played and how far into it.
    chunk: Vec<f32>,
    at: usize,
    /// The right sample of the frame whose left one was handed out last.
    right: Option<f32>,
}

impl StemsSource {
    /// Starts decoding `file` (a decrypted stems file) at `at`, and waits for the first of it,
    /// so that it does not hold up the audio output and an ffmpeg that can't read the file is
    /// found out here.
    pub fn open(file: &Path, at: Duration, gains: Gains) -> Result<Self, String> {
        let decoder = Decoder::start(file, at)?;
        let chunk = decoder
            .chunks
            .recv()
            .map_err(|_| "ffmpeg could not decode the stems".to_owned())?;
        Ok(Self {
            file: file.to_path_buf(),
            decoder,
            gains,
            chunk,
            at: 0,
            right: None,
        })
    }
}

impl Iterator for StemsSource {
    type Item = rodio::Sample;

    fn next(&mut self) -> Option<Self::Item> {
        if let Some(right) = self.right.take() {
            return Some(right);
        }
        while self.at + CHANNELS > self.chunk.len() {
            self.chunk = self.decoder.chunks.recv().ok()?;
            self.at = 0;
        }
        let (left, right) = mix(&self.chunk[self.at..self.at + CHANNELS], &self.gains.get());
        self.at += CHANNELS;
        self.right = Some(right);
        Some(left)
    }
}

impl Source for StemsSource {
    fn current_span_len(&self) -> Option<usize> {
        None
    }

    fn channels(&self) -> rodio::ChannelCount {
        NonZero::new(2).expect("two")
    }

    fn sample_rate(&self) -> rodio::SampleRate {
        NonZero::new(SAMPLE_RATE).expect("not zero")
    }

    fn total_duration(&self) -> Option<Duration> {
        None
    }

    fn try_seek(&mut self, to: Duration) -> Result<(), SeekError> {
        let decoder = Decoder::start(&self.file, to)
            .map_err(|error| SeekError::Other(Arc::new(io::Error::other(error))))?;
        self.decoder = decoder;
        self.chunk.clear();
        self.at = 0;
        self.right = None;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Not Engine's key: a made-up one, to encrypt made-up files with.
    const TEST_KEY: Key = *b"not-engines-key!";

    fn mp4_box(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let mut out = ((body.len() + 8) as u32).to_be_bytes().to_vec();
        out.extend_from_slice(kind);
        out.extend_from_slice(body);
        out
    }

    fn full_box(kind: &[u8; 4], fields: &[u32]) -> Vec<u8> {
        let mut body = vec![0; 4];
        for field in fields {
            body.extend_from_slice(&field.to_be_bytes());
        }
        mp4_box(kind, &body)
    }

    fn encrypt_sample(sample: &[u8]) -> Vec<u8> {
        let cipher = Aes128::new(GenericArray::from_slice(&TEST_KEY));
        let padding = 16 - sample.len() % 16;
        let mut blocks = sample.to_vec();
        blocks.extend(std::iter::repeat_n(padding as u8, padding));
        for block in blocks.as_chunks_mut::<16>().0 {
            cipher.encrypt_block(GenericArray::from_mut_slice(block));
        }
        blocks
    }

    /// A file laid out as Engine writes them (`ftyp`, `free`, `mdat`, `moov`), with the
    /// samples in two chunks of 2 and 1, encrypted with `encrypt`.
    fn stems_file(samples: &[&[u8]], encrypt: impl Fn(&[u8]) -> Vec<u8>) -> Vec<u8> {
        let head = [
            mp4_box(b"ftyp", b"isom\0\0\x02\0isomiso2mp41"),
            mp4_box(b"free", b""),
        ]
        .concat();
        let encrypted: Vec<Vec<u8>> = samples.iter().map(|sample| encrypt(sample)).collect();
        let audio = encrypted.concat();
        let mdat_start = head.len();
        let first = (mdat_start + 8) as u32;
        let second = first + (encrypted[0].len() + encrypted[1].len()) as u32;

        let mut sizes = vec![0, encrypted.len() as u32];
        sizes.extend(encrypted.iter().map(|sample| sample.len() as u32));
        let stbl = mp4_box(
            b"stbl",
            &[
                full_box(b"stsz", &sizes),
                full_box(b"stsc", &[2, 1, 2, 1, 2, 1, 1]),
                full_box(b"stco", &[2, first, second]),
            ]
            .concat(),
        );
        let moov = mp4_box(
            b"moov",
            &mp4_box(b"trak", &mp4_box(b"mdia", &mp4_box(b"minf", &stbl))),
        );
        [head, mp4_box(b"mdat", &audio), moov].concat()
    }

    fn sample_bytes(file: &[u8]) -> Vec<Vec<u8>> {
        let top = boxes(file, 0, file.len()).unwrap();
        let moov = top
            .iter()
            .copied()
            .find(|found| &found.kind == b"moov")
            .unwrap();
        let table = samples(file, moov).unwrap();
        let mut out = Vec::new();
        let mut sample = 0;
        for &(start, count) in &table.chunks {
            let mut at = start;
            for &size in &table.sizes[sample..sample + count] {
                out.push(file[at..at + size].to_vec());
                at += size;
            }
            sample += count;
        }
        out
    }

    #[test]
    fn decrypting_gives_back_the_samples_and_a_table_that_finds_them() {
        // One sample that fills whole blocks, so that it gets a whole block of padding.
        let samples: [&[u8]; 3] = [b"first sample", &[7; 32], b"x"];
        let file = stems_file(&samples, encrypt_sample);
        assert_ne!(sample_bytes(&file)[0], samples[0]);

        let plain = decrypt(&file, &TEST_KEY).unwrap();
        assert_eq!(sample_bytes(&plain), samples.map(<[u8]>::to_vec));
        // The audio shrank by the padding, and the boxes still add up.
        assert_eq!(plain.len(), file.len() - (4 + 16 + 15));
        assert!(boxes(&plain, 0, plain.len()).is_ok());
    }

    #[test]
    fn encrypting_does_what_engine_does_and_decrypting_undoes_it() {
        let samples: [&[u8]; 3] = [b"first sample", &[7; 32], b"x"];
        let plain = stems_file(&samples, <[u8]>::to_vec);
        let encrypted = encrypt(&plain, &TEST_KEY).unwrap();
        assert_eq!(encrypted, stems_file(&samples, encrypt_sample));
        assert_eq!(decrypt(&encrypted, &TEST_KEY).unwrap(), plain);
    }

    #[test]
    fn written_stems_play_back_each_in_its_own_pair() {
        if eight_channel_file("probe.m4a").is_none() {
            return;
        }
        // Two seconds, each stem a tone of its own loudness.
        let frames = 2 * SAMPLE_RATE as usize;
        let stems: [[Vec<f32>; 2]; STEMS] = std::array::from_fn(|stem| {
            let tone: Vec<f32> = (0..frames)
                .map(|n| {
                    (stem as f32 + 1.0)
                        * 0.05
                        * (n as f32 * 2.0 * std::f32::consts::PI * 1000.0 / 44_100.0).sin()
                })
                .collect();
            [tone.clone(), tone]
        });
        let dir = std::env::temp_dir().join(format!("open-engine-stems-{}", std::process::id()));
        let file = dir.join("written.stems");
        write_stems(&stems, &TEST_KEY, &file).unwrap();
        let plain = dir.join("written.m4a");
        decrypt_file(&file, &TEST_KEY, &plain).unwrap();

        for stem in 0..STEMS {
            let gains = Gains::default();
            let mut alone = [0.0; STEMS];
            alone[stem] = 1.0;
            gains.set(alone);
            let mut source = StemsSource::open(&plain, Duration::ZERO, gains).unwrap();
            left_loudness(&mut source, 4410);
            let loudness = left_loudness(&mut source, 4410);
            let expected = (stem as f32 + 1.0) * 0.05 / 2f32.sqrt();
            assert!(
                (loudness - expected).abs() < 0.01,
                "stem {stem}: {loudness} not {expected}"
            );
        }
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn a_key_that_does_not_fit_is_found_out() {
        let file = stems_file(&[b"first sample", b"second", b"third"], encrypt_sample);
        let error = decrypt(&file, b"some other key!!").unwrap_err();
        assert!(error.contains("does not fit"), "{error}");
    }

    #[test]
    fn a_file_that_is_not_encrypted_or_not_an_mp4_is_refused() {
        let plain = stems_file(&[b"first sample", b"second", b"third"], <[u8]>::to_vec);
        assert!(decrypt(&plain, &TEST_KEY).is_err());
        assert!(decrypt(b"not an mp4 at all", &TEST_KEY).is_err());
        assert!(decrypt(&[], &TEST_KEY).is_err());
    }

    #[test]
    fn the_stems_are_mixed_at_their_gains() {
        let frame = [1.0, 2.0, 10.0, 20.0, 100.0, 200.0, 1000.0, 2000.0];
        assert_eq!(mix(&frame, &[1.0; STEMS]), (1111.0, 2222.0));
        assert_eq!(mix(&frame, &[0.0, 1.0, 0.0, 0.5]), (510.0, 1020.0));

        let gains = Gains::default();
        assert_eq!(gains.get(), [1.0; STEMS]);
        gains.clone().set([0.0, 0.25, 1.0, 2.0]);
        assert_eq!(gains.get(), [0.0, 0.25, 1.0, 2.0]);
    }

    /// A plain 8-channel AAC file made by ffmpeg, two seconds of a 1 kHz tone at a different
    /// loudness in each channel, or `None` without ffmpeg (then the test has nothing to run).
    fn eight_channel_file(name: &str) -> Option<PathBuf> {
        let dir = std::env::temp_dir().join(format!("open-engine-stems-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let file = dir.join(name);
        let tones: Vec<String> = (1..=CHANNELS)
            .map(|channel| format!("{}*sin(2*PI*1000*t)", channel as f32 * 0.05))
            .collect();
        let made = Command::new(ffmpeg())
            .args(["-nostdin", "-v", "quiet", "-y", "-f", "lavfi", "-i"])
            .arg(format!("aevalsrc={}:s=44100:d=2:c=7.1", tones.join("|")))
            .args(["-c:a", "aac", "-b:a", "640k"])
            .arg(&file)
            .status();
        match made {
            Ok(status) if status.success() => Some(file),
            _ => {
                eprintln!("no ffmpeg: not testing the decoding");
                None
            }
        }
    }

    fn left_loudness(source: &mut StemsSource, frames: usize) -> f32 {
        let squares: f32 = (0..frames)
            .map(|_| {
                let left = source.next().unwrap();
                source.next().unwrap();
                left * left
            })
            .sum();
        (squares / frames as f32).sqrt()
    }

    #[test]
    fn ffmpeg_decodes_the_stems_and_they_are_mixed_and_moved_in_while_playing() {
        let Some(file) = eight_channel_file("mix.m4a") else {
            return;
        };
        let gains = Gains::default();
        gains.set([1.0, 0.0, 0.0, 0.0]);
        let mut source = StemsSource::open(&file, Duration::ZERO, gains.clone()).unwrap();
        // (Past the encoder's start.)
        left_loudness(&mut source, 4410);
        let first = left_loudness(&mut source, 4410);
        // The left of the second pair (channel 3) is three times as loud as the first's.
        gains.set([0.0, 1.0, 0.0, 0.0]);
        left_loudness(&mut source, 2048);
        let second = left_loudness(&mut source, 4410);
        assert!((second / first - 3.0).abs() < 0.1, "{first} and {second}");
        assert!((first - 0.05 / 2f32.sqrt()).abs() < 0.005, "{first}");

        // Moved to 1.5 seconds, half a second is left.
        source.try_seek(Duration::from_millis(1500)).unwrap();
        let left = source.by_ref().count() / 2;
        assert!((left as i64 - 22_050).abs() < 2048, "{left} frames");
        fs::remove_file(file).unwrap();
    }

    /// Decrypts a real stems file with a real key, both from the environment (the key is
    /// never written down here): `OPEN_ENGINE_LIB_STEMS_KEY` (32 hex digits),
    /// `OPEN_ENGINE_LIB_STEMS_FILE`, and where to put the plain MP4,
    /// `OPEN_ENGINE_LIB_STEMS_OUT`. Then plays the start of it.
    #[test]
    #[ignore = "needs Engine's key and a stems file"]
    fn decrypts_a_real_stems_file() {
        let var = |name: &str| std::env::var(name).unwrap_or_else(|_| panic!("set {name}"));
        let key = open_engine_api::stems_key::parse(&var("OPEN_ENGINE_LIB_STEMS_KEY"))
            .expect("the key fits Engine's stems");
        let out = PathBuf::from(var("OPEN_ENGINE_LIB_STEMS_OUT"));
        decrypt_file(Path::new(&var("OPEN_ENGINE_LIB_STEMS_FILE")), &key, &out).unwrap();
        let mut source =
            StemsSource::open(&out, Duration::from_secs(60), Gains::default()).unwrap();
        assert!(left_loudness(&mut source, 44_100) > 0.0);
    }
}
