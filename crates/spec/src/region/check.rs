//! The check of one region's definitions.

use std::collections::BTreeMap;
use std::fmt;

use types::name::{Name, Prefix};

use crate::channel;
use crate::definition::{Definition, Kind};

/// Checks the definitions of the region at `prefix`, by tree key: each problem of
/// [`channel::check`] on its channels, each name that the region does not govern, and
/// each definition that is not at a tree key of its kind.
///
/// The region governs a name under `prefix` that is under no child region. A child
/// region is one whose record `<child>.@region` this region governs. A record under a
/// child region is ungoverned, and makes no child. The region's own record
/// `<prefix>.@region` is in its parent's tree, so it is ungoverned here. The channel
/// check runs on the channels that the region governs, so an edge to a channel of
/// another region is [`channel::Problem::Dangling`].
///
/// Gives every problem, in tree key order.
#[must_use]
pub fn check(
    prefix: &Prefix,
    definitions: &BTreeMap<Name, Definition>,
) -> Vec<Problem> {
    let records: Vec<Name> = definitions
        .iter()
        .filter(|(_, definition)| matches!(definition, Definition::Region(_)))
        .filter_map(|(key, _)| Kind::Region.label(key))
        .filter(|label| inside(prefix, label))
        .collect();
    let children: Vec<&Name> = records
        .iter()
        .filter(|label| !records.iter().any(|other| below(label, other)))
        .collect();
    let mut problems: Vec<(Name, Problem)> = Vec::new();
    let mut channels = BTreeMap::new();
    for (key, definition) in definitions {
        let kind = definition.kind();
        let label = kind.label(key);
        if label.is_none() {
            problems.push((
                key.clone(),
                Problem::Misplaced {
                    name: key.clone(),
                    kind,
                },
            ));
        }
        let record = label.filter(|_| kind == Kind::Region);
        let governed = match &record {
            Some(label) => {
                inside(prefix, label)
                    && !children.iter().any(|child| below(label, child))
            }
            None => {
                prefix.contains(key)
                    && !children.iter().any(|child| key.starts_with(child))
            }
        };
        match definition {
            _ if !governed => problems.push((
                key.clone(),
                Problem::Ungoverned {
                    name: key.clone(),
                    region: prefix.clone(),
                },
            )),
            Definition::Channel(channel) => {
                channels.insert(key.clone(), channel.clone());
            }
            _ => {}
        }
    }
    for problem in channel::check(&channels) {
        problems.push((problem.name().clone(), Problem::Channel(problem)));
    }
    problems.sort_by(|a, b| a.0.cmp(&b.0));
    problems.into_iter().map(|(_, problem)| problem).collect()
}

// Whether `name` is under `prefix` and is not the prefix itself.
fn inside(prefix: &Prefix, name: &Name) -> bool {
    prefix.contains(name) && *prefix != Prefix::from(name.clone())
}

// Whether `name` is below `other` and is not `other` itself.
fn below(name: &Name, other: &Name) -> bool {
    name != other && name.starts_with(other)
}

/// A problem of a region's definitions. `Display` gives the message: a lower-case
/// clause with no final period. [`Problem::fix`] gives what to do instead.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Problem {
    /// A problem of the region's channels.
    Channel(channel::Problem),
    /// The region does not govern the name: it is not under the region's prefix, or it
    /// is under a child region.
    Ungoverned {
        /// The tree key.
        name: Name,
        /// The prefix of the region.
        region: Prefix,
    },
    /// The definition is not at a tree key of its kind.
    Misplaced {
        /// The tree key.
        name: Name,
        /// The kind of the definition.
        kind: Kind,
    },
}

impl Problem {
    /// What to do instead: a sentence with no final period.
    #[must_use]
    pub const fn fix(&self) -> &'static str {
        match self {
            Self::Channel(problem) => problem.fix(),
            Self::Ungoverned { .. } => {
                "Apply the definition in the region that governs its name"
            }
            Self::Misplaced { .. } => {
                "Put the definition at the key of its kind: `<label>.@<kind>`, or its \
                 own name for a connector or a channel"
            }
        }
    }
}

impl fmt::Display for Problem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Channel(problem) => problem.fmt(f),
            Self::Ungoverned { name, region } if *region == Prefix::ROOT => {
                write!(f, "the root region does not govern `{name}`")
            }
            Self::Ungoverned { name, region } => {
                write!(f, "the region `{region}` does not govern `{name}`")
            }
            Self::Misplaced { name, kind } => write!(
                f,
                "`{name}` is not a tree key of a definition of kind `{}`",
                kind.as_str()
            ),
        }
    }
}

#[cfg(test)]
mod tests;
