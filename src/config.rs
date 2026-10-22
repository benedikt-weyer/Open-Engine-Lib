//! Layered, Nix-friendly configuration.
//!
//! The base config is a TOML file, found in this order:
//!   1. `$OPEN_ENGINE_LIB_CONFIG`
//!   2. `<user config dir>/open-engine-lib/config.toml`
//!   3. `<dir>/open-engine-lib/config.toml` for each dir in `$XDG_CONFIG_DIRS`
//!      (always including `/etc/xdg`, where the NixOS module puts it)
//!
//! If the base file is writable (or doesn't exist) the app edits it directly.
//! If it is read-only, as with a declaratively generated file in the Nix store,
//! changes are stored in an override file in the user state dir, which is merged
//! on top of the base. Removing the override file resets to the base config.

use std::fs::{self, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

const APP_DIR: &str = "open-engine-lib";
const CONFIG_FILE: &str = "config.toml";
const OVERRIDES_FILE: &str = "overrides.toml";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ThemeMode {
    Light,
    #[default]
    Dark,
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
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    pub theme: ThemeMode,
    pub language: Language,
    pub waveform_colors: WaveformColors,
    /// Open the library that was open when the app was last closed.
    pub reopen_library: bool,
    /// When reopening it, also go back to the playlist, the song and its position.
    pub restore_position: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            theme: ThemeMode::default(),
            language: Language::default(),
            waveform_colors: WaveformColors::default(),
            reopen_library: true,
            restore_position: true,
        }
    }
}

/// One config layer: only the values it sets.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
struct Layer {
    #[serde(skip_serializing_if = "Option::is_none")]
    theme: Option<ThemeMode>,
    #[serde(skip_serializing_if = "Option::is_none")]
    language: Option<Language>,
    #[serde(skip_serializing_if = "Option::is_none")]
    waveform_colors: Option<WaveformColors>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reopen_library: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    restore_position: Option<bool>,
}

impl Layer {
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
        if let Some(reopen_library) = self.reopen_library {
            config.reopen_library = reopen_library;
        }
        if let Some(restore_position) = self.restore_position {
            config.restore_position = restore_position;
        }
    }
}

/// Where the config files live.
#[derive(Debug, Clone)]
pub struct Locations {
    /// Base config candidates, highest priority first.
    pub base_candidates: Vec<PathBuf>,
    /// Where a new base config is created when none exists.
    pub user_config: PathBuf,
    /// Override layer, used when the base config is read-only.
    pub overrides: PathBuf,
}

impl Locations {
    pub fn discover() -> Self {
        let user_config = dirs::config_dir()
            .unwrap_or_default()
            .join(APP_DIR)
            .join(CONFIG_FILE);
        let overrides = dirs::state_dir()
            .or_else(dirs::data_local_dir)
            .unwrap_or_default()
            .join(APP_DIR)
            .join(OVERRIDES_FILE);

        let mut base_candidates = Vec::new();
        if let Some(path) = std::env::var_os("OPEN_ENGINE_LIB_CONFIG") {
            base_candidates.push(PathBuf::from(path));
        }
        base_candidates.push(user_config.clone());
        if cfg!(unix) {
            let mut system_dirs: Vec<PathBuf> = std::env::var_os("XDG_CONFIG_DIRS")
                .map(|dirs| std::env::split_paths(&dirs).collect())
                .unwrap_or_default();
            let etc_xdg = PathBuf::from("/etc/xdg");
            if !system_dirs.contains(&etc_xdg) {
                system_dirs.push(etc_xdg);
            }
            base_candidates.extend(
                system_dirs
                    .into_iter()
                    .map(|dir| dir.join(APP_DIR).join(CONFIG_FILE)),
            );
        }

        Self {
            base_candidates,
            user_config,
            overrides,
        }
    }
}

#[derive(Debug)]
pub struct ConfigStore {
    base_path: PathBuf,
    overrides_path: PathBuf,
    base: Layer,
    overrides: Layer,
    /// The base config is read-only, so edits go to the override layer.
    layered: bool,
    /// Problems found while loading (unreadable or invalid files).
    pub warnings: Vec<String>,
}

impl ConfigStore {
    pub fn load(locations: Locations) -> Self {
        let mut warnings = Vec::new();
        let found = locations.base_candidates.iter().find(|path| path.exists());
        let layered = found.is_some_and(|path| !is_writable(path));
        let base_path = found.cloned().unwrap_or(locations.user_config);

        let base = read_layer(&base_path, &mut warnings);
        let overrides = if layered {
            read_layer(&locations.overrides, &mut warnings)
        } else {
            Layer::default()
        };

        Self {
            base_path,
            overrides_path: locations.overrides,
            base,
            overrides,
            layered,
            warnings,
        }
    }

    /// The merged config: defaults, then the base layer, then overrides.
    pub fn effective(&self) -> Config {
        let mut config = Config::default();
        self.base.apply_to(&mut config);
        self.overrides.apply_to(&mut config);
        config
    }

    /// The config as it would be without overrides.
    pub fn underlying(&self) -> Config {
        let mut config = Config::default();
        self.base.apply_to(&mut config);
        config
    }

    /// Whether the base config is read-only and edits are layered on top.
    pub fn is_layered(&self) -> bool {
        self.layered
    }

    /// Whether any value currently differs from the underlying config source.
    pub fn has_overrides(&self) -> bool {
        self.layered && self.overrides != Layer::default()
    }

    pub fn base_path(&self) -> &Path {
        &self.base_path
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

    pub fn set_reopen_library(&mut self, reopen: bool) -> io::Result<()> {
        self.edit(|layer| layer.reopen_library = Some(reopen))
    }

    pub fn set_restore_position(&mut self, restore: bool) -> io::Result<()> {
        self.edit(|layer| layer.restore_position = Some(restore))
    }

    /// Applies a change to the layer edits belong in (the override layer when
    /// layered, else the base) and saves it.
    fn edit(&mut self, change: impl FnOnce(&mut Layer)) -> io::Result<()> {
        let (layer, path) = if self.layered {
            (&mut self.overrides, &self.overrides_path)
        } else {
            (&mut self.base, &self.base_path)
        };
        change(layer);
        write_layer(path, layer)
    }

    /// Drop all overrides so the underlying config applies again. A no-op
    /// unless the config is layered, since there is nothing underneath
    /// a directly edited file.
    pub fn reset_overrides(&mut self) -> io::Result<()> {
        if !self.layered {
            return Ok(());
        }
        self.overrides = Layer::default();
        match fs::remove_file(&self.overrides_path) {
            Err(error) if error.kind() != io::ErrorKind::NotFound => Err(error),
            _ => Ok(()),
        }
    }
}

/// Checks write access without modifying the file.
fn is_writable(path: &Path) -> bool {
    OpenOptions::new().write(true).open(path).is_ok()
}

fn read_layer(path: &Path, warnings: &mut Vec<String>) -> Layer {
    match fs::read_to_string(path) {
        Ok(text) => toml::from_str(&text).unwrap_or_else(|error| {
            warnings.push(format!(
                "Ignoring invalid config {}: {error}",
                path.display()
            ));
            Layer::default()
        }),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Layer::default(),
        Err(error) => {
            warnings.push(format!("Could not read config {}: {error}", path.display()));
            Layer::default()
        }
    }
}

fn write_layer(path: &Path, layer: &Layer) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let text = toml::to_string_pretty(layer).map_err(io::Error::other)?;
    let tmp = path.with_extension("toml.tmp");
    fs::write(&tmp, text)?;
    fs::rename(&tmp, path)
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
                base_candidates: vec![self.path("etc.toml"), self.path("user.toml")],
                user_config: self.path("user.toml"),
                overrides: self.path("state/overrides.toml"),
            }
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
        assert!(!store.is_layered());
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
        let written = fs::read_to_string(dir.path("user.toml")).unwrap();
        assert!(
            written.contains("waveform_colors = \"spectrum\""),
            "{written}"
        );

        // An unknown name is ignored like any other invalid config, not a crash.
        fs::write(dir.path("user.toml"), "waveform_colors = \"neon\"\n").unwrap();
        let broken = ConfigStore::load(dir.locations());
        assert_eq!(broken.effective().waveform_colors, WaveformColors::Stacked);
        assert!(!broken.warnings.is_empty());
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
        let written = fs::read_to_string(dir.path("user.toml")).unwrap();
        assert!(!written.contains("theme") && written.contains("reopen_library = false"));
    }

    #[test]
    fn writable_base_is_edited_directly() {
        let dir = TempDir::new();
        let mut store = ConfigStore::load(dir.locations());
        store.set_theme(ThemeMode::Light).unwrap();

        assert!(!dir.path("state/overrides.toml").exists());
        let reloaded = ConfigStore::load(dir.locations());
        assert_eq!(reloaded.effective().theme, ThemeMode::Light);
        assert!(!reloaded.is_layered());
    }

    #[test]
    fn read_only_base_gets_an_override_layer() {
        let dir = TempDir::new();
        fs::write(dir.path("etc.toml"), "theme = \"light\"\n").unwrap();
        make_read_only(&dir.path("etc.toml"));

        let mut store = ConfigStore::load(dir.locations());
        // Skip when running with privileges that ignore read-only files.
        if !store.is_layered() {
            return;
        }
        assert_eq!(store.effective().theme, ThemeMode::Light);

        store.set_theme(ThemeMode::Dark).unwrap();
        assert_eq!(store.effective().theme, ThemeMode::Dark);
        assert_eq!(store.underlying().theme, ThemeMode::Light);
        assert!(store.has_overrides());
        assert_eq!(
            fs::read_to_string(dir.path("etc.toml")).unwrap(),
            "theme = \"light\"\n"
        );

        let reloaded = ConfigStore::load(dir.locations());
        assert_eq!(reloaded.effective().theme, ThemeMode::Dark);

        store.reset_overrides().unwrap();
        assert_eq!(store.effective().theme, ThemeMode::Light);
        assert!(!store.has_overrides());
        assert!(!dir.path("state/overrides.toml").exists());
    }

    #[test]
    fn language_is_saved_and_layered_independently_of_theme() {
        let dir = TempDir::new();
        fs::write(
            dir.path("etc.toml"),
            "theme = \"light\"\nlanguage = \"de\"\n",
        )
        .unwrap();
        make_read_only(&dir.path("etc.toml"));

        let mut store = ConfigStore::load(dir.locations());
        // Skip when running with privileges that ignore read-only files.
        if !store.is_layered() {
            return;
        }
        assert_eq!(store.effective().language, Language::German);

        store.set_language(Language::English).unwrap();
        let reloaded = ConfigStore::load(dir.locations());
        assert_eq!(reloaded.effective().language, Language::English);
        assert_eq!(reloaded.effective().theme, ThemeMode::Light);
        assert_eq!(reloaded.underlying().language, Language::German);
    }

    #[test]
    fn language_follows_locale() {
        assert_eq!(Language::from_locale("de_DE.UTF-8"), Language::German);
        assert_eq!(Language::from_locale("en_US.UTF-8"), Language::English);
        assert_eq!(Language::from_locale("C"), Language::English);
    }

    #[test]
    fn earlier_candidate_wins() {
        let dir = TempDir::new();
        fs::write(dir.path("etc.toml"), "theme = \"light\"\n").unwrap();
        fs::write(dir.path("user.toml"), "theme = \"dark\"\n").unwrap();
        let store = ConfigStore::load(dir.locations());
        assert_eq!(store.base_path(), dir.path("etc.toml"));
    }

    #[test]
    fn invalid_base_warns_and_falls_back() {
        let dir = TempDir::new();
        fs::write(dir.path("user.toml"), "theme = \"purple\"\n").unwrap();
        let store = ConfigStore::load(dir.locations());
        assert_eq!(store.effective(), Config::default());
        assert_eq!(store.warnings.len(), 1);
    }
}
