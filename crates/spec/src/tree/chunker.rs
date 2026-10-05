//! Cuts a run of entries into chunks at boundaries that the content decides.

use super::chunk;

/// The scale of the chunk size distribution, in bytes.
use types::digest::Digest;

pub(super) const SCALE: u32 = 4096;

// A larger scale takes the powers in `boundary` out of range.
const _: () = assert!(SCALE <= 8192, "the boundary rule overflows");

/// Reports whether a chunk ends after the entry with `key`, which fills the chunk's
/// entry bytes from `start` to `end`.
///
/// The chance grows with the chunk's size (a Weibull hazard of shape 4), so sizes stay
/// near `scale`. Only the key is hashed, so a new value of the same size moves no
/// boundary. The rule uses integers only, so every platform cuts at the same place.
///
/// A chunk above the leaves never ends after its first entry. Each level then has at
/// most half the chunks of the level below, for keys of any size.
#[expect(
    clippy::arithmetic_side_effects,
    reason = "the first check keeps each operand below 2^15 for a scale up to 8192"
)]
fn boundary(scale: u32, level: u8, key: &[u8], start: usize, end: usize) -> bool {
    if level > 0 && start == 0 {
        return false;
    }
    let scale = u128::from(scale);
    let start = u128::try_from(start).unwrap_or(u128::MAX);
    let end = u128::try_from(end).unwrap_or(u128::MAX);
    if end >= 4 * scale {
        return true;
    }
    let mut keyed = Vec::with_capacity(key.len() + 1);
    keyed.push(level);
    keyed.extend_from_slice(key);
    let digest = Digest::of(&keyed);
    let (draw, _) = digest.0.split_at(8);
    let draw = u64::from_le_bytes(draw.try_into().expect("invariant: split at 8"));
    // draw / 2^64 < (end^4 - start^4) / scale^4
    u128::from(draw) * scale.pow(4) < (end.pow(4) - start.pow(4)) << 64
}

/// Builds the chunks of one level from entries in key order.
pub(super) struct Writer {
    scale: u32,
    level: u8,
    chunk: Vec<u8>,
    last: Vec<u8>,
    done: Vec<(Vec<u8>, Vec<u8>)>,
}

impl Writer {
    pub(super) fn new(scale: u32, level: u8) -> Self {
        Self {
            scale,
            level,
            chunk: vec![level],
            last: Vec::new(),
            done: Vec::new(),
        }
    }

    pub(super) fn push(&mut self, key: &[u8], payload: &[u8]) {
        let start = self.chunk.len().saturating_sub(1);
        chunk::write(&mut self.chunk, self.level, key, payload);
        let end = self.chunk.len().saturating_sub(1);
        self.last.clear();
        self.last.extend_from_slice(key);
        if boundary(self.scale, self.level, key, start, end) {
            self.cut();
        }
    }

    /// Reports whether the last entry ended a chunk, or no entry was pushed.
    pub(super) fn at_boundary(&self) -> bool {
        self.chunk.len() == 1
    }

    /// Ends the level. Returns the last key and the bytes of each chunk.
    pub(super) fn finish(mut self) -> Vec<(Vec<u8>, Vec<u8>)> {
        if !self.at_boundary() {
            self.cut();
        }
        self.done
    }

    fn cut(&mut self) {
        let chunk = std::mem::replace(&mut self.chunk, vec![self.level]);
        self.done.push((self.last.clone(), chunk));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_chunk_ends_at_four_times_the_scale() {
        let keys = || (0..1000_u32).map(u32::to_le_bytes);
        for key in keys() {
            assert!(boundary(SCALE, 0, &key, 16_383, 16_384), "{key:?}");
            assert!(boundary(SCALE, 0, &key, 16_383, 1 << 30), "{key:?}");
        }
        let cut = keys().filter(|key| boundary(SCALE, 0, key, 16_382, 16_383));
        assert!(cut.count() < 500);
    }

    #[test]
    fn a_writer_cuts_where_the_boundaries_are() {
        let mut writer = Writer::new(64, 1);
        assert!(writer.at_boundary());
        let mut ends = Vec::new();
        let mut start = 0;
        for id in 0..200_u8 {
            writer.push(&[id], &[0; 32]);
            let end = start + 34;
            start = if boundary(64, 1, &[id], start, end) {
                ends.push(vec![id]);
                0
            } else {
                end
            };
            assert_eq!(writer.at_boundary(), start == 0, "{id}");
        }
        if start != 0 {
            ends.push(vec![199]);
        }
        let chunks = writer.finish();
        let lasts: Vec<_> = chunks.iter().map(|(last, _)| last.clone()).collect();
        assert_eq!(lasts, ends);
        assert!(chunks.len() > 20);
        let size: usize = chunks.iter().map(|(_, bytes)| bytes.len() - 1).sum();
        assert_eq!(size, 200 * 34);
    }
}
