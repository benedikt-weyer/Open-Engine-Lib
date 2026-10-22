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
    pub nav_library: String,
    pub nav_settings: String,
    pub settings_title: String,
    pub theme: String,
    pub theme_light: String,
    pub theme_dark: String,
    pub language_heading: String,
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
    pub close_library: String,
    pub library_summary: String,
    pub collection: String,
    pub tracks_heading: String,
    pub no_tracks: String,
    pub col_title: String,
    pub col_artist: String,
    pub col_album: String,
    pub col_genre: String,
    pub col_bpm: String,
    pub col_length: String,
    pub play: String,
    pub pause: String,
    pub stop: String,
    pub previous: String,
    pub next: String,
    pub nothing_playing: String,
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
