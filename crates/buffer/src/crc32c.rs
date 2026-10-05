//! CRC32C (Castagnoli), with the hardware instruction where the CPU has one.

/// A running CRC32C over the bytes given so far.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Crc32c(u32);

impl Crc32c {
    pub(crate) const fn new() -> Self {
        Self(0)
    }

    /// Continues from the `finish` value of earlier bytes.
    pub(crate) const fn resume(crc: u32) -> Self {
        Self(crc)
    }

    pub(crate) fn update(&mut self, bytes: &[u8]) {
        self.0 = crc32c::crc32c_append(self.0, bytes);
    }

    pub(crate) const fn finish(self) -> u32 {
        self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn crc(parts: &[&[u8]]) -> u32 {
        let mut crc = Crc32c::new();
        for part in parts {
            crc.update(part);
        }
        crc.finish()
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
            let mut resumed = Crc32c::resume(crc(&[head]));
            resumed.update(tail);
            prop_assert_eq!(resumed.finish(), crc(&[&bytes]));
        }
    }
}
