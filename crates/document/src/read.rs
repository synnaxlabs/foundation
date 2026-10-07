//! Readers for the values that kinds and `config` share, so a value reads the same in
//! every block.

use std::slice;

use types::name::{Error, Name, Selector};
use types::{byte, time};

use crate::diagnostic::{Code, Diagnostic};
use crate::value::{Kind, Value};
use crate::{Label, Span};

const BAD_SIZE: Code = Code::new("document.bad-size");
const BAD_NAME: Code = Code::new("document.bad-name");
const BAD_SELECTOR: Code = Code::new("document.bad-selector");
const BAD_SPAN: Code = Code::new("document.bad-span");

/// Reads a byte size from a string that [`byte::Size`] reads, such as `"200GiB"` or
/// `"1.5GiB"`.
///
/// # Errors
///
/// A `document.bad-size` diagnostic at the value's span when the value is not a
/// string, or when `byte::Size` refuses its text.
pub fn size(value: &Value) -> Result<byte::Size, Diagnostic> {
    let bad = |message: String, fix: String| {
        Diagnostic::new(BAD_SIZE, value.span, message, fix)
    };
    let Kind::String(text) = &value.kind else {
        return Err(bad(
            format!("a byte size is a string, not {}", noun(&value.kind)),
            "Write a string such as \"200GiB\"".into(),
        ));
    };
    text.parse::<byte::Size>().map_err(|error| {
        bad(
            format!("cannot read the byte size {text:?}: {error}"),
            size_fix(text, error),
        )
    })
}

/// The fix for `text`, which `byte::Size` refuses with `error`. It is `Write` and the
/// text with no whitespace and the unit that `text` likely means, when that text reads,
/// so `"200 GB"` gets `Write "200GiB"`. Otherwise it is the fix for `error`, with
/// each size quoted as the file writes it.
fn size_fix(text: &str, error: byte::Error) -> String {
    let words: Vec<&str> = text.split_whitespace().collect();
    // `1 5GiB` may mean `15GiB` or `1.5GiB`, so it has no likely text.
    let joins_digits = words.iter().zip(words.iter().skip(1)).any(|(a, b)| {
        a.ends_with(|c: char| c.is_ascii_digit())
            && b.starts_with(|c: char| c.is_ascii_digit())
    });
    if !joins_digits {
        let mut likely = words.concat();
        let mut parsed = likely.parse::<byte::Size>();
        if let Err(byte::Error::Unit {
            start,
            meant: Some(meant),
        }) = parsed
        {
            likely.truncate(start);
            likely.push_str(meant);
            parsed = likely.parse::<byte::Size>();
        }
        if parsed.is_ok() {
            return format!("Write \"{likely}\"");
        }
    }
    match error {
        byte::Error::Syntax => "Write a size such as \"200GiB\" or \"1.5GiB\"".into(),
        byte::Error::Range { largest } => format!("Use at most \"{largest}\""),
        byte::Error::Unit { .. } | byte::Error::Fraction => error.fix(),
    }
}

/// Reads a span from a string that [`time::Span`] reads, such as `"3d"` or `"1.5s"`.
///
/// # Errors
///
/// A `document.bad-span` diagnostic at the value's span when the value is not a
/// string, or when `time::Span` refuses its text.
pub fn span(value: &Value) -> Result<time::Span, Diagnostic> {
    let bad = |message: String, fix: String| {
        Diagnostic::new(BAD_SPAN, value.span, message, fix)
    };
    let Kind::String(text) = &value.kind else {
        return Err(bad(
            format!("a span is a string, not {}", noun(&value.kind)),
            "Write a string such as \"3d\"".into(),
        ));
    };
    text.parse::<time::Span>().map_err(|error| {
        bad(
            format!("cannot read the span {text:?}: {error}"),
            span_fix(error),
        )
    })
}

/// The fix for `error`, with each span quoted as the file writes it.
fn span_fix(error: time::Error) -> String {
    match error {
        time::Error::Span => {
            "Write a span such as \"250us\", \"1.5s\", or \"3d\"".into()
        }
        time::Error::Long => "Use a span from \"-106751d\" to \"106751d\"".into(),
        time::Error::Fraction
        | time::Error::Stamp
        | time::Error::Date
        | time::Error::Era
        | time::Error::Range
        | time::Error::Reversed
        | time::Error::Zero
        | time::Error::Period => error.fix().into(),
    }
}

/// Reads a value as a name: a string such as `"site_a.node_1"`, or a reference.
///
/// # Errors
///
/// A `document.bad-name` diagnostic at the value when it is not a string or a
/// reference, or when [`Name`] refuses its text, with the message and the fix of
/// its [`Error`].
pub fn name(value: &Value) -> Result<Name, Diagnostic> {
    match &value.kind {
        Kind::String(text) => text
            .parse()
            .map_err(|error| diagnose(BAD_NAME, value.span, &error)),
        Kind::Reference(name) => Ok(name.clone()),
        kind => Err(Diagnostic::new(
            BAD_NAME,
            value.span,
            format!("a name is a string or a reference, not {}", noun(kind)),
            "Write a name such as \"site_a.node_1\"".into(),
        )),
    }
}

/// Reads one name or a list of names, each as [`name`] reads it, such as
/// `["n_1", "n_2"]`. Keeps their order and repeats.
///
/// # Errors
///
/// The diagnostic of [`name`] for the first item that it refuses.
pub fn names(value: &Value) -> Result<Vec<Name>, Diagnostic> {
    items(value).iter().map(name).collect()
}

/// Reads a block label as a name, such as `"site_a.cell_1"`.
///
/// # Errors
///
/// A `document.bad-name` diagnostic at the label when [`Name`] refuses its text, with
/// the message and the fix of its [`Error`].
pub fn label(label: &Label) -> Result<Name, Diagnostic> {
    label
        .text
        .parse()
        .map_err(|error| diagnose(BAD_NAME, label.span, &error))
}

/// Reads a selector from one pattern or a list of patterns, each a string or a
/// reference, such as `"site_a.*"` or `["site_a.*", "!site_a.test"]`.
///
/// # Errors
///
/// A `document.bad-selector` diagnostic for the first problem in source order: at a
/// pattern that is not a string or a reference; at a pattern that [`Selector`]
/// refuses, with the message and the fix of its [`Error`]; or at the value when no
/// pattern includes names, with those of [`Error::NoInclude`].
pub fn selector(value: &Value) -> Result<Selector, Diagnostic> {
    let patterns = items(value);
    let mut texts = Vec::with_capacity(patterns.len());
    for pattern in patterns {
        let text: &str = match &pattern.kind {
            Kind::String(text) => text,
            Kind::Reference(name) => name.as_str(),
            kind => {
                return Err(Diagnostic::new(
                    BAD_SELECTOR,
                    pattern.span,
                    format!("a pattern is a string or a reference, not {}", noun(kind)),
                    "Write a string such as \"site_a.*\"".into(),
                ));
            }
        };
        // `Selector::new` does not say which pattern it refuses, so each reads alone
        // first. Alone, an exclusion includes no names, which is not its error.
        match Selector::new([text]) {
            Err(error) if error != Error::NoInclude => {
                return Err(diagnose(BAD_SELECTOR, pattern.span, &error));
            }
            _ => texts.push(text),
        }
    }
    Selector::new(texts).map_err(|error| diagnose(BAD_SELECTOR, value.span, &error))
}

/// The items of a value that holds one item or a list: the items of a list, or the
/// value itself.
fn items(value: &Value) -> &[Value] {
    match &value.kind {
        Kind::List(items) => items,
        _ => slice::from_ref(value),
    }
}

fn diagnose(code: Code, span: Option<Span>, error: &Error) -> Diagnostic {
    Diagnostic::new(code, span, error.to_string(), error.fix().into())
}

/// The noun for a kind of value, with its article.
fn noun(kind: &Kind) -> &'static str {
    match kind {
        Kind::Bool(_) => "a bool",
        Kind::Integer(_) => "an integer",
        Kind::Float(_) => "a float",
        Kind::String(_) => "a string",
        Kind::Reference(_) => "a reference",
        Kind::List(_) => "a list",
        Kind::Map(_) => "a map",
        Kind::Call(_) => "a call",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::value::{Call, Float};
    use crate::{Map, Position, Source, Span};
    use proptest::prelude::*;

    const SYNTAX: &str = "Write a size such as \"200GiB\" or \"1.5GiB\"";
    const UNIT: &str = "Use a unit such as `MiB` or `GiB`, with exact case";
    const FRACTION: &str = "Round the size to whole bytes";
    const SEGMENT: &str = "Use one or more ASCII letters, digits, `_`, and `-` in that \
                           segment, after an optional leading `@`";

    /// The message for `text`, which is not a number and a unit with no space.
    fn syntax(text: &str) -> String {
        format!(
            "cannot read the byte size {text:?}: expected a number and a unit, such as \
             1023B, 1.5GiB, or 200GiB"
        )
    }

    fn span() -> Span {
        let at = |offset| Position {
            offset,
            line: 2,
            column: offset,
        };
        Span::new(Source(3), at(7), at(20)).unwrap()
    }

    fn value(kind: Kind) -> Value {
        Value {
            kind,
            span: Some(span()),
        }
    }

    fn string(text: &str) -> Value {
        value(Kind::String(text.into()))
    }

    fn refused(message: &str, fix: &str) -> Result<byte::Size, Diagnostic> {
        Err(Diagnostic::new(
            Code::new("document.bad-size"),
            Some(span()),
            message.into(),
            fix.into(),
        ))
    }

    /// Asserts that each text is refused with its message and fix.
    fn assert_refused(cases: &[(&str, &str, &str)]) {
        for &(text, message, fix) in cases {
            assert_eq!(size(&string(text)), refused(message, fix), "{text:?}");
        }
    }

    #[test]
    fn reads_a_size() {
        for (text, bytes) in [
            ("200GiB", 200 << 30),
            ("1.5GiB", 3 << 29),
            ("1023B", 1023),
            ("0B", 0),
        ] {
            assert_eq!(
                size(&string(text)),
                Ok(byte::Size::from_bytes(bytes)),
                "{text:?}"
            );
        }
    }

    #[test]
    fn refuses_a_value_that_is_not_a_string() {
        let call = Call {
            function: "gib".into(),
            function_span: None,
            arguments: Vec::new(),
        };
        for (kind, name) in [
            (Kind::Integer(200), "an integer"),
            (Kind::Float(Float::new(1.5).unwrap()), "a float"),
            (Kind::Bool(true), "a bool"),
            (Kind::Reference("gib".parse().unwrap()), "a reference"),
            (Kind::List(Vec::new()), "a list"),
            (Kind::Map(Map::default()), "a map"),
            (Kind::Call(call), "a call"),
        ] {
            assert_eq!(
                size(&value(kind)),
                refused(
                    &format!("a byte size is a string, not {name}"),
                    "Write a string such as \"200GiB\"",
                ),
                "{name}"
            );
        }
    }

    #[test]
    fn gives_the_text_with_no_whitespace() {
        for (text, fix) in [
            ("200 GiB", "Write \"200GiB\""),
            (" 1.5 GiB\t", "Write \"1.5GiB\""),
            ("200\u{a0}GiB", "Write \"200GiB\""),
        ] {
            assert_eq!(size(&string(text)), refused(&syntax(text), fix), "{text:?}");
        }
    }

    #[test]
    fn gives_the_unit_that_the_text_likely_means() {
        let unit = |text: &str| {
            format!("cannot read the byte size {text:?}: expected the unit GiB")
        };
        assert_refused(&[
            ("200GB", &unit("200GB"), "Write \"200GiB\""),
            ("1.5gib", &unit("1.5gib"), "Write \"1.5GiB\""),
            ("200 GB", &syntax("200 GB"), "Write \"200GiB\""),
            ("200 gb", &syntax("200 gb"), "Write \"200GiB\""),
            ("0.3GB", &unit("0.3GB"), UNIT),
            ("20000000000GB", &unit("20000000000GB"), UNIT),
        ]);
    }

    #[test]
    fn gives_a_fix_for_each_cause() {
        for text in [
            "",
            "GiB",
            "1e3B",
            "-1B",
            "1 5GiB",
            "1 024B",
            "200 Gb",
            "0.3 B",
            "200 GiB GiB",
        ] {
            assert_eq!(
                size(&string(text)),
                refused(&syntax(text), SYNTAX),
                "{text:?}"
            );
        }
        let units = |text: &str| {
            format!(
                "cannot read the byte size {text:?}: expected the unit B, KiB, MiB, \
                 GiB, or TiB"
            )
        };
        assert_refused(&[
            ("200Gb", &units("200Gb"), UNIT),
            ("200PiB", &units("200PiB"), UNIT),
            (
                "0.3B",
                "cannot read the byte size \"0.3B\": expected a whole number of bytes",
                FRACTION,
            ),
            (
                "16777216TiB",
                "cannot read the byte size \"16777216TiB\": expected a size of at most \
                 16777215TiB",
                "Use at most \"16777215TiB\"",
            ),
            (
                "18446744073709551616B",
                "cannot read the byte size \"18446744073709551616B\": expected a size \
                 of at most 18446744073709551615B",
                "Use at most \"18446744073709551615B\"",
            ),
        ]);
    }

    #[test]
    fn names_a_string_as_a_string() {
        assert_eq!(noun(&Kind::String("200GiB".into())), "a string");
    }

    #[test]
    fn puts_no_span_on_a_value_with_none() {
        let value = Value {
            kind: Kind::String("200".into()),
            span: None,
        };
        assert_eq!(size(&value).unwrap_err().span, None);
    }

    /// A span that starts at `offset`, so each item in a list has its own.
    fn at(offset: u32) -> Option<Span> {
        let at = |offset| Position {
            offset,
            line: 0,
            column: offset,
        };
        Span::new(Source(3), at(offset), at(offset.saturating_add(1)))
    }

    /// A list of the items, each at its index.
    fn list(items: Vec<Kind>) -> Value {
        let items = (0..)
            .zip(items)
            .map(|(i, kind)| Value { kind, span: at(i) });
        value(Kind::List(items.collect()))
    }

    fn text(text: &str) -> Kind {
        Kind::String(text.into())
    }

    /// A `document.bad-name` diagnostic.
    fn bad_name(span: Option<Span>, message: &str, fix: &str) -> Diagnostic {
        Diagnostic::new(
            Code::new("document.bad-name"),
            span,
            message.into(),
            fix.into(),
        )
    }

    mod labels {
        use super::*;

        fn labeled(text: &str) -> Label {
            Label {
                text: text.into(),
                span: Some(span()),
            }
        }

        #[test]
        fn reads_a_label() {
            assert_eq!(
                label(&labeled("site_a.cell_1")),
                Ok("site_a.cell_1".parse().unwrap())
            );
        }

        #[test]
        fn refuses_a_label_that_name_refuses() {
            let long = "a".repeat(256);
            for (text, message, fix) in [
                (
                    "site a",
                    "a segment is not valid: \"site a\" in \"site a\"",
                    SEGMENT,
                ),
                (
                    "site_a.*",
                    "a wildcard is out of place: \"site_a.*\"",
                    "Use `*` and `**` only as whole segments of a pattern, never in a \
                     name",
                ),
                (
                    "",
                    "a name or pattern is empty",
                    "Write at least one segment",
                ),
                (
                    long.as_str(),
                    "a name or pattern is 256 bytes long, more than the limit of 255 \
                     bytes",
                    "Use fewer or shorter segments",
                ),
            ] {
                assert_eq!(
                    label(&labeled(text)),
                    Err(bad_name(Some(span()), message, fix)),
                    "{text:?}"
                );
            }
        }

        #[test]
        fn puts_no_span_on_a_label_with_none() {
            let unspanned = Label {
                text: "site a".into(),
                span: None,
            };
            assert_eq!(label(&unspanned).unwrap_err().span, None);
        }

        proptest! {
            #[test]
            fn reads_as_name_does(
                text in prop_oneof![any::<String>(), "[a-z_.* @-]{0,8}"],
            ) {
                let read = label(&labeled(&text));
                prop_assert_eq!(name(&string(&text)), read.clone());
                match (read, text.parse::<Name>()) {
                    (Ok(read), Ok(parsed)) => prop_assert_eq!(read, parsed),
                    (Err(diagnostic), Err(error)) => prop_assert_eq!(
                        diagnostic,
                        Diagnostic::new(
                            Code::new("document.bad-name"),
                            Some(span()),
                            error.to_string(),
                            error.fix().into(),
                        )
                    ),
                    (read, parsed) => {
                        prop_assert!(false, "{:?}: {:?} and {:?}", text, read, parsed);
                    }
                }
            }
        }
    }

    mod names {
        use super::*;

        const FIX: &str = "Write a name such as \"site_a.node_1\"";

        fn parsed(text: &str) -> Name {
            text.parse().unwrap()
        }

        #[test]
        fn reads_a_string_or_a_reference() {
            assert_eq!(name(&string("site_a.node_1")), Ok(parsed("site_a.node_1")));
            let reference = Kind::Reference(parsed("site_a.node_1"));
            assert_eq!(name(&value(reference)), Ok(parsed("site_a.node_1")));
        }

        #[test]
        fn refuses_a_value_that_is_not_a_string_or_a_reference() {
            let call = Call {
                function: "node".into(),
                function_span: None,
                arguments: Vec::new(),
            };
            for (kind, noun) in [
                (Kind::Integer(7), "an integer"),
                (Kind::Float(Float::new(1.5).unwrap()), "a float"),
                (Kind::Bool(true), "a bool"),
                (Kind::List(Vec::new()), "a list"),
                (Kind::Map(Map::default()), "a map"),
                (Kind::Call(call), "a call"),
            ] {
                let message = format!("a name is a string or a reference, not {noun}");
                assert_eq!(
                    name(&value(kind)),
                    Err(bad_name(Some(span()), &message, FIX)),
                    "{noun}"
                );
            }
        }

        #[test]
        fn refuses_a_string_that_name_refuses_at_the_value() {
            assert_eq!(
                name(&string("site a")),
                Err(bad_name(
                    Some(span()),
                    "a segment is not valid: \"site a\" in \"site a\"",
                    SEGMENT
                ))
            );
        }

        #[test]
        fn reads_one_name_or_a_list_in_order_with_repeats() {
            assert_eq!(names(&string("n_1")), Ok(vec![parsed("n_1")]));
            assert_eq!(names(&list(Vec::new())), Ok(Vec::new()));
            assert_eq!(
                names(&list(vec![
                    text("n_2"),
                    Kind::Reference(parsed("n_1")),
                    text("n_2"),
                ])),
                Ok(vec![parsed("n_2"), parsed("n_1"), parsed("n_2")])
            );
        }

        #[test]
        fn refuses_the_first_item_that_name_refuses_at_the_item() {
            assert_eq!(
                names(&list(vec![
                    text("n_1"),
                    Kind::List(vec![string("n_2")]),
                    text("site a"),
                ])),
                Err(bad_name(
                    at(1),
                    "a name is a string or a reference, not a list",
                    FIX
                ))
            );
        }
    }

    mod selectors {
        use super::*;

        fn reference(name: &str) -> Kind {
            Kind::Reference(name.parse().unwrap())
        }

        fn refused(span: Option<Span>, message: &str, fix: &str) -> Diagnostic {
            Diagnostic::new(
                Code::new("document.bad-selector"),
                span,
                message.into(),
                fix.into(),
            )
        }

        const NO_INCLUDE: &str = "Add a pattern without a leading `!`";

        #[test]
        fn reads_each_form() {
            let expected =
                |patterns: &[&str]| Ok(Selector::new(patterns.to_vec()).unwrap());
            assert_eq!(selector(&string("site_a.*")), expected(&["site_a.*"]));
            assert_eq!(
                selector(&value(reference("site_a.plc_7"))),
                expected(&["site_a.plc_7"])
            );
            assert_eq!(
                selector(&list(vec![
                    text("site_a.*"),
                    text("!site_a.test"),
                    reference("site_b.plc_7"),
                ])),
                expected(&["site_a.*", "!site_a.test", "site_b.plc_7"])
            );
        }

        #[test]
        fn refuses_a_list_that_includes_no_names() {
            for patterns in [vec![], vec![text("!site_a.test")]] {
                assert_eq!(
                    selector(&list(patterns)),
                    Err(refused(
                        Some(span()),
                        "a selector includes no names",
                        NO_INCLUDE
                    ))
                );
            }
        }

        #[test]
        fn refuses_a_pattern_at_the_pattern() {
            assert_eq!(
                selector(&list(vec![text("site_a.*"), text("site a"), text("a*b")])),
                Err(refused(
                    at(1),
                    "a segment is not valid: \"site a\" in \"site a\"",
                    SEGMENT
                ))
            );
            assert_eq!(
                selector(&list(vec![text("!site_a.test"), text("!")])),
                Err(refused(
                    at(1),
                    "a segment is not valid: \"\" in \"!\"",
                    SEGMENT
                ))
            );
            assert_eq!(
                selector(&string("a*b")),
                Err(refused(
                    Some(span()),
                    "a wildcard is out of place: \"a*b\"",
                    "Use `*` and `**` only as whole segments of a pattern, never in a \
                     name"
                ))
            );
        }

        #[test]
        fn refuses_a_pattern_too_long_or_empty_at_the_pattern() {
            let long = "a".repeat(256);
            assert_eq!(
                selector(&list(vec![text("site_a.*"), text(&long)])),
                Err(refused(
                    at(1),
                    "a name or pattern is 256 bytes long, more than the limit of 255 \
                     bytes",
                    "Use fewer or shorter segments"
                ))
            );
            assert_eq!(
                selector(&list(vec![text("site_a.*"), text("")])),
                Err(refused(
                    at(1),
                    "a name or pattern is empty",
                    "Write at least one segment"
                ))
            );
        }

        #[test]
        fn reads_an_exclusion_whose_pattern_is_at_the_limit() {
            let exclusion = format!("!{}", "a".repeat(255));
            assert_eq!(
                selector(&list(vec![text("b"), text(&exclusion)])),
                Ok(Selector::new(["b", exclusion.as_str()]).unwrap())
            );
        }

        #[test]
        fn refuses_the_first_problem_in_source_order() {
            assert_eq!(
                selector(&list(vec![text("site a"), Kind::Integer(7)])),
                Err(refused(
                    at(0),
                    "a segment is not valid: \"site a\" in \"site a\"",
                    SEGMENT
                ))
            );
            assert_eq!(
                selector(&list(vec![Kind::Integer(7), text("site a")])),
                Err(refused(
                    at(0),
                    "a pattern is a string or a reference, not an integer",
                    "Write a string such as \"site_a.*\""
                ))
            );
        }

        #[test]
        fn refuses_a_value_that_is_not_a_pattern() {
            let fix = "Write a string such as \"site_a.*\"";
            assert_eq!(
                selector(&value(Kind::Integer(7))),
                Err(refused(
                    Some(span()),
                    "a pattern is a string or a reference, not an integer",
                    fix
                ))
            );
            assert_eq!(
                selector(&list(vec![
                    text("site_a.*"),
                    Kind::List(vec![string("site_b.*")]),
                ])),
                Err(refused(
                    at(1),
                    "a pattern is a string or a reference, not a list",
                    fix
                ))
            );
        }

        fn pattern() -> impl Strategy<Value = String> {
            prop_oneof![
                any::<String>(),
                "!?[a-z_* @-]{0,3}(\\.[a-z_* @-]{0,3}){0,2}",
            ]
        }

        proptest! {
            #[test]
            fn reads_as_selector_new_does(
                texts in prop::collection::vec(pattern(), 0..4),
            ) {
                let read = selector(&list(texts.iter().map(|t| text(t)).collect()));
                match (read, Selector::new(texts.iter().map(String::as_str))) {
                    (Ok(read), Ok(parsed)) => prop_assert_eq!(read, parsed),
                    (Err(diagnostic), Err(error)) => {
                        prop_assert_eq!(&diagnostic.message, &error.to_string());
                        prop_assert_eq!(diagnostic.fix.as_str(), error.fix());
                        // After an include, the first prefix of the list that
                        // `Selector::new` refuses ends at the refused pattern.
                        let mut prefix = vec!["x"];
                        let refused_at = (0..).zip(&texts).find_map(|(i, text)| {
                            prefix.push(text);
                            Selector::new(prefix.iter().copied()).is_err().then_some(i)
                        });
                        let expected = refused_at.map_or(Some(span()), at);
                        prop_assert_eq!(diagnostic.span, expected);
                    }
                    (read, parsed) => {
                        prop_assert!(false, "{:?}: {:?} and {:?}", texts, read, parsed);
                    }
                }
            }
        }
    }

    fn refused_span(message: &str, fix: &str) -> Result<time::Span, Diagnostic> {
        Err(Diagnostic::new(
            Code::new("document.bad-span"),
            Some(span()),
            message.into(),
            fix.into(),
        ))
    }

    #[test]
    fn reads_a_span() {
        for (text, read) in [
            ("3d", time::Span::DAY.nanos() * 3),
            ("1.5s", 1_500_000_000),
            ("0s", 0),
            ("-2h", -time::Span::HOUR.nanos() * 2),
        ] {
            assert_eq!(
                super::span(&string(text)),
                Ok(time::Span::from_nanos(read)),
                "{text:?}"
            );
        }
    }

    #[test]
    fn refuses_a_span_that_is_not_a_string() {
        for (kind, name) in [
            (Kind::Integer(3), "an integer"),
            (Kind::Reference("d".parse().unwrap()), "a reference"),
            (Kind::List(Vec::new()), "a list"),
        ] {
            assert_eq!(
                super::span(&value(kind)),
                refused_span(
                    &format!("a span is a string, not {name}"),
                    "Write a string such as \"3d\"",
                ),
                "{name}"
            );
        }
    }

    #[test]
    fn refuses_a_span_that_time_refuses() {
        for (text, problem, fix) in [
            (
                "3 days",
                "a span is not a number and a unit",
                "Write a span such as \"250us\", \"1.5s\", or \"3d\"",
            ),
            (
                "0.5ns",
                "a span is not a whole number of nanoseconds",
                "Use fewer fraction digits or a smaller unit",
            ),
            (
                "106752d",
                "a span does not fit in 64-bit nanoseconds",
                "Use a span from \"-106751d\" to \"106751d\"",
            ),
        ] {
            assert_eq!(
                super::span(&string(text)),
                refused_span(&format!("cannot read the span {text:?}: {problem}"), fix),
                "{text:?}"
            );
        }
    }

    /// A text that is often close to a span.
    fn near_span() -> impl Strategy<Value = String> {
        prop_oneof![
            any::<String>(),
            "-?[0-9]{0,21}[.]?[0-9]{0,20} ?(ns|us|ms|s|m|h|d|x|)",
        ]
    }

    /// A text that is often close to a byte size.
    fn near() -> impl Strategy<Value = String> {
        prop_oneof![
            any::<String>(),
            concat!(
                "[ \t\u{a0}]?[0-9]{0,22}[ .\t]{0,2}[0-9]{0,2}[ \t]?",
                "(B|KiB|MiB|GiB|TiB|GB|gib|gb|Gb|PiB|)[ \t]?",
            ),
        ]
    }

    proptest! {
        #[test]
        fn reads_the_text_of_each_size(bytes in any::<u64>()) {
            let written = byte::Size::from_bytes(bytes);
            prop_assert_eq!(size(&string(&written.to_string())), Ok(written));
        }

        #[test]
        fn reads_as_byte_size_does(text in near()) {
            match (size(&string(&text)), text.parse::<byte::Size>()) {
                (Ok(read), Ok(parsed)) => prop_assert_eq!(read, parsed),
                (Err(diagnostic), Err(error)) => {
                    prop_assert_eq!(diagnostic.code, Code::new("document.bad-size"));
                    prop_assert_eq!(diagnostic.span, Some(span()));
                    prop_assert_eq!(
                        diagnostic.message,
                        format!("cannot read the byte size {text:?}: {error}")
                    );
                    let likely = diagnostic
                        .fix
                        .strip_prefix("Write \"")
                        .and_then(|rest| rest.strip_suffix('"'));
                    if let Some(likely) = likely {
                        prop_assert!(size(&string(likely)).is_ok(), "{:?}", text);
                    } else {
                        // The fix is the error's, with each size quoted.
                        prop_assert_eq!(
                            diagnostic.fix.replace('"', ""),
                            error.fix(),
                            "{:?}",
                            text
                        );
                    }
                }
                (read, parsed) => {
                    prop_assert!(false, "{:?}: {:?} and {:?}", text, read, parsed);
                }
            }
        }

        #[test]
        fn reads_the_text_of_each_span(nanos in any::<i64>()) {
            let written = time::Span::from_nanos(nanos);
            prop_assert_eq!(super::span(&string(&written.to_string())), Ok(written));
        }

        #[test]
        fn reads_as_time_span_does(text in near_span()) {
            match (super::span(&string(&text)), text.parse::<time::Span>()) {
                (Ok(read), Ok(parsed)) => prop_assert_eq!(read, parsed),
                (Err(diagnostic), Err(error)) => {
                    prop_assert_eq!(diagnostic.code, Code::new("document.bad-span"));
                    prop_assert_eq!(diagnostic.span, Some(span()));
                    prop_assert_eq!(
                        diagnostic.message,
                        format!("cannot read the span {text:?}: {error}")
                    );
                    // The fix is the error's, with each span quoted.
                    prop_assert_eq!(diagnostic.fix.replace('"', ""), error.fix());
                }
                (read, parsed) => {
                    prop_assert!(false, "{:?}: {:?} and {:?}", text, read, parsed);
                }
            }
        }
    }
}
