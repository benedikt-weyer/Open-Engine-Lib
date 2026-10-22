//! How the waveform is colored. See `WaveformColors` for the schemes.
//!
//! The band levels these functions take are the (smoothed) loudness of the low, mid
//! and high frequencies of a slice of the track, each from 0 to 1.

use iced::Color;

use crate::config::WaveformColors;

pub type Levels = [f32; 3];

/// The three layers of the stacked scheme, back (low) to front (high): the dark
/// blue, amber and white of the CDJ-3000.
pub const STACKED_LAYERS: [Color; 3] = [
    Color::from_rgb(0.10, 0.28, 0.85),
    Color::from_rgb(0.96, 0.62, 0.10),
    Color::from_rgb(0.96, 0.96, 0.98),
];

/// The color of mono waveforms where there are only lows, and where there are only highs.
const MONO_DARK: Color = Color::from_rgb(0.10, 0.34, 0.86);
const MONO_LIGHT: Color = Color::from_rgb(0.72, 0.88, 1.0);
/// The hue of the spectrum scheme runs from red (all lows) to this many degrees
/// (violet, all highs).
const SPECTRUM_DEGREES: f32 = 270.0;
/// Dim a bit so that no scheme glares on the dark background.
const BRIGHTNESS: f32 = 0.95;

/// How tall the waveform is: as loud as its loudest band.
pub fn amplitude(levels: Levels) -> f32 {
    levels.into_iter().fold(0.0, f32::max)
}

/// The color of one slice of the waveform, for the schemes that give each slice a color
/// of its own (all but the stacked one, which colors whole layers).
pub fn column(scheme: WaveformColors, levels: Levels) -> Color {
    let [low, mid, high] = levels;
    let loudest = amplitude(levels).max(1e-3);

    match scheme {
        // One color; the more of the sound is highs, the lighter it gets.
        WaveformColors::Mono => lerp(MONO_DARK, MONO_LIGHT, (high / loudest).clamp(0.0, 1.0)),

        // Lows are red, mids green, highs blue, mixed. Scaled so the strongest band is at
        // full strength: the color says what the sound is made of, the height how loud.
        WaveformColors::Rgb => Color::from_rgb(
            low / loudest * BRIGHTNESS,
            mid / loudest * BRIGHTNESS,
            high / loudest * BRIGHTNESS,
        ),

        // The dominant frequency (where the energy is on average) sets the hue.
        WaveformColors::Spectrum => {
            let total = (low + mid + high).max(1e-3);
            let centre = (0.5 * mid + high) / total;
            hsv(centre * SPECTRUM_DEGREES, 0.85, BRIGHTNESS)
        }

        WaveformColors::Stacked => STACKED_LAYERS[0],
    }
}

fn lerp(from: Color, to: Color, amount: f32) -> Color {
    Color::from_rgb(
        from.r + (to.r - from.r) * amount,
        from.g + (to.g - from.g) * amount,
        from.b + (to.b - from.b) * amount,
    )
}

/// Hue in degrees, saturation and value from 0 to 1.
fn hsv(hue: f32, saturation: f32, value: f32) -> Color {
    let sector = (hue.rem_euclid(360.0)) / 60.0;
    let chroma = value * saturation;
    let x = chroma * (1.0 - (sector % 2.0 - 1.0).abs());
    let (r, g, b) = match sector as u8 {
        0 => (chroma, x, 0.0),
        1 => (x, chroma, 0.0),
        2 => (0.0, chroma, x),
        3 => (0.0, x, chroma),
        4 => (x, 0.0, chroma),
        _ => (chroma, 0.0, x),
    };
    let floor = value - chroma;
    Color::from_rgb(r + floor, g + floor, b + floor)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rgb(color: Color) -> [f32; 3] {
        [color.r, color.g, color.b]
    }

    fn brightness(color: Color) -> f32 {
        0.2126 * color.r + 0.7152 * color.g + 0.0722 * color.b
    }

    #[test]
    fn mono_gets_lighter_with_more_highs() {
        let bass = column(WaveformColors::Mono, [0.9, 0.1, 0.0]);
        let hats = column(WaveformColors::Mono, [0.1, 0.1, 0.9]);
        assert!(brightness(hats) > brightness(bass) + 0.2);
        // Always blue-ish, never another hue.
        assert!(bass.b > bass.r && bass.b > bass.g);
        assert!(hats.b >= hats.r && hats.b >= hats.g);
        // Silence doesn't produce NaN.
        assert!(column(WaveformColors::Mono, [0.0; 3]).r.is_finite());
    }

    #[test]
    fn rgb_mixes_lows_mids_and_highs_into_red_green_and_blue() {
        let [r, g, b] = rgb(column(WaveformColors::Rgb, [0.8, 0.0, 0.0]));
        assert!(r > 0.9 && g == 0.0 && b == 0.0, "only lows is red");
        let [r, g, b] = rgb(column(WaveformColors::Rgb, [0.0, 0.0, 0.3]));
        assert!(
            b > 0.9 && r == 0.0 && g == 0.0,
            "only highs is blue, however quiet"
        );
        let [r, g, b] = rgb(column(WaveformColors::Rgb, [0.5, 0.5, 0.0]));
        assert!(r > 0.9 && g > 0.9 && b == 0.0, "lows and mids make yellow");
    }

    #[test]
    fn spectrum_runs_from_red_through_green_to_violet() {
        let low = rgb(column(WaveformColors::Spectrum, [1.0, 0.0, 0.0]));
        assert!(low[0] > 0.9 && low[1] < 0.3 && low[2] < 0.3, "bass is red");

        let mid = rgb(column(WaveformColors::Spectrum, [0.0, 1.0, 0.0]));
        assert!(
            mid[1] > mid[0] && mid[1] > mid[2],
            "mids are green-ish: {mid:?}"
        );

        let high = rgb(column(WaveformColors::Spectrum, [0.0, 0.0, 1.0]));
        assert!(
            high[2] > 0.9 && high[0] > high[1],
            "treble is violet: {high:?}"
        );

        // Everything at once lands in between the extremes, not outside the range.
        for levels in [[1.0, 1.0, 1.0], [0.3, 0.7, 0.2], [0.0; 3]] {
            for channel in rgb(column(WaveformColors::Spectrum, levels)) {
                assert!((0.0..=1.0).contains(&channel));
            }
        }
    }

    #[test]
    fn the_stacked_layers_are_dark_blue_amber_and_white() {
        let [low, mid, high] = STACKED_LAYERS;
        assert!(low.b > low.r && low.b > low.g);
        assert!(mid.r > mid.g && mid.g > mid.b);
        assert!(high.r > 0.9 && high.g > 0.9 && high.b > 0.9);
    }

    #[test]
    fn amplitude_is_the_loudest_band() {
        assert_eq!(amplitude([0.2, 0.7, 0.4]), 0.7);
        assert_eq!(amplitude([0.0; 3]), 0.0);
    }
}
