//! The reduction policy: the deadband of the data channels it selects.

use std::fmt;

use types::name::Selector;

/// Sends a sample of a data channel that `select` matches only when the value moves by
/// more than `deadband`, in the channel's unit, from the last sample sent.
#[derive(Clone, Debug, PartialEq)]
pub struct Policy {
    select: Selector,
    deadband: f64,
}

// `new` refuses NaN, so equality is reflexive.
impl Eq for Policy {}

impl Policy {
    /// Makes a policy.
    ///
    /// # Errors
    ///
    /// Returns [`Deadband`] when `deadband` is not finite or not above zero.
    pub fn new(select: Selector, deadband: f64) -> Result<Self, Deadband> {
        if !deadband.is_finite() || deadband <= 0.0 {
            return Err(Deadband(deadband));
        }
        Ok(Self { select, deadband })
    }

    /// The data channels the policy applies to.
    #[must_use]
    pub const fn select(&self) -> &Selector {
        &self.select
    }

    /// The deadband, finite and above zero.
    #[must_use]
    pub const fn deadband(&self) -> f64 {
        self.deadband
    }
}

/// A deadband that is not a finite number above zero.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Deadband(pub f64);

impl fmt::Display for Deadband {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "the deadband {} is not a finite number above zero",
            self.0
        )
    }
}

impl std::error::Error for Deadband {}

#[cfg(test)]
mod tests {
    use super::*;

    fn select() -> Selector {
        Selector::new(["site_a.**"]).unwrap()
    }

    #[test]
    fn refuses_a_deadband_that_is_not_finite_and_above_zero() {
        for deadband in [0.0, -0.0, -1.0, f64::INFINITY, f64::NEG_INFINITY] {
            assert_eq!(Policy::new(select(), deadband), Err(Deadband(deadband)));
        }
        let nan = Policy::new(select(), f64::NAN).unwrap_err();
        assert!(nan.0.is_nan());
        assert_eq!(
            Deadband(-1.5).to_string(),
            "the deadband -1.5 is not a finite number above zero"
        );
        assert_eq!(
            nan.to_string(),
            "the deadband NaN is not a finite number above zero"
        );
    }

    #[test]
    fn keeps_a_deadband_above_zero() {
        for deadband in [f64::MIN_POSITIVE, 5e-324, 0.25, f64::MAX] {
            let policy = Policy::new(select(), deadband).unwrap();
            assert_eq!(policy.deadband().to_bits(), deadband.to_bits());
            assert_eq!(policy.select(), &select());
        }
    }
}
