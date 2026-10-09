//! Entry points for the fuzz targets only. Not a stable surface.

use crate::identity::{self, LEN};

/// Decodes `data` as the bytes of `node.key`: 68 bytes as they are, or 64 bytes with
/// their CRC32C appended. Ignores any other length.
///
/// # Panics
///
/// When the decode gives an identity for bytes with a wrong tag or CRC32C, refuses
/// bytes that have both, or gives an identity that does not encode to the bytes.
pub fn identity(data: &[u8]) {
    if let Ok(bytes) = <&[u8; LEN]>::try_from(data) {
        identity::check(bytes);
    } else if let Ok(body) = <&[u8; 64]>::try_from(data) {
        identity::check(&identity::with_checksum(body));
    }
}
