//! Channel identity.

use std::fmt;
use std::str::FromStr;

/// A channel's identity: a UUIDv7 made with the channel. It never changes and is never
/// reused. Files never hold it; the stored spec maps each name to its key.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Key(u128);

impl Key {
    /// Makes a UUIDv7 key from a time and random bits.
    #[must_use]
    pub fn v7(time: crate::time::Stamp, random: u128) -> Self {
        let _ = (time, random);
        todo!()
    }

    /// Wraps a key's 128 bits.
    #[must_use]
    pub const fn from_u128(bits: u128) -> Self {
        Self(bits)
    }

    /// The key's 128 bits.
    #[must_use]
    pub const fn as_u128(self) -> u128 {
        self.0
    }
}

impl fmt::Display for Key {
    /// Writes the key as a hyphenated UUID string.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let _ = f;
        todo!()
    }
}

impl FromStr for Key {
    type Err = crate::ParseError;

    /// Reads a hyphenated UUID string.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let _ = s;
        todo!()
    }
}

/// A node-local number for a channel, used on the hot path in place of its key. A
/// slot is never sent on the wire or stored on disk.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Slot(u32);

impl Slot {
    /// Wraps a slot number.
    #[must_use]
    pub const fn new(n: u32) -> Self {
        Self(n)
    }

    /// The slot number.
    #[must_use]
    pub const fn get(self) -> u32 {
        self.0
    }
}
