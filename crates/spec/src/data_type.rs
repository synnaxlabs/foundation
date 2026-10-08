//! The data type of a data channel: a sample type, or quality.

use std::fmt;
use std::str::FromStr;

use types::sample::{self, Scalar};

/// What the values of a data channel are.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum DataType {
    /// Values with this byte layout.
    Sample(sample::Type),
    /// OPC UA 32-bit status codes, which data channels point at with `quality`.
    Quality,
}

impl DataType {
    /// The byte layout of one value.
    #[must_use]
    pub const fn sample(&self) -> sample::Type {
        match self {
            Self::Sample(sample) => *sample,
            Self::Quality => sample::Type::Scalar(Scalar::U32),
        }
    }

    /// Reports whether the values are numbers, so they can have a unit.
    pub(crate) const fn numeric(&self) -> bool {
        let element = match self {
            Self::Sample(
                sample::Type::Scalar(element)
                | sample::Type::Array { element, .. }
                | sample::Type::Matrix { element, .. }
                | sample::Type::List { element, .. },
            ) => *element,
            Self::Sample(sample::Type::String | sample::Type::Bytes)
            | Self::Quality => {
                return false;
            }
        };
        !matches!(
            element,
            Scalar::Bool | Scalar::Stamp | Scalar::Span | Scalar::Uuid
        )
    }
}

impl fmt::Display for DataType {
    /// Writes the text that [`DataType::from_str`] reads: `quality`, or the text of the
    /// sample type.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Sample(sample) => sample.fmt(f),
            Self::Quality => f.write_str("quality"),
        }
    }
}

impl FromStr for DataType {
    type Err = Error;

    /// Reads `quality`, or the text of a [`sample::Type`] with its exact form.
    ///
    /// # Errors
    ///
    /// [`Error`] when `text` is neither.
    fn from_str(text: &str) -> Result<Self, Self::Err> {
        match text {
            "quality" => Ok(Self::Quality),
            _ => text.parse().map(Self::Sample).map_err(Error),
        }
    }
}

/// The data types that the message of a text with no form shows.
const EXAMPLES: [&str; 6] = [
    "f64",
    "f32[3]",
    "list<u8, 16>",
    "string",
    "bytes",
    "quality",
];

/// A text that names no data type, for the reason that the sample type error gives.
/// `Display` gives the message: a lower-case clause with no final period.
/// [`Error::fix`] gives what to do instead.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Error(sample::Error);

impl Error {
    /// What to do instead: a sentence with no final period.
    #[must_use]
    pub const fn fix(&self) -> &'static str {
        self.0.fix()
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            sample::Error::Syntax => {
                f.write_str("expected a data type such as ")?;
                let (last, rest) = EXAMPLES.split_last().expect("invariant: examples");
                for example in rest {
                    write!(f, "{example}, ")?;
                }
                write!(f, "or {last}")
            }
            ref error => error.fmt(f),
        }
    }
}

impl std::error::Error for Error {}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) const NUMBERS: [Scalar; 10] = [
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
    pub(crate) const OTHERS: [Scalar; 4] =
        [Scalar::Bool, Scalar::Stamp, Scalar::Span, Scalar::Uuid];

    pub(crate) fn shapes(element: Scalar) -> [DataType; 4] {
        [
            DataType::Sample(sample::Type::Scalar(element)),
            DataType::Sample(sample::Type::Array { element, len: 3 }),
            DataType::Sample(sample::Type::Matrix {
                element,
                sides: sample::Sides {
                    rows: 2,
                    columns: 3,
                },
            }),
            DataType::Sample(sample::Type::List { element, max: 3 }),
        ]
    }

    #[test]
    fn stores_quality_as_u32() {
        assert_eq!(
            DataType::Quality.sample(),
            sample::Type::Scalar(Scalar::U32)
        );
        let string = sample::Type::String;
        assert_eq!(DataType::Sample(string).sample(), string);
    }

    #[test]
    fn reads_the_text_that_it_writes() {
        let data_types = NUMBERS.into_iter().chain(OTHERS).flat_map(shapes).chain([
            DataType::Sample(sample::Type::String),
            DataType::Sample(sample::Type::Bytes),
            DataType::Quality,
        ]);
        for data_type in data_types {
            assert_eq!(data_type.to_string().parse(), Ok(data_type));
        }
    }

    #[test]
    fn writes_quality_as_no_sample_type_writes() {
        assert_eq!(
            "quality".parse::<sample::Type>(),
            Err(sample::Error::Syntax)
        );
    }

    #[test]
    fn shows_examples_that_read() {
        for example in EXAMPLES {
            let data_type = example.parse::<DataType>().unwrap();
            assert_eq!(data_type.to_string(), example);
        }
    }

    #[test]
    fn reads_quality_and_each_form_of_sample_type() {
        let matrix = sample::Type::Matrix {
            element: Scalar::F32,
            sides: sample::Sides {
                rows: 2,
                columns: 3,
            },
        };
        for (text, data_type) in [
            ("quality", DataType::Quality),
            ("f64", DataType::Sample(sample::Type::Scalar(Scalar::F64))),
            ("f32[2][3]", DataType::Sample(matrix)),
            ("string", DataType::Sample(sample::Type::String)),
        ] {
            assert_eq!(text.parse(), Ok(data_type.clone()));
            assert_eq!(data_type.to_string(), text);
        }
    }

    #[test]
    fn names_quality_in_the_message_of_text_with_no_form() {
        for text in ["Quality", "quality ", "F64", "vector", "list<f32,64>", ""] {
            let error = text.parse::<DataType>().unwrap_err();
            assert_eq!(error, Error(sample::Error::Syntax), "{text:?}");
            assert_eq!(
                error.to_string(),
                "expected a data type such as f64, f32[3], list<u8, 16>, string, \
                 bytes, or quality"
            );
            assert_eq!(error.fix(), sample::Error::Syntax.fix());
        }
    }

    #[test]
    fn gives_the_sample_message_and_fix_of_each_other_cause() {
        for (text, cause) in [
            ("u8[]", sample::Error::Count),
            ("u8[4294967296]", sample::Error::Count),
            ("list<quality, 4>", sample::Error::Element),
            ("u8[1][1][1]", sample::Error::Lengths),
            ("f32[70000][3]", sample::Error::Matrix),
        ] {
            let error = text.parse::<DataType>().unwrap_err();
            assert_eq!(error, Error(cause), "{text:?}");
            assert_eq!(error.to_string(), cause.to_string());
            assert_eq!(error.fix(), cause.fix());
        }
    }
}
