//! QUIC variable-length integers: a value under 2^62 in 1, 2, 4, or 8 bytes,
//! big-endian, with the size in the top two bits of the first byte.

#![deny(
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::as_conversions,
    clippy::string_slice
)]

use std::ops::Deref;

/// A value in the fewest varint bytes. It derefs to them.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Varint {
    bytes: [u8; 8],
    len: usize,
}

impl Varint {
    /// 2^62 − 1, the largest value, in 8 bytes.
    pub(crate) const MAX: Self = Self {
        bytes: [0xff; 8],
        len: 8,
    };

    /// `value` in the fewest bytes, or `None` when it is 2^62 or more.
    pub(crate) fn new(value: impl TryInto<u64>) -> Option<Self> {
        let value = value.try_into().ok().filter(|&value| value < 1 << 62)?;
        let (tag, len, shift) = match value {
            0..64 => (0, 1, 56),
            64..16_384 => (0x40 << 56, 2, 48),
            16_384..1_073_741_824 => (0x80 << 56, 4, 32),
            _ => (0xc0 << 56, 8, 0),
        };
        let bytes = (value.wrapping_shl(shift) | tag).to_be_bytes();
        Some(Self { bytes, len })
    }
}

impl Deref for Varint {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        self.bytes
            .get(..self.len)
            .expect("invariant: a varint is at most 8 bytes")
    }
}

/// The bytes of the varint whose first byte is `first`.
pub(crate) fn size(first: u8) -> usize {
    match first >> 6 {
        0 => 1,
        1 => 2,
        2 => 4,
        _ => 8,
    }
}

/// The value of `bytes`, one whole varint.
///
/// # Panics
///
/// When `bytes` is empty or its length is not the [`size`] of its first byte.
pub(crate) fn value(bytes: &[u8]) -> u64 {
    let (&first, rest) = bytes.split_first().expect("a varint has a first byte");
    assert_eq!(bytes.len(), size(first), "the bytes of one varint");
    rest.iter().fold(u64::from(first & 0x3f), |value, &byte| {
        value.wrapping_shl(8) | u64::from(byte)
    })
}

/// Takes the varint at the front of `bytes`, and moves `bytes` past it. `None` when
/// `bytes` ends first.
pub(crate) fn take(bytes: &mut &[u8]) -> Option<u64> {
    let (varint, rest) = bytes.split_at_checked(size(*bytes.first()?))?;
    *bytes = rest;
    Some(value(varint))
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    /// The value of `bytes`, when they are one whole varint.
    fn decode(mut bytes: &[u8]) -> Option<u64> {
        let value = take(&mut bytes)?;
        bytes.is_empty().then_some(value)
    }

    /// `value` in `len` bytes, which may be more than the fewest.
    fn wide(value: u64, len: usize) -> Vec<u8> {
        let (tag, skip) = match len {
            1 => (0, 7),
            2 => (0x40, 6),
            4 => (0x80, 4),
            _ => (0xc0, 0),
        };
        let mut bytes = value.to_be_bytes().into_iter().skip(skip);
        let first = bytes.next().expect("a first byte") | tag;
        [first].into_iter().chain(bytes).collect()
    }

    #[test]
    fn takes_the_fewest_bytes() {
        let lens = [
            (0_u64, 1),
            (63, 1),
            (64, 2),
            (16_383, 2),
            (16_384, 4),
            ((1 << 30) - 1, 4),
            (1 << 30, 8),
            ((1 << 62) - 1, 8),
        ];
        for (value, len) in lens {
            let varint = Varint::new(value).expect("a varint");
            assert_eq!(varint.len(), len, "{value}");
        }
    }

    #[test]
    fn matches_rfc_9000() {
        // RFC 9000 appendix A.1.
        let vectors: [(u64, &[u8]); 4] = [
            (
                151_288_809_941_952_652,
                &[0xc2, 0x19, 0x7c, 0x5e, 0xff, 0x14, 0xe8, 0x8c],
            ),
            (494_878_333, &[0x9d, 0x7f, 0x3e, 0x7d]),
            (15_293, &[0x7b, 0xbd]),
            (37, &[0x25]),
        ];
        for (value, bytes) in vectors {
            let varint = Varint::new(value).expect("a varint");
            assert_eq!(&*varint, bytes, "{value}");
            assert_eq!(decode(bytes), Some(value), "{value}");
        }
        assert_eq!(decode(&[0x40, 0x25]), Some(37));
    }

    #[test]
    fn new_refuses_2_to_the_62_and_more() {
        let max = Varint::new((1_u64 << 62) - 1).expect("a varint");
        assert_eq!(&*max, &*Varint::MAX);
        assert_eq!(decode(&Varint::MAX), Some((1 << 62) - 1));
        for value in [1 << 62, u64::MAX] {
            assert!(Varint::new(value).is_none(), "{value}");
        }
        assert!(Varint::new(-1).is_none());
    }

    #[test]
    #[should_panic(expected = "the bytes of one varint")]
    fn value_panics_when_the_bytes_are_not_one_varint() {
        let _ = value(&[0x40]);
    }

    proptest! {
        #[test]
        fn take_gives_what_new_encoded(
            value in 0..1_u64 << 62,
            rest in prop::collection::vec(any::<u8>(), 0..4),
        ) {
            let varint = Varint::new(value).expect("a varint");
            let bytes = [&*varint, &rest].concat();
            let mut taken = bytes.as_slice();
            prop_assert_eq!(take(&mut taken), Some(value));
            prop_assert_eq!(taken, rest.as_slice());
        }

        #[test]
        fn take_gives_none_for_each_cut(value in 0..1_u64 << 62) {
            let varint = Varint::new(value).expect("a varint");
            for cut in 0..varint.len() {
                let mut bytes = varint.get(..cut).expect("a cut");
                prop_assert_eq!(take(&mut bytes), None, "{} bytes", cut);
            }
        }

        #[test]
        fn take_reads_a_value_in_more_bytes_than_the_fewest(
            value in 0..1_u64 << 62,
            (len, mask) in prop::sample::select(vec![
                (1, 0x3f),
                (2, 0x3fff),
                (4, 0x3fff_ffff),
                (8, 0x3fff_ffff_ffff_ffff),
            ]),
            narrow in prop::sample::select(vec![0x3f, 0x3fff, 0x3fff_ffff, u64::MAX]),
        ) {
            let value = value & mask & narrow;
            prop_assert_eq!(decode(&wide(value, len)), Some(value));
        }
    }
}
