//! The placement policy: where copies of the connectors and indexes it selects live.

use std::fmt;

use types::name::{Name, Selector};

/// Places the connectors and indexes that `select` matches: a standby node that takes
/// over when the home fails, and copy nodes that are never promoted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Policy {
    select: Selector,
    standby: Option<Name>,
    copies: Vec<Name>,
}

impl Policy {
    /// Makes a policy. The order and repeats of `copies` do not count.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Empty`] with no standby and no copy, and [`Error::Overlap`]
    /// when the standby is also a copy.
    pub fn new(
        select: Selector,
        standby: Option<Name>,
        copies: impl IntoIterator<Item = Name>,
    ) -> Result<Self, Error> {
        let mut copies = copies.into_iter().collect::<Vec<_>>();
        copies.sort();
        copies.dedup();
        match &standby {
            None if copies.is_empty() => return Err(Error::Empty),
            Some(node) if copies.contains(node) => {
                return Err(Error::Overlap(node.clone()));
            }
            _ => {}
        }
        Ok(Self {
            select,
            standby,
            copies,
        })
    }

    /// The connectors and indexes the policy applies to.
    #[must_use]
    pub const fn select(&self) -> &Selector {
        &self.select
    }

    /// The node that takes over when the home fails.
    #[must_use]
    pub const fn standby(&self) -> Option<&Name> {
        self.standby.as_ref()
    }

    /// The nodes that keep a copy, in name order with no repeat.
    #[must_use]
    pub fn copies(&self) -> &[Name] {
        &self.copies
    }
}

/// A placement that places nothing, or names one node twice.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Error {
    /// The policy has no standby and no copy.
    Empty,
    /// The node is both the standby and a copy.
    Overlap(Name),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => write!(f, "a placement has no standby and no copy"),
            Self::Overlap(node) => {
                write!(f, "node {node} is both the standby and a copy")
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

    #[test]
    fn refuses_a_placement_with_no_standby_and_no_copy() {
        assert_eq!(Policy::new(select(), None, []), Err(Error::Empty));
        assert_eq!(
            Error::Empty.to_string(),
            "a placement has no standby and no copy"
        );
    }

    #[test]
    fn refuses_a_standby_that_is_also_a_copy() {
        let copies = [name("n_2"), name("n_1")];
        let error = Error::Overlap(name("n_1"));
        assert_eq!(
            Policy::new(select(), Some(name("n_1")), copies),
            Err(error.clone())
        );
        assert_eq!(error.to_string(), "node n_1 is both the standby and a copy");
    }

    #[test]
    fn keeps_the_copies_in_name_order_with_no_repeat() {
        let copies = [name("n_3"), name("n_1"), name("n_3")];
        let policy = Policy::new(select(), Some(name("n_2")), copies).unwrap();
        assert_eq!(policy.copies(), [name("n_1"), name("n_3")]);
        assert_eq!(policy.standby(), Some(&name("n_2")));
        assert_eq!(policy.select(), &select());
    }

    #[test]
    fn makes_a_placement_with_only_a_standby_or_only_copies() {
        let standby = Policy::new(select(), Some(name("n_1")), []).unwrap();
        assert!(standby.copies().is_empty());
        let copies = Policy::new(select(), None, [name("n_1")]).unwrap();
        assert_eq!(copies.standby(), None);
    }
}
