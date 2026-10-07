//! The byte layout of samples.

use std::fmt;
use std::str::FromStr;

/// Defines [`Scalar`], its text, and `SCALARS` from one list, so no scalar can miss its
/// text or its place in `SCALARS`.
macro_rules! scalars {
    ($($(#[$doc:meta])* $variant:ident = $name:literal,)*) => {
        /// A fixed-width sample type.
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
        pub enum Scalar {
            $($(#[$doc])* $variant,)*
        }

        impl Scalar {
            /// The name of the scalar in the text of a [`Type`].
            const fn name(self) -> &'static str {
                match self {
                    $(Self::$variant => $name,)*
                }
            }
        }

        /// Each scalar, in the order of its declaration.
        const SCALARS: &[Scalar] = &[$(Scalar::$variant,)*];
    };
}

scalars! {
    /// One byte: 0 or 1.
    Bool = "bool",
    /// Signed 8-bit integer.
    I8 = "i8",
    /// Signed 16-bit integer.
    I16 = "i16",
    /// Signed 32-bit integer.
    I32 = "i32",
    /// Signed 64-bit integer.
    I64 = "i64",
    /// Unsigned 8-bit integer.
    U8 = "u8",
    /// Unsigned 16-bit integer.
    U16 = "u16",
    /// Unsigned 32-bit integer.
    U32 = "u32",
    /// Unsigned 64-bit integer.
    U64 = "u64",
    /// 32-bit float.
    F32 = "f32",
    /// 64-bit float.
    F64 = "f64",
    /// A [`crate::time::Stamp`].
    Stamp = "timestamp",
    /// A [`crate::time::Span`].
    Span = "duration",
    /// A 128-bit UUID.
    Uuid = "uuid",
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

    /// The scalar that `text` names.
    fn named(text: &str) -> Option<Self> {
        SCALARS.iter().copied().find(|scalar| scalar.name() == text)
    }
}

/// The byte layout of one channel's samples.
///
/// Enums and flags use an integer layout, and quality uses `U32`; their meaning is in
/// `spec`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Type {
    /// One fixed-width value per sample.
    Scalar(Scalar),
    /// A fixed array per sample.
    Array {
        /// The element type.
        element: Scalar,
        /// Elements per sample.
        len: u32,
    },
    /// A fixed array of arrays per sample, row-major. A sample has the bytes of an
    /// array of `rows * columns` elements; the shape is only in the type.
    Matrix {
        /// The element type.
        element: Scalar,
        /// Arrays per sample.
        rows: u16,
        /// Elements per array.
        columns: u16,
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
            Self::Matrix {
                element,
                rows,
                columns,
            } => Some(element.width() * rows as usize * columns as usize),
            Self::List { .. } | Self::String | Self::Bytes => None,
        }
    }
}

impl fmt::Display for Type {
    /// Writes the text that a user writes and [`Type::from_str`] reads: `f64`,
    /// `f32[3]`, `f32[2][3]`, `list<u8, 16>`, `string`, or `bytes`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Self::Scalar(scalar) => f.write_str(scalar.name()),
            Self::Array { element, len } => write!(f, "{}[{len}]", element.name()),
            Self::Matrix {
                element,
                rows,
                columns,
            } => write!(f, "{}[{rows}][{columns}]", element.name()),
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
    /// an array `<scalar>[<len>]`, a matrix `<scalar>[<rows>][<columns>]`, a list
    /// `list<<scalar>, <max>>`, `string`, or `bytes`. Case is exact, a count has no
    /// leading zero, and the one space is after the comma of a list.
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
            let mut lengths = len.split("][");
            let (Some(len), columns, None) =
                (lengths.next(), lengths.next(), lengths.next())
            else {
                return Err(Error::Lengths);
            };
            return match columns {
                None => Ok(Self::Array {
                    element,
                    len: count(len)?,
                }),
                Some(columns) => Ok(Self::Matrix {
                    element,
                    rows: side(len)?,
                    columns: side(columns)?,
                }),
            };
        }
        Scalar::named(text).map(Self::Scalar).ok_or(Error::Syntax)
    }
}

/// The scalar that `text` names as the element of an array or a list. A scalar with
/// space around it is a fault of syntax, not of the element.
fn element(text: &str) -> Result<Scalar, Error> {
    match Scalar::named(text) {
        Some(scalar) => Ok(scalar),
        None if Scalar::named(text.trim()).is_some() => Err(Error::Syntax),
        None => Err(Error::Element),
    }
}

/// The count that `text` writes in ASCII digits with no leading zero.
fn count(text: &str) -> Result<u32, Error> {
    digits(text)?.parse().map_err(|_over_u32| Error::Count)
}

/// The length of a matrix side that `text` writes as a [`count`]. A count over 65535,
/// of any size, is [`Error::Matrix`].
fn side(text: &str) -> Result<u16, Error> {
    digits(text)?.parse().map_err(|_over_u16| Error::Matrix)
}

/// `text` when it is one or more ASCII digits with no leading zero.
fn digits(text: &str) -> Result<&str, Error> {
    let digits = !text.is_empty() && text.bytes().all(|byte| byte.is_ascii_digit());
    if !digits || (text.len() > 1 && text.starts_with('0')) {
        return Err(Error::Count);
    }
    Ok(text)
}

/// Why a text is not a sample type. `Display` gives the message: a lower-case clause
/// with no final period. [`Error::fix`] gives what to do instead.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// The text has none of the forms of a type. `F64`, `float`, `f32[3`,
    /// `list<u8,16>`, and a scalar element with space around it, as in `f32 [3]`, give
    /// this error.
    Syntax,
    /// The element of an array or a list is not a scalar, as in `string[3]` or
    /// `list<f32[2], 4>`.
    Element,
    /// A length or a maximum is not ASCII digits with no leading zero that fit in a
    /// `u32`, as in `f32[]`, `f32[03]`, or `f32[-1]`.
    Count,
    /// An array has more than two lengths, as in `u8[1][1][1]`.
    Lengths,
    /// A matrix has a length over 65535, as in `f32[70000][3]`.
    Matrix,
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
                "Use one or two array lengths, such as f32[3] or f32[2][3]"
            }
            Self::Matrix => "Use at most 65535 rows and 65535 columns, or an array",
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
            Self::Lengths => "expected one or two array lengths",
            Self::Matrix => "expected each matrix length from 0 to 65535",
        })
    }
}

impl std::error::Error for Error {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::assert_stated;
    use proptest::prelude::*;

    fn types() -> impl Strategy<Value = Type> {
        let scalar = prop::sample::select(SCALARS);
        prop_oneof![
            scalar.clone().prop_map(Type::Scalar),
            (scalar.clone(), any::<u32>())
                .prop_map(|(element, len)| Type::Array { element, len }),
            (scalar.clone(), any::<u16>(), any::<u16>()).prop_map(
                |(element, rows, columns)| Type::Matrix {
                    element,
                    rows,
                    columns,
                }
            ),
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
            (matrix(Scalar::F32, 2, 3), "f32[2][3]"),
            (matrix(Scalar::U8, 0, 5), "u8[0][5]"),
            (matrix(Scalar::F32, 1, 3), "f32[1][3]"),
            (matrix(Scalar::U8, 65535, 65535), "u8[65535][65535]"),
            (matrix(Scalar::U8, 0, 0), "u8[0][0]"),
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
            .iter()
            .map(|&scalar| Type::Scalar(scalar).to_string())
            .collect();
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
            ("list<u8, 16>[2]", Error::Element),
            ("list<u8, 16>[2][3]", Error::Element),
            ("list<list<u8, 16>, 4>", Error::Element),
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
            ("f32[2][]", Error::Count),
            ("f32[2][03]", Error::Count),
            ("f32[2]][3]", Error::Count),
            ("f32[2] [3]", Error::Count),
            ("f32[2][4294967296]", Error::Matrix),
            ("f32[2][3", Error::Syntax),
            ("f32[2][3][", Error::Syntax),
            ("string[2][3]", Error::Element),
            ("u8[1][1][1]", Error::Lengths),
            ("u8[1][1][]", Error::Lengths),
            ("f32[70000][3]", Error::Matrix),
            ("u8[65535][65536]", Error::Matrix),
            ("u8[65536][0]", Error::Matrix),
            ("u8[4294967295][2]", Error::Matrix),
            ("u8[2][4294967296]", Error::Matrix),
            ("u8[99999999999999999999][1]", Error::Matrix),
        ];
        for (text, error) in cases {
            assert_eq!(text.parse::<Type>(), Err(error), "{text:?}");
        }
    }

    fn matrix(element: Scalar, rows: u16, columns: u16) -> Type {
        Type::Matrix {
            element,
            rows,
            columns,
        }
    }

    /// The #1152 test of `f32[2][3]`: it reads as an array of arrays.
    #[test]
    fn reads_two_lengths() {
        let read = "f32[2][3]".parse::<Type>();
        assert_eq!(read, Ok(matrix(Scalar::F32, 2, 3)));
        assert_eq!(read.unwrap().width(), Some(24));
    }

    #[test]
    fn gives_the_width_of_the_largest_matrix() {
        let max = matrix(Scalar::Uuid, u16::MAX, u16::MAX);
        assert_eq!(max.width(), Some(16 * 65_535 * 65_535));
        assert_eq!(matrix(Scalar::U8, 0, u16::MAX).width(), Some(0));
    }

    #[test]
    fn stays_eight_bytes() {
        assert_eq!(size_of::<Type>(), 8);
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
                "expected one or two array lengths",
                "Use one or two array lengths, such as f32[3] or f32[2][3]",
            ),
            (
                Error::Matrix,
                "expected each matrix length from 0 to 65535",
                "Use at most 65535 rows and 65535 columns, or an array",
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
            text in "(list<)?[a-z]{0,2}[0-9]{0,2}(\\[|, )?[0-9]{0,3}(\\]|>)?\
                     (\\[[0-9]{0,3}\\]?)?"
        ) {
            if let Ok(sample) = text.parse::<Type>() {
                prop_assert_eq!(sample.to_string(), text);
            }
        }
    }
}
