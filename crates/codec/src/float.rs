//! Float vectors: ALP, or the integer codecs on the sortable integers of their bits.

use crate::alp::{self, Pairs};
use crate::int::{self, Order};
use crate::vector::{self, Plan, Vector};
use crate::word;

// The float paths stay out of line. Inlined, they cost the integer paths about 60
// instructions a short decode call and 4% on long encode and decode calls (box2).

/// Writes one vector of floats of `W` bytes into the front of `out` and returns its
/// length: ALP with a pair of `pairs`, or what [`int::encode`] writes for their
/// sortable integers, whichever is smaller.
///
/// # Panics
///
/// Panics when `chunk` holds no sample.
#[inline(never)]
pub(crate) fn encode<const W: usize>(
    chunk: &[u8],
    pairs: &mut Pairs,
    out: &mut [u8],
) -> usize {
    let plan = int::plan::<W>(chunk, Order::Unsigned, sortable::<W>);
    let plan = pairs.plan::<W>(chunk, plan);
    vector::write::<W>(chunk, plan, sortable::<W>, out)
}

/// Writes the floats of `W` bytes of `vector` into `out`, which holds exactly them.
#[inline(never)]
pub(crate) fn decode<const W: usize>(vector: &Vector<'_>, out: &mut [u8]) {
    match vector.plan {
        Plan::Alp {
            reference,
            bits,
            pair,
            exceptions,
        } => alp::decode::<W>(reference, bits, pair, exceptions, vector.body, out),
        _ => vector.decode::<W>(out, self::bits::<W>),
    }
}

/// The integer that the codecs see for the bits of a float of `W` bytes. A float with
/// its sign bit set has each bit flipped, and any other only its sign bit, so the
/// integers keep the order of the floats in unsigned order. It is a bijection on the
/// bits.
pub(crate) fn sortable<const W: usize>(bits: u64) -> u64 {
    let mask = word::mask(W);
    let high = mask ^ mask.wrapping_shr(1);
    let negative = 0_u64.wrapping_sub(u64::from(bits & high != 0));
    bits ^ (negative & mask | high)
}

/// The bits of the float of `W` bytes whose [`sortable`] integer is `sortable`.
pub(crate) fn bits<const W: usize>(sortable: u64) -> u64 {
    let mask = word::mask(W);
    let high = mask ^ mask.wrapping_shr(1);
    let negative = 0_u64.wrapping_sub(u64::from(sortable & high == 0));
    sortable ^ (negative & mask | high)
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    proptest! {
        #[test]
        fn sortable_keeps_the_order_of_floats(a in any::<f64>(), b in any::<f64>()) {
            let order = |value: f64| sortable::<8>(value.to_bits());
            prop_assert_eq!(a.total_cmp(&b), order(a).cmp(&order(b)));
            #[expect(
                clippy::as_conversions,
                clippy::cast_possible_truncation,
                reason = "an `f32` sample"
            )]
            let (a, b) = (a as f32, b as f32);
            let order = |value: f32| sortable::<4>(value.to_bits().into());
            prop_assert_eq!(a.total_cmp(&b), order(a).cmp(&order(b)));
        }

        #[test]
        fn bits_inverts_sortable(value in any::<u64>()) {
            prop_assert_eq!(bits::<8>(sortable::<8>(value)), value);
            let narrow = value & word::mask(4);
            prop_assert_eq!(sortable::<4>(narrow) & !word::mask(4), 0);
            prop_assert_eq!(bits::<4>(sortable::<4>(narrow)), narrow);
        }
    }
}
