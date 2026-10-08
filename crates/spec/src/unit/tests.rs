use proptest::prelude::*;

use super::*;

fn unit(text: &str) -> Unit {
    Unit::new(text).unwrap()
}

#[test]
fn gives_the_fix_of_each_error() {
    for (error, fix) in [
        (Error::Empty, "Write a unit such as kPa, or remove the unit"),
        (Error::Long { len: 33 }, "Use a shorter unit, such as kPa"),
        (
            Error::Character { at: 1, found: ' ' },
            "Use only printable ASCII characters with no space, such as m/s2",
        ),
    ] {
        assert_eq!(error.fix(), fix, "{error:?}");
    }
}

#[test]
fn refuses_an_empty_unit() {
    assert_eq!(Unit::new(""), Err(Error::Empty));
    assert_eq!(Error::Empty.to_string(), "a unit is empty");
}

#[test]
fn refuses_a_unit_longer_than_the_limit() {
    let longest = "a".repeat(32);
    assert_eq!(unit(&longest).as_str(), longest);
    assert_eq!(Unit::new(&"a".repeat(33)), Err(Error::Long { len: 33 }));
    assert_eq!(
        Error::Long { len: 33 }.to_string(),
        "a unit has 33 bytes, more than 32"
    );
}

#[test]
fn refuses_a_character_that_is_not_printable_ascii() {
    for (text, at, found) in [
        ("k Pa", 1, ' '),
        ("kPa\n", 3, '\n'),
        ("kPa\u{200B}", 3, '\u{200B}'),
        ("k\u{200D}Pa", 1, '\u{200D}'),
        ("k\u{AD}Pa", 1, '\u{AD}'),
        ("k\u{202E}aP", 1, '\u{202E}'),
        ("\u{FEFF}kPa", 0, '\u{FEFF}'),
        ("°C", 0, '°'),
    ] {
        assert_eq!(
            Unit::new(text),
            Err(Error::Character { at, found }),
            "{text}"
        );
    }
    assert_eq!(
        Error::Character { at: 1, found: ' ' }.to_string(),
        "a unit has the character ' ' at byte 1, which is not printable ASCII"
    );
}

#[test]
fn gives_the_code_of_a_unit_in_the_table() {
    assert_eq!(unit("kPa").code(), Some("KPA"));
    assert_eq!(unit("degC").code(), Some("CEL"));
    assert_eq!(unit("mPa").code(), None);
    assert_eq!(unit("kpa").code(), None);
    assert_eq!(unit("furlong").code(), None);
    assert_eq!(unit("furlong").as_str(), "furlong");
}

#[test]
fn holds_a_sorted_table_of_units_that_read() {
    assert!(TABLE.is_sorted_by(|a, b| a.0 < b.0));
    for (text, code) in TABLE {
        assert_eq!(unit(text).code(), Some(*code), "{text}");
    }
}

proptest! {
    #[test]
    fn reads_exactly_the_short_printable_ascii_texts(
        text in "[\\x00-\\x7f°\u{200B}]{0,40}",
    ) {
        let valid = (1..=32).contains(&text.len())
            && text.bytes().all(|b| (0x21..=0x7e).contains(&b));
        prop_assert_eq!(Unit::new(&text).is_ok(), valid);
        if valid {
            prop_assert_eq!(unit(&text).as_str().to_owned(), text);
        }
    }
}
