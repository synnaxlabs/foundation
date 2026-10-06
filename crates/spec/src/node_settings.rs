//! The node settings policy: the budgets of the nodes it selects.

use std::fmt;

use types::byte;
use types::name::Selector;

/// Sets the disk and pool budgets of the nodes that `select` matches. A budget that is
/// `None` comes from a less specific policy, so a policy sets at least one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Policy {
    select: Selector,
    disk: Option<byte::Size>,
    pool: Option<byte::Size>,
}

impl Policy {
    /// Makes a policy.
    ///
    /// # Errors
    ///
    /// Returns an [`Error`] when `disk` or `pool` is zero bytes, disk first, or when
    /// both are `None`.
    pub fn new(
        select: Selector,
        disk: Option<byte::Size>,
        pool: Option<byte::Size>,
    ) -> Result<Self, Error> {
        if disk == Some(byte::Size::ZERO) {
            return Err(Error::ZeroDisk);
        }
        if pool == Some(byte::Size::ZERO) {
            return Err(Error::ZeroPool);
        }
        if disk.is_none() && pool.is_none() {
            return Err(Error::NoBudget);
        }
        Ok(Self { select, disk, pool })
    }

    /// The nodes the policy applies to.
    #[must_use]
    pub const fn select(&self) -> &Selector {
        &self.select
    }

    /// The disk budget, above zero.
    #[must_use]
    pub const fn disk(&self) -> Option<byte::Size> {
        self.disk
    }

    /// The pool budget, above zero.
    #[must_use]
    pub const fn pool(&self) -> Option<byte::Size> {
        self.pool
    }
}

/// Budgets that make no policy.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// The disk budget is zero bytes.
    ZeroDisk,
    /// The pool budget is zero bytes.
    ZeroPool,
    /// The policy sets neither budget, so it changes nothing.
    NoBudget,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ZeroDisk => f.write_str("the disk budget is zero"),
            Self::ZeroPool => f.write_str("the pool budget is zero"),
            Self::NoBudget => f.write_str("the policy sets no budget"),
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

    #[test]
    fn refuses_a_zero_budget() {
        let zero = Some(byte::Size::ZERO);
        let one = Some(byte::Size::KIBIBYTE);
        assert_eq!(Policy::new(select(), zero, one), Err(Error::ZeroDisk));
        assert_eq!(Policy::new(select(), one, zero), Err(Error::ZeroPool));
        assert_eq!(Policy::new(select(), zero, zero), Err(Error::ZeroDisk));
        assert_eq!(Policy::new(select(), None, zero), Err(Error::ZeroPool));
        assert_eq!(Error::ZeroDisk.to_string(), "the disk budget is zero");
        assert_eq!(Error::ZeroPool.to_string(), "the pool budget is zero");
        let policy = Policy::new(select(), one, None).unwrap();
        assert_eq!((policy.disk(), policy.pool()), (one, None));
        assert_eq!(policy.select(), &select());
    }

    #[test]
    fn refuses_a_policy_with_no_budget() {
        assert_eq!(Policy::new(select(), None, None), Err(Error::NoBudget));
        assert_eq!(Error::NoBudget.to_string(), "the policy sets no budget");
    }
}
