//! The checks across channels.

use std::collections::BTreeMap;
use std::fmt;

use types::channel;
use types::name::Name;

use super::{Channel, DataType, Kind};

/// Checks the edges between `channels`: a data channel's index is an index channel,
/// its quality is a data channel of type quality, an index's error channel is a data
/// channel, and its control channel is a data channel on another index.
///
/// Gives every problem, in name order.
///
/// # Panics
///
/// When two channels have one key, which only a defect gives.
#[must_use]
pub fn check(channels: &BTreeMap<Name, Channel>) -> Vec<Problem> {
    let mut keys = BTreeMap::new();
    for (name, channel) in channels {
        if let Some((first, _)) = keys.insert(channel.key, (name, channel)) {
            panic!("`{first}` and `{name}` have the same key {}", channel.key);
        }
    }
    let mut problems = Vec::new();
    for (name, channel) in channels {
        for (edge, to) in edges(&channel.kind).into_iter().flatten() {
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

fn edges(kind: &Kind) -> [Option<(Edge, channel::Key)>; 2] {
    match kind {
        Kind::Index { error, control } => [
            error.map(|to| (Edge::Error, to)),
            control.map(|to| (Edge::Control, to)),
        ],
        Kind::Data(data) => [
            Some((Edge::Index, data.index())),
            data.quality().map(|to| (Edge::Quality, to)),
        ],
    }
}

/// An edge to a missing or wrong channel. `Display` gives the message: a lower-case
/// clause with no final period. [`Problem::fix`] gives what to do instead.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Problem {
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
            Self::Dangling { .. } => "Point it at a channel that exists",
            Self::Wrong { edge, .. } => edge.need().1,
        }
    }
}

impl fmt::Display for Problem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
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

impl Edge {
    /// Reports whether the edge from the channel `from` can point at a channel of
    /// `kind`.
    fn fits(self, from: channel::Key, kind: &Kind) -> bool {
        match (self, kind) {
            (Self::Quality, Kind::Data(data)) => {
                matches!(data.data_type(), DataType::Quality)
            }
            (Self::Control, Kind::Data(data)) => data.index() != from,
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

#[cfg(test)]
mod tests;
