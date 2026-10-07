//! The region state that the voters agree on, and the change records that move it.

use std::collections::BTreeMap;
use std::fmt;

use types::{channel, node};

use crate::bytes::put_key;
use crate::member::Member;

/// The region state that this node holds: its members, and the homes that it applied.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct State {
    members: BTreeMap<node::Key, Member>,
    homes: BTreeMap<channel::Key, node::Key>,
}

impl State {
    /// A state with `members` and no home.
    pub(crate) const fn new(members: BTreeMap<node::Key, Member>) -> Self {
        Self {
            members,
            homes: BTreeMap::new(),
        }
    }

    /// The member with `key`, or `None` when the region has no such member.
    pub(crate) fn member(&self, key: node::Key) -> Option<&Member> {
        self.members.get(&key)
    }

    /// The home of `index`, or `None` when none is set.
    pub(crate) fn home(&self, index: channel::Key) -> Option<node::Key> {
        self.homes.get(&index).copied()
    }

    /// Applies `change`. Returns the index whose home it moved, or `None` when the home
    /// was already that node.
    pub(crate) fn apply(&mut self, change: Change) -> Option<channel::Key> {
        match change {
            Change::Home { index, home } => {
                (self.homes.insert(index, home) != Some(home)).then_some(index)
            }
        }
    }
}

/// A change record: the data of one log entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Change {
    /// Makes `home` the home of `index`.
    Home {
        /// The index channel.
        index: channel::Key,
        /// The node that stores it.
        home: node::Key,
    },
}

const HOME: u8 = 1;

impl Change {
    /// Adds the one byte form of the change to `out`: a kind byte, then each key as
    /// 16 little-endian bytes.
    pub(crate) fn encode(self, out: &mut Vec<u8>) {
        match self {
            Self::Home { index, home } => {
                out.push(HOME);
                out.extend(index.as_u128().to_le_bytes());
                put_key(home, out);
            }
        }
    }

    /// Decodes the bytes that [`Change::encode`] gives, and no others.
    ///
    /// # Errors
    ///
    /// [`Malformed::Kind`] when the kind byte is unknown, and [`Malformed::Length`]
    /// when the bytes are not the length of their kind.
    pub(crate) fn decode(bytes: &[u8]) -> Result<Self, Malformed> {
        let length = || Malformed::Length { found: bytes.len() };
        let (&kind, rest) = bytes.split_first().ok_or_else(length)?;
        if kind != HOME {
            return Err(Malformed::Kind { kind });
        }
        let (&index, rest) = rest.split_first_chunk().ok_or_else(length)?;
        let home = <[u8; 16]>::try_from(rest).map_err(|_wrong_length| length())?;
        Ok(Self::Home {
            index: channel::Key::from_u128(u128::from_le_bytes(index)),
            home: node::Key::from_u128(u128::from_le_bytes(home)),
        })
    }
}

/// Bytes that are not a change record.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Malformed {
    /// The kind byte names no change.
    Kind {
        /// The kind byte.
        kind: u8,
    },
    /// The bytes are not the length of their kind.
    Length {
        /// The length of the bytes.
        found: usize,
    },
}

impl fmt::Display for Malformed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Kind { kind } => write!(f, "change kind {kind} is unknown"),
            Self::Length { found } => {
                write!(f, "a change of {found} bytes is not 33 bytes long")
            }
        }
    }
}

impl std::error::Error for Malformed {}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    fn index(bits: u128) -> channel::Key {
        channel::Key::from_u128(bits)
    }

    fn node(bits: u128) -> node::Key {
        node::Key::from_u128(bits)
    }

    fn change() -> impl Strategy<Value = Change> {
        (any::<u128>(), any::<u128>()).prop_map(|(i, h)| Change::Home {
            index: index(i),
            home: node(h),
        })
    }

    #[test]
    fn a_home_is_none_until_a_change_sets_it() {
        let mut state = State::new(BTreeMap::new());
        assert_eq!(state.home(index(7)), None);
        let moved = state.apply(Change::Home {
            index: index(7),
            home: node(1),
        });
        assert_eq!(moved, Some(index(7)));
        assert_eq!(state.home(index(7)), Some(node(1)));
        assert_eq!(state.home(index(8)), None);
    }

    #[test]
    fn a_change_to_the_same_home_moves_nothing() {
        let mut state = State::new(BTreeMap::new());
        let change = Change::Home {
            index: index(7),
            home: node(1),
        };
        state.apply(change);
        assert_eq!(state.apply(change), None);
        let moved = state.apply(Change::Home {
            index: index(7),
            home: node(2),
        });
        assert_eq!(moved, Some(index(7)));
        assert_eq!(state.home(index(7)), Some(node(2)));
    }

    fn encoded(change: Change) -> Vec<u8> {
        let mut out = Vec::new();
        change.encode(&mut out);
        out
    }

    #[test]
    fn a_home_change_has_a_fixed_byte_form() {
        let change = Change::Home {
            index: index(0x0102),
            home: node(0x0a0b),
        };
        let mut expected = vec![1, 0x02, 0x01];
        expected.extend([0; 14]);
        expected.extend([0x0b, 0x0a]);
        expected.extend([0; 14]);
        assert_eq!(encoded(change), expected);
    }

    #[test]
    fn decode_refuses_an_unknown_kind() {
        let mut bytes = encoded(Change::Home {
            index: index(1),
            home: node(2),
        });
        bytes[0] = 2;
        let error = Change::decode(&bytes).unwrap_err();
        assert_eq!(error, Malformed::Kind { kind: 2 });
        assert_eq!(error.to_string(), "change kind 2 is unknown");
    }

    #[test]
    fn decode_refuses_bytes_of_another_length() {
        let bytes = encoded(Change::Home {
            index: index(1),
            home: node(2),
        });
        for found in [0, 1, 17, 32, 34] {
            let mut cut = bytes.clone();
            cut.resize(found, 0);
            let error = Change::decode(&cut).unwrap_err();
            assert_eq!(error, Malformed::Length { found });
            assert_eq!(
                error.to_string(),
                format!("a change of {found} bytes is not 33 bytes long")
            );
        }
    }

    proptest! {
        #[test]
        fn a_change_round_trips(change in change()) {
            prop_assert_eq!(Change::decode(&encoded(change)), Ok(change));
        }

        // Every byte form that decodes is the one that its change encodes to.
        #[test]
        fn a_change_has_one_byte_form(
            bytes in prop::collection::vec(any::<u8>(), 0..40),
        ) {
            if let Ok(change) = Change::decode(&bytes) {
                prop_assert_eq!(encoded(change), bytes);
            }
        }

        // The applied state is the last home that each index was given.
        #[test]
        fn the_state_keeps_the_last_home_of_each_index(
            changes in prop::collection::vec((0..4u128, 0..3u128), 0..32),
        ) {
            let mut state = State::new(BTreeMap::new());
            let mut last = BTreeMap::new();
            for (i, h) in changes {
                let before = last.insert(i, h);
                let change = Change::Home { index: index(i), home: node(h) };
                let moved = state.apply(change);
                prop_assert_eq!(moved, (before != Some(h)).then_some(index(i)));
            }
            for i in 0..4 {
                prop_assert_eq!(state.home(index(i)), last.get(&i).map(|&h| node(h)));
            }
        }
    }
}
