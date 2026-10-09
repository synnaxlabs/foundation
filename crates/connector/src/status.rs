//! The status channels of a connector, `<connector>.status.<name>`, on one index of
//! their own.

use types::sample::{Scalar, Type};

/// The name of the index of the status channels.
pub const TIME: &str = "time";

/// The channels that the supervisor writes, with the sample type of each.
pub const CHANNELS: [(&str, Type); 3] = [
    ("state", Type::Scalar(Scalar::U8)),
    ("class", Type::Scalar(Scalar::U8)),
    ("restarts", Type::Scalar(Scalar::U64)),
];

/// Whether a kind may not name `count`: the supervisor's channels and the index have
/// those names.
pub(crate) fn reserved(count: &str) -> bool {
    count == TIME || CHANNELS.iter().any(|&(name, _)| name == count)
}
