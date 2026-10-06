//! Readers for the quantities that kinds and `config` share, so a value reads the same
//! in every block.

use types::byte;

use crate::diagnostic::{Code, Diagnostic};
use crate::value::{Kind, Value};

const BAD_SIZE: Code = Code::new("document.bad-size");

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
            fix(text),
        )
    })
}

/// The fix for `text`, which `byte::Size` refuses. It comes from the text with no
/// whitespace and with the unit that the text likely means, so `"200 GB"` gets
/// `Write "200GiB"`.
fn fix(text: &str) -> String {
    let words: Vec<&str> = text.split_whitespace().collect();
    // `1 5GiB` may mean `15GiB` or `1.5GiB`, so it keeps its space.
    let joins_digits = words.iter().zip(words.iter().skip(1)).any(|(a, b)| {
        a.ends_with(|c: char| c.is_ascii_digit())
            && b.starts_with(|c: char| c.is_ascii_digit())
    });
    let mut written = if joins_digits {
        text.to_owned()
    } else {
        words.concat()
    };
    let mut parsed = written.parse::<byte::Size>();
    if let Err(byte::Error::Unit {
        start,
        meant: Some(meant),
    }) = parsed
    {
        written.truncate(start);
        written.push_str(meant);
        parsed = written.parse::<byte::Size>();
    }
    match parsed {
        Ok(_) => format!("Write \"{written}\""),
        Err(byte::Error::Syntax) => {
            "Write a size such as \"200GiB\" or \"1.5GiB\"".into()
        }
        Err(byte::Error::Unit { .. }) => {
            "Use a unit such as \"MiB\" or \"GiB\", with exact case".into()
        }
        Err(byte::Error::Fraction) => "Round the number, or use a smaller unit".into(),
        Err(byte::Error::Range { largest }) => format!("Use at most {largest}"),
    }
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
    const UNIT: &str = "Use a unit such as \"MiB\" or \"GiB\", with exact case";
    const FRACTION: &str = "Round the number, or use a smaller unit";

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
            ("200 gb", &syntax("200 gb"), "Write \"200GiB\""),
            ("0.3GB", &unit("0.3GB"), FRACTION),
            (
                "20000000000GB",
                &unit("20000000000GB"),
                "Use at most 17179869183GiB",
            ),
        ]);
    }

    #[test]
    fn gives_a_fix_for_each_cause() {
        for text in ["", "GiB", "1e3B", "-1B", "1 5GiB", "1 024B"] {
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
            ("200 Gb", &syntax("200 Gb"), UNIT),
            (
                "0.3B",
                "cannot read the byte size \"0.3B\": expected a whole number of bytes",
                FRACTION,
            ),
            (
                "16777216TiB",
                "cannot read the byte size \"16777216TiB\": expected a size of at most \
                 16777215TiB",
                "Use at most 16777215TiB",
            ),
            (
                "18446744073709551616B",
                "cannot read the byte size \"18446744073709551616B\": expected a size \
                 of at most 18446744073709551615B",
                "Use at most 18446744073709551615B",
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
                    let fix = diagnostic.fix.as_str();
                    let written = fix
                        .strip_prefix("Write \"")
                        .and_then(|rest| rest.strip_suffix('"'));
                    if let Some(written) = written {
                        prop_assert!(size(&string(written)).is_ok(), "{:?}", text);
                    } else if let Some(largest) = fix.strip_prefix("Use at most ") {
                        prop_assert!(size(&string(largest)).is_ok(), "{:?}", text);
                    } else {
                        let fixes = [SYNTAX, UNIT, FRACTION];
                        prop_assert!(fixes.contains(&fix), "{:?}", text);
                    }
                }
                (read, parsed) => {
                    prop_assert!(false, "{:?}: {:?} and {:?}", text, read, parsed);
                }
            }
        }
    }
}
