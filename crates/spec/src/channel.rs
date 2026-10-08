//! The channel definition: an index channel, or a data channel on an index.

use std::fmt;

use types::channel;

use crate::data_type::DataType;
use crate::unit::Unit;

mod check;

pub use check::{Problem, check};

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

/// An edge from one channel to another.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Edge {
    /// From a data channel to the index channel that times it.
    Index,
    /// From a data channel to the channel that holds its quality.
    Quality,
    /// From an index channel to the channel that holds its clock error bound.
    Error,
    /// From an index channel to the channel that holds its control handoffs, which is
    /// on another index.
    Control,
}

impl fmt::Display for Edge {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Index => "index channel",
            Self::Quality => "quality channel",
            Self::Error => "error channel",
            Self::Control => "control channel",
        })
    }
}

impl<R> Kind<R> {
    /// Each edge that the channel has, and the channel that it points at, in this
    /// order: the error then the control channel of an index, or the index then the
    /// quality channel of a data channel.
    pub fn edges(&self) -> impl Iterator<Item = (Edge, &R)> {
        let edges = match self {
            Self::Index { error, control } => [
                error.as_ref().map(|to| (Edge::Error, to)),
                control.as_ref().map(|to| (Edge::Control, to)),
            ],
            Self::Data(data) => [
                Some((Edge::Index, data.index())),
                data.quality().map(|to| (Edge::Quality, to)),
            ],
        };
        edges.into_iter().flatten()
    }

    /// The same kind, with each edge mapped by `f`, in the order index, quality, error,
    /// control. Units are not checked again.
    pub fn map<S>(self, mut f: impl FnMut(R) -> S) -> Kind<S> {
        match self {
            Self::Index { error, control } => Kind::Index {
                error: error.map(&mut f),
                control: control.map(&mut f),
            },
            Self::Data(Data {
                index,
                quality,
                data_type,
                unit,
            }) => Kind::Data(Data {
                index: f(index),
                quality: quality.map(f),
                data_type,
                unit,
            }),
        }
    }
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
    use types::sample::{self, Scalar};

    use super::*;
    use crate::data_type::tests::{NUMBERS, OTHERS, shapes};

    fn data_on_time(quality: Option<&'static str>) -> Kind<&'static str> {
        let data_type = DataType::Sample(sample::Type::Scalar(Scalar::F64));
        Kind::Data(Data::new("edge.time", quality, data_type, None).unwrap())
    }

    #[test]
    fn gives_each_edge_in_order() {
        let index = |error, control| Kind::Index { error, control };
        for (kind, edges) in [
            (index(None, None), vec![]),
            (index(Some("e"), None), vec![(Edge::Error, "e")]),
            (index(None, Some("c")), vec![(Edge::Control, "c")]),
            (
                index(Some("e"), Some("c")),
                vec![(Edge::Error, "e"), (Edge::Control, "c")],
            ),
            (data_on_time(None), vec![(Edge::Index, "edge.time")]),
            (
                data_on_time(Some("q")),
                vec![(Edge::Index, "edge.time"), (Edge::Quality, "q")],
            ),
        ] {
            let found: Vec<_> = kind.edges().map(|(edge, &to)| (edge, to)).collect();
            assert_eq!(found, edges, "{kind:?}");
        }
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

    mod map {
        use super::*;

        /// Maps each edge in `kind` to upper case, and gives each edge it saw.
        fn map(kind: Kind<&'static str>) -> (Kind<String>, Vec<&'static str>) {
            let mut seen = Vec::new();
            let mapped = kind.map(|to| {
                seen.push(to);
                to.to_uppercase()
            });
            (mapped, seen)
        }

        #[test]
        fn maps_each_edge_in_edge_order_and_keeps_the_rest() {
            let unit = || Some(Unit::new("kPa").unwrap());
            let f64 = || DataType::Sample(sample::Type::Scalar(Scalar::F64));
            let data = Data::new("t", Some("q"), f64(), unit()).unwrap();
            let upper = Data::new("T".into(), Some("Q".into()), f64(), unit());
            assert_eq!(
                map(Kind::Data(data)),
                (Kind::Data(upper.unwrap()), vec!["t", "q"])
            );
            let index = |error, control| Kind::Index { error, control };
            let upper = |error: Option<&str>, control: Option<&str>| Kind::Index {
                error: error.map(Into::into),
                control: control.map(Into::into),
            };
            assert_eq!(
                map(index(Some("e"), Some("c"))),
                (upper(Some("E"), Some("C")), vec!["e", "c"])
            );
            assert_eq!(
                map(index(None, Some("c"))),
                (upper(None, Some("C")), vec!["c"])
            );
            assert_eq!(map(index(None, None)), (upper(None, None), vec![]));
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
}
