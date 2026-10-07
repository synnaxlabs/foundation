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
    pub(crate) writer: u64,
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
            Self::Lease { span } => control::Error::Lease { span }.fmt(f),
        }
    }
}

impl std::error::Error for Error {}

/// The control lease of `span`.
///
/// # Errors
///
/// [`Error::Lease`] when `span` is not longer than zero.
pub(crate) fn lease(span: Span) -> Result<control::Lease, Error> {
    control::Lease::new(span).map_err(|error| match error {
        control::Error::Lease { span } => Error::Lease { span },
        control::Error::Waiting
        | control::Error::Reserved
        | control::Error::Expired => {
            panic!("invariant: a lease gives only its own error, got {error}")
        }
    })
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
    fn makes_a_lease_only_of_a_span_longer_than_zero() {
        let span = Span::from_nanos(1);

        assert_eq!(lease(span).map(control::Lease::span), Ok(span));
        assert_eq!(lease(Span::ZERO), Err(Error::Lease { span: Span::ZERO }));
    }
}
