//! The byte layout of samples.

use std::fmt;
use std::str::FromStr;

/// A fixed-width sample type.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Scalar {
    /// One byte: 0 or 1.
    Bool,
    /// Signed 8-bit integer.
    I8,
    /// Signed 16-bit integer.
    I16,
    /// Signed 32-bit integer.
    I32,
    /// Signed 64-bit integer.
    I64,
    /// Unsigned 8-bit integer.
    U8,
    /// Unsigned 16-bit integer.
    U16,
    /// Unsigned 32-bit integer.
    U32,
    /// Unsigned 64-bit integer.
    U64,
    /// 32-bit float.
    F32,
    /// 64-bit float.
    F64,
    /// A [`crate::time::Stamp`].
    Stamp,
    /// A [`crate::time::Span`].
    Span,
    /// A 128-bit UUID.
    Uuid,
}

impl Scalar {
    /// Bytes per sample.
    #[must_use]
    pub const fn width(self) -> usize {
        match self {
            Self::Bool | Self::I8 | Self::U8 => 1,
            Self::I16 | Self::U16 => 2,
            Self::I32 | Self::U32 | Self::F32 => 4,
            Self::I64 | Self::U64 | Self::F64 | Self::Stamp | Self::Span => 8,
            Self::Uuid => 16,
        }
    }

    /// The name of the scalar in the text of a [`Type`].
    const fn name(self) -> &'static str {
        match self {
            Self::Bool => "bool",
            Self::I8 => "i8",
            Self::I16 => "i16",
            Self::I32 => "i32",
            Self::I64 => "i64",
            Self::U8 => "u8",
            Self::U16 => "u16",
            Self::U32 => "u32",
            Self::U64 => "u64",
            Self::F32 => "f32",
            Self::F64 => "f64",
            Self::Stamp => "timestamp",
            Self::Span => "duration",
            Self::Uuid => "uuid",
        }
    }

    /// The scalar that `text` names.
    fn named(text: &str) -> Option<Self> {
        SCALARS.into_iter().find(|scalar| scalar.name() == text)
    }
}

const SCALARS: [Scalar; 14] = [
    Scalar::Bool,
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
    Scalar::Stamp,
    Scalar::Span,
    Scalar::Uuid,
];

/// The byte layout of one channel's samples.
///
/// Enums and flags use an integer layout, and quality uses `U32`; their meaning is in
/// `spec`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Type {
    /// One fixed-width value per sample.
    Scalar(Scalar),
    /// A fixed array per sample, row-major for more than one dimension.
    Array {
        /// The element type.
        element: Scalar,
        /// Elements per sample.
        len: u32,
    },
    /// A list of at most `max` elements per sample.
    List {
        /// The element type.
        element: Scalar,
        /// The most elements one sample may hold.
        max: u32,
    },
    /// UTF-8 text per sample.
    String,
    /// Unlabeled bytes per sample.
    Bytes,
}

impl Type {
    /// Bytes per sample, or `None` when samples vary in size.
    #[must_use]
    pub const fn width(self) -> Option<usize> {
        match self {
            Self::Scalar(s) => Some(s.width()),
            Self::Array { element, len } => Some(element.width() * len as usize),
            Self::List { .. } | Self::String | Self::Bytes => None,
        }
    }
}

impl fmt::Display for Type {
    /// Writes the text that a user writes and [`Type::from_str`] reads: `f64`,
    /// `f32[3]`, `list<u8, 16>`, `string`, or `bytes`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Self::Scalar(scalar) => f.write_str(scalar.name()),
            Self::Array { element, len } => write!(f, "{}[{len}]", element.name()),
            Self::List { element, max } => {
                write!(f, "list<{}, {max}>", element.name())
            }
            Self::String => f.write_str("string"),
            Self::Bytes => f.write_str("bytes"),
        }
    }
}

impl FromStr for Type {
    type Err = Error;

    /// Reads the text that `Display` writes, and only that text: a scalar (`bool`,
    /// `i8` to `i64`, `u8` to `u64`, `f32`, `f64`, `timestamp`, `duration`, or `uuid`),
    /// an array `<scalar>[<len>]`, a list `list<<scalar>, <max>>`, `string`, or
    /// `bytes`. Case is exact, a count has no leading zero, and the one space is after
    /// the comma of a list.
    fn from_str(text: &str) -> Result<Self, Self::Err> {
        match text {
            "string" => return Ok(Self::String),
            "bytes" => return Ok(Self::Bytes),
            _ => {}
        }
        if let Some(list) = text
            .strip_prefix("list<")
            .and_then(|rest| rest.strip_suffix('>'))
        {
            let (name, max) = list.split_once(", ").ok_or(Error::Syntax)?;
            return Ok(Self::List {
                element: element(name)?,
                max: count(max)?,
            });
        }
        if let Some(array) = text.strip_suffix(']') {
            let (name, len) = array.split_once('[').ok_or(Error::Syntax)?;
            let element = element(name)?;
            if len.contains("][") {
                return Err(Error::Lengths);
            }
            return Ok(Self::Array {
                element,
                len: count(len)?,
            });
        }
        Scalar::named(text).map(Self::Scalar).ok_or(Error::Syntax)
    }
}

/// The scalar that `text` names as the element of an array or a list. A space is a
/// fault of syntax, not of the element.
fn element(text: &str) -> Result<Scalar, Error> {
    if text.contains(char::is_whitespace) {
        return Err(Error::Syntax);
    }
    Scalar::named(text).ok_or(Error::Element)
}

/// The count that `text` writes in ASCII digits with no leading zero.
fn count(text: &str) -> Result<u32, Error> {
    let digits = text.bytes().all(|byte| byte.is_ascii_digit());
    if !digits || (text.len() > 1 && text.starts_with('0')) {
        return Err(Error::Count);
    }
    text.parse().map_err(|_too_large| Error::Count)
}

/// Why a text is not a sample type. `Display` gives the message: a lower-case clause
/// with no final period. [`Error::fix`] gives what to do instead.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// The text has none of the forms of a type. `F64`, `float`, `f32[3`, and
    /// `list<u8,16>` give this error.
    Syntax,
    /// The element of an array or a list is not a scalar, as in `string[3]` or
    /// `list<f32[2], 4>`.
    Element,
    /// A length or a maximum is not ASCII digits with no leading zero that fit in a
    /// `u32`, as in `f32[]`, `f32[03]`, or `f32[-1]`.
    Count,
    /// An array has more than one length, as in `f32[2][3]`.
    Lengths,
}

impl Error {
    /// What to do instead: a sentence with no final period.
    #[must_use]
    pub const fn fix(self) -> &'static str {
        match self {
            Self::Syntax => {
                "Use one of the forms that the message names, with exact case and a \
                 space only after the comma of a list"
            }
            Self::Element => {
                "Use a scalar as the element, such as f32[3] or list<u8, 16>"
            }
            Self::Count => "Write the count in plain digits, such as 16",
            Self::Lengths => {
                "Use one array length: a type of two lengths does not read yet"
            }
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Syntax => {
                "expected a sample type such as f64, f32[3], list<u8, 16>, string, or \
                 bytes"
            }
            Self::Element => {
                f.write_str("expected a scalar element: ")?;
                for (index, scalar) in SCALARS.iter().enumerate() {
                    let gap = match index {
                        0 => "",
                        _ if index + 1 == SCALARS.len() => ", or ",
                        _ => ", ",
                    };
                    write!(f, "{gap}{}", scalar.name())?;
                }
                return Ok(());
            }
            Self::Count => "expected a count of 0 to 4294967295, with no leading zero",
            Self::Lengths => "expected one array length",
        })
    }
}

impl std::error::Error for Error {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::assert_stated;
    use proptest::prelude::*;

    #[test]
    fn holds_each_scalar_in_order() {
        for (index, scalar) in SCALARS.into_iter().enumerate() {
            assert_eq!(scalar as usize, index, "{scalar:?}");
        }
        assert_eq!(Scalar::Uuid as usize + 1, SCALARS.len());
    }

    fn types() -> impl Strategy<Value = Type> {
        let scalar = prop::sample::select(SCALARS.as_slice());
        prop_oneof![
            scalar.clone().prop_map(Type::Scalar),
            (scalar.clone(), any::<u32>())
                .prop_map(|(element, len)| Type::Array { element, len }),
            (scalar, any::<u32>())
                .prop_map(|(element, max)| Type::List { element, max }),
            Just(Type::String),
            Just(Type::Bytes),
        ]
    }

    #[test]
    fn writes_and_reads_each_form() {
        let cases = [
            (Type::Scalar(Scalar::F64), "f64"),
            (
                Type::Array {
                    element: Scalar::F32,
                    len: 3,
                },
                "f32[3]",
            ),
            (
                Type::Array {
                    element: Scalar::Uuid,
                    len: 0,
                },
                "uuid[0]",
            ),
            (
                Type::List {
                    element: Scalar::U8,
                    max: u32::MAX,
                },
                "list<u8, 4294967295>",
            ),
            (Type::String, "string"),
            (Type::Bytes, "bytes"),
        ];
        for (sample, text) in cases {
            assert_eq!(sample.to_string(), text);
            assert_eq!(text.parse(), Ok(sample), "{text}");
        }
        let names: Vec<_> = SCALARS
            .map(|scalar| Type::Scalar(scalar).to_string())
            .into();
        let expected = [
            "bool",
            "i8",
            "i16",
            "i32",
            "i64",
            "u8",
            "u16",
            "u32",
            "u64",
            "f32",
            "f64",
            "timestamp",
            "duration",
            "uuid",
        ];
        assert_eq!(names, expected);
    }

    #[test]
    fn refuses_each_other_text() {
        let cases = [
            ("", Error::Syntax),
            ("F64", Error::Syntax),
            ("float", Error::Syntax),
            (" f64", Error::Syntax),
            ("f32[3", Error::Syntax),
            ("f32 [3]", Error::Syntax),
            (" f32[3]", Error::Syntax),
            ("list<u8 , 16>", Error::Syntax),
            ("stamp", Error::Syntax),
            ("span", Error::Syntax),
            ("stamp[3]", Error::Element),
            ("list<span, 16>", Error::Element),
            ("f32 [2][3]", Error::Syntax),
            ("f32 [03]", Error::Syntax),
            ("list<u8,16>", Error::Syntax),
            ("list<u8, 16> ", Error::Syntax),
            ("String", Error::Syntax),
            ("string[3]", Error::Element),
            ("[3]", Error::Element),
            ("list<f32[2], 4>", Error::Element),
            ("list<, 4>", Error::Element),
            ("f32[]", Error::Count),
            ("f32[03]", Error::Count),
            ("f32[ 3]", Error::Count),
            ("f32[-1]", Error::Count),
            ("f32[+1]", Error::Count),
            ("f32[4294967296]", Error::Count),
            ("list<u8, 016>", Error::Count),
            ("list<u8,  16>", Error::Count),
        ];
        for (text, error) in cases {
            assert_eq!(text.parse::<Type>(), Err(error), "{text:?}");
        }
    }

    /// #1341 reads these as arrays of arrays.
    #[test]
    fn refuses_two_lengths() {
        for text in ["f32[2][3]", "u8[1][1][1]"] {
            assert_eq!(text.parse::<Type>(), Err(Error::Lengths), "{text}");
        }
    }

    #[test]
    fn states_each_error() {
        let cases = [
            (
                Error::Syntax,
                "expected a sample type such as f64, f32[3], list<u8, 16>, string, or \
                 bytes",
                "Use one of the forms that the message names, with exact case and a \
                 space only after the comma of a list",
            ),
            (
                Error::Element,
                "expected a scalar element: bool, i8, i16, i32, i64, u8, u16, u32, \
                 u64, f32, f64, timestamp, duration, or uuid",
                "Use a scalar as the element, such as f32[3] or list<u8, 16>",
            ),
            (
                Error::Count,
                "expected a count of 0 to 4294967295, with no leading zero",
                "Write the count in plain digits, such as 16",
            ),
            (
                Error::Lengths,
                "expected one array length",
                "Use one array length: a type of two lengths does not read yet",
            ),
        ];
        for (error, message, fix) in cases {
            assert_eq!(error.to_string(), message);
            assert_eq!(error.fix(), fix);
            assert_stated(message, fix);
        }
    }

    proptest! {
        #[test]
        fn reads_back_the_text_of_each_type(sample in types()) {
            prop_assert_eq!(sample.to_string().parse(), Ok(sample));
        }

        #[test]
        fn shows_a_text_that_reads_as_that_text(
            text in "(list<)?[a-z]{0,2}[0-9]{0,2}(\\[|, )?[0-9]{0,3}(\\]|>)?"
        ) {
            if let Ok(sample) = text.parse::<Type>() {
                prop_assert_eq!(sample.to_string(), text);
            }
        }
    }
}
