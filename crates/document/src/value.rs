//! Values: what an attribute, a list item, or a call argument holds.

use types::name::Name;

use crate::{Map, Span};

/// A value and where it is. `==` does not read spans.
#[derive(Clone, Debug)]
pub struct Value {
    /// What the value is.
    pub kind: Kind,
    /// Where the value is.
    pub span: Option<Span>,
}

impl PartialEq for Value {
    fn eq(&self, other: &Self) -> bool {
        let Self { kind, span: _ } = self;
        *kind == other.kind
    }
}

impl Eq for Value {}

/// What a value is. Files hold data only, so no kind is an expression.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Kind {
    /// `true` or `false`.
    Bool(bool),
    /// A whole number. Every `i64` and `u64` fits.
    Integer(i128),
    /// A finite binary64 number. `Integer(1)` and `Float(1.0)` are different values.
    Float(Float),
    /// Text.
    String(Box<str>),
    /// A bare name: a channel such as `site_a.plc_7.pt_101`, or a type such as `f64`.
    Reference(Name),
    /// Values in order.
    List(Vec<Value>),
    /// Values by key.
    Map(Map),
    /// A function applied to values, such as `secret("plc_7_password")`.
    Call(Call),
}

/// A finite float. It stores -0.0 as 0.0, so equal floats have equal bits.
#[derive(Clone, Copy, Debug)]
pub struct Float(f64);

impl Float {
    /// Wraps a float, or returns `None` for NaN and the infinities.
    #[must_use]
    pub fn new(value: f64) -> Option<Self> {
        let value = if value == 0.0 { 0.0 } else { value };
        value.is_finite().then_some(Self(value))
    }

    /// The value. It is never NaN, an infinity, or -0.0.
    #[must_use]
    pub const fn get(self) -> f64 {
        self.0
    }
}

impl PartialEq for Float {
    fn eq(&self, other: &Self) -> bool {
        self.0.to_bits() == other.0.to_bits()
    }
}

impl Eq for Float {}

/// A function applied to values, such as `secret("plc_7_password")` or `f64("rpm")`.
/// `==` does not read spans.
#[derive(Clone, Debug)]
pub struct Call {
    /// The function's name.
    pub function: Box<str>,
    /// Where the function's name is.
    pub function_span: Option<Span>,
    /// The arguments, in order.
    pub arguments: Vec<Value>,
}

impl PartialEq for Call {
    fn eq(&self, other: &Self) -> bool {
        let Self {
            function,
            function_span: _,
            arguments,
        } = self;
        *function == other.function && *arguments == other.arguments
    }
}

impl Eq for Call {}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    mod float {
        use super::*;

        #[test]
        fn refuses_nan() {
            assert_eq!(Float::new(f64::NAN), None);
        }

        #[test]
        fn refuses_the_infinities() {
            assert_eq!(Float::new(f64::INFINITY), None);
            assert_eq!(Float::new(f64::NEG_INFINITY), None);
        }

        #[test]
        fn compares_by_value() {
            assert_ne!(Float::new(1.0).unwrap(), Float::new(2.0).unwrap());
        }

        #[test]
        fn stores_negative_zero_as_zero() {
            let zero = Float::new(-0.0).unwrap();
            assert_eq!(zero.get().to_bits(), 0.0f64.to_bits());
            assert_eq!(zero, Float::new(0.0).unwrap());
        }

        proptest! {
            #[test]
            fn keeps_every_other_finite_value(value in any::<f64>()) {
                prop_assume!(value.is_finite() && value != 0.0);
                let kept = Float::new(value).unwrap().get();
                prop_assert_eq!(kept.to_bits(), value.to_bits());
            }
        }
    }
}
