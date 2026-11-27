//! Settings in profiles, as in iron-file: Nix-friendly, and several at once.
//!
//! A profile is a TOML file of settings. They are read from the user's
//! `<config dir>/open-engine-lib/profiles/*.toml` and from
//! `<dir>/open-engine-lib/profiles/*.toml` for each dir in `$XDG_CONFIG_DIRS` (always
//! with `/etc/xdg`, where the NixOS module puts its profile). A profile only says what
//! it sets: the rest comes from its `base_profile` (a path), if it has one, and then
//! from the defaults. `state.toml` in the user's folder says which profile is in use.
//!
//! The app changes a profile it can write directly. A read-only one, such as a profile
//! made declaratively in the Nix store, is left alone: a change goes to an overlay in
//! the user's folder (`<name>-override-<hash>.toml`, with the profile as its base),
//! which is used from then on, and deleting the overlay goes back to the profile.

use std::fs::{self, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};

use open_engine_api::{
    WAVEFORM_GAIN_HIGH, WAVEFORM_GAIN_LOW, WAVEFORM_GAIN_MID, WAVEFORM_LOW_MID_HZ,
    WAVEFORM_MAX_GAIN, WAVEFORM_MAX_SPLIT_HZ, WAVEFORM_MID_HIGH_HZ, WAVEFORM_MIN_SPLIT_HZ,
};
use serde::{Deserialize, Serialize};

/// How far the waveform's delay can be set either way, in milliseconds.
pub const WAVEFORM_DELAY_MAX_MS: i32 = 500;

const APP_DIR: &str = "open-engine-lib";
const PROFILES_DIR: &str = "profiles";
const STATE_FILE: &str = "state.toml";
/// The profile made when there is none.
const DEFAULT_PROFILE_FILE: &str = "default.toml";
const DEFAULT_PROFILE_NAME: &str = "Default";
/// The settings files of the versions before profiles.
const LEGACY_CONFIG_FILE: &str = "config.toml";
const LEGACY_OVERRIDES_FILE: &str = "overrides.toml";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ThemeMode {
    Light,
    #[default]
    Dark,
}

/// One of the three frequency bands of the waveform.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WaveformBand {
    Low,
    Mid,
    High,
}

impl WaveformBand {
    pub const ALL: [WaveformBand; 3] = [WaveformBand::Low, WaveformBand::Mid, WaveformBand::High];
}

/// How the waveform of the playing track is colored.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WaveformColors {
    /// One color (blue); the more highs, the lighter.
    Mono,
    /// Lows red, mids green, highs blue, mixed into one color per slice, as in the
    /// "RGB" waveforms of Serato and rekordbox.
    Rgb,
    /// Three layers over each other, lows biggest at the back and highs smallest in
    /// front: dark blue, amber and white, as on a CDJ-3000.
    #[default]
    Stacked,
    /// The dominant frequency sets the hue, from red for bass to violet for treble.
    Spectrum,
}

impl WaveformColors {
    pub const ALL: [WaveformColors; 4] = [
        WaveformColors::Mono,
        WaveformColors::Rgb,
        WaveformColors::Stacked,
        WaveformColors::Spectrum,
    ];
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Language {
    #[serde(rename = "en")]
    English,
    #[serde(rename = "de")]
    German,
}

impl Language {
    pub const ALL: [Language; 2] = [Language::English, Language::German];

    /// Picks the language from the locale environment, defaulting to English.
    fn from_locale(locale: &str) -> Self {
        if locale.starts_with("de") {
            Language::German
        } else {
            Language::English
        }
    }
}

impl Default for Language {
    fn default() -> Self {
        ["LC_ALL", "LC_MESSAGES", "LANG"]
            .iter()
            .filter_map(|name| std::env::var(name).ok())
            .find(|value| !value.is_empty())
            .map_or(Language::English, |locale| Language::from_locale(&locale))
    }
}

/// The fully resolved settings the app runs with.
#[derive(Debug, Clone, PartialEq)]
pub struct Config {
    pub theme: ThemeMode,
    pub language: Language,
    pub waveform_colors: WaveformColors,
    /// Where the waveform's low band ends and its mid band starts, in Hz.
    pub waveform_low_mid_hz: u32,
    /// Where its mid band ends and its high band starts, in Hz.
    pub waveform_mid_high_hz: u32,
    /// How much each band of the waveform is amplified before it is drawn: 1 draws a band
    /// at its true level. A fixed scale, the same for every track.
    pub waveform_gain_low: f32,
    pub waveform_gain_mid: f32,
    pub waveform_gain_high: f32,
    /// How far the waveform trails the position the player reports, in milliseconds: what
    /// makes it line up with what is heard, since the sound reaches the speakers a little
    /// after the player has played it. Negative moves the waveform ahead.
    pub waveform_delay_ms: i32,
    /// Open the library that was open when the app was last closed.
    pub reopen_library: bool,
    /// When reopening it, also go back to the playlist, the song and its position.
    pub restore_position: bool,
    /// Listen to DJ controllers (anything that sends MIDI) to control the player.
    pub controller_enabled: bool,
    /// The key of Engine's stems files, as the user wrote it (32 hexadecimal digits): Open
    /// Engine Lib does not come with it. Empty for none.
    pub stems_key: String,
}

fn gain_is_valid(gain: f32) -> bool {
    gain.is_finite() && gain > 0.0 && gain <= WAVEFORM_MAX_GAIN
}

impl Config {
    /// The stems key, if one is set and it is Engine's.
    pub fn stems_key(&self) -> Option<[u8; open_engine_api::stems_key::LEN]> {
        open_engine_api::stems_key::parse(&self.stems_key).ok()
    }

    pub fn waveform_gain(&self, band: WaveformBand) -> f32 {
        match band {
            WaveformBand::Low => self.waveform_gain_low,
            WaveformBand::Mid => self.waveform_gain_mid,
            WaveformBand::High => self.waveform_gain_high,
        }
    }

    /// Whether the waveform's split points are in range and in order.
    fn waveform_split_is_valid(&self) -> bool {
        let range = WAVEFORM_MIN_SPLIT_HZ..=WAVEFORM_MAX_SPLIT_HZ;
        range.contains(&self.waveform_low_mid_hz)
            && range.contains(&self.waveform_mid_high_hz)
            && self.waveform_low_mid_hz < self.waveform_mid_high_hz
    }

    /// Makes the config safe to use: split points that make no sense are replaced by the
    /// default ones (`ConfigStore::load` has already warned about them).
    fn sanitized(mut self) -> Self {
        if !self.waveform_split_is_valid() {
            self.waveform_low_mid_hz = WAVEFORM_LOW_MID_HZ;
            self.waveform_mid_high_hz = WAVEFORM_MID_HIGH_HZ;
        }
        self.waveform_delay_ms = self
            .waveform_delay_ms
            .clamp(-WAVEFORM_DELAY_MAX_MS, WAVEFORM_DELAY_MAX_MS);
        // A gain that makes no sense is replaced on its own; the others are kept.
        for (gain, default) in [
            (&mut self.waveform_gain_low, WAVEFORM_GAIN_LOW),
            (&mut self.waveform_gain_mid, WAVEFORM_GAIN_MID),
            (&mut self.waveform_gain_high, WAVEFORM_GAIN_HIGH),
        ] {
            if !gain_is_valid(*gain) {
                *gain = default;
            }
        }
        self
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            theme: ThemeMode::default(),
            language: Language::default(),
            waveform_colors: WaveformColors::default(),
            waveform_low_mid_hz: WAVEFORM_LOW_MID_HZ,
            waveform_mid_high_hz: WAVEFORM_MID_HIGH_HZ,
            waveform_gain_low: WAVEFORM_GAIN_LOW,
            waveform_gain_mid: WAVEFORM_GAIN_MID,
            waveform_gain_high: WAVEFORM_GAIN_HIGH,
            waveform_delay_ms: 0,
            reopen_library: true,
            restore_position: true,
            controller_enabled: true,
            stems_key: String::new(),
        }
    }
}

/// One config layer: only the values it sets.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
struct Layer {
    #[serde(skip_serializing_if = "Option::is_none")]
    theme: Option<ThemeMode>,
    #[serde(skip_serializing_if = "Option::is_none")]
    language: Option<Language>,
    #[serde(skip_serializing_if = "Option::is_none")]
    waveform_colors: Option<WaveformColors>,
    #[serde(skip_serializing_if = "Option::is_none")]
    waveform_low_mid_hz: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    waveform_mid_high_hz: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    waveform_gain_low: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    waveform_gain_mid: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    waveform_gain_high: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    waveform_delay_ms: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reopen_library: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    restore_position: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    controller_enabled: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    stems_key: Option<String>,
}

impl Layer {
    /// Takes over what `other` sets.
    fn take_from(&mut self, other: Layer) {
        macro_rules! take {
            ($($field:ident),*) => {$(if other.$field.is_some() { self.$field = other.$field; })*};
        }
        take!(
            theme,
            language,
            waveform_colors,
            waveform_low_mid_hz,
            waveform_mid_high_hz,
            waveform_gain_low,
            waveform_gain_mid,
            waveform_gain_high,
            waveform_delay_ms,
            reopen_library,
            restore_position,
            controller_enabled,
            stems_key
        );
    }

    fn apply_to(&self, config: &mut Config) {
        if let Some(theme) = self.theme {
            config.theme = theme;
        }
        if let Some(language) = self.language {
            config.language = language;
        }
        if let Some(waveform_colors) = self.waveform_colors {
            config.waveform_colors = waveform_colors;
        }
        if let Some(hz) = self.waveform_low_mid_hz {
            config.waveform_low_mid_hz = hz;
        }
        if let Some(hz) = self.waveform_mid_high_hz {
            config.waveform_mid_high_hz = hz;
        }
        if let Some(gain) = self.waveform_gain_low {
            config.waveform_gain_low = gain;
        }
        if let Some(gain) = self.waveform_gain_mid {
            config.waveform_gain_mid = gain;
        }
        if let Some(gain) = self.waveform_gain_high {
            config.waveform_gain_high = gain;
        }
        if let Some(delay) = self.waveform_delay_ms {
            config.waveform_delay_ms = delay;
        }
        if let Some(reopen_library) = self.reopen_library {
            config.reopen_library = reopen_library;
        }
        if let Some(restore_position) = self.restore_position {
            config.restore_position = restore_position;
        }
        if let Some(controller_enabled) = self.controller_enabled {
            config.controller_enabled = controller_enabled;
        }
        if let Some(stems_key) = &self.stems_key {
            config.stems_key = stems_key.clone();
        }
    }
}

/// Where the profiles live.
#[derive(Debug, Clone)]
pub struct Locations {
    /// The user's own folder (`~/.config/open-engine-lib`): its `profiles` folder holds the
    /// profiles the user made (and the overlays of read-only ones), and `state.toml` says
    /// which profile is active.
    pub user_dir: PathBuf,
    /// The folders profiles are read from, the user's own first: `<user_dir>/profiles`, then
    /// `<dir>/open-engine-lib/profiles` for each dir in `$XDG_CONFIG_DIRS` (always with
    /// `/etc/xdg`, where the NixOS module puts its profile).
    pub search_paths: Vec<PathBuf>,
    /// The settings files from before there were profiles: moved into a profile once.
    pub legacy_config: PathBuf,
    pub legacy_overrides: PathBuf,
}

impl Locations {
    pub fn discover() -> Self {
        let user_dir = dirs::config_dir().unwrap_or_default().join(APP_DIR);
        let legacy_overrides = dirs::state_dir()
            .or_else(dirs::data_local_dir)
            .unwrap_or_default()
            .join(APP_DIR)
            .join(LEGACY_OVERRIDES_FILE);

        let mut system_dirs: Vec<PathBuf> = std::env::var_os("XDG_CONFIG_DIRS")
            .map(|dirs| std::env::split_paths(&dirs).collect())
            .unwrap_or_default();
        let etc_xdg = PathBuf::from("/etc/xdg");
        if cfg!(unix) && !system_dirs.contains(&etc_xdg) {
            system_dirs.push(etc_xdg);
        }
        let mut search_paths = vec![user_dir.join(PROFILES_DIR)];
        search_paths.extend(
            system_dirs
                .into_iter()
                .map(|dir| dir.join(APP_DIR).join(PROFILES_DIR)),
        );

        Self {
            legacy_config: user_dir.join(LEGACY_CONFIG_FILE),
            user_dir,
            search_paths,
            legacy_overrides,
        }
    }
}

/// A profile file: the settings it sets, and the profile it takes the others from.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
struct ProfileFile {
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    base_profile: Option<PathBuf>,
    #[serde(flatten)]
    settings: Layer,
}

/// A set of settings in a file of its own. A profile only says what it sets; the rest comes
/// from its `base_profile`, if it has one, and then from the defaults.
#[derive(Debug, Clone, PartialEq)]
pub struct Profile {
    pub path: PathBuf,
    pub name: String,
    /// The file can't be written (a profile of the system, as the NixOS module makes): a
    /// change makes an overlay of it in the user's folder instead.
    pub read_only: bool,
    pub base_profile: Option<PathBuf>,
    /// What the profile and the ones it inherits from set, the furthest base first.
    layers: Vec<Layer>,
}

impl Profile {
    /// The settings of the profile: the defaults, then what each layer sets.
    pub fn config(&self) -> Config {
        self.merged().sanitized()
    }

    fn merged(&self) -> Config {
        let mut config = Config::default();
        for layer in &self.layers {
            layer.apply_to(&mut config);
        }
        config
    }
}

/// How many profiles deep `base_profile` is followed (a loop stops there).
const MAX_INHERITANCE: usize = 16;

#[derive(Debug)]
pub struct ConfigStore {
    locations: Locations,
    /// By name.
    profiles: Vec<Profile>,
    /// The index of the profile in use.
    active: usize,
    /// Problems found while loading (unreadable or invalid files).
    pub warnings: Vec<String>,
}

impl ConfigStore {
    pub fn load(locations: Locations) -> Self {
        let mut warnings = Vec::new();
        migrate_legacy(&locations, &mut warnings);
        let mut store = Self {
            locations,
            profiles: Vec::new(),
            active: 0,
            warnings,
        };
        store.rescan();
        let path = store.user_profiles_dir().join(DEFAULT_PROFILE_FILE);
        // (A default profile that is there but can't be read is not written over.)
        if store.profiles.is_empty() && !path.exists() {
            let file = ProfileFile {
                name: Some(DEFAULT_PROFILE_NAME.to_owned()),
                ..ProfileFile::default()
            };
            if let Err(error) = write_profile(&path, &file) {
                store
                    .warnings
                    .push(format!("Could not create {}: {error}", path.display()));
            }
            store.rescan();
        }
        if store.profiles.is_empty() {
            // Nothing could be read or written: the defaults, in memory.
            store.profiles.push(Profile {
                path: store.user_profiles_dir().join(DEFAULT_PROFILE_FILE),
                name: DEFAULT_PROFILE_NAME.to_owned(),
                read_only: false,
                base_profile: None,
                layers: Vec::new(),
            });
        }
        let remembered = read_state(&store.locations).active_profile;
        store.active = remembered
            .and_then(|path| store.position(&path))
            .unwrap_or(0);
        store.warn_about_unusable_values();
        store
    }

    /// Reads all profiles again, keeping the active one active if it is still there.
    fn rescan(&mut self) {
        let active = self
            .profiles
            .get(self.active)
            .map(|profile| profile.path.clone());
        let mut seen = std::collections::HashSet::new();
        let mut profiles = Vec::new();
        for dir in &self.locations.search_paths {
            let entries = match fs::read_dir(dir) {
                Ok(entries) => entries,
                Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                Err(error) => {
                    self.warnings
                        .push(format!("Could not read {}: {error}", dir.display()));
                    continue;
                }
            };
            let mut paths: Vec<PathBuf> = entries
                .flatten()
                .map(|entry| entry.path())
                .filter(|path| {
                    path.extension()
                        .is_some_and(|extension| extension == "toml")
                })
                .collect();
            paths.sort();
            for path in paths {
                if seen.insert(path.clone()) {
                    match read_profile(&path, 0) {
                        Ok(profile) => profiles.push(profile),
                        Err(error) => self.warnings.push(error),
                    }
                }
            }
        }
        profiles.sort_by_key(|profile| profile.name.to_lowercase());
        self.profiles = profiles;
        self.active = active.and_then(|path| self.position(&path)).unwrap_or(0);
    }

    fn position(&self, path: &Path) -> Option<usize> {
        self.profiles
            .iter()
            .position(|profile| profile.path == path)
    }

    fn user_profiles_dir(&self) -> PathBuf {
        self.locations.user_dir.join(PROFILES_DIR)
    }

    fn warn_about_unusable_values(&mut self) {
        let config = self.active_profile().merged();
        let gains = [
            ("waveform_gain_low", config.waveform_gain_low),
            ("waveform_gain_mid", config.waveform_gain_mid),
            ("waveform_gain_high", config.waveform_gain_high),
        ];
        for (name, gain) in gains {
            if !gain_is_valid(gain) {
                self.warnings.push(format!(
                    "Ignoring {name} = {gain}: it must be above 0 and at most {WAVEFORM_MAX_GAIN}. \
                     Using the default."
                ));
            }
        }
        if config.waveform_delay_ms.abs() > WAVEFORM_DELAY_MAX_MS {
            self.warnings.push(format!(
                "Limiting waveform_delay_ms = {}: it must be between -{WAVEFORM_DELAY_MAX_MS} and \
                 {WAVEFORM_DELAY_MAX_MS}.",
                config.waveform_delay_ms
            ));
        }
        if !config.waveform_split_is_valid() {
            self.warnings.push(format!(
                "Ignoring the waveform split frequencies {} Hz and {} Hz: they must be between \
                 {WAVEFORM_MIN_SPLIT_HZ} and {WAVEFORM_MAX_SPLIT_HZ} Hz, the first below the \
                 second. Using {WAVEFORM_LOW_MID_HZ} Hz and {WAVEFORM_MID_HIGH_HZ} Hz.",
                config.waveform_low_mid_hz, config.waveform_mid_high_hz
            ));
        }
    }

    /// The settings of the profile in use.
    pub fn effective(&self) -> Config {
        self.active_profile().config()
    }

    /// All profiles, by name.
    pub fn profiles(&self) -> &[Profile] {
        &self.profiles
    }

    pub fn active_profile(&self) -> &Profile {
        &self.profiles[self.active]
    }

    /// The folders profiles are read from.
    pub fn search_paths(&self) -> &[PathBuf] {
        &self.locations.search_paths
    }

    /// Uses the profile at `path` from now on (and next time).
    pub fn select_profile(&mut self, path: &Path) -> io::Result<()> {
        let Some(index) = self.position(path) else {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("there is no profile {}", path.display()),
            ));
        };
        self.active = index;
        write_state(
            &self.locations,
            &StateFile {
                active_profile: Some(path.to_path_buf()),
            },
        )
    }

    /// Makes a new profile, with the default settings, in the user's folder and uses it.
    pub fn create_profile(&mut self, name: &str) -> io::Result<()> {
        let name = name.trim();
        let stem = file_stem(name);
        if stem.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "a profile's name needs a letter or a digit",
            ));
        }
        let path = self.user_profiles_dir().join(format!("{stem}.toml"));
        if path.exists() {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!("there is a profile called {name} already"),
            ));
        }
        let file = ProfileFile {
            name: Some(name.to_owned()),
            ..ProfileFile::default()
        };
        write_profile(&path, &file)?;
        self.rescan();
        self.select_profile(&path)
    }

    /// The read-only profile the one in use is an overlay of: the user's changes to it.
    pub fn overlaid(&self) -> Option<&Profile> {
        let profile = self.active_profile();
        if profile.read_only {
            return None;
        }
        let base = self.position(profile.base_profile.as_deref()?)?;
        let base = &self.profiles[base];
        (base.read_only && profile.path == self.overlay_path(&base.path)).then_some(base)
    }

    /// Throws the user's changes to a read-only profile away and uses the profile itself
    /// again. Does nothing unless the profile in use is such an overlay.
    pub fn reset_overrides(&mut self) -> io::Result<()> {
        let Some(base) = self.overlaid().map(|base| base.path.clone()) else {
            return Ok(());
        };
        fs::remove_file(&self.active_profile().path)?;
        self.rescan();
        self.select_profile(&base)
    }

    pub fn set_theme(&mut self, theme: ThemeMode) -> io::Result<()> {
        self.edit(|layer| layer.theme = Some(theme))
    }

    pub fn set_language(&mut self, language: Language) -> io::Result<()> {
        self.edit(|layer| layer.language = Some(language))
    }

    pub fn set_waveform_colors(&mut self, colors: WaveformColors) -> io::Result<()> {
        self.edit(|layer| layer.waveform_colors = Some(colors))
    }

    /// Sets how much one band of the waveform is amplified.
    pub fn set_waveform_gain(&mut self, band: WaveformBand, gain: f32) -> io::Result<()> {
        self.edit(|layer| match band {
            WaveformBand::Low => layer.waveform_gain_low = Some(gain),
            WaveformBand::Mid => layer.waveform_gain_mid = Some(gain),
            WaveformBand::High => layer.waveform_gain_high = Some(gain),
        })
    }

    /// Sets all three gains back to the defaults.
    pub fn reset_waveform_gains(&mut self) -> io::Result<()> {
        self.edit(|layer| {
            layer.waveform_gain_low = Some(WAVEFORM_GAIN_LOW);
            layer.waveform_gain_mid = Some(WAVEFORM_GAIN_MID);
            layer.waveform_gain_high = Some(WAVEFORM_GAIN_HIGH);
        })
    }

    /// Sets how far the waveform trails the player, in milliseconds.
    pub fn set_waveform_delay_ms(&mut self, delay: i32) -> io::Result<()> {
        self.edit(|layer| layer.waveform_delay_ms = Some(delay))
    }

    pub fn set_reopen_library(&mut self, reopen: bool) -> io::Result<()> {
        self.edit(|layer| layer.reopen_library = Some(reopen))
    }

    /// Sets the stems key (empty for none). Kept as it was written.
    pub fn set_stems_key(&mut self, key: &str) -> io::Result<()> {
        let key = key.trim().to_owned();
        self.edit(|layer| layer.stems_key = Some(key))
    }

    pub fn set_controller_enabled(&mut self, enabled: bool) -> io::Result<()> {
        self.edit(|layer| layer.controller_enabled = Some(enabled))
    }

    pub fn set_restore_position(&mut self, restore: bool) -> io::Result<()> {
        self.edit(|layer| layer.restore_position = Some(restore))
    }

    /// Changes the profile in use and saves it. A read-only profile is not changed: the change
    /// goes to an overlay of it in the user's folder (`<name>-override-<hash>.toml`, with the
    /// profile as its base), which is used from then on.
    fn edit(&mut self, change: impl FnOnce(&mut Layer)) -> io::Result<()> {
        let profile = self.active_profile().clone();
        let (path, mut file) = if profile.read_only {
            let overlay = self.overlay_path(&profile.path);
            let file = ProfileFile {
                name: Some(profile.name.clone()),
                base_profile: Some(profile.path.clone()),
                ..ProfileFile::default()
            };
            (overlay, file)
        } else {
            let file = match fs::read_to_string(&profile.path) {
                Ok(text) => toml::from_str(&text).map_err(io::Error::other)?,
                Err(error) if error.kind() == io::ErrorKind::NotFound => ProfileFile {
                    name: Some(profile.name.clone()),
                    ..ProfileFile::default()
                },
                Err(error) => return Err(error),
            };
            (profile.path.clone(), file)
        };
        change(&mut file.settings);
        write_profile(&path, &file)?;
        self.rescan();
        if profile.read_only {
            self.select_profile(&path)?;
        }
        Ok(())
    }

    /// Where the overlay of the read-only profile at `source` goes.
    fn overlay_path(&self, source: &Path) -> PathBuf {
        let stem = source
            .file_stem()
            .and_then(|stem| stem.to_str())
            .unwrap_or("profile");
        self.user_profiles_dir()
            .join(format!("{stem}-override-{:x}.toml", path_hash(source)))
    }
}

/// Which profile is in use, kept in the user's folder.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
struct StateFile {
    #[serde(skip_serializing_if = "Option::is_none")]
    active_profile: Option<PathBuf>,
}

fn read_state(locations: &Locations) -> StateFile {
    fs::read_to_string(locations.user_dir.join(STATE_FILE))
        .ok()
        .and_then(|text| toml::from_str(&text).ok())
        .unwrap_or_default()
}

fn write_state(locations: &Locations, state: &StateFile) -> io::Result<()> {
    write_toml(&locations.user_dir.join(STATE_FILE), state)
}

/// Moves the settings of a version from before profiles (`config.toml` in the user's folder,
/// with the overrides that were kept on top of a read-only one) into the user's `Default`
/// profile, once: only while the user has no profiles yet.
fn migrate_legacy(locations: &Locations, warnings: &mut Vec<String>) {
    let profiles = locations.user_dir.join(PROFILES_DIR);
    let has_profiles = fs::read_dir(&profiles).is_ok_and(|mut entries| entries.next().is_some());
    let legacy = [&locations.legacy_config, &locations.legacy_overrides];
    if has_profiles || !legacy.iter().any(|path| path.is_file()) {
        return;
    }
    let mut settings = Layer::default();
    for path in legacy {
        match fs::read_to_string(path) {
            Ok(text) => match toml::from_str::<Layer>(&text) {
                Ok(layer) => settings.take_from(layer),
                Err(error) => {
                    warnings.push(format!(
                        "Ignoring invalid config {}: {error}",
                        path.display()
                    ));
                }
            },
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => {
                warnings.push(format!("Could not read config {}: {error}", path.display()));
                return;
            }
        }
    }
    let file = ProfileFile {
        name: Some(DEFAULT_PROFILE_NAME.to_owned()),
        base_profile: None,
        settings,
    };
    if let Err(error) = write_profile(&profiles.join(DEFAULT_PROFILE_FILE), &file) {
        warnings.push(format!(
            "Could not move the settings into a profile: {error}"
        ));
        return;
    }
    for path in legacy {
        let _ = fs::remove_file(path);
    }
}

/// Checks write access without modifying the file.
fn is_writable(path: &Path) -> bool {
    OpenOptions::new().write(true).open(path).is_ok()
}

/// Reads the profile at `path` with the ones it inherits from.
fn read_profile(path: &Path, depth: usize) -> Result<Profile, String> {
    if depth > MAX_INHERITANCE {
        return Err(format!(
            "Ignoring {}: its base profiles go round in a circle",
            path.display()
        ));
    }
    let text = fs::read_to_string(path)
        .map_err(|error| format!("Could not read config {}: {error}", path.display()))?;
    let file: ProfileFile = toml::from_str(&text)
        .map_err(|error| format!("Ignoring invalid config {}: {error}", path.display()))?;
    let base_profile = file.base_profile.as_deref().map(expand_home);
    let mut layers = match &base_profile {
        Some(base) => read_profile(base, depth + 1)?.layers,
        None => Vec::new(),
    };
    layers.push(file.settings);
    Ok(Profile {
        path: path.to_path_buf(),
        name: file.name.unwrap_or_else(|| {
            path.file_stem()
                .and_then(|stem| stem.to_str())
                .unwrap_or(DEFAULT_PROFILE_NAME)
                .to_owned()
        }),
        read_only: !is_writable(path),
        base_profile,
        layers,
    })
}

fn write_profile(path: &Path, file: &ProfileFile) -> io::Result<()> {
    write_toml(path, file)
}

fn write_toml(path: &Path, value: &impl Serialize) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let text = toml::to_string_pretty(value).map_err(io::Error::other)?;
    let tmp = path.with_extension("toml.tmp");
    fs::write(&tmp, text)?;
    fs::rename(&tmp, path)
}

/// `~/…` in a profile's base means the home folder.
fn expand_home(path: &Path) -> PathBuf {
    match (path.strip_prefix("~"), dirs::home_dir()) {
        (Ok(rest), Some(home)) => home.join(rest),
        _ => path.to_path_buf(),
    }
}

/// A file name for a profile called `name`: letters and digits, the rest dashes.
fn file_stem(name: &str) -> String {
    name.chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || character == '_' {
                character.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect::<String>()
        .trim_matches('-')
        .to_owned()
}

/// FNV-1a of the path, so that an overlay's name says which profile it belongs to.
fn path_hash(path: &Path) -> u64 {
    path.as_os_str()
        .to_string_lossy()
        .bytes()
        .fold(0xcbf2_9ce4_8422_2325, |hash, byte| {
            (hash ^ u64::from(byte)).wrapping_mul(0x0100_0000_01b3)
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            static COUNTER: AtomicU32 = AtomicU32::new(0);
            let dir = std::env::temp_dir().join(format!(
                "oel-config-test-{}-{}",
                std::process::id(),
                COUNTER.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        fn path(&self, name: &str) -> PathBuf {
            self.0.join(name)
        }

        fn locations(&self) -> Locations {
            Locations {
                user_dir: self.path("user"),
                search_paths: vec![self.path("user/profiles"), self.path("etc/profiles")],
                legacy_config: self.path("user/config.toml"),
                legacy_overrides: self.path("state/overrides.toml"),
            }
        }

        /// The user's own profile, made when there is none.
        fn user_profile(&self) -> PathBuf {
            self.path("user/profiles/default.toml")
        }

        /// Writes the user's profile (before the store is loaded).
        fn write_user(&self, text: &str) {
            fs::create_dir_all(self.path("user/profiles")).unwrap();
            fs::write(self.user_profile(), text).unwrap();
        }

        /// Writes a read-only profile of the system.
        fn write_system(&self, file: &str, text: &str) -> PathBuf {
            fs::create_dir_all(self.path("etc/profiles")).unwrap();
            let path = self.path(&format!("etc/profiles/{file}"));
            fs::write(&path, text).unwrap();
            make_read_only(&path);
            path
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn make_read_only(path: &Path) {
        let mut permissions = fs::metadata(path).unwrap().permissions();
        permissions.set_readonly(true);
        fs::set_permissions(path, permissions).unwrap();
    }

    #[test]
    fn defaults_without_any_file() {
        let dir = TempDir::new();
        let store = ConfigStore::load(dir.locations());
        assert_eq!(store.effective(), Config::default());
        // A default profile was made for the user.
        assert_eq!(store.profiles().len(), 1);
        assert_eq!(store.active_profile().name, "Default");
        assert!(dir.user_profile().is_file());
    }

    #[test]
    fn the_waveform_colors_are_a_saved_choice() {
        let dir = TempDir::new();
        let mut store = ConfigStore::load(dir.locations());
        assert_eq!(store.effective().waveform_colors, WaveformColors::Stacked);

        for colors in WaveformColors::ALL {
            store.set_waveform_colors(colors).unwrap();
            let reloaded = ConfigStore::load(dir.locations());
            assert_eq!(reloaded.effective().waveform_colors, colors);
        }
        let written = fs::read_to_string(dir.user_profile()).unwrap();
        assert!(
            written.contains("waveform_colors = \"spectrum\""),
            "{written}"
        );

        // An unknown name is ignored like any other invalid config, not a crash.
        fs::write(dir.user_profile(), "waveform_colors = \"neon\"\n").unwrap();
        let broken = ConfigStore::load(dir.locations());
        assert_eq!(broken.effective().waveform_colors, WaveformColors::Stacked);
        assert!(!broken.warnings.is_empty());
    }

    #[test]
    fn the_waveform_bands_meet_at_600_hz_and_4_khz_unless_configured() {
        let dir = TempDir::new();
        let store = ConfigStore::load(dir.locations());
        let config = store.effective();
        assert_eq!(
            (config.waveform_low_mid_hz, config.waveform_mid_high_hz),
            (600, 4_000)
        );
        assert!(store.warnings.is_empty());

        dir.write_user("waveform_low_mid_hz = 250\nwaveform_mid_high_hz = 3000\n");
        let configured = ConfigStore::load(dir.locations());
        let config = configured.effective();
        assert_eq!(
            (config.waveform_low_mid_hz, config.waveform_mid_high_hz),
            (250, 3_000)
        );
        assert!(configured.warnings.is_empty());

        // Editing something else keeps them.
        let mut configured = ConfigStore::load(dir.locations());
        configured.set_theme(ThemeMode::Light).unwrap();
        let reloaded = ConfigStore::load(dir.locations()).effective();
        assert_eq!(
            (reloaded.waveform_low_mid_hz, reloaded.waveform_mid_high_hz),
            (250, 3_000)
        );
    }

    #[test]
    fn nonsensical_split_frequencies_are_replaced_by_the_defaults_with_a_warning() {
        for (low, high) in [(4_000, 600), (600, 600), (5, 4_000), (600, 40_000), (0, 0)] {
            let dir = TempDir::new();
            dir.write_user(&format!(
                "waveform_low_mid_hz = {low}\nwaveform_mid_high_hz = {high}\n"
            ));
            let store = ConfigStore::load(dir.locations());

            let config = store.effective();
            assert_eq!(
                (config.waveform_low_mid_hz, config.waveform_mid_high_hz),
                (600, 4_000),
                "{low} / {high}"
            );
            assert_eq!(store.warnings.len(), 1, "{low} / {high}");
            assert!(
                store.warnings[0].contains("waveform split"),
                "{}",
                store.warnings[0]
            );
        }
    }

    #[test]
    fn the_waveform_gains_are_a_saved_choice_with_a_reset() {
        let dir = TempDir::new();
        let mut store = ConfigStore::load(dir.locations());
        let config = store.effective();
        assert_eq!(
            [
                config.waveform_gain_low,
                config.waveform_gain_mid,
                config.waveform_gain_high
            ],
            [1.0, 1.0, 1.0]
        );

        store.set_waveform_gain(WaveformBand::Mid, 1.75).unwrap();
        store.set_waveform_gain(WaveformBand::High, 3.0).unwrap();
        let reloaded = ConfigStore::load(dir.locations()).effective();
        assert_eq!(reloaded.waveform_gain(WaveformBand::Low), 1.0);
        assert_eq!(reloaded.waveform_gain(WaveformBand::Mid), 1.75);
        assert_eq!(reloaded.waveform_gain(WaveformBand::High), 3.0);

        store.reset_waveform_gains().unwrap();
        let reset = ConfigStore::load(dir.locations()).effective();
        assert_eq!(reset.waveform_gain(WaveformBand::Mid), 1.0);
        assert_eq!(reset.waveform_gain(WaveformBand::High), 1.0);
    }

    #[test]
    fn the_waveform_delay_is_a_saved_choice_within_limits() {
        let dir = TempDir::new();
        let mut store = ConfigStore::load(dir.locations());
        assert_eq!(store.effective().waveform_delay_ms, 0);

        store.set_waveform_delay_ms(120).unwrap();
        assert_eq!(
            ConfigStore::load(dir.locations())
                .effective()
                .waveform_delay_ms,
            120
        );
        store.set_waveform_delay_ms(-80).unwrap();
        assert_eq!(
            ConfigStore::load(dir.locations())
                .effective()
                .waveform_delay_ms,
            -80
        );

        // A value from a hand-written file that is out of range is brought into range.
        fs::write(dir.user_profile(), "waveform_delay_ms = 9000\n").unwrap();
        let store = ConfigStore::load(dir.locations());
        assert_eq!(store.effective().waveform_delay_ms, WAVEFORM_DELAY_MAX_MS);
        assert!(store.warnings[0].contains("waveform_delay_ms"));
    }

    #[test]
    fn a_nonsensical_gain_is_replaced_on_its_own_with_a_warning() {
        for bad in ["0", "-2.0", "25.0", "nan", "inf"] {
            let dir = TempDir::new();
            dir.write_user(&format!(
                "waveform_gain_low = 2.5\nwaveform_gain_mid = {bad}\n"
            ));
            let store = ConfigStore::load(dir.locations());

            let config = store.effective();
            assert_eq!(config.waveform_gain_mid, 1.0, "{bad}");
            assert_eq!(
                config.waveform_gain_low, 2.5,
                "the good one is kept ({bad})"
            );
            assert_eq!(store.warnings.len(), 1, "{bad}");
            assert!(
                store.warnings[0].contains("waveform_gain_mid"),
                "{}",
                store.warnings[0]
            );
        }
    }

    #[test]
    fn restoring_the_last_session_is_on_by_default_and_can_be_turned_off() {
        let dir = TempDir::new();
        let mut store = ConfigStore::load(dir.locations());
        assert!(store.effective().reopen_library);
        assert!(store.effective().restore_position);

        store.set_reopen_library(false).unwrap();
        store.set_restore_position(false).unwrap();

        let reloaded = ConfigStore::load(dir.locations());
        assert!(!reloaded.effective().reopen_library);
        assert!(!reloaded.effective().restore_position);
        // Only what was changed is written down.
        let written = fs::read_to_string(dir.user_profile()).unwrap();
        assert!(!written.contains("theme") && written.contains("reopen_library = false"));
    }

    #[test]
    fn a_writable_profile_is_edited_directly() {
        let dir = TempDir::new();
        let mut store = ConfigStore::load(dir.locations());
        store.set_theme(ThemeMode::Light).unwrap();

        let reloaded = ConfigStore::load(dir.locations());
        assert_eq!(reloaded.effective().theme, ThemeMode::Light);
        assert_eq!(reloaded.profiles().len(), 1, "no overlay");
        assert!(reloaded.overlaid().is_none());
    }

    #[test]
    fn profiles_come_from_every_folder_and_the_one_in_use_is_remembered() {
        let dir = TempDir::new();
        dir.write_user("theme = \"light\"\n");
        let system = dir.write_system("nixos.toml", "name = \"NixOS\"\nstems_key = \"ab\"\n");

        let mut store = ConfigStore::load(dir.locations());
        let names: Vec<_> = store.profiles().iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, ["default", "NixOS"]);
        assert_eq!(store.effective().theme, ThemeMode::Light);

        store.select_profile(&system).unwrap();
        assert_eq!(store.effective().stems_key, "ab");
        assert_eq!(store.effective().theme, ThemeMode::Dark, "a default");
        let reloaded = ConfigStore::load(dir.locations());
        assert_eq!(reloaded.active_profile().path, system);

        // One the user made is used at once, with the defaults.
        store.create_profile("Club gig").unwrap();
        assert_eq!(store.active_profile().name, "Club gig");
        assert!(dir.path("user/profiles/club-gig.toml").is_file());
        assert!(store.create_profile("club gig").is_err(), "the same file");
        assert!(store.create_profile(" ?! ").is_err());
    }

    #[test]
    fn editing_a_read_only_profile_makes_an_overlay_that_can_be_thrown_away() {
        let dir = TempDir::new();
        let system = dir.write_system(
            "nixos.toml",
            "name = \"NixOS\"\ntheme = \"light\"\nlanguage = \"de\"\n",
        );
        let mut store = ConfigStore::load(dir.locations());
        store.select_profile(&system).unwrap();
        // Skip when running with privileges that ignore read-only files.
        if !store.active_profile().read_only {
            return;
        }

        store.set_language(Language::English).unwrap();
        let overlay = store.active_profile().clone();
        assert_ne!(overlay.path, system);
        assert_eq!(overlay.base_profile.as_deref(), Some(system.as_path()));
        assert_eq!(
            store.overlaid().map(|base| base.path.clone()),
            Some(system.clone())
        );
        // The change is in the overlay, the rest comes from the profile underneath.
        let config = store.effective();
        assert_eq!(
            (config.language, config.theme),
            (Language::English, ThemeMode::Light)
        );
        assert_eq!(
            fs::read_to_string(&system).unwrap(),
            "name = \"NixOS\"\ntheme = \"light\"\nlanguage = \"de\"\n"
        );

        // Changed again, the overlay is edited, and it is still in use next time.
        store.set_theme(ThemeMode::Dark).unwrap();
        let reloaded = ConfigStore::load(dir.locations());
        assert_eq!(reloaded.active_profile().path, overlay.path);
        let config = reloaded.effective();
        assert_eq!(
            (config.language, config.theme),
            (Language::English, ThemeMode::Dark)
        );

        store.reset_overrides().unwrap();
        assert_eq!(store.active_profile().path, system);
        assert_eq!(store.effective().language, Language::German);
        assert!(!overlay.path.exists());
        assert!(store.overlaid().is_none());
    }

    #[test]
    fn a_profile_takes_what_it_does_not_set_from_its_base() {
        let dir = TempDir::new();
        let base = dir.write_system("base.toml", "theme = \"light\"\nwaveform_delay_ms = 40\n");
        dir.write_user(&format!(
            "base_profile = \"{}\"\nwaveform_delay_ms = 80\n",
            base.display()
        ));
        let mut store = ConfigStore::load(dir.locations());
        store.select_profile(&dir.user_profile()).unwrap();
        let config = store.effective();
        assert_eq!(
            (config.theme, config.waveform_delay_ms),
            (ThemeMode::Light, 80)
        );

        // A circle of bases is not followed for ever.
        let circle = dir.path("user/profiles/circle.toml");
        fs::write(
            &circle,
            format!("base_profile = \"{}\"\n", circle.display()),
        )
        .unwrap();
        let store = ConfigStore::load(dir.locations());
        assert!(
            store
                .warnings
                .iter()
                .any(|warning| warning.contains("circle"))
        );
        assert!(
            store
                .profiles()
                .iter()
                .all(|profile| profile.path != circle)
        );
    }

    #[test]
    fn the_settings_from_before_profiles_move_into_the_default_profile() {
        let dir = TempDir::new();
        fs::create_dir_all(dir.path("user")).unwrap();
        fs::write(
            dir.path("user/config.toml"),
            "theme = \"light\"\nwaveform_delay_ms = 265\n",
        )
        .unwrap();
        fs::create_dir_all(dir.path("state")).unwrap();
        fs::write(
            dir.path("state/overrides.toml"),
            "waveform_delay_ms = 100\n",
        )
        .unwrap();

        let store = ConfigStore::load(dir.locations());
        let config = store.effective();
        assert_eq!(
            (config.theme, config.waveform_delay_ms),
            (ThemeMode::Light, 100)
        );
        assert_eq!(store.active_profile().name, "Default");
        assert!(!dir.path("user/config.toml").exists());
        assert!(!dir.path("state/overrides.toml").exists());

        // Only once: a config.toml that turns up later is not read.
        fs::write(dir.path("user/config.toml"), "theme = \"dark\"\n").unwrap();
        assert_eq!(
            ConfigStore::load(dir.locations()).effective().theme,
            ThemeMode::Light
        );
    }

    #[test]
    fn language_follows_locale() {
        assert_eq!(Language::from_locale("de_DE.UTF-8"), Language::German);
        assert_eq!(Language::from_locale("en_US.UTF-8"), Language::English);
        assert_eq!(Language::from_locale("C"), Language::English);
    }

    #[test]
    fn an_invalid_profile_warns_and_is_left_out() {
        let dir = TempDir::new();
        dir.write_user("theme = \"purple\"\n");
        let store = ConfigStore::load(dir.locations());
        assert_eq!(store.effective(), Config::default());
        assert_eq!(store.warnings.len(), 1);
    }
}
