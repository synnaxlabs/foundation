//! The units of data channels and the table that maps common units to standard codes.

use std::fmt;

/// The unit of the values of a data channel, as a file wrote it (`kPa`). Units are
/// case-sensitive: `mPa` and `MPa` are different units.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Unit(Box<str>);

/// The most bytes a unit may have.
const MAX_BYTES: usize = 32;

impl Unit {
    /// Reads a unit: 1 to 32 printable ASCII characters, with no space. A unit that is
    /// not in the table is valid; it has no code.
    ///
    /// # Errors
    ///
    /// Returns an [`Error`] when `text` is empty, longer than 32 bytes, or has a
    /// character that is not printable ASCII or is a space.
    pub fn new(text: &str) -> Result<Self, Error> {
        if text.is_empty() {
            return Err(Error::Empty);
        }
        if text.len() > MAX_BYTES {
            return Err(Error::Long { len: text.len() });
        }
        if let Some((at, found)) =
            text.char_indices().find(|(_, c)| !c.is_ascii_graphic())
        {
            return Err(Error::Character { at, found });
        }
        Ok(Self(text.into()))
    }

    /// The text of the unit.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The UN/CEFACT Recommendation 20 common code of the unit (`KPA` for `kPa`), or
    /// `None` when the table does not have the unit.
    #[must_use]
    pub fn code(&self) -> Option<&'static str> {
        TABLE
            .binary_search_by(|(text, _)| (*text).cmp(&*self.0))
            .ok()
            .and_then(|at| TABLE.get(at))
            .map(|(_, code)| *code)
    }
}

/// Common units and their Recommendation 20 codes, sorted by text for binary search.
const TABLE: &[(&str, &str)] = &[
    ("%", "P1"),
    ("A", "AMP"),
    ("Hz", "HTZ"),
    ("J", "JOU"),
    ("K", "KEL"),
    ("L", "LTR"),
    ("L/min", "L2"),
    ("MHz", "MHZ"),
    ("MPa", "MPA"),
    ("MW", "MAW"),
    ("N", "NEW"),
    ("N.m", "NU"),
    ("Pa", "PAL"),
    ("V", "VLT"),
    ("W", "WTT"),
    ("bar", "BAR"),
    ("cm", "CMT"),
    ("deg", "DD"),
    ("degC", "CEL"),
    ("degF", "FAH"),
    ("ft", "FOT"),
    ("g", "GRM"),
    ("h", "HUR"),
    ("in", "INH"),
    ("kHz", "KHZ"),
    ("kPa", "KPA"),
    ("kV", "KVT"),
    ("kW", "KWT"),
    ("kWh", "KWH"),
    ("kg", "KGM"),
    ("km", "KMT"),
    ("lb", "LBR"),
    ("m", "MTR"),
    ("m/s", "MTS"),
    ("m/s2", "MSK"),
    ("m3", "MTQ"),
    ("m3/h", "MQH"),
    ("mA", "4K"),
    ("mV", "2Z"),
    ("mbar", "MBR"),
    ("min", "MIN"),
    ("mm", "MMT"),
    ("ms", "C26"),
    ("ohm", "OHM"),
    ("psi", "PS"),
    ("rad", "C81"),
    ("s", "SEC"),
];

/// A text that is not a unit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Error {
    /// The text is empty.
    Empty,
    /// The text is longer than 32 bytes.
    Long {
        /// Its length in bytes.
        len: usize,
    },
    /// The text has a character that is not printable ASCII, or a space.
    Character {
        /// The byte offset of the character.
        at: usize,
        /// The character.
        found: char,
    },
}

impl Error {
    /// What to do instead: a sentence with no final period.
    #[must_use]
    pub const fn fix(&self) -> &'static str {
        match self {
            Self::Empty => "Write a unit such as kPa, or remove the unit",
            Self::Long { .. } => "Use a unit of at most 32 bytes",
            Self::Character { .. } => {
                "Use only printable ASCII characters with no space, such as m/s2"
            }
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => write!(f, "a unit is empty"),
            Self::Long { len } => {
                write!(f, "a unit has {len} bytes, more than {MAX_BYTES}")
            }
            Self::Character { at, found } => {
                write!(
                    f,
                    "a unit has the character {found:?} at byte {at}, which is not \
                     printable ASCII"
                )
            }
        }
    }
}

impl std::error::Error for Error {}

#[cfg(test)]
mod tests;
