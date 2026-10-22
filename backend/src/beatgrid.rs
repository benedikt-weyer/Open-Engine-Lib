//! Reads the beatgrid Engine stores for an analysed track (`PerformanceData.beatData`).
//!
//! Layout (see `docs/engine-library-format.md`): a 4-byte big-endian length and a zlib
//! stream, holding a big-endian sample rate and sample count, a "grid is set" byte,
//! then two lists of markers (the grid as analysed, and as adjusted by the user), each
//! preceded by a big-endian count. A marker is little-endian: the position in samples
//! (f64), the beat number (i64), the beats until the next marker (i32) and an unknown i32.

use std::io::Read;

use flate2::read::ZlibDecoder;

/// More than any real beatgrid needs; guards against corrupt data.
const MAX_DECOMPRESSED: u64 = 4 * 1024 * 1024;
const MARKER_SIZE: usize = 24;
const HEADER_SIZE: usize = 17;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Marker {
    pub position_seconds: f64,
    pub beat_number: i64,
}

/// The markers of the beatgrid in `blob`, or `None` if the track has no usable grid.
/// The user's adjusted grid is preferred over the analysed one.
pub fn parse(blob: &[u8]) -> Option<Vec<Marker>> {
    let compressed = blob.get(4..)?;
    let mut data = Vec::new();
    ZlibDecoder::new(compressed)
        .take(MAX_DECOMPRESSED)
        .read_to_end(&mut data)
        .ok()?;

    let sample_rate = f64::from_be_bytes(data.get(0..8)?.try_into().ok()?);
    if !(sample_rate.is_finite() && sample_rate > 0.0) || *data.get(16)? == 0 {
        return None;
    }

    let (analysed, after) = markers(&data, HEADER_SIZE, sample_rate)?;
    let adjusted = markers(&data, after, sample_rate).map(|(markers, _)| markers);

    // At least two markers are needed to know the spacing of the beats.
    [adjusted, Some(analysed)]
        .into_iter()
        .flatten()
        .find(|markers| markers.len() >= 2)
}

/// Reads a list of markers starting at `at`; returns it and where it ends.
fn markers(data: &[u8], at: usize, sample_rate: f64) -> Option<(Vec<Marker>, usize)> {
    let count = i64::from_be_bytes(data.get(at..at + 8)?.try_into().ok()?);
    let count = usize::try_from(count)
        .ok()
        .filter(|count| *count <= data.len() / MARKER_SIZE)?;

    let mut markers = Vec::with_capacity(count);
    for index in 0..count {
        let start = at + 8 + index * MARKER_SIZE;
        let marker = data.get(start..start + MARKER_SIZE)?;
        let samples = f64::from_le_bytes(marker[0..8].try_into().ok()?);
        let beat_number = i64::from_le_bytes(marker[8..16].try_into().ok()?);
        if !samples.is_finite() {
            return None;
        }
        markers.push(Marker {
            position_seconds: samples / sample_rate,
            beat_number,
        });
    }
    Some((markers, at + 8 + count * MARKER_SIZE))
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use flate2::Compression;
    use flate2::write::ZlibEncoder;

    use super::*;

    /// A `beatData` blob the way Engine writes it.
    fn blob(is_set: bool, analysed: &[(f64, i64)], adjusted: &[(f64, i64)]) -> Vec<u8> {
        let mut data = Vec::new();
        data.extend(44_100f64.to_be_bytes());
        data.extend(18_531_648f64.to_be_bytes());
        data.push(u8::from(is_set));
        for list in [analysed, adjusted] {
            data.extend((list.len() as i64).to_be_bytes());
            for (samples, beat) in list {
                data.extend(samples.to_le_bytes());
                data.extend(beat.to_le_bytes());
                data.extend(0i32.to_le_bytes());
                data.extend(0i32.to_le_bytes());
            }
        }
        let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(&data).unwrap();
        let mut blob = (data.len() as u32).to_be_bytes().to_vec();
        blob.extend(encoder.finish().unwrap());
        blob
    }

    // The two markers of a real 127 BPM track.
    const GRID: [(f64, i64); 2] = [(-83_338.582_677_165_34, -4), (15_355_133.858_267_713, 737)];

    #[test]
    fn reads_markers_in_seconds() {
        let markers = parse(&blob(true, &GRID, &GRID)).unwrap();
        assert_eq!(markers.len(), 2);
        assert!((markers[0].position_seconds - -1.889_8).abs() < 1e-3);
        assert_eq!((markers[0].beat_number, markers[1].beat_number), (-4, 737));
        // 741 beats in that time: the 127 BPM the library reports.
        let bpm = 60.0 * 741.0 / (markers[1].position_seconds - markers[0].position_seconds);
        assert!((bpm - 127.0).abs() < 0.01, "{bpm}");
    }

    #[test]
    fn prefers_the_adjusted_grid() {
        let adjusted = [(44_100.0, 0), (88_200.0, 2)];
        let markers = parse(&blob(true, &GRID, &adjusted)).unwrap();
        assert_eq!(markers[0].position_seconds, 1.0);
        assert_eq!(markers[1].beat_number, 2);

        // An empty adjusted list falls back to the analysed one.
        let markers = parse(&blob(true, &GRID, &[])).unwrap();
        assert_eq!(markers[1].beat_number, 737);
    }

    #[test]
    fn no_grid_when_it_is_not_set_or_has_too_few_markers() {
        assert_eq!(parse(&blob(false, &GRID, &GRID)), None);
        assert_eq!(parse(&blob(true, &GRID[..1], &GRID[..1])), None);
        assert_eq!(parse(&blob(true, &[], &[])), None);
    }

    #[test]
    fn garbage_is_no_grid() {
        assert_eq!(parse(&[]), None);
        assert_eq!(parse(&[0, 0, 0, 9, 1, 2, 3]), None);
        // A valid stream cut short, and a count that promises more than there is.
        let good = blob(true, &GRID, &GRID);
        assert_eq!(parse(&good[..good.len() / 2]), None);
        let mut data = vec![0u8; HEADER_SIZE];
        data[..8].copy_from_slice(&44_100f64.to_be_bytes());
        data[16] = 1;
        data.extend(i64::MAX.to_be_bytes());
        let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(&data).unwrap();
        let mut blob = 0u32.to_be_bytes().to_vec();
        blob.extend(encoder.finish().unwrap());
        assert_eq!(parse(&blob), None);
    }
}
