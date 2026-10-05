//! Little-endian integer samples of `W` bytes, held as `u64`.

/// The bits a sample of `width` bytes holds.
pub(crate) fn mask(width: usize) -> u64 {
    u64::MAX.unbounded_shr(64_u32.strict_sub(bits(width).into()))
}

/// The bit count of a sample of `width` bytes.
pub(crate) fn bits(width: usize) -> u8 {
    u8::try_from(width.strict_mul(8)).expect("invariant: a sample is at most 16 bytes")
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
