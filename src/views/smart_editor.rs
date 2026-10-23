//! The window for adding a smart playlist or changing the rules of one.

use std::fmt;

use iced::widget::{
    Space, button, center, column, container, opaque, pick_list, row, scrollable, text, text_input,
    tooltip,
};
use iced::{Color, Element, Fill, Theme};
use open_engine_api::v1::{SmartRule, SmartRules};

use crate::Message;
use crate::config::Language;
use crate::error_text;
use crate::i18n::{Strings, fill};

const WIDTH: f32 = 720.0;
/// The height of the list of rules; it scrolls inside the window.
const RULES_HEIGHT: f32 = 260.0;

/// The track fields a rule can look at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Field {
    Title,
    Artist,
    Album,
    Genre,
    Bpm,
}

impl Field {
    const ALL: [Field; 5] = [
        Field::Title,
        Field::Artist,
        Field::Album,
        Field::Genre,
        Field::Bpm,
    ];

    fn column(self) -> &'static str {
        match self {
            Field::Title => "title",
            Field::Artist => "artist",
            Field::Album => "album",
            Field::Genre => "genre",
            Field::Bpm => "bpm",
        }
    }

    fn of_column(column: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|field| field.column().eq_ignore_ascii_case(column))
    }

    fn is_number(self) -> bool {
        self == Field::Bpm
    }

    fn label(self, strings: &Strings) -> &str {
        match self {
            Field::Title => &strings.col_title,
            Field::Artist => &strings.col_artist,
            Field::Album => &strings.col_album,
            Field::Genre => &strings.col_genre,
            Field::Bpm => &strings.col_bpm,
        }
    }
}

/// How a rule compares a field with its value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    // Text.
    Contains,
    NotContains,
    Is,
    IsNot,
    /// An SQL `LIKE` pattern, as stored.
    Pattern,
    // Numbers.
    Equals,
    NotEquals,
    Less,
    LessOrEqual,
    Greater,
    GreaterOrEqual,
}

impl Op {
    const TEXT: [Op; 5] = [
        Op::Contains,
        Op::NotContains,
        Op::Is,
        Op::IsNot,
        Op::Pattern,
    ];
    const NUMBER: [Op; 6] = [
        Op::Equals,
        Op::NotEquals,
        Op::Less,
        Op::LessOrEqual,
        Op::Greater,
        Op::GreaterOrEqual,
    ];

    fn for_field(field: Field) -> &'static [Op] {
        if field.is_number() {
            &Self::NUMBER
        } else {
            &Self::TEXT
        }
    }

    fn label(self, strings: &Strings) -> String {
        match self {
            Op::Contains => strings.smart_op_contains.clone(),
            Op::NotContains => strings.smart_op_not_contains.clone(),
            Op::Is => strings.smart_op_is.clone(),
            Op::IsNot => strings.smart_op_is_not.clone(),
            Op::Pattern => strings.smart_op_pattern.clone(),
            Op::Equals => "=".into(),
            Op::NotEquals => "≠".into(),
            Op::Less => "<".into(),
            Op::LessOrEqual => "≤".into(),
            Op::Greater => ">".into(),
            Op::GreaterOrEqual => "≥".into(),
        }
    }

    /// The condition as the backend takes it.
    fn condition(self) -> &'static str {
        match self {
            Op::Contains | Op::Pattern => "LIKE",
            Op::NotContains => "NOT LIKE",
            Op::Is | Op::Equals => "=",
            Op::IsNot | Op::NotEquals => "!=",
            Op::Less => "<",
            Op::LessOrEqual => "<=",
            Op::Greater => ">",
            Op::GreaterOrEqual => ">=",
        }
    }
}

/// One line of the rules.
#[derive(Debug, Clone, PartialEq)]
pub enum Row {
    Rule {
        field: Field,
        op: Op,
        value: String,
    },
    /// A rule this window can't show as a sentence. It is kept as it is, unless removed.
    Raw(SmartRule),
}

impl Row {
    fn blank() -> Self {
        Row::Rule {
            field: Field::Genre,
            op: Op::Contains,
            value: String::new(),
        }
    }

    fn from_rule(rule: &SmartRule) -> Self {
        let raw = || Row::Raw(rule.clone());
        let Some(field) = Field::of_column(&rule.column) else {
            return raw();
        };
        let condition = rule
            .condition
            .to_ascii_uppercase()
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        let param = rule.param.as_str();
        // `%word%` is "contains", unless the word has wildcards of its own.
        let contained = param
            .strip_prefix('%')
            .and_then(|rest| rest.strip_suffix('%'))
            .filter(|word| !word.contains(['%', '_']));

        let (op, value) = match (field.is_number(), condition.as_str()) {
            (false, "LIKE") => match contained {
                Some(word) => (Op::Contains, word),
                None => (Op::Pattern, param),
            },
            (false, "NOT LIKE") => match contained {
                Some(word) => (Op::NotContains, word),
                None => return raw(),
            },
            (false, "=") => (Op::Is, param),
            (false, "!=" | "<>") => (Op::IsNot, param),
            (true, "=" | "==") => (Op::Equals, param),
            (true, "!=" | "<>") => (Op::NotEquals, param),
            (true, "<") => (Op::Less, param),
            (true, "<=") => (Op::LessOrEqual, param),
            (true, ">") => (Op::Greater, param),
            (true, ">=") => (Op::GreaterOrEqual, param),
            _ => return raw(),
        };
        Row::Rule {
            field,
            op,
            value: value.to_string(),
        }
    }

    fn to_rule(&self) -> SmartRule {
        match self {
            Row::Raw(rule) => rule.clone(),
            Row::Rule { field, op, value } => SmartRule {
                column: field.column().into(),
                condition: op.condition().into(),
                param: match op {
                    Op::Contains | Op::NotContains => format!("%{value}%"),
                    _ => value.clone(),
                },
            },
        }
    }
}

/// What the user changed in the window.
#[derive(Debug, Clone)]
pub enum Event {
    Name(String),
    MatchAll(bool),
    Field(usize, Field),
    Op(usize, Op),
    Value(usize, String),
    AddRule,
    RemoveRule(usize),
}

/// A smart playlist being written.
#[derive(Debug, Clone)]
pub struct Editor {
    /// The playlist being changed, an index into the library's playlists, or `None` for
    /// a new one.
    pub target: Option<usize>,
    pub name: String,
    pub match_all: bool,
    pub rows: Vec<Row>,
    /// Why the last attempt to save failed (or what is missing).
    pub error: Option<String>,
    /// Waiting for the backend to write it.
    pub saving: bool,
}

impl Editor {
    pub fn new() -> Self {
        Self {
            target: None,
            name: String::new(),
            match_all: true,
            rows: vec![Row::blank()],
            error: None,
            saving: false,
        }
    }

    /// The window for the playlist `target`, which is called `name` and has `rules` (none
    /// if the backend couldn't read them: then there is a blank rule to start from).
    pub fn edit(target: usize, name: &str, rules: Option<&SmartRules>) -> Self {
        let rows: Vec<Row> = rules
            .map(|rules| rules.rules.iter().map(Row::from_rule).collect())
            .unwrap_or_default();
        Self {
            target: Some(target),
            name: name.to_string(),
            match_all: rules.is_none_or(|rules| rules.match_all),
            rows: if rows.is_empty() {
                vec![Row::blank()]
            } else {
                rows
            },
            error: None,
            saving: false,
        }
    }

    pub fn update(&mut self, event: Event) {
        self.error = None;
        match event {
            Event::Name(name) => self.name = name,
            Event::MatchAll(all) => self.match_all = all,
            Event::Field(index, new) => {
                if let Some(Row::Rule { field, op, .. }) = self.rows.get_mut(index) {
                    *field = new;
                    // A comparison of text makes no sense for a number and back.
                    if !Op::for_field(new).contains(op) {
                        *op = Op::for_field(new)[0];
                    }
                }
            }
            Event::Op(index, new) => {
                if let Some(Row::Rule { op, .. }) = self.rows.get_mut(index) {
                    *op = new;
                }
            }
            Event::Value(index, new) => {
                if let Some(Row::Rule { value, .. }) = self.rows.get_mut(index) {
                    *value = new;
                }
            }
            Event::AddRule => self.rows.push(Row::blank()),
            Event::RemoveRule(index) => {
                if index < self.rows.len() {
                    self.rows.remove(index);
                }
            }
        }
    }

    /// The name and rules to send to the backend, or what is wrong with them.
    pub fn finished(&self, strings: &Strings) -> Result<(String, SmartRules), String> {
        let name = self.name.trim();
        if name.is_empty() {
            return Err(strings.smart_name_missing.clone());
        }
        Ok((
            name.to_string(),
            SmartRules {
                match_all: self.match_all,
                rules: self.rows.iter().map(Row::to_rule).collect(),
            },
        ))
    }
}

/// A choice in a drop-down: what it is, and what it is called.
#[derive(Clone)]
pub struct Choice<T> {
    pub value: T,
    label: String,
}

impl<T> fmt::Display for Choice<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.label)
    }
}

impl<T: PartialEq> PartialEq for Choice<T> {
    fn eq(&self, other: &Self) -> bool {
        self.value == other.value
    }
}

pub fn choice<T>(value: T, label: impl Into<String>) -> Choice<T> {
    Choice {
        value,
        label: label.into(),
    }
}

/// The window, centred over a dimmed view of the app that can't be clicked.
pub fn modal<'a>(editor: &'a Editor, language: Language) -> Element<'a, Message> {
    let strings = language.strings();

    let heading = if editor.target.is_some() {
        &strings.smart_title_edit
    } else {
        &strings.smart_title_new
    };

    let name = text_input(&strings.smart_name, &editor.name)
        .on_input(|name| Message::SmartEditor(Event::Name(name)))
        .on_submit(Message::SmartSave);

    let matching = [
        choice(true, strings.smart_match_all.clone()),
        choice(false, strings.smart_match_any.clone()),
    ];
    let selected = choice(
        editor.match_all,
        if editor.match_all {
            &strings.smart_match_all
        } else {
            &strings.smart_match_any
        }
        .clone(),
    );
    let matching = row![
        text(&strings.smart_match),
        pick_list(matching, Some(selected), |picked: Choice<bool>| {
            Message::SmartEditor(Event::MatchAll(picked.value))
        }),
    ]
    .spacing(10)
    .align_y(iced::Center);

    let mut rules = column![].spacing(8);
    let mut has_pattern = false;
    for (index, line) in editor.rows.iter().enumerate() {
        rules = rules.push(rule_row(index, line, language));
        has_pattern |= matches!(
            line,
            Row::Rule {
                op: Op::Pattern,
                ..
            }
        );
    }

    let mut body = column![
        name,
        matching,
        scrollable(rules).height(iced::Length::Shrink).width(Fill),
        button(text(&strings.smart_add_rule))
            .style(button::secondary)
            .on_press(Message::SmartEditor(Event::AddRule)),
    ]
    .spacing(14);
    if has_pattern {
        body = body.push(
            text(&strings.smart_pattern_hint)
                .size(13)
                .style(text::secondary),
        );
    }
    if let Some(error) = &editor.error {
        body = body.push(error_text::view(error.as_str(), language));
    }

    let save = button(text(if editor.saving {
        &strings.smart_saving
    } else {
        &strings.smart_save
    }))
    .on_press_maybe((!editor.saving).then_some(Message::SmartSave));
    let cancel = button(text(&strings.smart_cancel))
        .style(button::secondary)
        .on_press_maybe((!editor.saving).then_some(Message::SmartCancel));
    body = body.push(row![Space::new().width(Fill), cancel, save].spacing(10));

    let dialog = container(column![text(heading).size(24), body].spacing(14))
        .padding(20)
        .width(WIDTH)
        .max_height(RULES_HEIGHT + 330.0)
        .style(container::rounded_box);

    // The dimmed backdrop swallows clicks, so nothing behind the window can be used.
    opaque(center(opaque(dialog)).style(|_: &Theme| {
        container::Style {
            background: Some(
                Color {
                    a: 0.6,
                    ..Color::BLACK
                }
                .into(),
            ),
            ..container::Style::default()
        }
    }))
}

fn rule_row<'a>(index: usize, line: &'a Row, language: Language) -> Element<'a, Message> {
    let strings = language.strings();

    let remove = tooltip(
        button(text("×"))
            .padding([4, 8])
            .style(button::text)
            .on_press(Message::SmartEditor(Event::RemoveRule(index))),
        text(&strings.smart_remove_rule),
        tooltip::Position::Bottom,
    )
    .style(container::rounded_box);

    match line {
        Row::Raw(rule) => row![
            text(fill(
                &strings.smart_rule_unsupported,
                &[(
                    "rule",
                    &format!("{} {} {}", rule.column, rule.condition, rule.param)
                )],
            ))
            .style(text::secondary)
            .width(Fill),
            remove,
        ]
        .spacing(8)
        .align_y(iced::Center)
        .into(),
        Row::Rule { field, op, value } => {
            let fields: Vec<Choice<Field>> = Field::ALL
                .into_iter()
                .map(|field| choice(field, field.label(strings)))
                .collect();
            let ops: Vec<Choice<Op>> = Op::for_field(*field)
                .iter()
                .map(|&op| choice(op, op.label(strings)))
                .collect();
            row![
                pick_list(
                    fields,
                    Some(choice(*field, field.label(strings))),
                    move |picked: Choice<Field>| Message::SmartEditor(Event::Field(
                        index,
                        picked.value
                    )),
                )
                .width(140),
                pick_list(
                    ops,
                    Some(choice(*op, op.label(strings))),
                    move |picked: Choice<Op>| Message::SmartEditor(Event::Op(index, picked.value)),
                )
                .width(200),
                text_input(&strings.smart_value, value)
                    .on_input(move |value| Message::SmartEditor(Event::Value(index, value)))
                    .on_submit(Message::SmartSave)
                    .width(Fill),
                remove,
            ]
            .spacing(8)
            .align_y(iced::Center)
            .into()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rule(column: &str, condition: &str, param: &str) -> SmartRule {
        SmartRule {
            column: column.into(),
            condition: condition.into(),
            param: param.into(),
        }
    }

    #[test]
    fn rules_are_shown_as_sentences_and_sent_back_as_they_were() {
        for (original, shown) in [
            (
                rule("genre", "LIKE", "%Techno%"),
                Row::Rule {
                    field: Field::Genre,
                    op: Op::Contains,
                    value: "Techno".into(),
                },
            ),
            (
                rule("artist", "NOT LIKE", "%Mix%"),
                Row::Rule {
                    field: Field::Artist,
                    op: Op::NotContains,
                    value: "Mix".into(),
                },
            ),
            (
                rule("title", "LIKE", "A%"),
                Row::Rule {
                    field: Field::Title,
                    op: Op::Pattern,
                    value: "A%".into(),
                },
            ),
            (
                rule("album", "=", "Best of"),
                Row::Rule {
                    field: Field::Album,
                    op: Op::Is,
                    value: "Best of".into(),
                },
            ),
            (
                rule("bpm", ">=", "125"),
                Row::Rule {
                    field: Field::Bpm,
                    op: Op::GreaterOrEqual,
                    value: "125".into(),
                },
            ),
        ] {
            let row = Row::from_rule(&original);
            assert_eq!(row, shown);
            assert_eq!(row.to_rule(), original);
        }
    }

    #[test]
    fn a_rule_that_is_not_understood_is_kept_whole() {
        for odd in [
            rule("mood", "LIKE", "%x%"),
            rule("genre", "SOUNDS LIKE", "x"),
            rule("bpm", "LIKE", "12%"),
            rule("genre", "NOT LIKE", "a%b"),
        ] {
            let row = Row::from_rule(&odd);
            assert_eq!(row, Row::Raw(odd.clone()));
            assert_eq!(row.to_rule(), odd);
        }
    }

    #[test]
    fn changing_the_field_keeps_the_comparison_sensible() {
        let mut editor = Editor::new();
        assert!(editor.match_all);
        editor.update(Event::Field(0, Field::Bpm));
        assert!(matches!(
            editor.rows[0],
            Row::Rule {
                field: Field::Bpm,
                op: Op::Equals,
                ..
            }
        ));
        editor.update(Event::Op(0, Op::Greater));
        editor.update(Event::Field(0, Field::Bpm));
        assert!(matches!(
            editor.rows[0],
            Row::Rule {
                op: Op::Greater,
                ..
            }
        ));
        editor.update(Event::Field(0, Field::Title));
        assert!(matches!(
            editor.rows[0],
            Row::Rule {
                field: Field::Title,
                op: Op::Contains,
                ..
            }
        ));
    }

    #[test]
    fn editing_starts_from_the_playlist_and_finishing_needs_a_name() {
        let strings = Language::English.strings();
        let rules = SmartRules {
            match_all: false,
            rules: vec![rule("genre", "LIKE", "%House%"), rule("mood", "=", "x")],
        };
        let mut editor = Editor::edit(4, "House", Some(&rules));
        assert_eq!(editor.target, Some(4));
        assert!(!editor.match_all);
        assert_eq!(editor.rows.len(), 2);

        editor.update(Event::Value(0, "Deep".into()));
        editor.update(Event::RemoveRule(5));
        let (name, sent) = editor.finished(strings).unwrap();
        assert_eq!(name, "House");
        assert_eq!(sent.rules[0], rule("genre", "LIKE", "%Deep%"));
        assert_eq!(sent.rules[1], rule("mood", "=", "x"));
        assert!(!sent.match_all);

        editor.update(Event::Name("   ".into()));
        assert_eq!(
            editor.finished(strings).unwrap_err(),
            strings.smart_name_missing
        );

        // Rules that couldn't be read leave a blank rule to start from.
        assert_eq!(Editor::edit(0, "x", None).rows, [Row::blank()]);
    }
}
