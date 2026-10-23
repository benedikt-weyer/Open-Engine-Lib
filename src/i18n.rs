//! UI translations, loaded from `locales/<code>.yaml`. The files are embedded
//! at build time and parsed into `Strings`, so a missing or misspelled key in
//! any language fails the `locale_files_are_complete` test.
//!
//! Strings may contain `{name}` placeholders, filled with `fill`. `{{` and `}}`
//! produce literal braces. Every language must use the same placeholders for a key.

use std::fmt::{Display, Write};
use std::sync::OnceLock;

use serde::Deserialize;

use crate::config::Language;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Strings {
    /// The language's name in that language, so it is recognizable from any UI language.
    pub native_name: String,
    pub menu_file: String,
    pub menu_open: String,
    pub menu_save_as: String,
    pub menu_exit: String,
    pub save_as_title: String,
    pub saving_library: String,
    pub save_library_failed: String,
    pub sync_button: String,
    pub sync_title: String,
    pub sync_source: String,
    pub sync_working_copy: String,
    pub sync_comparing: String,
    pub sync_unavailable: String,
    pub sync_busy: String,
    pub sync_outside_warning: String,
    pub sync_outside_confirm: String,
    pub sync_no_differences: String,
    pub sync_direction_hint: String,
    pub sync_tracks_summary: String,
    pub sync_playlists_summary: String,
    pub sync_databases_summary: String,
    pub sync_more: String,
    pub sync_run: String,
    pub sync_refresh: String,
    pub sync_close: String,
    pub sync_writing: String,
    pub sync_done: String,
    pub sync_nothing_written: String,
    pub sync_failed: String,
    pub sync_load_failed: String,
    pub sync_playlist_added: String,
    pub sync_playlist_removed: String,
    pub sync_playlist_renamed: String,
    pub sync_playlist_moved: String,
    pub sync_playlist_tracks: String,
    pub sync_playlist_track_counts: String,
    pub sync_playlist_order: String,
    pub sync_playlist_rules: String,
    pub sync_playlist_smart: String,
    pub nav_library: String,
    pub nav_settings: String,
    pub settings_title: String,
    pub theme: String,
    pub theme_light: String,
    pub theme_dark: String,
    pub language_heading: String,
    pub waveform_colors_heading: String,
    pub waveform_colors_mono: String,
    pub waveform_colors_rgb: String,
    pub waveform_colors_stacked: String,
    pub waveform_colors_spectrum: String,
    pub waveform_colors_short_mono: String,
    pub waveform_colors_short_rgb: String,
    pub waveform_colors_short_stacked: String,
    pub waveform_colors_short_spectrum: String,
    pub waveform_gains_heading: String,
    pub waveform_gains_hint: String,
    pub waveform_gain_low: String,
    pub waveform_gain_mid: String,
    pub waveform_gain_high: String,
    pub reset_waveform_gains: String,
    pub waveform_delay_heading: String,
    pub waveform_delay_hint: String,
    pub reset_waveform_delay: String,
    pub startup_heading: String,
    pub reopen_library: String,
    pub restore_position: String,
    pub config_read_only_notice: String,
    pub config_file: String,
    pub underlying_theme: String,
    pub underlying_language: String,
    pub reset_overrides: String,
    pub save_failed: String,
    pub backend_starting: String,
    pub backend_failed: String,
    pub backend_error: String,
    pub retry: String,
    pub library_closed_title: String,
    pub library_closed_hint: String,
    pub open_library: String,
    pub choose_library: String,
    pub opening_library: String,
    pub open_failed: String,
    pub recent_libraries: String,
    pub no_recent_libraries: String,
    pub forget_library: String,
    pub drives: String,
    pub no_drives: String,
    pub drive_has_library: String,
    pub drive_no_library: String,
    pub recents_save_failed: String,
    pub close_library: String,
    pub library_summary: String,
    pub library_cache: String,
    pub collection: String,
    pub smart_new: String,
    pub smart_edit: String,
    pub smart_title_new: String,
    pub smart_title_edit: String,
    pub smart_name: String,
    pub smart_match: String,
    pub smart_match_all: String,
    pub smart_match_any: String,
    pub smart_op_contains: String,
    pub smart_op_not_contains: String,
    pub smart_op_is: String,
    pub smart_op_is_not: String,
    pub smart_op_pattern: String,
    pub smart_value: String,
    pub smart_add_rule: String,
    pub smart_remove_rule: String,
    pub smart_rule_unsupported: String,
    pub smart_pattern_hint: String,
    pub smart_save: String,
    pub smart_saving: String,
    pub smart_cancel: String,
    pub smart_name_missing: String,
    pub smart_save_failed: String,
    pub playlist_new: String,
    pub playlist_rename: String,
    pub playlist_delete: String,
    pub playlist_title_new: String,
    pub playlist_title_rename: String,
    pub playlist_title_delete: String,
    pub playlist_place: String,
    pub playlist_top_level: String,
    pub playlist_create: String,
    pub playlist_rename_confirm: String,
    pub playlist_delete_confirm: String,
    pub playlist_delete_question: String,
    pub playlist_delete_inside: String,
    pub playlist_delete_note: String,
    pub playlist_failed: String,
    pub col_origin: String,
    pub origin_migrate: String,
    pub origin_migrate_tip: String,
    pub origin_migrated: String,
    pub origin_none: String,
    pub sync_playlist_missing: String,
    pub libsync_button: String,
    pub libsync_title: String,
    pub libsync_pick: String,
    pub libsync_add: String,
    pub libsync_add_title: String,
    pub libsync_target: String,
    pub libsync_comparing: String,
    pub libsync_failed: String,
    pub libsync_back: String,
    pub libsync_summary: String,
    pub libsync_nothing: String,
    pub libsync_hint: String,
    pub libsync_remove: String,
    pub libsync_missing: String,
    pub libsync_run: String,
    pub libsync_writing: String,
    pub libsync_run_failed: String,
    pub libsync_done: String,
    pub libsync_none_written: String,
    pub tracks_heading: String,
    pub no_tracks: String,
    pub col_title: String,
    pub col_artist: String,
    pub col_album: String,
    pub col_genre: String,
    pub col_bpm: String,
    pub col_length: String,
    pub col_rating: String,
    pub col_lighting: String,
    pub lighting_rendered: String,
    pub lighting_analysed: String,
    pub lighting_none: String,
    pub col_location: String,
    pub location_streaming: String,
    pub play: String,
    pub pause: String,
    pub stop: String,
    pub previous: String,
    pub next: String,
    pub nothing_playing: String,
    pub waveform_loading: String,
    pub waveform_unavailable: String,
    pub beatgrid_bpm: String,
    pub beatgrid_none: String,
    pub now_playing: String,
    pub streaming_not_playable: String,
    pub file_missing: String,
    pub play_failed: String,
}

impl Language {
    pub fn strings(self) -> &'static Strings {
        static ENGLISH: OnceLock<Strings> = OnceLock::new();
        static GERMAN: OnceLock<Strings> = OnceLock::new();

        match self {
            Language::English => {
                ENGLISH.get_or_init(|| parse("en", include_str!("../locales/en.yaml")))
            }
            Language::German => {
                GERMAN.get_or_init(|| parse("de", include_str!("../locales/de.yaml")))
            }
        }
    }
}

/// Replaces `{name}` placeholders in a translated string with the given values.
/// Any `Display` value works, so numbers can be put straight into a sentence.
/// An unknown placeholder is kept as written.
pub fn fill(template: &str, args: &[(&str, &dyn Display)]) -> String {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;

    while let Some(start) = rest.find(['{', '}']) {
        out.push_str(&rest[..start]);
        rest = &rest[start..];

        if let Some(after) = rest.strip_prefix("{{") {
            out.push('{');
            rest = after;
        } else if let Some(after) = rest.strip_prefix("}}") {
            out.push('}');
            rest = after;
        } else if let Some((name, after)) = rest
            .strip_prefix('{')
            .and_then(|inner| inner.split_once('}'))
        {
            match args.iter().find(|(key, _)| *key == name) {
                Some((_, value)) => write!(out, "{value}").expect("writing to a String"),
                None => write!(out, "{{{name}}}").expect("writing to a String"),
            }
            rest = after;
        } else {
            // A lone brace: keep it.
            out.push_str(&rest[..1]);
            rest = &rest[1..];
        }
    }

    out.push_str(rest);
    out
}

/// The names of the `{name}` placeholders in a template.
#[cfg(test)]
fn placeholders(template: &str) -> std::collections::BTreeSet<String> {
    let template = template.replace("{{", "").replace("}}", "");
    let mut names = std::collections::BTreeSet::new();
    let mut rest = template.as_str();
    while let Some((_, after)) = rest.split_once('{') {
        let Some((name, after)) = after.split_once('}') else {
            break;
        };
        names.insert(name.to_owned());
        rest = after;
    }
    names
}

fn parse(code: &str, source: &str) -> Strings {
    // The files are embedded, so a failure here is a bug caught by the tests.
    yaml_serde::from_str(source).unwrap_or_else(|error| panic!("invalid locale {code}: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn locale_files_are_complete() {
        for language in Language::ALL {
            let strings = language.strings();
            assert!(!strings.native_name.is_empty());
            assert!(!strings.save_failed.is_empty());
        }
    }

    #[test]
    fn locales_use_the_same_placeholders() {
        let load = |source: &str| -> std::collections::BTreeMap<String, String> {
            yaml_serde::from_str(source).unwrap()
        };
        let english = load(include_str!("../locales/en.yaml"));
        let german = load(include_str!("../locales/de.yaml"));

        assert_eq!(
            english.keys().collect::<Vec<_>>(),
            german.keys().collect::<Vec<_>>()
        );
        for (key, template) in &english {
            assert_eq!(
                placeholders(template),
                placeholders(&german[key]),
                "placeholders differ for `{key}`"
            );
        }
    }

    #[test]
    fn fill_substitutes_numbers_and_text() {
        assert_eq!(fill("At {value}.", &[("value", &-3)]), "At -3.");
        assert_eq!(
            fill("{a} of {b} ({a})", &[("a", &1), ("b", &"two")]),
            "1 of two (1)"
        );
    }

    #[test]
    fn fill_handles_escapes_and_unknown_names() {
        assert_eq!(fill("{{literal}} {x}", &[]), "{literal} {x}");
        assert_eq!(fill("a { b", &[]), "a { b");
        assert_eq!(fill("Zähler: {n}€", &[("n", &1.5)]), "Zähler: 1.5€");
    }
}
