//! The waveform and beatgrid of a track, as the backend computed them.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;

/// How many columns the whole-track overview is made of, whatever the track's length.
pub const OVERVIEW_COLUMNS: usize = 1200;
/// How many waveforms are kept: the track playing and a few it was switched from.
const MAX_KEPT: usize = 6;
/// The share of overview bins that are at or below full height.
const OVERVIEW_FULL_SCALE_PERCENTILE: f64 = 0.98;
/// More beats than any window shows; guards against a broken grid.
const MAX_BEATS: usize = 4000;

/// How loud the low, mid and high frequencies are in one slice of time, 0 to 255.
pub type Bands = [u8; 3];

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Marker {
    pub position: f64,
    pub beat_number: i64,
}

/// One beat of the grid.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Beat {
    /// Seconds into the track.
    pub time: f64,
    pub number: i64,
}

impl Beat {
    /// The first beat of a bar. Beats are numbered so that those divisible by 4 are.
    pub fn is_bar_start(&self) -> bool {
        self.number.rem_euclid(4) == 0
    }
}

/// Where the beats are: evenly spaced between two markers, and carried on at the
/// nearest spacing before the first and after the last.
#[derive(Debug, Clone, PartialEq)]
pub struct BeatGrid {
    markers: Vec<Marker>,
}

impl BeatGrid {
    /// `None` unless the markers can give a spacing: at least two, in time order, with
    /// beat numbers that increase.
    pub fn new(markers: Vec<Marker>) -> Option<Self> {
        let sensible = markers.len() >= 2
            && markers.windows(2).all(|pair| {
                pair[1].position > pair[0].position
                    && pair[1].beat_number > pair[0].beat_number
                    && pair[0].position.is_finite()
                    && pair[1].position.is_finite()
            });
        sensible.then_some(Self { markers })
    }

    /// The tempo of the grid as a whole, in beats per minute.
    pub fn bpm(&self) -> f64 {
        let (first, last) = (self.markers[0], self.markers[self.markers.len() - 1]);
        60.0 * (last.beat_number - first.beat_number) as f64 / (last.position - first.position)
    }

    /// The beats from `from` to `to` seconds, in time order.
    pub fn beats(&self, from: f64, to: f64) -> Vec<Beat> {
        let mut beats = Vec::new();
        let last = self.markers.len() - 1;

        for (index, marker) in self.markers.iter().enumerate() {
            // The segment starting at this marker runs to the next one. The first marker
            // also owns everything before it, and the last everything after it.
            let (spacing_from, spacing_to) = if index < last {
                (index, index + 1)
            } else {
                (index - 1, index)
            };
            let (a, b) = (self.markers[spacing_from], self.markers[spacing_to]);
            let spacing = (b.position - a.position) / (b.beat_number - a.beat_number) as f64;

            let lowest = if index == 0 {
                i64::MIN
            } else {
                marker.beat_number
            };
            let highest = if index == last {
                i64::MAX
            } else {
                self.markers[index + 1].beat_number - 1
            };

            // Beat numbers whose time falls in the window, within this segment.
            let first = (((from - marker.position) / spacing).ceil() as i64)
                .saturating_add(marker.beat_number)
                .max(lowest);
            let end = (((to - marker.position) / spacing).floor() as i64)
                .saturating_add(marker.beat_number)
                .min(highest);

            for number in first..=end {
                if beats.len() >= MAX_BEATS {
                    return beats;
                }
                beats.push(Beat {
                    time: marker.position + (number - marker.beat_number) as f64 * spacing,
                    number,
                });
            }
        }
        beats
    }
}

#[derive(Debug, PartialEq)]
pub struct Waveform {
    pub duration: f64,
    pub columns_per_second: f64,
    pub columns: Vec<Bands>,
    /// Pooled to `OVERVIEW_COLUMNS` (or fewer, for a very short track), to draw the whole track.
    pub overview: Vec<Bands>,
    pub grid: Option<BeatGrid>,
}

impl Waveform {
    pub fn new(
        duration: f64,
        columns_per_second: f64,
        columns: Vec<Bands>,
        grid: Option<BeatGrid>,
    ) -> Self {
        let overview = pool(&columns, OVERVIEW_COLUMNS);
        Self {
            duration,
            columns_per_second,
            columns,
            overview,
            grid,
        }
    }
}

/// Shrinks `columns` to at most `count` for drawing the whole track.
///
/// Each bin is the RMS of its columns: how loud the stretch is typically, not how loud
/// its loudest moment is. Keeping the peaks looks right when zoomed in, but a bin of a
/// few seconds always holds a peak of every band (hi-hats are in nearly every one), so
/// all three bands would reach full height everywhere and the front layer would cover
/// the others. A common gain then brings the loudest bins back to full height, which
/// keeps the bands in proportion to each other.
fn pool(columns: &[Bands], count: usize) -> Vec<Bands> {
    if columns.len() <= count {
        return columns.to_vec();
    }

    let rms: Vec<[f32; 3]> = (0..count)
        .map(|bin| {
            let from = bin * columns.len() / count;
            let to = ((bin + 1) * columns.len() / count).max(from + 1);
            let group = &columns[from..to];
            [0, 1, 2].map(|band| {
                let energy: f32 = group.iter().map(|c| f32::from(c[band]).powi(2)).sum();
                (energy / group.len() as f32).sqrt()
            })
        })
        .collect();

    // The loudest few bins are left to clip, so one outlier can't dim everything else.
    let mut loudest: Vec<f32> = rms
        .iter()
        .map(|bands| bands.iter().copied().fold(0.0, f32::max))
        .collect();
    loudest.sort_by(f32::total_cmp);
    let reference = loudest
        [((loudest.len() as f64 * OVERVIEW_FULL_SCALE_PERCENTILE) as usize).min(loudest.len() - 1)];
    let gain = if reference > 0.0 {
        255.0 / reference
    } else {
        1.0
    };

    rms.iter()
        .map(|bands| bands.map(|level| (level * gain).round().min(255.0) as u8))
        .collect()
}

enum Entry {
    Loading,
    Ready(Arc<Waveform>),
    /// The backend has none (a streaming track, a file that is gone or can't be read).
    Unavailable,
}

/// How a track's waveform is doing.
pub enum Display<'a> {
    Loading,
    Unavailable,
    Ready(&'a Arc<Waveform>),
}

/// The waveforms fetched for the tracks played, a few at a time.
#[derive(Default)]
pub struct Waveforms {
    entries: HashMap<u32, Entry>,
    /// Ids in the order they were answered, oldest first.
    order: VecDeque<u32>,
    /// Changes with the library, so a late answer about another library's track
    /// (whose id means another track now) is told apart and dropped.
    generation: u32,
}

impl Waveforms {
    pub fn generation(&self) -> u32 {
        self.generation
    }

    pub fn clear(&mut self) {
        self.entries.clear();
        self.order.clear();
        self.generation += 1;
    }

    pub fn display(&self, id: u32) -> Display<'_> {
        match self.entries.get(&id) {
            Some(Entry::Ready(waveform)) => Display::Ready(waveform),
            Some(Entry::Unavailable) => Display::Unavailable,
            _ => Display::Loading,
        }
    }

    /// Whether the waveform of `id` has not been asked for yet. If so it counts as
    /// being loaded from now on.
    pub fn claim(&mut self, id: u32) -> bool {
        if self.entries.contains_key(&id) {
            return false;
        }
        self.entries.insert(id, Entry::Loading);
        true
    }

    /// Stores the answer for `id` (`None`: there is no waveform). Answers for an earlier
    /// library are ignored.
    pub fn set(&mut self, generation: u32, id: u32, waveform: Option<Arc<Waveform>>) {
        if generation != self.generation {
            return;
        }
        self.entries
            .insert(id, waveform.map_or(Entry::Unavailable, Entry::Ready));
        self.order.retain(|&kept| kept != id);
        self.order.push_back(id);
        while self.order.len() > MAX_KEPT {
            if let Some(oldest) = self.order.pop_front() {
                self.entries.remove(&oldest);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn grid(markers: &[(f64, i64)]) -> BeatGrid {
        BeatGrid::new(
            markers
                .iter()
                .map(|&(position, beat_number)| Marker {
                    position,
                    beat_number,
                })
                .collect(),
        )
        .unwrap()
    }

    fn times(beats: &[Beat]) -> Vec<(f64, i64)> {
        beats.iter().map(|beat| (beat.time, beat.number)).collect()
    }

    #[test]
    fn beats_are_evenly_spaced_between_markers() {
        // 120 BPM: a beat every half second, starting a quarter second in.
        let grid = grid(&[(0.25, 0), (2.25, 4)]);
        assert_eq!(
            times(&grid.beats(0.0, 3.0)),
            [
                (0.25, 0),
                (0.75, 1),
                (1.25, 2),
                (1.75, 3),
                (2.25, 4),
                (2.75, 5)
            ]
        );
        assert!((grid.bpm() - 120.0).abs() < 1e-9);
    }

    #[test]
    fn the_grid_carries_on_before_the_first_and_after_the_last_marker() {
        let grid = grid(&[(0.25, 0), (2.25, 4)]);
        assert_eq!(times(&grid.beats(-1.0, 0.0)), [(-0.75, -2), (-0.25, -1)]);
        assert_eq!(times(&grid.beats(10.0, 11.0)), [(10.25, 20), (10.75, 21)]);
    }

    #[test]
    fn a_marker_beat_is_listed_once_and_a_window_can_end_on_one() {
        let grid = grid(&[(0.0, 0), (1.0, 2), (3.0, 6)]);
        let beats = grid.beats(0.0, 3.0);
        assert_eq!(
            beats.iter().map(|b| b.number).collect::<Vec<_>>(),
            [0, 1, 2, 3, 4, 5, 6]
        );
        assert_eq!(times(&grid.beats(1.0, 1.0)), [(1.0, 2)]);
    }

    #[test]
    fn follows_a_tempo_change_between_markers() {
        // Half a second per beat, then a quarter second per beat.
        let grid = grid(&[(0.0, 0), (2.0, 4), (3.0, 8)]);
        assert_eq!(
            times(&grid.beats(2.0, 3.0)),
            [(2.0, 4), (2.25, 5), (2.5, 6), (2.75, 7), (3.0, 8)]
        );
    }

    #[test]
    fn bars_start_every_four_beats_even_before_the_first_marker() {
        let grid = grid(&[(1.0, -4), (3.0, 0)]);
        let starts: Vec<i64> = grid
            .beats(0.0, 5.0)
            .iter()
            .filter(|beat| beat.is_bar_start())
            .map(|beat| beat.number)
            .collect();
        assert_eq!(starts, [-4, 0, 4]);
        assert!(
            Beat {
                time: 0.0,
                number: -8
            }
            .is_bar_start()
        );
        assert!(
            !Beat {
                time: 0.0,
                number: -3
            }
            .is_bar_start()
        );
    }

    #[test]
    fn a_broken_grid_is_rejected_or_bounded() {
        assert!(BeatGrid::new(vec![]).is_none());
        let marker = |position, beat_number| Marker {
            position,
            beat_number,
        };
        assert!(
            BeatGrid::new(vec![marker(0.0, 0)]).is_none(),
            "one marker has no spacing"
        );
        assert!(
            BeatGrid::new(vec![marker(1.0, 0), marker(0.5, 4)]).is_none(),
            "time goes back"
        );
        assert!(
            BeatGrid::new(vec![marker(0.0, 4), marker(1.0, 4)]).is_none(),
            "no beats between"
        );

        // An absurdly fast grid doesn't produce an endless list.
        let fast = grid(&[(0.0, 0), (1.0, 1_000_000)]);
        assert_eq!(fast.beats(0.0, 1.0).len(), MAX_BEATS);
    }

    #[test]
    fn the_overview_shows_typical_loudness_not_peaks() {
        // Lows 100, mids 50, highs 10 throughout, but a loud high-hat click now and then:
        // every bin of a long track holds one, yet it is not what the stretch sounds like.
        let columns: Vec<Bands> = (0..4000)
            .map(|index| {
                if index % 50 == 0 {
                    [100, 50, 255]
                } else {
                    [100, 50, 10]
                }
            })
            .collect();
        let waveform = Waveform::new(40.0, 100.0, columns, None);

        assert_eq!(waveform.overview.len(), OVERVIEW_COLUMNS);
        // The median bin of each band: most bins hold no click at all (a click is every
        // 50th column, a bin is about 3).
        let median = |band: usize| {
            let mut levels: Vec<u8> = waveform.overview.iter().map(|bands| bands[band]).collect();
            levels.sort_unstable();
            levels[levels.len() / 2]
        };
        let typical = [median(0), median(1), median(2)];
        assert!(
            typical[0] > typical[1] && typical[1] > typical[2],
            "layers stay in order: {typical:?}"
        );
        assert!(
            typical[2] < typical[0] / 2,
            "the clicks don't make the highs as tall as the lows: {typical:?}"
        );
    }

    #[test]
    fn the_overview_keeps_loud_and_quiet_parts_apart_and_the_bands_in_proportion() {
        // A loud first half and a quiet second half, lows twice the mids.
        let columns: Vec<Bands> = (0..4000)
            .map(|index| {
                if index < 2000 {
                    [160, 80, 40]
                } else {
                    [16, 8, 4]
                }
            })
            .collect();
        let overview = Waveform::new(40.0, 100.0, columns, None).overview;

        let (loud, quiet) = (overview[100], overview[OVERVIEW_COLUMNS - 100]);
        assert!(
            loud[0] > 200,
            "the loud part is brought up to full height: {loud:?}"
        );
        assert!(
            quiet[0] * 5 < loud[0],
            "and the quiet part stays small: {quiet:?}"
        );
        assert!(
            u32::from(loud[0]).abs_diff(u32::from(loud[1]) * 2) <= 2,
            "lows are still twice the mids: {loud:?}"
        );
    }

    #[test]
    fn a_short_loud_sound_still_shows_in_the_overview() {
        let mut columns = vec![[0u8, 0, 0]; 4000];
        columns[2000] = [255, 255, 255];
        let overview = Waveform::new(40.0, 100.0, columns, None).overview;

        let seen = overview.iter().map(|bands| bands[0]).max().unwrap();
        assert!(seen > 100, "{seen}");
        assert_eq!(overview[0], [0, 0, 0]);

        let silent = Waveform::new(40.0, 100.0, vec![[0u8; 3]; 4000], None).overview;
        assert!(silent.iter().all(|bands| *bands == [0, 0, 0]));
    }

    #[test]
    fn a_short_track_is_not_stretched() {
        let short = Waveform::new(1.0, 100.0, vec![[1, 2, 3]; 100], None);
        assert_eq!(short.overview.len(), 100);
        assert_eq!(
            short.overview[0],
            [1, 2, 3],
            "and its columns are used as they are"
        );
    }

    #[test]
    fn asks_for_each_waveform_once_and_keeps_a_few() {
        let mut waveforms = Waveforms::default();
        assert!(waveforms.claim(1));
        assert!(!waveforms.claim(1));
        assert!(matches!(waveforms.display(1), Display::Loading));

        let generation = waveforms.generation();
        let waveform = Arc::new(Waveform::new(1.0, 100.0, Vec::new(), None));
        waveforms.set(generation, 1, Some(Arc::clone(&waveform)));
        assert!(matches!(waveforms.display(1), Display::Ready(_)));
        waveforms.set(generation, 2, None);
        assert!(matches!(waveforms.display(2), Display::Unavailable));

        for id in 10..10 + MAX_KEPT as u32 {
            waveforms.set(generation, id, Some(Arc::clone(&waveform)));
        }
        assert!(
            matches!(waveforms.display(1), Display::Loading),
            "the oldest was dropped"
        );
        assert!(waveforms.claim(1), "and can be fetched again");
    }

    #[test]
    fn drops_answers_for_a_closed_library() {
        let mut waveforms = Waveforms::default();
        let before = waveforms.generation();
        waveforms.claim(1);
        waveforms.clear();

        waveforms.set(
            before,
            1,
            Some(Arc::new(Waveform::new(1.0, 100.0, Vec::new(), None))),
        );
        assert!(matches!(waveforms.display(1), Display::Loading));
        assert!(waveforms.claim(1));
    }
}
