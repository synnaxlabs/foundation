//! The checks across channels: each edge points at a channel of the kind it needs.

use std::collections::BTreeMap;
use std::fmt;

use types::channel;
use types::name::Name;

use super::{Channel, DataType, Kind};

/// Checks the edges between `channels`, each with its name: a data channel's index is
/// an index channel, its quality is a data channel of type quality, an index's error
/// and control channels are data channels, and no two channels share a key. An
/// edge to a shared key gives only the [`Problem::Shared`].
///
/// Gives every problem, in name order.
pub fn check<'a>(
    channels: impl IntoIterator<Item = (&'a Name, &'a Channel)>,
) -> Vec<Problem> {
    let mut channels: Vec<_> = channels.into_iter().collect();
    channels.sort_by_key(|(name, _)| *name);
    let mut keys = BTreeMap::new();
    for (at, &(_, channel)) in channels.iter().enumerate() {
        keys.entry(channel.key)
            .and_modify(|(_, shared)| *shared = true)
            .or_insert((at, false));
    }
    let mut problems = Vec::new();
    for (at, &(name, channel)) in channels.iter().enumerate() {
        let (first, _) = keys[&channel.key];
        if first != at {
            problems.push(Problem::Shared {
                key: channel.key,
                first: channels[first].0.clone(),
                second: name.clone(),
            });
        }
        for (edge, to) in edges(&channel.kind).into_iter().flatten() {
            match keys.get(&to) {
                None => problems.push(Problem::Dangling {
                    from: name.clone(),
                    edge,
                    to,
                }),
                Some(&(target, false)) if !edge.fits(&channels[target].1.kind) => {
                    problems.push(Problem::Wrong {
                        from: name.clone(),
                        edge,
                        to: channels[target].0.clone(),
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

/// A problem across channels: an edge to a missing or wrong channel, or one key on
/// two channels. `Display` gives the message: a lower-case clause with no final
/// period. [`Problem::fix`] gives what to do instead.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Problem {
    /// No channel has the key `to`.
    Dangling {
        /// The channel the edge starts at.
        from: Name,
        /// The edge.
        edge: Edge,
        /// The key the edge points at.
        to: channel::Key,
    },
    /// The channel `to` is not the kind that `edge` needs.
    Wrong {
        /// The channel the edge starts at.
        from: Name,
        /// The edge.
        edge: Edge,
        /// The channel the edge points at.
        to: Name,
    },
    /// Two channels have one key.
    Shared {
        /// The key.
        key: channel::Key,
        /// The first channel with the key, in name order.
        first: Name,
        /// The next channel with the key.
        second: Name,
    },
}

impl Problem {
    /// What to do instead: a sentence with no final period.
    #[must_use]
    pub const fn fix(&self) -> &'static str {
        match self {
            Self::Dangling { .. } => "Point it at a channel that exists",
            Self::Wrong { edge, .. } => edge.need().1,
            Self::Shared { .. } => "Give each channel its own key",
        }
    }
}

impl fmt::Display for Problem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Dangling { from, edge, to } => {
                write!(f, "the {edge} of `{from}` is {to}, which no channel has")
            }
            Self::Wrong { from, edge, to } => {
                let need = edge.need().0;
                write!(f, "the {edge} of `{from}` is `{to}`, which is not {need}")
            }
            Self::Shared { key, first, second } => {
                write!(f, "`{first}` and `{second}` have the same key {key}")
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
    /// From an index channel to the channel that holds its control handoffs.
    Control,
}

impl Edge {
    /// Reports whether the edge can point at a channel of `kind`.
    const fn fits(self, kind: &Kind) -> bool {
        match (self, kind) {
            (Self::Quality, Kind::Data(data)) => {
                matches!(data.data_type(), DataType::Quality)
            }
            (Self::Index, Kind::Index { .. })
            | (Self::Error | Self::Control, Kind::Data(_)) => true,
            _ => false,
        }
    }

    /// The kind of channel the edge needs, and the fix that points it there.
    const fn need(self) -> (&'static str, &'static str) {
        match self {
            Self::Index => ("an index channel", "Point it at an index channel"),
            Self::Quality => (
                "a data channel of type quality",
                "Point it at a data channel of type quality",
            ),
            Self::Error | Self::Control => {
                ("a data channel", "Point it at a data channel")
            }
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
