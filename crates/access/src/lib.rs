//! Decides whether a subject may do an action on a name: union of allows, authority
//! cap.

#![deny(clippy::wildcard_enum_match_arm)]

use spec::access::{Action, Actions, Policy};
use types::authority::Authority;
use types::hash::Set;
use types::name::Name;

/// The access rules of a mesh: its access policies, its regions, and its connectors.
/// Owners build one from the spec they read and ask it for each decision.
#[derive(Clone, Debug)]
pub struct Rules {
    policies: Vec<Placed>,
    connectors: Set<Name>,
}

/// A policy and its governing region. `None` is the root region.
#[derive(Clone, Debug)]
struct Placed {
    region: Option<Name>,
    policy: Policy,
}

impl Rules {
    /// Builds the rules. `policies` are keyed by policy name (tree key
    /// `<name>.@access`). `regions` are the region prefixes; the root region is
    /// implied. `connectors` are the names of the connector definitions.
    pub fn new(
        policies: impl IntoIterator<Item = (Name, Policy)>,
        regions: impl IntoIterator<Item = Name>,
        connectors: impl IntoIterator<Item = Name>,
    ) -> Self {
        let regions = regions.into_iter().collect::<Vec<_>>();
        let policies = policies
            .into_iter()
            .map(|(name, policy)| Placed {
                region: regions
                    .iter()
                    .filter(|region| name.starts_with(region))
                    .max_by_key(|region| region.as_str().len())
                    .cloned(),
                policy,
            })
            .collect();
        Self {
            policies,
            connectors: connectors.into_iter().collect(),
        }
    }

    /// What `subject` may do on `name`: the union of the policies that match both,
    /// and what a connector may do under its own name. A policy reaches only names in
    /// its governing region and the regions below it.
    #[must_use]
    pub fn grant(&self, subject: &Name, name: &Name) -> Grant {
        let mut grant = Grant::NONE;
        if self.connectors.contains(subject)
            && name != subject
            && name.starts_with(subject)
        {
            grant = grant.add(write(), Some(CONNECTOR));
        }
        for placed in &self.policies {
            let policy = &placed.policy;
            let reached = placed.region.as_ref().is_none_or(|r| name.starts_with(r))
                && policy.subjects().selector().matches(subject).is_some()
                && policy.select().selector().matches(name).is_some();
            if reached {
                grant = grant.add(policy.allow(), policy.authority());
            }
        }
        grant
    }
}

/// The authority of a connector's write under its own name, with no policy.
const CONNECTOR: Authority = Authority::ABSOLUTE;

fn write() -> Actions {
    [Action::Write].into_iter().collect()
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
