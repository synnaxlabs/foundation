//! Cuts a run of entries into chunks at boundaries that the content decides.

use super::chunk;

/// The scale of the chunk size distribution, in bytes.
pub(super) const SCALE: u32 = 4096;

/// Reports whether a chunk ends after the entry with `key`, which fills the chunk's
/// entry bytes from `start` to `end`.
///
/// The chance grows with the chunk's size (a Weibull hazard of shape 4), so sizes stay
/// near `scale`. Only the key is hashed, so a new value of the same size moves no
/// boundary. The rule uses integers only, so every platform cuts at the same place.
fn boundary(scale: u32, level: u8, key: &[u8], start: usize, end: usize) -> bool {
    let scale = u128::from(scale);
    let (start, end) = (start as u128, end as u128);
    if end >= 4 * scale {
        return true;
    }
    let mut hasher = blake3::Hasher::new();
    hasher.update(&[level]).update(key);
    let hash = hasher.finalize();
    let (draw, _) = hash.as_bytes().split_at(8);
    let draw = u64::from_le_bytes(draw.try_into().expect("invariant: split at 8"));
    // draw / 2^64 < (end^4 - start^4) / scale^4
    u128::from(draw) * scale.pow(4) < (end.pow(4) - start.pow(4)) << 64
}

/// A finished chunk: its last key and its bytes.
pub(super) type Chunk = (Vec<u8>, Vec<u8>);

/// Builds the chunks of one level from entries in key order.
pub(super) struct Writer {
    scale: u32,
    level: u8,
    chunk: Vec<u8>,
    last: Vec<u8>,
    done: Vec<Chunk>,
}

impl Writer {
    /// `scale` must be at most 8192, so that the boundary rule cannot overflow.
    pub fn new(scale: u32, level: u8) -> Self {
        Self {
            scale,
            level,
            chunk: vec![level],
            last: Vec::new(),
            done: Vec::new(),
        }
    }

    pub fn push(&mut self, key: &[u8], payload: &[u8]) {
        let start = self.chunk.len() - 1;
        chunk::write(&mut self.chunk, self.level, key, payload);
        let end = self.chunk.len() - 1;
        self.last.clear();
        self.last.extend_from_slice(key);
        if boundary(self.scale, self.level, key, start, end) {
            self.cut();
        }
    }

    /// Reports whether the last entry ended a chunk, or no entry was pushed.
    pub fn at_boundary(&self) -> bool {
        self.chunk.len() == 1
    }

    pub fn finish(mut self) -> Vec<Chunk> {
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
