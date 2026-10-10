//! Fuzz entry points, for the fuzz targets only. Not a stable surface.

use crate::identity::{self, LEN};
use crate::sector;

/// Decodes `data` as the bytes of `node.key`: 100 bytes as they are, or 96 bytes with
/// their CRC32C appended. Ignores any other length.
///
/// # Panics
///
/// When the decode gives an identity for bytes with a wrong tag or CRC32C, refuses
/// bytes that have both, or gives an identity that does not encode to the bytes.
pub fn identity(data: &[u8]) {
    let bytes = <[u8; LEN]>::try_from(data).ok();
    if let Some(bytes) = bytes.or_else(|| sector::summed(data)) {
        identity::check(&bytes);
    }
}
