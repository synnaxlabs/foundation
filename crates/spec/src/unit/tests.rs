use proptest::prelude::*;

use super::*;

fn unit(text: &str) -> Unit {
    Unit::new(text).unwrap()
}

#[test]
fn refuses_an_empty_unit() {
    assert_eq!(Unit::new(""), Err(Error::Empty));
    assert_eq!(Error::Empty.to_string(), "a unit is empty");
}

#[test]
fn refuses_a_unit_longer_than_the_limit() {
    let longest = "a".repeat(Unit::MAX_BYTES);
    assert_eq!(unit(&longest).as_str(), longest);
    let longer = "a".repeat(Unit::MAX_BYTES + 1);
    assert_eq!(Unit::new(&longer), Err(Error::Long { len: 33 }));
    assert_eq!(
        Error::Long { len: 33 }.to_string(),
        "a unit has 33 bytes, more than 32"
    );
}

#[test]
fn refuses_whitespace_and_control_characters() {
    assert_eq!(
        Unit::new("k Pa"),
        Err(Error::Character { at: 1, found: ' ' })
    );
    assert_eq!(
        Unit::new("°C\n"),
        Err(Error::Character { at: 3, found: '\n' })
    );
    assert_eq!(
        Error::Character { at: 1, found: ' ' }.to_string(),
        "a unit has the character ' ' at byte 1"
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
fn fits_numbers_and_their_arrays_and_lists() {
    let numbers = [
        Scalar::I8,
        Scalar::I16,
        Scalar::I32,
        Scalar::I64,
        Scalar::U8,
        Scalar::U16,
        Scalar::U32,
        Scalar::U64,
        Scalar::F32,
        Scalar::F64,
    ];
    let others = [Scalar::Bool, Scalar::Stamp, Scalar::Span, Scalar::Uuid];
    let kpa = unit("kPa");
    for element in numbers {
        assert!(kpa.fits(Type::Scalar(element)), "{element:?}");
        assert!(kpa.fits(Type::Array { element, len: 3 }), "{element:?}");
        assert!(kpa.fits(Type::List { element, max: 3 }), "{element:?}");
    }
    for element in others {
        assert!(!kpa.fits(Type::Scalar(element)), "{element:?}");
        assert!(!kpa.fits(Type::Array { element, len: 3 }), "{element:?}");
        assert!(!kpa.fits(Type::List { element, max: 3 }), "{element:?}");
    }
    assert!(!kpa.fits(Type::String));
    assert!(!kpa.fits(Type::Bytes));
}

#[test]
fn holds_a_sorted_table_of_units_that_read_and_fit_a_number() {
    assert!(TABLE.is_sorted_by(|a, b| a.0 < b.0));
    for (text, code) in TABLE {
        let unit = unit(text);
        assert_eq!(unit.code(), Some(*code), "{text}");
        assert!(unit.fits(Type::Scalar(Scalar::F64)), "{text}");
    }
}

proptest! {
    #[test]
    fn reads_exactly_the_short_printable_texts_with_no_whitespace(text in ".{0,40}") {
        let valid = (1..=Unit::MAX_BYTES).contains(&text.len())
            && text.chars().all(|c| !c.is_whitespace() && !c.is_control());
        prop_assert_eq!(Unit::new(&text).is_ok(), valid);
        if valid {
            prop_assert_eq!(unit(&text).as_str().to_owned(), text);
        }
    }
}
