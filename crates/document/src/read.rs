//! Readers for the quantities that kinds and `config` share, so a value reads the same
//! in every block.

use std::num::NonZeroU64;

use crate::diagnostic::{Code, Diagnostic};
use crate::value::{Kind, Value};

const BAD_SIZE: Code = Code::new("document.bad-size");

/// A byte size unit and the decimal unit that people write for it.
struct Unit {
    name: &'static str,
    bytes: NonZeroU64,
    decimal: Option<&'static str>,
}

/// The units, each 1024 times the one before.
const UNITS: [Unit; 5] = [
    Unit::new("B", 1, None),
    Unit::new("KiB", 1024, Some("kB")),
    Unit::new("MiB", 1_048_576, Some("MB")),
    Unit::new("GiB", 1_073_741_824, Some("GB")),
    Unit::new("TiB", 1_099_511_627_776, Some("TB")),
];

impl Unit {
    const fn new(
        name: &'static str,
        bytes: u64,
        decimal: Option<&'static str>,
    ) -> Self {
        let Some(bytes) = NonZeroU64::new(bytes) else {
            panic!("a unit has at least one byte");
        };
        Self {
            name,
            bytes,
            decimal,
        }
    }
}

/// Reads a byte size in bytes: a string of ASCII digits, one space, and a unit, such
/// as `"200 GiB"`. The units are `B`, `KiB`, `MiB`, `GiB`, and `TiB`, each 1024 times
/// the one before. Zero is a size.
///
/// # Errors
///
/// A `document.bad-size` diagnostic at the value's span when the value is not such a
/// string, or when the size is more than `u64::MAX` bytes. When one text is clearly
/// meant, the fix gives it, such as `Write "200 GiB"` for `"200GiB"`.
pub fn size(value: &Value) -> Result<u64, Diagnostic> {
    let bad = |message: String, fix: String| {
        Diagnostic::new(BAD_SIZE, value.span, message, fix)
    };
    let Kind::String(text) = &value.kind else {
        return Err(bad(
            format!("a byte size is a string, not {}", noun(&value.kind)),
            "Write a string such as \"200 GiB\"".into(),
        ));
    };
    let trimmed = text.trim();
    let at = trimmed
        .rfind(char::is_whitespace)
        .or_else(|| trimmed.find(char::is_alphabetic))
        .unwrap_or(trimmed.len());
    let (number, unit) = trimmed.split_at(at);
    let (number, unit) = (number.trim_end(), unit.trim_start());
    if number.is_empty() {
        return Err(bad(
            format!("the byte size {text:?} has no number"),
            "Write a number, one space, and a unit, such as \"200 GiB\"".into(),
        ));
    }
    if unit.is_empty() {
        return Err(bad(
            format!("the byte size {text:?} has no unit"),
            "Add a unit: B, KiB, MiB, GiB, or TiB".into(),
        ));
    }
    let Some(found) = UNITS.iter().find(|known| known.name == unit) else {
        let (message, fix) = unknown(unit);
        return Err(bad(message, fix));
    };
    if !number.bytes().all(|b| b.is_ascii_digit()) {
        let (message, fix) = not_digits(text, number, found);
        return Err(bad(message, fix));
    }
    let Some(bytes) = number
        .parse::<u64>()
        .ok()
        .and_then(|count| count.checked_mul(found.bytes.get()))
    else {
        return Err(bad(
            format!("the byte size {text:?} is more than {} bytes", u64::MAX),
            format!("Use at most {} {unit}", u64::MAX / found.bytes),
        ));
    };
    if **text != format!("{number} {unit}") {
        return Err(bad(
            format!("the byte size {text:?} is not a number, one space, and a unit"),
            format!("Write \"{number} {unit}\""),
        ));
    }
    Ok(bytes)
}

/// The message and fix for a unit that is not in [`UNITS`].
fn unknown(unit: &str) -> (String, String) {
    if unit.ends_with('b') {
        return (
            format!("the unit {unit:?} ends in b, which means bits"),
            "Write bytes as B, KiB, MiB, GiB, or TiB".into(),
        );
    }
    for known in &UNITS {
        if known.name.eq_ignore_ascii_case(unit) {
            return (
                format!("the unit {unit:?} has the wrong case"),
                format!("Write it as {}", known.name),
            );
        }
        if known
            .decimal
            .is_some_and(|decimal| decimal.eq_ignore_ascii_case(unit))
        {
            return (
                format!("the unit {unit:?} is decimal"),
                format!("Use the binary unit {}", known.name),
            );
        }
    }
    (
        format!("the unit {unit:?} is not a byte size unit"),
        "Use B, KiB, MiB, GiB, or TiB".into(),
    )
}

/// The message and fix for a number that is not all ASCII digits.
fn not_digits(text: &str, number: &str, unit: &Unit) -> (String, String) {
    let digits = |part: &str| part.bytes().all(|b| b.is_ascii_digit());
    let fraction = number
        .split_once('.')
        .is_some_and(|(whole, part)| digits(whole) && digits(part) && number != ".");
    if !fraction {
        return (
            format!("the number {number:?} is not digits 0 to 9"),
            "Write the number with digits 0 to 9 only".into(),
        );
    }
    let smaller = UNITS
        .iter()
        .zip(UNITS.iter().skip(1))
        .find(|(_, larger)| larger.name == unit.name);
    let fix = match smaller {
        Some((smaller, _)) => {
            format!(
                "Use a whole number, or a smaller unit such as {}",
                smaller.name
            )
        }
        None => "Use a whole number of bytes".into(),
    };
    (format!("the byte size {text:?} has a fraction"), fix)
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

    const SIZES: [(&str, u64); 5] = [
        ("B", 1),
        ("KiB", 1024),
        ("MiB", 1_048_576),
        ("GiB", 1_073_741_824),
        ("TiB", 1_099_511_627_776),
    ];

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

    fn refused(message: &str, fix: &str) -> Result<u64, Diagnostic> {
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
    fn reads_each_unit() {
        for (text, bytes) in [
            ("1 B", 1),
            ("1 KiB", 1024),
            ("1 MiB", 1_048_576),
            ("1 GiB", 1_073_741_824),
            ("1 TiB", 1_099_511_627_776),
            ("200 GiB", 214_748_364_800),
        ] {
            assert_eq!(size(&string(text)), Ok(bytes), "{text:?}");
        }
    }

    #[test]
    fn reads_zero_and_leading_zeros() {
        for (text, bytes) in [("0 B", 0), ("0 TiB", 0), ("007 KiB", 7168)] {
            assert_eq!(size(&string(text)), Ok(bytes), "{text:?}");
        }
        let zeros = format!("{}1 B", "0".repeat(40));
        assert_eq!(size(&string(&zeros)), Ok(1));
    }

    #[test]
    fn reads_up_to_the_largest_u64() {
        assert_eq!(size(&string("18446744073709551615 B")), Ok(u64::MAX));
        assert_eq!(
            size(&string("16777215 TiB")),
            Ok(18_446_742_974_197_923_840)
        );
    }

    #[test]
    fn refuses_a_size_past_the_largest_u64() {
        assert_refused(&[
            (
                "18446744073709551616 B",
                "the byte size \"18446744073709551616 B\" is more than \
                 18446744073709551615 bytes",
                "Use at most 18446744073709551615 B",
            ),
            (
                "16777216 TiB",
                "the byte size \"16777216 TiB\" is more than \
                 18446744073709551615 bytes",
                "Use at most 16777215 TiB",
            ),
            (
                "18446744073709551616B",
                "the byte size \"18446744073709551616B\" is more than \
                 18446744073709551615 bytes",
                "Use at most 18446744073709551615 B",
            ),
            (
                " 16777216 TiB",
                "the byte size \" 16777216 TiB\" is more than \
                 18446744073709551615 bytes",
                "Use at most 16777215 TiB",
            ),
            (
                "99999999999999999999999 KiB",
                "the byte size \"99999999999999999999999 KiB\" is more than \
                 18446744073709551615 bytes",
                "Use at most 18014398509481983 KiB",
            ),
        ]);
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
                    "Write a string such as \"200 GiB\"",
                ),
                "{name}"
            );
        }
    }

    #[test]
    fn refuses_a_size_with_no_number() {
        let fix = "Write a number, one space, and a unit, such as \"200 GiB\"";
        assert_refused(&[
            ("GiB", "the byte size \"GiB\" has no number", fix),
            ("", "the byte size \"\" has no number", fix),
            ("  ", "the byte size \"  \" has no number", fix),
        ]);
    }

    #[test]
    fn refuses_a_size_with_no_unit() {
        let fix = "Add a unit: B, KiB, MiB, GiB, or TiB";
        assert_refused(&[
            ("200", "the byte size \"200\" has no unit", fix),
            (" 200 ", "the byte size \" 200 \" has no unit", fix),
        ]);
    }

    #[test]
    fn names_the_unit_in_its_case() {
        assert_refused(&[
            (
                "200 GIB",
                "the unit \"GIB\" has the wrong case",
                "Write it as GiB",
            ),
            (
                "1 KIB",
                "the unit \"KIB\" has the wrong case",
                "Write it as KiB",
            ),
            (
                "1.5 mIB",
                "the unit \"mIB\" has the wrong case",
                "Write it as MiB",
            ),
        ]);
    }

    #[test]
    fn names_a_unit_that_ends_in_b_as_bits() {
        let fix = "Write bytes as B, KiB, MiB, GiB, or TiB";
        assert_refused(&[
            ("1 b", "the unit \"b\" ends in b, which means bits", fix),
            (
                "200 gib",
                "the unit \"gib\" ends in b, which means bits",
                fix,
            ),
            ("1 Gb", "the unit \"Gb\" ends in b, which means bits", fix),
            ("1 mb", "the unit \"mb\" ends in b, which means bits", fix),
            (
                "1.5 tIb",
                "the unit \"tIb\" ends in b, which means bits",
                fix,
            ),
        ]);
    }

    #[test]
    fn names_the_binary_unit_for_a_decimal_unit() {
        assert_refused(&[
            (
                "200 GB",
                "the unit \"GB\" is decimal",
                "Use the binary unit GiB",
            ),
            (
                "1 kB",
                "the unit \"kB\" is decimal",
                "Use the binary unit KiB",
            ),
            (
                "1 KB",
                "the unit \"KB\" is decimal",
                "Use the binary unit KiB",
            ),
            (
                "1 mB",
                "the unit \"mB\" is decimal",
                "Use the binary unit MiB",
            ),
            (
                "1 TB",
                "the unit \"TB\" is decimal",
                "Use the binary unit TiB",
            ),
        ]);
    }

    #[test]
    fn refuses_an_unknown_unit() {
        let fix = "Use B, KiB, MiB, GiB, or TiB";
        assert_refused(&[
            ("200 PiB", "the unit \"PiB\" is not a byte size unit", fix),
            (
                "200 bytes",
                "the unit \"bytes\" is not a byte size unit",
                fix,
            ),
            ("1 G", "the unit \"G\" is not a byte size unit", fix),
            ("200 B2", "the unit \"B2\" is not a byte size unit", fix),
            ("1e3B", "the unit \"e3B\" is not a byte size unit", fix),
            (
                "2\u{2170}GiB",
                "the unit \"\u{2170}GiB\" is not a byte size unit",
                fix,
            ),
        ]);
    }

    #[test]
    fn refuses_a_fraction() {
        assert_refused(&[
            (
                "1.5 GiB",
                "the byte size \"1.5 GiB\" has a fraction",
                "Use a whole number, or a smaller unit such as MiB",
            ),
            (
                "1.0 KiB",
                "the byte size \"1.0 KiB\" has a fraction",
                "Use a whole number, or a smaller unit such as B",
            ),
            (
                ".5 TiB",
                "the byte size \".5 TiB\" has a fraction",
                "Use a whole number, or a smaller unit such as GiB",
            ),
            (
                "0.5 B",
                "the byte size \"0.5 B\" has a fraction",
                "Use a whole number of bytes",
            ),
        ]);
    }

    #[test]
    fn refuses_a_number_that_is_not_digits() {
        let fix = "Write the number with digits 0 to 9 only";
        assert_refused(&[
            ("-1 GiB", "the number \"-1\" is not digits 0 to 9", fix),
            ("+1 GiB", "the number \"+1\" is not digits 0 to 9", fix),
            ("1_000 B", "the number \"1_000\" is not digits 0 to 9", fix),
            ("1,000 B", "the number \"1,000\" is not digits 0 to 9", fix),
            ("1.2.3 B", "the number \"1.2.3\" is not digits 0 to 9", fix),
            ("-1.5 GiB", "the number \"-1.5\" is not digits 0 to 9", fix),
            ("+1.5 GiB", "the number \"+1.5\" is not digits 0 to 9", fix),
            (
                "1 000 GiB",
                "the number \"1 000\" is not digits 0 to 9",
                fix,
            ),
            ("1e3 B", "the number \"1e3\" is not digits 0 to 9", fix),
            ("0x10 B", "the number \"0x10\" is not digits 0 to 9", fix),
            (
                "2 GiB GiB",
                "the number \"2 GiB\" is not digits 0 to 9",
                fix,
            ),
            (
                "2\u{2170} GiB",
                "the number \"2\u{2170}\" is not digits 0 to 9",
                fix,
            ),
            (". B", "the number \".\" is not digits 0 to 9", fix),
            (
                "\u{661}\u{660} B",
                "the number \"١٠\" is not digits 0 to 9",
                fix,
            ),
        ]);
    }

    #[test]
    fn gives_the_text_with_one_space() {
        for text in [
            "200GiB",
            "200  GiB",
            "200\tGiB",
            " 200 GiB",
            "200 GiB\n",
            "200\u{a0}GiB",
        ] {
            assert_eq!(
                size(&string(text)),
                refused(
                    &format!(
                        "the byte size {text:?} is not a number, one space, and a unit"
                    ),
                    "Write \"200 GiB\"",
                ),
                "{text:?}"
            );
        }
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
            "[0-9]{0,22}[ .\t+-]{0,2}[0-9]{0,2} ?(B|KiB|MiB|GiB|TiB|kB|GB|gib|b| |)",
        ]
    }

    proptest! {
        #[test]
        fn reads_each_count_of_each_unit(
            count in any::<u64>(),
            (unit, bytes) in prop::sample::select(SIZES.to_vec()),
        ) {
            let text = format!("{count} {unit}");
            let expected = count.checked_mul(bytes).ok_or_else(|| {
                let most = u64::MAX.checked_div(bytes).unwrap();
                refused(
                    &format!(
                        "the byte size {text:?} is more than 18446744073709551615 bytes"
                    ),
                    &format!("Use at most {most} {unit}"),
                )
                .unwrap_err()
            });
            prop_assert_eq!(size(&string(&text)), expected);
        }

        #[test]
        fn reads_only_digits_one_space_and_a_unit(text in near()) {
            match size(&string(&text)) {
                Ok(read) => {
                    let (count, unit) = text.split_once(' ').unwrap();
                    let digits = count.bytes().all(|b| b.is_ascii_digit());
                    prop_assert!(!count.is_empty() && digits, "{:?}", text);
                    let (_, bytes) = SIZES.iter().find(|(name, _)| *name == unit)
                        .unwrap();
                    let count = count.parse::<u64>().unwrap();
                    prop_assert_eq!(count.checked_mul(*bytes), Some(read));
                }
                Err(diagnostic) => {
                    prop_assert_eq!(diagnostic.code, Code::new("document.bad-size"));
                    prop_assert_eq!(diagnostic.span, Some(span()));
                    let written = diagnostic
                        .fix
                        .strip_prefix("Write \"")
                        .and_then(|rest| rest.strip_suffix('"'));
                    if let Some(written) = written {
                        prop_assert!(size(&string(written)).is_ok(), "{:?}", text);
                    }
                }
            }
        }
    }
}
