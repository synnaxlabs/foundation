//! Decides whether a subject may do an action on a name: union of allows, authority
//! cap.

#![deny(clippy::wildcard_enum_match_arm)]

use spec::access::{Action, Actions, Policy};
use types::authority::Authority;
use types::hash::Set;
use types::name::{Name, Prefix};

/// The access rules of a mesh: its access policies and its connectors. Owners build
/// one from the spec they read and ask it for each decision.
#[derive(Clone, Debug)]
pub struct Rules {
    policies: Vec<(Prefix, Policy)>,
    connectors: Set<Name>,
}

impl Rules {
    /// Builds the rules. Each policy comes with the prefix of the region whose spec
    /// tree holds it; [`Prefix::ROOT`] is the root region. `connectors` are the names
    /// of the connector definitions.
    pub fn new(
        policies: impl IntoIterator<Item = (Prefix, Policy)>,
        connectors: impl IntoIterator<Item = Name>,
    ) -> Self {
        Self {
            policies: policies.into_iter().collect(),
            connectors: connectors.into_iter().collect(),
        }
    }

    /// What `subject` may do on `name`: the union of the policies that match both,
    /// and what a connector may do under its own name. A policy reaches only names in
    /// its region and the regions below it.
    ///
    /// A connector may write every name strictly under its own name at
    /// [`Authority::ABSOLUTE`], with no policy. That includes a nested connector and
    /// the connector's parameter index.
    #[must_use]
    pub fn grant(&self, subject: &Name, name: &Name) -> Grant {
        let mut grant = Grant::NONE;
        if self.connectors.contains(subject)
            && name != subject
            && name.starts_with(subject)
        {
            let write = [Action::Write].into_iter().collect();
            grant = grant.add(write, Some(Authority::ABSOLUTE));
        }
        for (region, policy) in &self.policies {
            let reached = region.contains(name)
                && policy.subjects().matches(subject).is_some()
                && policy.select().matches(name).is_some();
            if reached {
                grant = grant.add(policy.allow(), policy.authority());
            }
        }
        grant
    }
}

/// What one subject may do on one name.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Grant {
    actions: Actions,
    authority: Authority,
}

impl Grant {
    const NONE: Self = Self {
        actions: Actions::NONE,
        authority: Authority(0),
    };

    fn add(self, actions: Actions, authority: Option<Authority>) -> Self {
        Self {
            actions: self.actions.union(actions),
            authority: authority.map_or(self.authority, |a| a.max(self.authority)),
        }
    }

    /// The actions allowed. Empty when nothing matches: access denies by default.
    #[must_use]
    pub const fn actions(self) -> Actions {
        self.actions
    }

    /// The highest authority a write may claim, or `None` when write is not allowed.
    #[must_use]
    pub fn authority(self) -> Option<Authority> {
        self.actions
            .contains(Action::Write)
            .then_some(self.authority)
    }
}

#[cfg(test)]
mod tests;
