//! The node settings policy shell: the budgets of the nodes it selects. `config` and
//! `spec::resolve` pick the policy that wins for a node.

use std::fmt;

use types::byte;
use types::name::Selector;

/// Sets the disk and pool budgets of the nodes that `select` matches. A budget that is
/// `None` comes from a less specific policy.
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
    /// Returns [`Zero`] when `disk` or `pool` is zero bytes.
    pub fn new(
        select: Selector,
        disk: Option<byte::Size>,
        pool: Option<byte::Size>,
    ) -> Result<Self, Zero> {
        if disk == Some(byte::Size::ZERO) {
            return Err(Zero::Disk);
        }
        if pool == Some(byte::Size::ZERO) {
            return Err(Zero::Pool);
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

/// A budget of zero bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Zero {
    /// The disk budget is zero.
    Disk,
    /// The pool budget is zero.
    Pool,
}

impl fmt::Display for Zero {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Disk => f.write_str("the disk budget is zero"),
            Self::Pool => f.write_str("the pool budget is zero"),
        }
    }
}

impl std::error::Error for Zero {}

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
        assert_eq!(Policy::new(select(), zero, one), Err(Zero::Disk));
        assert_eq!(Policy::new(select(), one, zero), Err(Zero::Pool));
        assert_eq!(Zero::Disk.to_string(), "the disk budget is zero");
        assert_eq!(Zero::Pool.to_string(), "the pool budget is zero");
        let policy = Policy::new(select(), one, None).unwrap();
        assert_eq!((policy.disk(), policy.pool()), (one, None));
        assert_eq!(policy.select(), &select());
    }
}
