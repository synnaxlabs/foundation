//! The status channels of a connector, `<connector>.status.<name>`, on one index of
//! their own.

use types::name::Name;
use types::sample::{Scalar, Type};

/// The name of the index of the status channels.
pub const TIME: &str = "time";

/// The channels that the supervisor writes, with the sample type of each.
pub const CHANNELS: [(&str, Type); 3] = [
    ("state", Type::Scalar(Scalar::U8)),
    ("class", Type::Scalar(Scalar::U8)),
    ("restarts", Type::Scalar(Scalar::U64)),
];

/// Panics when `kind` names a count of more than one segment, a count that the index
/// or a channel of the supervisor names in any case, or one count twice in any case.
pub(crate) fn check(kind: &str, counts: &[Name]) {
    for (i, count) in counts.iter().enumerate() {
        let same = |name: &str| name.eq_ignore_ascii_case(count.as_str());
        assert!(
            count.segments().nth(1).is_none(),
            "the kind {kind:?} names the count `{count}`, which is not one segment"
        );
        assert!(
            !same(TIME) && !CHANNELS.iter().any(|&(name, _)| same(name)),
            "the kind {kind:?} names the count `{count}`, a status channel of the \
             supervisor"
        );
        assert!(
            !counts[..i].iter().any(|earlier| same(earlier.as_str())),
            "the kind {kind:?} names the count `{count}` twice"
        );
    }
}
