//! Decides whether a proof is of its subject (signed hellos and requests), and whether
//! a subject may do an action on a name: union of allows, authority cap.

#![deny(clippy::wildcard_enum_match_arm)]

pub mod proof;

use proof::{Admitted, CAP, Error};
use spec::access::{Action, Actions, Policy};
use spec::definition::{Definition, Kind};
use spec::subject::Subject;
use types::authority::Authority;
use types::ed25519::PublicKey;
use types::hash::{Map, Set};
use types::hello::Hello;
use types::name::{Name, Prefix};
use types::node;
use types::time::Interval;

/// The access rules of a mesh: its access policies, its connectors, and the keys of
/// its subjects. Owners build one from the spec they read and ask it for each
/// decision.
#[derive(Clone, Debug)]
pub struct Rules {
    policies: Vec<(Prefix, Policy)>,
    connectors: Set<Name>,
    subjects: Map<Name, Subject>,
}

impl Rules {
    /// Builds the rules from the region trees that the owner reads. Each item is the
    /// prefix of a region, with [`Prefix::ROOT`] for the root region, and the
    /// definitions of its tree by name. Access keeps the access policies, the
    /// connectors, and each subject by its name, and ignores each other kind.
    pub fn new<'a, T>(trees: impl IntoIterator<Item = (Prefix, T)>) -> Self
    where
        T: IntoIterator<Item = (&'a Name, &'a Definition)>,
    {
        let mut rules = Self {
            policies: Vec::new(),
            connectors: Set::default(),
            subjects: Map::default(),
        };
        for (region, tree) in trees {
            for (name, definition) in tree {
                match definition {
                    Definition::Access(policy) => {
                        rules.policies.push((region.clone(), policy.clone()));
                    }
                    Definition::Connector(_) => {
                        rules.connectors.insert(name.clone());
                    }
                    Definition::Subject(subject) => {
                        rules.subjects.insert(label(name), subject.clone());
                    }
                    Definition::Region(_)
                    | Definition::NodeSettings(_)
                    | Definition::Compression(_)
                    | Definition::Placement(_)
                    | Definition::Time(_)
                    | Definition::Channel(_)
                    | Definition::Retention(_) => {}
                }
            }
        }
        rules
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

    /// Checks `hello`, signed with `signature`, at mesh time `now` (`None` when the
    /// node has none). `peer` is the node that carried the hello: this node when the
    /// program connected to it, else the node whose transport session forwarded it.
    ///
    /// # Errors
    ///
    /// The first that applies, in order: [`Error::Unsynced`], [`Error::Unknown`],
    /// [`Error::Unlisted`], [`Error::Signature`], [`Error::Via`],
    /// [`Error::Expired`], [`Error::Capped`].
    pub fn admit(
        &self,
        now: Option<Interval>,
        peer: node::Key,
        hello: Hello,
        signature: &[u8; 64],
    ) -> Result<Admitted, Error> {
        let now = now.ok_or(Error::Unsynced)?;
        let key = self.listed(&hello)?;
        key.verify(&proof::hello(&hello), signature)
            .map_err(|_bad| Error::Signature)?;
        if hello.via != peer {
            return Err(Error::Via {
                via: hello.via,
                peer,
            });
        }
        live(&hello, now)?;
        let cap = now.earliest + CAP;
        if hello.expires > cap {
            return Err(Error::Capped {
                expires: hello.expires,
                cap,
            });
        }
        Ok(Admitted { hello })
    }

    /// Checks that `body`, signed with `signature`, is a request of the connection of
    /// `admitted`, at mesh time `now`: its subject still lists its key, the hello has
    /// not expired, and the key signed [`proof::request`] of the hello's connection and
    /// `body`.
    ///
    /// # Errors
    ///
    /// The first that applies, in order: [`Error::Unsynced`], [`Error::Unknown`],
    /// [`Error::Unlisted`], [`Error::Signature`], [`Error::Expired`].
    pub fn verify(
        &self,
        admitted: &Admitted,
        now: Option<Interval>,
        body: &[u8],
        signature: &[u8; 64],
    ) -> Result<(), Error> {
        let now = now.ok_or(Error::Unsynced)?;
        let hello = &admitted.hello;
        let key = self.listed(hello)?;
        key.verify(&proof::request(hello.connection, body), signature)
            .map_err(|_bad| Error::Signature)?;
        live(hello, now)
    }

    /// The key of `hello`, when the spec lists it for the hello's subject.
    fn listed(&self, hello: &Hello) -> Result<PublicKey, Error> {
        let subject =
            self.subjects
                .get(&hello.subject)
                .ok_or_else(|| Error::Unknown {
                    subject: hello.subject.clone(),
                })?;
        subject
            .keys()
            .binary_search(&hello.key)
            .map(|_at| hello.key)
            .map_err(|_at| Error::Unlisted {
                subject: hello.subject.clone(),
                key: hello.key,
            })
    }
}

/// Refuses `hello` once the latest mesh time reaches its expiry.
fn live(hello: &Hello, now: Interval) -> Result<(), Error> {
    if now.latest >= hello.expires {
        return Err(Error::Expired {
            expires: hello.expires,
            now: now.latest,
        });
    }
    Ok(())
}

/// The name of the subject whose tree key is `key`: `key` without its `@subject`
/// segment.
fn label(key: &Name) -> Name {
    let suffix = Kind::Subject.as_str();
    key.as_str()
        .strip_suffix(suffix)
        .and_then(|label| label.strip_suffix(".@"))
        .and_then(|label| label.parse().ok())
        .expect("the spec puts a subject at `<name>.@subject`")
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
