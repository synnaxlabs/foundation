//! Little-endian integer samples of `W` bytes, held as `u64`.

/// The bits a sample of `width` bytes holds.
pub(crate) const fn mask(width: usize) -> u64 {
    match width {
        1 => 0xff,
        2 => 0xffff,
        4 => 0xffff_ffff,
        _ => u64::MAX,
    }
}

/// The bit width of a sample of `width` bytes.
pub(crate) const fn bits(width: usize) -> u8 {
    match width {
        1 => 8,
        2 => 16,
        4 => 32,
        _ => 64,
    }
}

/// The samples in `bytes`, zero-extended.
pub(crate) fn samples<const W: usize>(
    bytes: &[u8],
) -> impl Iterator<Item = u64> + Clone + '_ {
    bytes.as_chunks::<W>().0.iter().map(load)
}

/// One sample, zero-extended.
pub(crate) fn load<const W: usize>(sample: &[u8; W]) -> u64 {
    let mut word = [0; 8];
    word.split_at_mut(W).0.copy_from_slice(sample);
    u64::from_le_bytes(word)
}

/// The low `W` bytes of `value`.
pub(crate) fn store<const W: usize>(value: u64) -> [u8; W] {
    *value
        .to_le_bytes()
        .first_chunk()
        .expect("invariant: a sample is at most 8 bytes")
}

/// Reads field `index` of a vector header: `width` bytes after the tag and the bit
/// width.
pub(crate) fn field(header: &[u8], index: usize, width: usize) -> u64 {
    header
        .iter()
        .skip(index.strict_mul(width).strict_add(2))
        .take(width)
        .rev()
        .fold(0, |value, byte| value.wrapping_shl(8) | u64::from(*byte))
}
