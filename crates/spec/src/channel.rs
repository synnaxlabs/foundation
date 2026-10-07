//! The channel definition: an index channel, or a data channel on an index.

use std::fmt;
use std::str::FromStr;

use types::channel;
use types::sample::{self, Scalar};

use crate::unit::Unit;

mod check;

pub use check::{Edge, Problem, check};

/// A channel. Its name is the tree key, so it is not part of the definition.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Channel {
    /// The key: made with the channel, never reused, and never changed.
    pub key: channel::Key,
    /// An index channel or a data channel.
    pub kind: Kind,
}

/// An index channel or a data channel.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Kind {
    /// An index channel: the time base of the data channels that point at it.
    Index {
        /// The channel that holds the clock error bound of its timestamps.
        error: Option<channel::Key>,
        /// The channel that the home writes control handoffs to.
        control: Option<channel::Key>,
    },
    /// A data channel.
    Data(Data),
}

/// A data channel: what its values are, their unit, and the channels it points at.
#[derive(Clone, Debug, PartialEq, Eq)]
#[expect(clippy::struct_field_names, reason = "the data type of a data channel")]
pub struct Data {
    index: channel::Key,
    quality: Option<channel::Key>,
    data_type: DataType,
    unit: Option<Unit>,
}

impl Data {
    /// Makes a data channel on the index channel `index`, with the quality channel
    /// `quality`.
    ///
    /// # Errors
    ///
    /// [`Error::Unit`] when `unit` is set and `data_type` holds no number.
    pub fn new(
        index: channel::Key,
        quality: Option<channel::Key>,
        data_type: DataType,
        unit: Option<Unit>,
    ) -> Result<Self, Error> {
        if unit.is_some() && !data_type.numeric() {
            return Err(Error::Unit { data_type });
        }
        Ok(Self {
            index,
            quality,
            data_type,
            unit,
        })
    }

    /// The index channel that times the values.
    #[must_use]
    pub const fn index(&self) -> channel::Key {
        self.index
    }

    /// The channel that holds the quality of the values, if any.
    #[must_use]
    pub const fn quality(&self) -> Option<channel::Key> {
        self.quality
    }

    /// What the values are.
    #[must_use]
    pub const fn data_type(&self) -> &DataType {
        &self.data_type
    }

    /// The unit of the values, if any. Only a numeric data type has one.
    #[must_use]
    pub const fn unit(&self) -> Option<&Unit> {
        self.unit.as_ref()
    }
}

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
    const fn numeric(&self) -> bool {
        let element = match self {
            Self::Sample(
                sample::Type::Scalar(element)
                | sample::Type::Array { element, .. }
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

/// Each scalar, so that a text finds the scalar it names.
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

impl FromStr for DataType {
    type Err = Error;

    /// Reads a data type: a scalar (`bool`, `i8` to `i64`, `u8` to `u64`, `f32`,
    /// `f64`, `stamp`, `span`, `uuid`), an array `<scalar>[<len>]`, a list
    /// `list<<scalar>, <max>>`, `string`, `bytes`, or `quality`. The text has exact
    /// case and no other space.
    ///
    /// # Errors
    ///
    /// [`Error::Type`] when `text` is no data type.
    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let sample = match text {
            "string" => sample::Type::String,
            "bytes" => sample::Type::Bytes,
            "quality" => return Ok(Self::Quality),
            _ => {
                if let Some(list) = text
                    .strip_prefix("list<")
                    .and_then(|rest| rest.strip_suffix('>'))
                {
                    let (element, max) = list.split_once(", ").ok_or(Error::Type)?;
                    sample::Type::List {
                        element: scalar(element)?,
                        max: count(max)?,
                    }
                } else if let Some(array) = text.strip_suffix(']') {
                    let (element, len) = array.split_once('[').ok_or(Error::Type)?;
                    sample::Type::Array {
                        element: scalar(element)?,
                        len: count(len)?,
                    }
                } else {
                    sample::Type::Scalar(scalar(text)?)
                }
            }
        };
        Ok(Self::Sample(sample))
    }
}

/// The scalar that `text` names.
fn scalar(text: &str) -> Result<Scalar, Error> {
    SCALARS
        .into_iter()
        .find(|scalar| name(*scalar) == text)
        .ok_or(Error::Type)
}

/// The text that names `scalar` in a `data_type`.
const fn name(scalar: Scalar) -> &'static str {
    match scalar {
        Scalar::Bool => "bool",
        Scalar::I8 => "i8",
        Scalar::I16 => "i16",
        Scalar::I32 => "i32",
        Scalar::I64 => "i64",
        Scalar::U8 => "u8",
        Scalar::U16 => "u16",
        Scalar::U32 => "u32",
        Scalar::U64 => "u64",
        Scalar::F32 => "f32",
        Scalar::F64 => "f64",
        Scalar::Stamp => "stamp",
        Scalar::Span => "span",
        Scalar::Uuid => "uuid",
    }
}

/// The count that `text` writes in ASCII digits.
fn count(text: &str) -> Result<u32, Error> {
    if text.is_empty() || !text.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(Error::Type);
    }
    text.parse().map_err(|_too_large| Error::Type)
}

impl fmt::Display for DataType {
    /// Writes the text that [`DataType::from_str`] reads.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Sample(sample::Type::Scalar(element)) => f.write_str(name(*element)),
            Self::Sample(sample::Type::Array { element, len }) => {
                write!(f, "{}[{len}]", name(*element))
            }
            Self::Sample(sample::Type::List { element, max }) => {
                write!(f, "list<{}, {max}>", name(*element))
            }
            Self::Sample(sample::Type::String) => f.write_str("string"),
            Self::Sample(sample::Type::Bytes) => f.write_str("bytes"),
            Self::Quality => f.write_str("quality"),
        }
    }
}

/// A data channel that cannot exist. `Display` gives the message: a lower-case clause
/// with no final period. [`Error::fix`] gives what to do instead.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Error {
    /// A unit is on a data type that holds no number: a bool, a stamp, a span, a UUID,
    /// a string, bytes, or quality.
    Unit {
        /// The data type.
        data_type: DataType,
    },
    /// A `data_type` text that is no data type.
    Type,
}

impl Error {
    /// What to do instead: a sentence with no final period.
    #[must_use]
    pub const fn fix(&self) -> &'static str {
        match self {
            Self::Unit { .. } => "Remove the unit, or give the channel a numeric type",
            Self::Type => {
                "Use a scalar such as `f64`, an array such as `u8[16]`, a list such as \
                 `list<f32, 64>`, `string`, `bytes`, or `quality`"
            }
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unit { .. } => {
                f.write_str("a unit is on a data type that holds no number")
            }
            Self::Type => f.write_str(
                "expected a data type such as f64, u8[16], list<f32, 64>, string, \
                 bytes, or quality",
            ),
        }
    }
}

impl std::error::Error for Error {}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    const NUMBERS: [Scalar; 10] = [
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
    const OTHERS: [Scalar; 4] =
        [Scalar::Bool, Scalar::Stamp, Scalar::Span, Scalar::Uuid];

    fn shapes(element: Scalar) -> [DataType; 3] {
        [
            DataType::Sample(sample::Type::Scalar(element)),
            DataType::Sample(sample::Type::Array { element, len: 3 }),
            DataType::Sample(sample::Type::List { element, max: 3 }),
        ]
    }

    fn data(data_type: DataType, unit: Option<&str>) -> Result<Data, Error> {
        let unit = unit.map(|text| Unit::new(text).unwrap());
        Data::new(channel::Key::from_u128(1), None, data_type, unit)
    }

    #[test]
    fn gives_a_unit_to_each_numeric_type() {
        for data_type in NUMBERS.into_iter().flat_map(shapes) {
            let data = data(data_type.clone(), Some("kPa")).unwrap();
            assert_eq!(data.unit().map(Unit::as_str), Some("kPa"));
            assert_eq!(data.data_type(), &data_type);
        }
    }

    #[test]
    fn refuses_a_unit_on_a_type_that_holds_no_number() {
        let others = OTHERS.into_iter().flat_map(shapes).chain([
            DataType::Sample(sample::Type::String),
            DataType::Sample(sample::Type::Bytes),
            DataType::Quality,
        ]);
        for data_type in others {
            data(data_type.clone(), None).unwrap();
            assert_eq!(
                data(data_type.clone(), Some("kPa")),
                Err(Error::Unit { data_type })
            );
        }
        let error = Error::Unit {
            data_type: DataType::Quality,
        };
        assert_eq!(
            error.to_string(),
            "a unit is on a data type that holds no number"
        );
        assert_eq!(
            error.fix(),
            "Remove the unit, or give the channel a numeric type"
        );
    }

    #[test]
    fn accepts_an_array_or_list_that_holds_no_element() {
        for element in [Scalar::F64, Scalar::Bool] {
            for data_type in [
                sample::Type::Array { element, len: 0 },
                sample::Type::List { element, max: 0 },
            ]
            .map(DataType::Sample)
            {
                let unit = (element == Scalar::F64).then_some("kPa");
                let data = data(data_type.clone(), unit).unwrap();
                assert_eq!(data.data_type(), &data_type);
            }
        }
    }

    #[test]
    fn reads_the_text_of_each_data_type() {
        let sample = DataType::Sample;
        for element in SCALARS {
            let text = name(element);
            assert_eq!(text.parse(), Ok(sample(sample::Type::Scalar(element))));
            assert_eq!(
                format!("{text}[3]").parse(),
                Ok(sample(sample::Type::Array { element, len: 3 }))
            );
            assert_eq!(
                format!("list<{text}, 64>").parse(),
                Ok(sample(sample::Type::List { element, max: 64 }))
            );
        }
        let element = Scalar::U8;
        for (text, data_type) in [
            ("string", sample(sample::Type::String)),
            ("bytes", sample(sample::Type::Bytes)),
            ("quality", DataType::Quality),
            ("u8[0]", sample(sample::Type::Array { element, len: 0 })),
            (
                "list<u8, 4294967295>",
                sample(sample::Type::List {
                    element,
                    max: u32::MAX,
                }),
            ),
        ] {
            assert_eq!(text.parse(), Ok(data_type), "{text:?}");
        }
    }

    #[test]
    fn names_each_stored_scalar() {
        let stored: Vec<Scalar> = (0..=u8::MAX)
            .filter_map(crate::definition::scalar)
            .collect();
        assert_eq!(stored, SCALARS);
    }

    #[test]
    fn refuses_a_text_that_is_no_data_type() {
        for text in [
            "",
            "F64",
            " f64",
            "f64 ",
            "vector",
            "u8[]",
            "u8[-1]",
            "u8[+1]",
            "u8[ 1]",
            "u8[4294967296]",
            "u8[1",
            "f32[2][3]",
            "string[2]",
            "quality[2]",
            "list<f32,64>",
            "list<f32 , 64>",
            "list<f32, 64",
            "list<f32>",
            "list<string, 4>",
            "list<f32[2], 4>",
            "list<list<u8, 2>, 2>",
        ] {
            assert_eq!(text.parse::<DataType>(), Err(Error::Type), "{text:?}");
        }
        assert_eq!(
            Error::Type.to_string(),
            "expected a data type such as f64, u8[16], list<f32, 64>, string, bytes, \
             or quality"
        );
        assert_eq!(
            Error::Type.fix(),
            "Use a scalar such as `f64`, an array such as `u8[16]`, a list such as \
             `list<f32, 64>`, `string`, `bytes`, or `quality`"
        );
    }

    fn data_types() -> impl Strategy<Value = DataType> {
        let element = prop::sample::select(SCALARS.to_vec());
        let sample = |data_type| DataType::Sample(data_type);
        prop_oneof![
            element
                .clone()
                .prop_map(move |element| sample(sample::Type::Scalar(element))),
            (element.clone(), any::<u32>()).prop_map(move |(element, len)| {
                sample(sample::Type::Array { element, len })
            }),
            (element, any::<u32>()).prop_map(move |(element, max)| {
                sample(sample::Type::List { element, max })
            }),
            Just(sample(sample::Type::String)),
            Just(sample(sample::Type::Bytes)),
            Just(DataType::Quality),
        ]
    }

    proptest! {
        #[test]
        fn reads_the_text_that_it_writes(data_type in data_types()) {
            prop_assert_eq!(data_type.to_string().parse(), Ok(data_type));
        }
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
}
