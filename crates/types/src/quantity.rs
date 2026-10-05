//! The number grammar that spans and byte sizes share: digits, then an optional `.`
//! and digits, then a unit.

/// A decimal number and the unit text after it, as in `1.5s` or `200GiB`.
pub(crate) struct Quantity<'a> {
    /// The digits before the point. Never empty.
    pub(crate) whole: &'a str,
    /// The digits after the point, with no trailing `0`. Empty for a whole number.
    pub(crate) fraction: &'a str,
    /// The text after the number. Never empty.
    pub(crate) unit: &'a str,
}

/// Splits `text` at its first byte that is not a digit or `.`. Returns `None` when no
/// unit follows, or when the number is not digits, then an optional `.` and digits.
pub(crate) fn split(text: &str) -> Option<Quantity<'_>> {
    let at = text.find(|c: char| !c.is_ascii_digit() && c != '.')?;
    let (number, unit) = text.split_at(at);
    let (whole, fraction) = number.split_once('.').unwrap_or((number, "0"));
    let digits = |t: &str| !t.is_empty() && t.bytes().all(|b| b.is_ascii_digit());
    (digits(whole) && digits(fraction)).then(|| Quantity {
        whole,
        fraction: fraction.trim_end_matches('0'),
        unit,
    })
}
