//! What the fuzz targets have in common.

pub mod codec;
pub mod hub;

/// The stream messages in an input: each is a length byte and then that many bytes.
/// The last message ends with the input.
pub fn messages(mut bytes: &[u8]) -> impl Iterator<Item = &[u8]> {
    std::iter::from_fn(move || {
        let (&len, rest) = bytes.split_first()?;
        let (message, rest) = rest.split_at(usize::from(len).min(rest.len()));
        bytes = rest;
        Some(message)
    })
}

/// Checks that a value read from `text` prints as text that reads back to it.
///
/// # Panics
///
/// When the printed text does not read back to the same value.
pub fn check_round_trip<T>(text: &str)
where
    T: std::str::FromStr + std::fmt::Display + PartialEq + std::fmt::Debug,
    T::Err: std::fmt::Debug + PartialEq,
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
