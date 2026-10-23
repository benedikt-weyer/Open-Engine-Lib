//! Smart playlists: playlists that are not a list of tracks but a set of rules (see
//! `docs/engine-library-format.md`). The rules are JSON text such as
//! `{"match":"all","rules":[{"col":"genre","con":"LIKE","param":"'%Techno%'"}]}`,
//! and a smart playlist holds the tracks that satisfy them.
//!
//! Only `genre LIKE` has been seen in a real library. The other columns and conditions
//! here are what the rule format suggests, and a rule that is not understood matches no
//! track, so a list never shows tracks it should not.

use crate::library::Track;

/// The rules of one smart playlist.
#[derive(Debug, PartialEq)]
pub struct Rules {
    /// Whether every rule has to match (`"all"`) or any one (`"any"`).
    all: bool,
    rules: Vec<Rule>,
}

#[derive(Debug, Default, PartialEq)]
struct Rule {
    column: String,
    condition: String,
    /// The parameter as the SQL literal Engine stores: text in single quotes.
    param: String,
}

impl Rules {
    /// `None` if `text` is not rules at all.
    pub fn parse(text: &str) -> Option<Self> {
        let Json::Object(root) = Json::parse(text)? else {
            return None;
        };
        let all = match member(&root, "match") {
            Some(Json::String(mode)) => !mode.eq_ignore_ascii_case("any"),
            _ => true,
        };
        let Some(Json::Array(items)) = member(&root, "rules") else {
            return None;
        };
        let rules = items
            .iter()
            .map(|item| {
                let Json::Object(fields) = item else {
                    return Rule::default();
                };
                let string = |key| match member(fields, key) {
                    Some(Json::String(value)) => value.clone(),
                    Some(Json::Number(value)) => value.to_string(),
                    _ => String::new(),
                };
                Rule {
                    column: string("col"),
                    condition: string("con"),
                    param: string("param"),
                }
            })
            .collect();
        Some(Self { all, rules })
    }

    /// Rules from what an editor sends: whether all must match, and each rule's column,
    /// condition and parameter as plain text (not an SQL literal). Errors say what is wrong.
    pub fn from_parts(all: bool, parts: Vec<(String, String, String)>) -> Result<Self, String> {
        if parts.is_empty() {
            return Err("a smart playlist needs at least one rule".into());
        }
        let mut rules = Vec::with_capacity(parts.len());
        for (column, condition, param) in parts {
            let column = column.trim().to_string();
            if column.is_empty()
                || !column
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_')
            {
                return Err(format!("{column:?} is not a column"));
            }
            let condition = condition
                .to_ascii_uppercase()
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ");
            if condition.is_empty()
                || !condition
                    .chars()
                    .all(|c| c.is_ascii_uppercase() || " =<>!".contains(c))
            {
                return Err(format!("{condition:?} is not a condition"));
            }
            let param = if column.eq_ignore_ascii_case("bpm") {
                let number = param.trim();
                if number.parse::<f64>().is_err() {
                    return Err(format!("{number:?} is not a number"));
                }
                number.to_string()
            } else {
                format!("'{}'", param.replace('\'', "''"))
            };
            rules.push(Rule {
                column,
                condition,
                param,
            });
        }
        Ok(Self { all, rules })
    }

    /// Whether all rules have to match, rather than any one.
    pub fn match_all(&self) -> bool {
        self.all
    }

    /// Each rule's column, condition and parameter as plain text, see `from_parts`.
    pub fn parts(&self) -> impl Iterator<Item = (&str, &str, String)> {
        self.rules.iter().map(|rule| {
            (
                rule.column.as_str(),
                rule.condition.as_str(),
                unquote(&rule.param),
            )
        })
    }

    /// The JSON text Engine keeps in the `rules` column.
    pub fn to_json(&self) -> String {
        let rules: Vec<String> = self
            .rules
            .iter()
            .map(|rule| {
                format!(
                    r#"{{"col":{},"con":{},"param":{},"v":"2.20.0"}}"#,
                    json_string(&rule.column),
                    json_string(&rule.condition),
                    json_string(&rule.param),
                )
            })
            .collect();
        format!(
            r#"{{"match":"{}","rules":[{}],"rv":2}}"#,
            if self.all { "all" } else { "any" },
            rules.join(",")
        )
    }

    /// Whether `track` satisfies the rules. A list without rules holds nothing.
    pub fn matches(&self, track: &Track) -> bool {
        if self.rules.is_empty() {
            return false;
        }
        if self.all {
            self.rules.iter().all(|rule| rule.matches(track))
        } else {
            self.rules.iter().any(|rule| rule.matches(track))
        }
    }
}

impl Rule {
    fn matches(&self, track: &Track) -> bool {
        let condition = self.condition.to_ascii_uppercase();
        let condition = condition.split_whitespace().collect::<Vec<_>>().join(" ");
        let param = unquote(&self.param);

        let text = match self.column.to_ascii_lowercase().as_str() {
            "title" => &track.title,
            "artist" => &track.artist,
            "album" => &track.album,
            "genre" => &track.genre,
            "bpm" => {
                let (Some(bpm), Ok(param)) = (track.bpm, param.trim().parse::<f64>()) else {
                    return false;
                };
                return match condition.as_str() {
                    "=" | "==" => bpm == param,
                    "!=" | "<>" => bpm != param,
                    "<" => bpm < param,
                    "<=" => bpm <= param,
                    ">" => bpm > param,
                    ">=" => bpm >= param,
                    _ => false,
                };
            }
            _ => return false,
        };
        match condition.as_str() {
            "LIKE" => like(text, &param),
            "NOT LIKE" => !like(text, &param),
            "=" | "==" => text.eq_ignore_ascii_case(&param),
            "!=" | "<>" => !text.eq_ignore_ascii_case(&param),
            _ => false,
        }
    }
}

/// The text of an SQL string literal: `'It''s'` is `It's`. Anything else is as it is.
fn unquote(literal: &str) -> String {
    match literal
        .strip_prefix('\'')
        .and_then(|rest| rest.strip_suffix('\''))
    {
        Some(inner) => inner.replace("''", "'"),
        None => literal.to_string(),
    }
}

fn json_string(text: &str) -> String {
    let mut out = String::from('"');
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c < ' ' => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// SQL `LIKE`: `%` is any run of characters, `_` any one, and case does not matter.
fn like(text: &str, pattern: &str) -> bool {
    let text: Vec<char> = text.to_lowercase().chars().collect();
    let pattern: Vec<char> = pattern.to_lowercase().chars().collect();

    // Walk both, remembering the last `%` to fall back to when a literal does not fit.
    let (mut t, mut p) = (0, 0);
    let mut backtrack: Option<(usize, usize)> = None;
    while t < text.len() {
        match pattern.get(p) {
            Some('%') => {
                backtrack = Some((p, t));
                p += 1;
            }
            Some(&c) if c == '_' || c == text[t] => {
                p += 1;
                t += 1;
            }
            _ => match backtrack {
                Some((percent, start)) => {
                    backtrack = Some((percent, start + 1));
                    p = percent + 1;
                    t = start + 1;
                }
                None => return false,
            },
        }
    }
    pattern[p..].iter().all(|&c| c == '%')
}

fn member<'a>(fields: &'a [(String, Json)], key: &str) -> Option<&'a Json> {
    fields
        .iter()
        .find_map(|(name, value)| (name == key).then_some(value))
}

/// Just enough JSON to read the rules.
#[derive(Debug, PartialEq)]
enum Json {
    Null,
    Bool(bool),
    Number(f64),
    String(String),
    Array(Vec<Json>),
    Object(Vec<(String, Json)>),
}

impl Json {
    fn parse(text: &str) -> Option<Json> {
        let chars: Vec<char> = text.chars().collect();
        let mut position = 0;
        let value = Self::value(&chars, &mut position)?;
        skip_space(&chars, &mut position);
        (position == chars.len()).then_some(value)
    }

    fn value(chars: &[char], at: &mut usize) -> Option<Json> {
        skip_space(chars, at);
        match *chars.get(*at)? {
            '{' => {
                *at += 1;
                let mut fields = Vec::new();
                loop {
                    skip_space(chars, at);
                    if chars.get(*at) == Some(&'}') {
                        *at += 1;
                        return Some(Json::Object(fields));
                    }
                    if !fields.is_empty() {
                        expect(chars, at, ',')?;
                        skip_space(chars, at);
                    }
                    let Json::String(name) = Self::value(chars, at)? else {
                        return None;
                    };
                    skip_space(chars, at);
                    expect(chars, at, ':')?;
                    fields.push((name, Self::value(chars, at)?));
                }
            }
            '[' => {
                *at += 1;
                let mut items = Vec::new();
                loop {
                    skip_space(chars, at);
                    if chars.get(*at) == Some(&']') {
                        *at += 1;
                        return Some(Json::Array(items));
                    }
                    if !items.is_empty() {
                        expect(chars, at, ',')?;
                    }
                    items.push(Self::value(chars, at)?);
                }
            }
            '"' => {
                *at += 1;
                let mut string = String::new();
                loop {
                    let c = *chars.get(*at)?;
                    *at += 1;
                    match c {
                        '"' => return Some(Json::String(string)),
                        '\\' => {
                            let escaped = *chars.get(*at)?;
                            *at += 1;
                            string.push(match escaped {
                                'n' => '\n',
                                't' => '\t',
                                'r' => '\r',
                                'b' => '\u{8}',
                                'f' => '\u{c}',
                                'u' => {
                                    let hex: String = chars.get(*at..*at + 4)?.iter().collect();
                                    *at += 4;
                                    // A surrogate half can't stand alone; a rule with one is
                                    // not worth more than a replacement character.
                                    char::from_u32(u32::from_str_radix(&hex, 16).ok()?)
                                        .unwrap_or('\u{fffd}')
                                }
                                other => other,
                            });
                        }
                        c => string.push(c),
                    }
                }
            }
            't' => word(chars, at, "true", Json::Bool(true)),
            'f' => word(chars, at, "false", Json::Bool(false)),
            'n' => word(chars, at, "null", Json::Null),
            _ => {
                let start = *at;
                while chars
                    .get(*at)
                    .is_some_and(|c| c.is_ascii_digit() || "+-.eE".contains(*c))
                {
                    *at += 1;
                }
                let number: String = chars[start..*at].iter().collect();
                number.parse().ok().map(Json::Number)
            }
        }
    }
}

fn skip_space(chars: &[char], at: &mut usize) {
    while chars.get(*at).is_some_and(|c| c.is_whitespace()) {
        *at += 1;
    }
}

fn expect(chars: &[char], at: &mut usize, wanted: char) -> Option<()> {
    (chars.get(*at) == Some(&wanted)).then(|| *at += 1)
}

fn word(chars: &[char], at: &mut usize, word: &str, value: Json) -> Option<Json> {
    let end = *at + word.len();
    (chars.get(*at..end)?.iter().copied().eq(word.chars())).then(|| {
        *at = end;
        value
    })
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::soundswitch::Lighting;

    fn track(genre: &str, bpm: Option<f64>) -> Track {
        Track {
            db_id: 1,
            title: "Title".into(),
            artist: "Artist".into(),
            album: "Album".into(),
            genre: genre.into(),
            bpm,
            length: None,
            relative_path: String::new(),
            path: PathBuf::new(),
            origin_key: None,
            streaming: false,
            album_art: None,
            rating: None,
            lighting: Lighting::default(),
        }
    }

    #[test]
    fn reads_the_rules_engine_writes() {
        let rules = Rules::parse(
            r#"{"match":"all","rules":[{"col":"genre","con":"LIKE","param":"'%Techno%'","v":"2.20.0"}],"rv":2}"#,
        )
        .unwrap();
        assert!(rules.all);
        assert_eq!(
            rules.rules,
            [Rule {
                column: "genre".into(),
                condition: "LIKE".into(),
                param: "'%Techno%'".into(),
            }]
        );
        assert!(Rules::parse("not json").is_none());
        assert!(Rules::parse("[]").is_none());
        assert!(Rules::parse(r#"{"match":"all"}"#).is_none());
    }

    #[test]
    fn matches_genres_like_sql_like() {
        let rules = Rules::parse(
            r#"{"match":"all","rules":[{"col":"genre","con":"LIKE","param":"'%Techno%'"}]}"#,
        )
        .unwrap();
        assert!(rules.matches(&track("Techno", None)));
        assert!(rules.matches(&track("Melodic techno / House", None)));
        assert!(!rules.matches(&track("House", None)));
        assert!(!rules.matches(&track("", None)));
    }

    #[test]
    fn like_wildcards() {
        assert!(like("House", "house"));
        assert!(like("House", "h_use"));
        assert!(like("House", "%"));
        assert!(like("", "%"));
        assert!(like("Deep House", "%e%u%"));
        assert!(like("abcabd", "%abd"));
        assert!(!like("House", "hous"));
        assert!(!like("House", "%ouse_"));
        assert!(!like("", "_"));
    }

    #[test]
    fn combines_rules_with_all_or_any() {
        let house_fast = |mode| {
            Rules::parse(&format!(
                r#"{{"match":"{mode}","rules":[
                    {{"col":"genre","con":"LIKE","param":"'%House%'"}},
                    {{"col":"bpm","con":">=","param":"125"}}]}}"#
            ))
            .unwrap()
        };
        let slow_house = track("House", Some(120.0));
        let fast_techno = track("Techno", Some(140.0));
        let fast_house = track("House", Some(128.0));
        let unknown = track("Techno", None);

        let all = house_fast("all");
        assert!(all.matches(&fast_house));
        assert!(!all.matches(&slow_house));
        assert!(!all.matches(&fast_techno));

        let any = house_fast("any");
        assert!(any.matches(&slow_house));
        assert!(any.matches(&fast_techno));
        assert!(!any.matches(&unknown));
    }

    #[test]
    fn what_is_not_understood_matches_nothing() {
        let unknown_column = Rules::parse(
            r#"{"match":"all","rules":[{"col":"mood","con":"LIKE","param":"'%x%'"}]}"#,
        )
        .unwrap();
        assert!(!unknown_column.matches(&track("x", None)));

        let unknown_condition = Rules::parse(
            r#"{"match":"all","rules":[{"col":"genre","con":"SOUNDS LIKE","param":"'x'"}]}"#,
        )
        .unwrap();
        assert!(!unknown_condition.matches(&track("x", None)));

        let none = Rules::parse(r#"{"match":"any","rules":[]}"#).unwrap();
        assert!(!none.matches(&track("x", None)));
    }

    #[test]
    fn rules_from_an_editor_are_written_as_engine_writes_them() {
        let rules = Rules::from_parts(
            true,
            vec![
                ("genre".into(), "like".into(), "%Tech\"no%".into()),
                ("bpm".into(), ">=".into(), " 125 ".into()),
                ("title".into(), "=".into(), "It's".into()),
            ],
        )
        .unwrap();
        let json = rules.to_json();
        assert_eq!(
            json,
            r#"{"match":"all","rules":[{"col":"genre","con":"LIKE","param":"'%Tech\"no%'","v":"2.20.0"},{"col":"bpm","con":">=","param":"125","v":"2.20.0"},{"col":"title","con":"=","param":"'It''s'","v":"2.20.0"}],"rv":2}"#
        );
        // What was written reads back as what was put in.
        let again = Rules::parse(&json).unwrap();
        assert_eq!(again.rules, rules.rules);
        assert!(again.match_all());
        assert_eq!(
            again.parts().collect::<Vec<_>>(),
            [
                ("genre", "LIKE", "%Tech\"no%".to_string()),
                ("bpm", ">=", "125".to_string()),
                ("title", "=", "It's".to_string()),
            ]
        );
    }

    #[test]
    fn rules_from_an_editor_are_checked() {
        let one = |column: &str, condition: &str, param: &str| {
            Rules::from_parts(false, vec![(column.into(), condition.into(), param.into())])
        };
        assert!(one("genre", "LIKE", "x").is_ok());
        assert!(Rules::from_parts(true, Vec::new()).is_err());
        assert!(one("", "LIKE", "x").is_err());
        assert!(one("genre; DROP TABLE Track", "LIKE", "x").is_err());
        assert!(one("genre", "LIKE 'x' OR 1", "x").is_err());
        assert!(one("genre", "", "x").is_err());
        assert!(one("bpm", ">", "fast").is_err());
        assert!(!one("genre", "LIKE", "x").unwrap().match_all());
    }

    #[test]
    fn unquotes_sql_literals() {
        assert_eq!(unquote("'%Techno%'"), "%Techno%");
        assert_eq!(unquote("'It''s'"), "It's");
        assert_eq!(unquote("128"), "128");
    }

    #[test]
    fn parses_json_escapes() {
        assert_eq!(
            Json::parse(r#"{"a":["x\"yé",1.5,true,null]}"#),
            Some(Json::Object(vec![(
                "a".into(),
                Json::Array(vec![
                    Json::String("x\"yé".into()),
                    Json::Number(1.5),
                    Json::Bool(true),
                    Json::Null,
                ])
            )]))
        );
        assert_eq!(Json::parse(r#"{"a":1,}"#), None);
        assert_eq!(Json::parse(r#"{"a":1} x"#), None);
    }
}
