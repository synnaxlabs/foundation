//! CRC32C (Castagnoli), with the hardware instruction where the CPU has one.

/// Continues `crc` over `bytes`. A CRC starts at `0`: the value after one call over
/// `a` and then one over `b` equals the value of one call over `a` then `b`.
pub(crate) fn append(crc: u32, bytes: &[u8]) -> u32 {
    crc32c::crc32c_append(crc, bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn crc(parts: &[&[u8]]) -> u32 {
        parts.iter().fold(0, |crc, part| append(crc, part))
    }

    /// The check value of the CRC catalogue, then the iSCSI test vectors.
    #[test]
    fn matches_the_standard_vectors() {
        let up: Vec<u8> = (0..32).collect();
        let down: Vec<u8> = (0..32).rev().collect();
        let vectors: [(&[u8], u32); 5] = [
            (b"123456789", 0xE306_9283),
            (&[0x00; 32], 0x8A91_36AA),
            (&[0xFF; 32], 0x62A8_AB43),
            (&up, 0x46DD_794E),
            (&down, 0x113F_DB5C),
        ];
        for (bytes, expected) in vectors {
            assert_eq!(crc(&[bytes]), expected, "bytes {bytes:02x?}");
        }
    }

    proptest! {
        #[test]
        fn a_split_or_resumed_update_equals_one_update(
            bytes in prop::collection::vec(any::<u8>(), 0..512),
            cut in any::<prop::sample::Index>(),
        ) {
            let (head, tail) = bytes.split_at(cut.index(bytes.len() + 1));
            prop_assert_eq!(crc(&[head, tail]), crc(&[&bytes]));
            prop_assert_eq!(append(crc(&[head]), tail), crc(&[&bytes]));
        }
    }
}
