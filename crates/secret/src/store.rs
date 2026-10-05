//! The seam each secret store implements.

use std::fmt;
use std::pin::Pin;

use crate::Value;

/// The name of one configured store on a node, such as `sealed` or `vault`.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Key(Box<str>);

impl Key {
    /// Makes a key from the store's name.
    pub fn new(name: impl Into<Box<str>>) -> Self {
        Self(name.into())
    }
}

impl fmt::Display for Key {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// The future a store returns. It stays on the shard that made it.
pub type Request<'a> = Pin<Box<dyn Future<Output = Result<Value, Error>> + 'a>>;

/// Why a store gave no value.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Error {
    /// The store has no value for the name.
    Missing,
    /// The store refused this node.
    Denied {
        /// The store's own text.
        detail: String,
    },
    /// The store did not answer. Retrying may succeed.
    Unavailable {
        /// The store's own text.
        detail: String,
    },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Missing => f.write_str(
                "the store has no value. Add the value to the store, or point the \
                 secret store policy at the store that holds it",
            ),
            Self::Denied { detail } => write!(
                f,
                "the store refused this node: {detail}. Grant the node access in the \
                 store"
            ),
            Self::Unavailable { detail } => {
                write!(
                    f,
                    "the store did not answer: {detail}. Retrying may succeed"
                )
            }
        }
    }
}

impl std::error::Error for Error {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tells_the_caller_what_to_do() {
        let cases = [
            (
                Error::Missing,
                "the store has no value. Add the value to the store, or point the \
                 secret store policy at the store that holds it",
            ),
            (
                Error::Denied {
                    detail: "403".into(),
                },
                "the store refused this node: 403. Grant the node access in the store",
            ),
            (
                Error::Unavailable {
                    detail: "timed out".into(),
                },
                "the store did not answer: timed out. Retrying may succeed",
            ),
        ];
        for (error, message) in cases {
            assert_eq!(error.to_string(), message);
        }
    }
}
