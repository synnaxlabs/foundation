//! The readers of stamps, spans, and ranges never panic, and a value that one reads
//! prints as text that reads back to the same value.

#![no_main]

use std::fmt::{Debug, Display};
use std::str::FromStr;

use libfuzzer_sys::fuzz_target;
use types::time::{Range, Span, Stamp};

fuzz_target!(|text: &str| {
    check::<Stamp>(text);
    check::<Span>(text);
    check::<Range>(text);
});

fn check<T>(text: &str)
where
    T: FromStr + Display + PartialEq + Debug,
    T::Err: Debug + PartialEq,
{
    let Ok(value) = text.parse::<T>() else {
        return;
    };
    let printed = value.to_string();
    assert_eq!(
        printed.parse::<T>().as_ref(),
        Ok(&value),
        "{printed:?} does not read back"
    );
}
