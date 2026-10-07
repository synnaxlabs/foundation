//! The placement policy: where copies of the connectors and indexes it selects live.

use std::fmt;

use types::name::{Name, Selector};

/// Places the connectors and indexes that `select` matches: a home node, a standby node
/// that takes over when the home fails, and copy nodes that are never promoted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Policy {
    select: Selector,
    nodes: Nodes,
}

/// The nodes a placement names. Plain data; [`Policy::new`] checks it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Nodes {
    /// The node that orders, buffers, and gates the selected indexes.
    pub home: Option<Name>,
    /// The node that takes over when the home fails.
    pub standby: Option<Name>,
    /// The nodes that keep a copy that is never promoted.
    pub copies: Vec<Name>,
}

impl Policy {
    /// Makes a policy. The order and repeats of `nodes.copies` do not count.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Empty`] with no home, no standby, and no copy, and
    /// [`Error::Overlap`] when one node has two roles.
    pub fn new(select: Selector, mut nodes: Nodes) -> Result<Self, Error> {
        nodes.copies.sort();
        nodes.copies.dedup();
        let Nodes {
            home,
            standby,
            copies,
        } = &nodes;
        if home.is_none() && standby.is_none() && copies.is_empty() {
            return Err(Error::Empty);
        }
        let overlap = match (home, standby) {
            (Some(home), Some(standby)) if home == standby => Some(home),
            _ => [home, standby]
                .into_iter()
                .flatten()
                .find(|node| copies.contains(node)),
        };
        if let Some(node) = overlap {
            return Err(Error::Overlap(node.clone()));
        }
        Ok(Self { select, nodes })
    }

    /// The connectors and indexes the policy applies to.
    #[must_use]
    pub const fn select(&self) -> &Selector {
        &self.select
    }

    /// The node that orders, buffers, and gates the selected indexes.
    #[must_use]
    pub const fn home(&self) -> Option<&Name> {
        self.nodes.home.as_ref()
    }

    /// The node that takes over when the home fails.
    #[must_use]
    pub const fn standby(&self) -> Option<&Name> {
        self.nodes.standby.as_ref()
    }

    /// The nodes that keep a copy, in name order with no repeat.
    #[must_use]
    pub fn copies(&self) -> &[Name] {
        &self.nodes.copies
    }
}

/// A placement that places nothing, or names one node twice.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Error {
    /// The policy names no home, no standby, and no copy.
    Empty,
    /// The node has more than one role in the policy.
    Overlap(Name),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => {
                write!(f, "a placement names no home, no standby, and no copy")
            }
            Self::Overlap(node) => {
                write!(f, "node {node} has more than one role in the placement")
            }
        }
    }
}

impl std::error::Error for Error {}

#[cfg(test)]
mod tests {
    use super::*;

    fn select() -> Selector {
        Selector::new(["site_a.**"]).unwrap()
    }

    fn name(text: &str) -> Name {
        text.parse().unwrap()
    }

    fn nodes(home: Option<&str>, standby: Option<&str>, copies: &[&str]) -> Nodes {
        Nodes {
            home: home.map(name),
            standby: standby.map(name),
            copies: copies.iter().map(|copy| name(copy)).collect(),
        }
    }

    #[test]
    fn refuses_a_placement_that_names_no_node() {
        assert_eq!(Policy::new(select(), Nodes::default()), Err(Error::Empty));
        assert_eq!(
            Error::Empty.to_string(),
            "a placement names no home, no standby, and no copy"
        );
    }

    #[test]
    fn refuses_a_node_with_two_roles() {
        for nodes in [
            nodes(Some("n_1"), Some("n_1"), &[]),
            nodes(Some("n_1"), None, &["n_2", "n_1"]),
            nodes(None, Some("n_1"), &["n_2", "n_1"]),
            nodes(Some("n_3"), Some("n_1"), &["n_2", "n_1"]),
        ] {
            assert_eq!(
                Policy::new(select(), nodes),
                Err(Error::Overlap(name("n_1")))
            );
        }
        assert_eq!(
            Error::Overlap(name("n_1")).to_string(),
            "node n_1 has more than one role in the placement"
        );
    }

    #[test]
    fn keeps_the_copies_in_name_order_with_no_repeat() {
        let nodes = nodes(Some("n_4"), Some("n_2"), &["n_3", "n_1", "n_3"]);
        let policy = Policy::new(select(), nodes).unwrap();
        assert_eq!(policy.copies(), [name("n_1"), name("n_3")]);
        assert_eq!(policy.home(), Some(&name("n_4")));
        assert_eq!(policy.standby(), Some(&name("n_2")));
        assert_eq!(policy.select(), &select());
    }

    #[test]
    fn makes_a_placement_with_any_one_role() {
        let home = Policy::new(select(), nodes(Some("n_1"), None, &[])).unwrap();
        assert_eq!(home.home(), Some(&name("n_1")));
        assert_eq!(home.standby(), None);
        assert!(home.copies().is_empty());
        let standby = Policy::new(select(), nodes(None, Some("n_1"), &[])).unwrap();
        assert_eq!(standby.home(), None);
        assert_eq!(standby.standby(), Some(&name("n_1")));
        let copies = Policy::new(select(), nodes(None, None, &["n_1"])).unwrap();
        assert_eq!(copies.home(), None);
        assert_eq!(copies.standby(), None);
    }
}
