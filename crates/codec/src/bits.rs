//! Bit packing in natural order, least significant bit first.

/// The bytes that `count` values of `bits` bits fill.
pub(crate) fn len(count: usize, bits: u8) -> usize {
    count.strict_mul(usize::from(bits)).div_ceil(8)
}

/// Packs `values`, each under 2^`bits`, into `out`, which holds exactly
/// [`len`] bytes for them.
pub(crate) fn pack(values: impl Iterator<Item = u64>, bits: u8, out: &mut [u8]) {
    let bits = u32::from(bits);
    let (words, tail) = out.as_chunks_mut::<8>();
    let mut words = words.iter_mut();
    let (mut acc, mut filled) = (0_u128, 0_u32);
    for value in values {
        acc |= u128::from(value).wrapping_shl(filled);
        filled = filled.strict_add(bits);
        if filled >= 64 {
            let word = words.next().expect("invariant: out holds every packed bit");
            *word = low(acc).to_le_bytes();
            acc = acc.wrapping_shr(64);
            filled = filled.strict_sub(64);
        }
    }
    let rest = words.flat_map(|word| word.iter_mut()).chain(tail);
    for (byte, value) in rest.zip(low(acc).to_le_bytes()) {
        *byte = value;
    }
}

/// The values of `bits` bits packed in `bytes`, in order. It never ends: past the
/// bytes, it yields zeros.
pub(crate) fn unpack(bytes: &[u8], bits: u8) -> impl Iterator<Item = u64> + '_ {
    let mask = u64::MAX.unbounded_shr(u32::from(64_u8.strict_sub(bits)));
    let bits = usize::from(bits);
    (0_usize..).map(move |index| {
        let start = index.strict_mul(bits);
        let rest = bytes.get(start.div_euclid(8)..).unwrap_or_default();
        let window = rest.first_chunk().copied().unwrap_or_else(|| {
            let mut window = [0; 16];
            for (byte, value) in window.iter_mut().zip(rest) {
                *byte = *value;
            }
            window
        });
        let shift = u32::try_from(start.rem_euclid(8)).expect("invariant: under 8");
        low(u128::from_le_bytes(window).wrapping_shr(shift)) & mask
    })
}

#[expect(
    clippy::as_conversions,
    clippy::cast_possible_truncation,
    reason = "keeps the low 64 bits"
)]
fn low(acc: u128) -> u64 {
    acc as u64
}

#[cfg(test)]
#[expect(
    clippy::arithmetic_side_effects,
    reason = "tests compute expected values from small inputs"
)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    #[test]
    fn packs_least_significant_bit_first() {
        let mut out = [0; 2];
        pack([0, 1, 2, 3, 1].into_iter(), 2, &mut out);
        assert_eq!(out, [0xe4, 0x01]);
        let mut out = [0; 9];
        pack([(1 << 36) - 1, 0].into_iter(), 36, &mut out);
        assert_eq!(out, [0xff, 0xff, 0xff, 0xff, 0x0f, 0, 0, 0, 0]);
    }

    #[test]
    fn unpacks_zeros_past_the_bytes() {
        let values: Vec<u64> = unpack(&[0xe4, 0x01], 2).take(40).collect();
        assert_eq!(values, [[0, 1, 2, 3, 1].as_slice(), &[0; 35]].concat());
    }

    proptest! {
        #[test]
        fn round_trips_at_every_width(
            bits in 0..=64_u8,
            words in proptest::collection::vec(any::<u64>(), 0..130),
        ) {
            let mask = u64::MAX.unbounded_shr(u32::from(64 - bits));
            let values: Vec<u64> = words.iter().map(|word| word & mask).collect();
            let mut out = vec![0xaa; len(values.len(), bits)];
            pack(values.iter().copied(), bits, &mut out);
            let unpacked: Vec<u64> = unpack(&out, bits).take(values.len()).collect();
            prop_assert_eq!(unpacked, values);
        }
    }
}
