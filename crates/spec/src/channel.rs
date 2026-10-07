//! The channel definition: an index channel, or a data channel on an index.

use std::fmt;

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

/// An index channel or a data channel. `R` is how an edge names the channel that it
/// points at: a key in the spec, a name in a file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Kind<R = channel::Key> {
    /// An index channel: the time base of the data channels that point at it.
    Index {
        /// The channel that holds the clock error bound of its timestamps.
        error: Option<R>,
        /// The channel that the home writes control handoffs to.
        control: Option<R>,
    },
    /// A data channel.
    Data(Data<R>),
}

/// A data channel: what its values are, their unit, and the channels it points at.
/// `R` is how an edge names the channel that it points at.
#[derive(Clone, Debug, PartialEq, Eq)]
#[expect(clippy::struct_field_names, reason = "the data type of a data channel")]
pub struct Data<R = channel::Key> {
    index: R,
    quality: Option<R>,
    data_type: DataType,
    unit: Option<Unit>,
}

impl<R> Data<R> {
    /// Makes a data channel on the index channel `index`, with the quality channel
    /// `quality`.
    ///
    /// # Errors
    ///
    /// [`Error::Unit`] when `unit` is set and `data_type` holds no number.
    pub fn new(
        index: R,
        quality: Option<R>,
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
    pub const fn index(&self) -> &R {
        &self.index
    }

    /// The channel that holds the quality of the values, if any.
    #[must_use]
    pub const fn quality(&self) -> Option<&R> {
        self.quality.as_ref()
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
}

impl Error {
    /// What to do instead: a sentence with no final period.
    #[must_use]
    pub const fn fix(&self) -> &'static str {
        match self {
            Self::Unit { .. } => "Remove the unit, or give the channel a numeric type",
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unit { .. } => {
                f.write_str("a unit is on a data type that holds no number")
            }
        }
    }
}

impl std::error::Error for Error {}

#[cfg(test)]
mod tests {
    use types::name::Name;

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

    fn shapes(element: Scalar) -> [DataType; 4] {
        [
            DataType::Sample(sample::Type::Scalar(element)),
            DataType::Sample(sample::Type::Array { element, len: 3 }),
            DataType::Sample(sample::Type::Matrix {
                element,
                rows: 2,
                columns: 3,
            }),
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
    fn keeps_the_unit_rule_for_name_edges() {
        let name = |text: &str| text.parse::<Name>().unwrap();
        let unit = || Some(Unit::new("kPa").unwrap());
        let f64 = DataType::Sample(sample::Type::Scalar(Scalar::F64));
        let data =
            Data::new(name("edge.time"), Some(name("edge.q")), f64, unit()).unwrap();
        assert_eq!(data.index(), &name("edge.time"));
        assert_eq!(data.quality(), Some(&name("edge.q")));
        assert_eq!(
            Data::new(name("edge.time"), None, DataType::Quality, unit()),
            Err(Error::Unit {
                data_type: DataType::Quality
            })
        );
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
