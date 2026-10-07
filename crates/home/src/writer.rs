//! What a writer opens with on the indexes of one shard, the key of an open writer,
//! and why a writer did not open.

use std::fmt;
use std::sync::Arc;

use types::authority::Authority;
use types::frame::key_set::KeySet;
use types::name::Name;
use types::time::Span;

/// What a writer opens with.
#[derive(Clone, Debug)]
pub(crate) struct Writer {
    /// The subject that opened the writer.
    pub(crate) subject: Name,
    /// The writer's authority. The home does not cap it by access yet.
    pub(crate) authority: Authority,
    /// How long the writer may go without a write and keep control, or `None` for
    /// no limit.
    pub(crate) lease: Option<Span>,
    /// The key set of every frame the writer writes.
    pub(crate) set: Arc<KeySet>,
}

/// An open writer on its shard.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct Key {
    /// The number of the shard that opened the writer.
    pub(crate) shard: u32,
    /// The writer's number on its shard.
    pub(crate) number: u64,
}

impl Key {
    /// The writer's number on shard `shard`.
    ///
    /// # Panics
    ///
    /// If the key is of another shard, whose writers count from 0 too.
    pub(crate) fn on(self, shard: u32) -> u64 {
        assert!(
            self.shard == shard,
            "writer {} is of shard {}, not shard {shard}",
            self.number,
            self.shard
        );
        self.number
    }
}

/// Why a writer did not open. Nothing changed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Error {
    /// The node has no mesh time yet. Open the writer again later.
    Unsynced,
    /// A control lease must be longer than zero.
    Lease {
        /// The span that was asked for.
        span: Span,
    },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Self::Unsynced => f.write_str(
                "the node has no mesh time yet: open the writer again later",
            ),
            Self::Lease { span } => control::lease::Error { span }.fmt(f),
        }
    }
}

impl std::error::Error for Error {}

impl From<control::lease::Error> for Error {
    fn from(error: control::lease::Error) -> Self {
        Self::Lease { span: error.span }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn says_what_to_do_for_each_error() {
        let lease = Error::Lease { span: Span::ZERO };

        assert_eq!(
            Error::Unsynced.to_string(),
            "the node has no mesh time yet: open the writer again later"
        );
        assert_eq!(
            lease.to_string(),
            format!("control lease must be longer than zero, got {}", Span::ZERO)
        );
    }

    #[test]
    fn keeps_the_span_of_a_lease_error() {
        let span = Span::from_nanos(-3);

        assert_eq!(
            Error::from(control::lease::Error { span }),
            Error::Lease { span }
        );
    }

    #[test]
    #[should_panic(expected = "writer 7 is of shard 5, not shard 9")]
    fn panics_on_a_key_of_another_shard() {
        let key = Key {
            shard: 5,
            number: 7,
        };

        key.on(9);
    }
}
