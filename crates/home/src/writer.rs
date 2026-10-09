//! What a writer opens with on the indexes of one shard, the key of an open writer,
//! and why a writer did not open.

use std::fmt;
use std::sync::Arc;

use control::lease::Lease;
use types::authority::Authority;
use types::frame::key_set::KeySet;
use types::name::Name;
use types::time::Span;

/// What a writer opens with.
#[derive(Clone, Debug)]
pub struct Writer {
    /// The subject that opened the writer.
    pub subject: Name,
    /// The writer's authority. The home does not cap it by access yet.
    pub authority: Authority,
    /// The control lease, or `None` for no limit. It runs on each index apart: from
    /// when the writer takes control there, and again from each group applied or
    /// lost there. When it runs out, the writer loses control of that index.
    pub lease: Option<Span>,
    /// The key set of every frame the writer writes.
    pub set: Arc<KeySet>,
}

/// An open writer on its shard.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Key {
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
pub enum Error {
    /// The node has no mesh time yet.
    Unsynced,
    /// A control lease must be longer than zero.
    Lease {
        /// The span that was asked for.
        span: Span,
    },
}

impl Error {
    /// What to do instead: a sentence with no final period.
    #[must_use]
    pub const fn fix(self) -> &'static str {
        match self {
            Self::Unsynced => "Open the writer again later",
            Self::Lease { .. } => "Give a lease longer than zero, or none",
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Self::Unsynced => f.write_str("the node has no mesh time yet"),
            Self::Lease { span } => control::lease::Error { span }.fmt(f),
        }
    }
}

impl std::error::Error for Error {}

/// The control lease of `span`.
///
/// # Errors
///
/// [`Error::Lease`] when `span` is not longer than zero.
pub(crate) fn lease(span: Span) -> Result<Lease, Error> {
    Lease::new(span).map_err(|error| Error::Lease { span: error.span })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn says_what_is_wrong_and_what_to_do_for_each_error() {
        let lease = Error::Lease {
            span: Span::from_nanos(-3),
        };

        assert_eq!(Error::Unsynced.to_string(), "the node has no mesh time yet");
        assert_eq!(Error::Unsynced.fix(), "Open the writer again later");
        assert_eq!(
            lease.to_string(),
            "control lease must be longer than zero, got -3ns"
        );
        assert_eq!(lease.fix(), "Give a lease longer than zero, or none");
    }

    #[test]
    fn keeps_the_span_of_a_lease_error() {
        let span = Span::from_nanos(-3);

        assert_eq!(lease(span).err(), Some(Error::Lease { span }));
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
