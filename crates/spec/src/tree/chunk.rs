//! The bytes of one chunk: a level, then entries in key order.
//!
//! A leaf (level 0) entry is a key and a value. An entry of a higher level is the
//! last key of a child chunk and the child's hash. A length is a LEB128 `u32`.

use types::digest::Digest;

use super::Error;

/// One entry of a chunk. `payload` is a value in a leaf and a child hash above.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Entry<'a> {
    pub key: &'a [u8],
    pub payload: &'a [u8],
}

impl Entry<'_> {
    pub(super) fn child(&self) -> Digest {
        let bytes = self.payload.try_into();
        Digest(bytes.expect("invariant: an entry above a leaf holds a 32-byte hash"))
    }
}

/// A chunk that was read and checked.
#[derive(Debug)]
pub(super) struct Node<'a> {
    pub hash: Digest,
    pub level: u8,
    pub entries: Vec<Entry<'a>>,
}

impl<'a> Node<'a> {
    /// Reads the chunk `bytes`, whose hash is `hash`.
    pub(super) fn read(hash: Digest, bytes: &'a [u8]) -> Result<Self, Error> {
        let corrupt = Error::Corrupt(hash);
        let (&level, mut rest) = bytes.split_first().ok_or(corrupt)?;
        let mut entries = Vec::new();
        while !rest.is_empty() {
            let key = take_bytes(&mut rest).ok_or(corrupt)?;
            let payload = if level == 0 {
                take_bytes(&mut rest)
            } else {
                rest.split_off(..32)
            };
            let sorted = entries.last().is_none_or(|last: &Entry<'_>| last.key < key);
            if !sorted {
                return Err(corrupt);
            }
            entries.push(Entry {
                key,
                payload: payload.ok_or(corrupt)?,
            });
        }
        if level > 0 && entries.is_empty() {
            return Err(corrupt);
        }
        Ok(Self {
            hash,
            level,
            entries,
        })
    }

    pub(super) fn last_key(&self) -> Option<&'a [u8]> {
        self.entries.last().map(|entry| entry.key)
    }
}

/// Appends one entry to the bytes of a chunk at `level`.
pub(super) fn write(chunk: &mut Vec<u8>, level: u8, key: &[u8], payload: &[u8]) {
    write_bytes(chunk, key);
    if level == 0 {
        write_bytes(chunk, payload);
    } else {
        chunk.extend_from_slice(payload);
    }
}

fn write_bytes(chunk: &mut Vec<u8>, bytes: &[u8]) {
    let length = u32::try_from(bytes.len());
    let mut length = length.expect("invariant: a key or value is under 4 GiB");
    loop {
        let [low, ..] = length.to_le_bytes();
        length /= 0x80;
        if length == 0 {
            chunk.push(low & 0x7F);
            break;
        }
        chunk.push(low | 0x80);
    }
    chunk.extend_from_slice(bytes);
}

// Refuses a length that `write_bytes` does not make, so one entry has one encoding.
fn take_bytes<'a>(rest: &mut &'a [u8]) -> Option<&'a [u8]> {
    let mut length = 0_u32;
    for place in [1_u32, 0x80, 0x4000, 0x20_0000, 0x1000_0000] {
        let (&byte, tail) = rest.split_first()?;
        *rest = tail;
        if byte == 0 && place > 1 {
            return None;
        }
        let part = u32::from(byte & 0x7F).checked_mul(place)?;
        length = length.checked_add(part)?;
        if byte & 0x80 == 0 {
            return rest.split_off(..usize::try_from(length).ok()?);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chunk(level: u8, entries: &[(&[u8], &[u8])]) -> Vec<u8> {
        let mut bytes = vec![level];
        for (key, payload) in entries {
            write(&mut bytes, level, key, payload);
        }
        bytes
    }

    #[test]
    fn reads_the_leaf_entries_it_wrote() {
        let long = vec![7_u8; 300];
        let bytes = chunk(0, &[(b"", b"x"), (b"a", b""), (b"b.c", &long)]);
        let node = Node::read(Digest::of(&bytes), &bytes).unwrap();
        assert_eq!(node.level, 0);
        let entries: Vec<_> = node.entries.iter().map(|e| (e.key, e.payload)).collect();
        assert_eq!(
            entries,
            [(&b""[..], &b"x"[..]), (b"a", b""), (b"b.c", &long)]
        );
        assert_eq!(node.last_key(), Some(&b"b.c"[..]));
    }

    #[test]
    fn reads_the_child_hashes_it_wrote() {
        let child = Digest::of(b"child");
        let bytes = chunk(2, &[(b"site_a.pt_9", &child.0)]);
        let node = Node::read(Digest::of(&bytes), &bytes).unwrap();
        assert_eq!((node.level, node.entries.len()), (2, 1));
        assert_eq!(node.entries[0].child(), child);
    }

    #[test]
    fn rejects_bytes_that_are_not_a_chunk() {
        let whole = chunk(0, &[(b"key", b"value")]);
        let short_digest = chunk(1, &[(b"key", &[0; 31])]);
        let cases: [&[u8]; 11] = [
            b"",
            &[1],
            // Keys out of order, and a key twice.
            &[0, 1, b'b', 0, 1, b'a', 0],
            &[0, 1, b'a', 0, 1, b'a', 0],
            // Lengths in a longer form than needed, or over 32 bits.
            &[0, 0x81, 0, b'a', 0],
            &[0, 0x80, 0x80, 0x80, 0x80, 0, 0],
            &[0, 0x80, 0x80, 0x80, 0x80, 0x10, 0],
            &whole[..whole.len() - 1],
            &whole[..2],
            &short_digest,
            &[0, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF],
        ];
        for bytes in cases {
            let hash = Digest::of(bytes);
            let err = Node::read(hash, bytes).unwrap_err();
            assert_eq!(err, Error::Corrupt(hash), "{bytes:?}");
        }
    }
}
