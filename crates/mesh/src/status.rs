//! The status channels of a node.

use std::collections::BTreeMap;
use std::fmt;

use types::channel;
use types::name::Name;

use crate::bytes::{put_channel, put_count, put_name, take_channel, take_count};
use crate::bytes::{take_name, take_rising};

// A node's status channels are a fixed set per release; the cap leaves room for later
// releases. Every member keeps every member's status, so one node must not set the
// size of each member's state.
const MAX: u8 = 64;

/// A node's status channel keys, by name under the node's name: at most 64.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Status(BTreeMap<Name, channel::Key>);

impl Status {
    /// The entries of `map`.
    ///
    /// # Errors
    ///
    /// [`Many`] for more than 64 entries.
    pub fn new(map: BTreeMap<Name, channel::Key>) -> Result<Self, Many> {
        let count = map.len();
        if count > usize::from(MAX) {
            return Err(Many { count });
        }
        Ok(Self(map))
    }

    /// The entries, by name.
    #[must_use]
    pub fn as_map(&self) -> &BTreeMap<Name, channel::Key> {
        &self.0
    }

    /// Adds the count, then each entry in name order: the name, then the channel key
    /// as 16 little-endian bytes.
    pub(crate) fn encode(&self, out: &mut Vec<u8>) {
        put_count(self.0.len(), out);
        for (name, &key) in &self.0 {
            put_name(name, out);
            put_channel(key, out);
        }
    }

    /// Takes what [`Status::encode`] gives. `None` when the count is over 64, before
    /// any entry is read, or when the names are not in rising order.
    pub(crate) fn decode(bytes: &mut &[u8]) -> Option<Self> {
        // Needs no check of `new`: the count is checked first.
        let mut count = *bytes;
        if take_count(&mut count)? > u64::from(MAX) {
            return None;
        }
        let mut map = BTreeMap::new();
        take_rising(bytes, take_name, |name, bytes| {
            map.insert(name, take_channel(bytes)?);
            Some(())
        })?;
        Some(Self(map))
    }
}

/// More than 64 status entries. Text: "{count} status entries, more than 64".
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Many {
    /// The number of entries.
    pub count: usize,
}

impl fmt::Display for Many {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} status entries, more than {MAX}", self.count)
    }
}

impl std::error::Error for Many {}

#[cfg(test)]
mod tests {
    use super::*;

    fn entries(count: u128) -> BTreeMap<Name, channel::Key> {
        (0..count)
            .map(|i| {
                (
                    format!("s{i:02}").parse().unwrap(),
                    channel::Key::from_u128(i),
                )
            })
            .collect()
    }

    #[test]
    fn a_status_holds_at_most_64_entries() {
        let most = entries(64);
        assert_eq!(
            Status::new(most.clone()).map(|s| s.as_map().clone()),
            Ok(most)
        );
        let many = Status::new(entries(65));
        assert_eq!(many, Err(Many { count: 65 }));
        let text = "65 status entries, more than 64";
        assert_eq!(many.unwrap_err().to_string(), text);
    }
}
