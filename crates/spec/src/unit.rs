//! The units of data channels and the table that maps common units to standard codes.

use std::fmt;

use types::sample::{Scalar, Type};

/// The unit of the values of a data channel, as a file wrote it (`kPa`). Units are
/// case-sensitive: `mPa` and `MPa` are different units.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Unit(Box<str>);

impl Unit {
    /// The most bytes a unit may have.
    pub const MAX_BYTES: usize = 32;

    /// Reads a unit: 1 to [`Unit::MAX_BYTES`] bytes of text with no whitespace and no
    /// control character. A unit that is not in the table is valid; it has no code.
    ///
    /// # Errors
    ///
    /// Returns an [`Error`] when `text` is empty, too long, or has whitespace or a
    /// control character.
    pub fn new(text: &str) -> Result<Self, Error> {
        if text.is_empty() {
            return Err(Error::Empty);
        }
        if text.len() > Self::MAX_BYTES {
            return Err(Error::Long { len: text.len() });
        }
        if let Some((at, found)) = text
            .char_indices()
            .find(|(_, c)| c.is_whitespace() || c.is_control())
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

    /// Reports whether values of `data_type` can have this unit: integers and floats,
    /// and arrays and lists of them. Bool, stamp, span, UUID, string, and bytes values
    /// cannot.
    #[must_use]
    pub fn fits(&self, data_type: Type) -> bool {
        match data_type {
            Type::Scalar(s)
            | Type::Array { element: s, .. }
            | Type::List { element: s, .. } => number(s),
            Type::String | Type::Bytes => false,
        }
    }
}

const fn number(scalar: Scalar) -> bool {
    match scalar {
        Scalar::I8
        | Scalar::I16
        | Scalar::I32
        | Scalar::I64
        | Scalar::U8
        | Scalar::U16
        | Scalar::U32
        | Scalar::U64
        | Scalar::F32
        | Scalar::F64 => true,
        Scalar::Bool | Scalar::Stamp | Scalar::Span | Scalar::Uuid => false,
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
    /// The text is longer than [`Unit::MAX_BYTES`].
    Long {
        /// Its length in bytes.
        len: usize,
    },
    /// The text has whitespace or a control character.
    Character {
        /// The byte offset of the character.
        at: usize,
        /// The character.
        found: char,
    },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => write!(f, "a unit is empty"),
            Self::Long { len } => {
                write!(f, "a unit has {len} bytes, more than {}", Unit::MAX_BYTES)
            }
            Self::Character { at, found } => {
                write!(f, "a unit has the character {found:?} at byte {at}")
            }
        }
    }
}

impl std::error::Error for Error {}

#[cfg(test)]
mod tests;
