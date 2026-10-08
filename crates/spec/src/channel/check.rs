//! The checks across channels.

use std::collections::BTreeMap;
use std::fmt;

use types::channel;
use types::name::Name;

use super::{Channel, Edge, Kind};
use crate::data_type::DataType;

/// Checks `channels`: each has its own key, a data channel's index is an index
/// channel, its quality is a data channel of type quality, an index's error channel is
/// a data channel, and its control channel is a data channel on another index. An edge
/// to a key that two channels hold points at the first in name order.
///
/// Gives every problem, in name order.
#[must_use]
pub fn check(channels: &BTreeMap<Name, Channel>) -> Vec<Problem> {
    let mut keys: BTreeMap<channel::Key, (&Name, &Channel)> = BTreeMap::new();
    let mut duplicates = BTreeMap::new();
    for (name, channel) in channels {
        if let Some(&(first, _)) = keys.get(&channel.key) {
            let duplicate = Problem::Duplicate {
                first: first.clone(),
                second: name.clone(),
                key: channel.key,
            };
            duplicates.insert(name, duplicate);
        } else {
            keys.insert(channel.key, (name, channel));
        }
    }
    let mut problems = Vec::new();
    for (name, channel) in channels {
        problems.extend(duplicates.remove(name));
        for (edge, &to) in channel.kind.edges() {
            match keys.get(&to) {
                None => problems.push(Problem::Dangling {
                    from: name.clone(),
                    edge,
                    to,
                }),
                Some(&(target_name, target))
                    if !edge.fits(channel.key, &target.kind) =>
                {
                    problems.push(Problem::Wrong {
                        from: name.clone(),
                        edge,
                        to: target_name.clone(),
                    });
                }
                Some(_) => {}
            }
        }
    }
    problems
}

/// Two channels with one key, or an edge to a missing or wrong channel. `Display`
/// gives the message: a lower-case clause with no final period. [`Problem::fix`] gives
/// what to do instead.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Problem {
    /// The channel `second` has the key of `first`, which is before it in name order.
    /// Keys are given once, so only a defect gives this. The message does not show
    /// the key.
    Duplicate {
        /// The first channel with the key.
        first: Name,
        /// A later channel with the key.
        second: Name,
        /// The key.
        key: channel::Key,
    },
    /// No channel has the key `to`. The message does not show the key.
    Dangling {
        /// The channel the edge starts at.
        from: Name,
        /// The edge.
        edge: Edge,
        /// The key the edge points at.
        to: channel::Key,
    },
    /// The channel `to` is not what `edge` needs.
    Wrong {
        /// The channel the edge starts at.
        from: Name,
        /// The edge.
        edge: Edge,
        /// The channel the edge points at.
        to: Name,
    },
}

impl Problem {
    /// What to do instead: a sentence with no final period.
    #[must_use]
    pub const fn fix(&self) -> &'static str {
        match self {
            Self::Duplicate { .. } => {
                "Report a defect: each channel gets its own key when it is first applied"
            }
            Self::Dangling { .. } => "Point it at a channel that exists",
            Self::Wrong { edge, .. } => edge.need().1,
        }
    }
}

impl fmt::Display for Problem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Duplicate { first, second, .. } => {
                write!(f, "`{first}` and `{second}` have the same key")
            }
            Self::Dangling { from, edge, .. } => {
                write!(f, "the {edge} of `{from}` points at no channel")
            }
            Self::Wrong { from, edge, to } => {
                let need = edge.need().0;
                write!(f, "the {edge} of `{from}` is `{to}`, which is not {need}")
            }
        }
    }
}

impl Edge {
    /// Reports whether the edge from the channel `from` can point at a channel of
    /// `kind`.
    fn fits(self, from: channel::Key, kind: &Kind) -> bool {
        match (self, kind) {
            (Self::Quality, Kind::Data(data)) => {
                matches!(data.data_type(), DataType::Quality)
            }
            (Self::Control, Kind::Data(data)) => *data.index() != from,
            (Self::Index, Kind::Index { .. }) | (Self::Error, Kind::Data(_)) => true,
            _ => false,
        }
    }

    /// What the edge needs, and the fix that points it there.
    const fn need(self) -> (&'static str, &'static str) {
        match self {
            Self::Index => ("an index channel", "Point it at an index channel"),
            Self::Quality => (
                "a data channel of type quality",
                "Point it at a data channel of type quality",
            ),
            Self::Error => ("a data channel", "Point it at a data channel"),
            Self::Control => (
                "a data channel on another index",
                "Point it at a data channel on another index",
            ),
        }
    }
}

#[cfg(test)]
mod tests;
