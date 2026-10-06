//! The access policy shell: who may do which actions on which names. `access` decides
//! with these policies.

use std::fmt;

use types::authority::Authority;
use types::name::Patterns;

/// What a subject may do on a name.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Action {
    /// Read values and definitions.
    Read,
    /// Write values.
    Write,
    /// Compute a plan.
    Plan,
    /// Apply a plan.
    Apply,
    /// Set or delete a secret.
    Secret,
    /// Administer a region.
    Admin,
}

impl Action {
    pub(crate) const ALL: [Self; 6] = [
        Self::Read,
        Self::Write,
        Self::Plan,
        Self::Apply,
        Self::Secret,
        Self::Admin,
    ];

    /// The bit of the action in a stored set. Changing one changes stored bytes.
    const fn bit(self) -> u8 {
        match self {
            Self::Read => 1,
            Self::Write => 1 << 1,
            Self::Plan => 1 << 2,
            Self::Apply => 1 << 3,
            Self::Secret => 1 << 4,
            Self::Admin => 1 << 5,
        }
    }
}

/// A set of actions.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Actions(u8);

impl Actions {
    /// The set with no action.
    pub const NONE: Self = Self(0);

    /// Reports whether the set holds `action`.
    #[must_use]
    pub const fn contains(self, action: Action) -> bool {
        self.0 & action.bit() != 0
    }

    /// The actions in either set.
    #[must_use]
    pub const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    pub(crate) const fn bits(self) -> u8 {
        self.0
    }

    /// The set with these bits, or `None` when a bit names no action.
    pub(crate) fn from_bits(bits: u8) -> Option<Self> {
        let all = Action::ALL.into_iter().collect::<Self>();
        (bits & !all.0 == 0).then_some(Self(bits))
    }
}

impl FromIterator<Action> for Actions {
    fn from_iter<I: IntoIterator<Item = Action>>(iter: I) -> Self {
        Self(iter.into_iter().fold(0, |bits, action| bits | action.bit()))
    }
}

impl fmt::Debug for Actions {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let held = Action::ALL.into_iter().filter(|a| self.contains(*a));
        f.debug_set().entries(held).finish()
    }
}

/// Allows the subjects that `subjects` matches to do the actions in `allow` on the
/// names that `select` matches. Policies only allow: a subject may do what the union of
/// the matching policies allows, and nothing else.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Policy {
    subjects: Patterns,
    select: Patterns,
    allow: Actions,
    authority: Authority,
}

impl Policy {
    /// Makes a policy. `authority` caps the control authority of a write, so it counts
    /// only when `allow` holds [`Action::Write`]; without it the authority is zero.
    #[must_use]
    pub fn new(
        subjects: Patterns,
        select: Patterns,
        allow: Actions,
        authority: Authority,
    ) -> Self {
        let authority = if allow.contains(Action::Write) {
            authority
        } else {
            Authority(0)
        };
        Self {
            subjects,
            select,
            allow,
            authority,
        }
    }

    /// The subjects the policy applies to.
    #[must_use]
    pub const fn subjects(&self) -> &Patterns {
        &self.subjects
    }

    /// The names the policy applies to.
    #[must_use]
    pub const fn select(&self) -> &Patterns {
        &self.select
    }

    /// The actions the policy allows.
    #[must_use]
    pub const fn allow(&self) -> Actions {
        self.allow
    }

    /// The highest control authority a write may claim under this policy, or `None`
    /// when the policy does not allow [`Action::Write`].
    #[must_use]
    pub fn authority(&self) -> Option<Authority> {
        self.allow.contains(Action::Write).then_some(self.authority)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn patterns(texts: &[&str]) -> Patterns {
        Patterns::new(texts.iter().copied()).unwrap()
    }

    #[test]
    fn holds_the_actions_of_either_set_in_a_union() {
        let read = [Action::Read].into_iter().collect::<Actions>();
        let write = [Action::Write].into_iter().collect::<Actions>();
        let both = read.union(write);
        assert!(both.contains(Action::Read));
        assert!(both.contains(Action::Write));
        assert!(!both.contains(Action::Admin));
        assert!(!Actions::NONE.contains(Action::Read));
        assert_eq!(both.union(read), both);
    }

    #[test]
    fn holds_a_repeated_action_once() {
        let twice = [Action::Read, Action::Read]
            .into_iter()
            .collect::<Actions>();
        assert!(twice.contains(Action::Read));
        assert_eq!(twice, [Action::Read].into_iter().collect());
    }

    #[test]
    fn shows_its_actions_in_order() {
        let set = [Action::Admin, Action::Read]
            .into_iter()
            .collect::<Actions>();
        assert_eq!(format!("{set:?}"), "{Read, Admin}");
        assert_eq!(format!("{:?}", Actions::NONE), "{}");
    }

    #[test]
    fn refuses_bits_that_name_no_action() {
        assert_eq!(Actions::from_bits(0b100_0000), None);
        let all = Action::ALL.into_iter().collect::<Actions>();
        assert_eq!(Actions::from_bits(0b11_1111), Some(all));
    }

    #[test]
    fn gives_an_authority_only_with_write() {
        let write = [Action::Write].into_iter().collect();
        let policy = Policy::new(
            patterns(&["ops.*"]),
            patterns(&["a.**"]),
            write,
            Authority(7),
        );
        assert_eq!(policy.authority(), Some(Authority(7)));

        let read = [Action::Read].into_iter().collect();
        let policy = Policy::new(
            patterns(&["ops.*"]),
            patterns(&["a.**"]),
            read,
            Authority(7),
        );
        assert_eq!(policy.authority(), None);
        let zero = Policy::new(
            patterns(&["ops.*"]),
            patterns(&["a.**"]),
            read,
            Authority(0),
        );
        assert_eq!(policy, zero);
    }
}
