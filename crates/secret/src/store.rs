//! The seam each secret store implements: the sealed store, a node environment
//! variable or file, or an outside secret manager.

use std::fmt;
use std::pin::Pin;

use types::name::Name;

use crate::Value;

mod sealed;

pub use sealed::Sealed;

/// A place that holds secret values.
pub trait Store {
    /// Reads the value of `name`, the secret's full name in the tree, such as
    /// `site_a.secrets.influx_token`. Each store maps the name to its own key (an
    /// environment variable, a Vault path) by its config.
    ///
    /// # Errors
    ///
    /// [`Error::Missing`] when the store holds no value at that key,
    /// [`Error::Denied`] when the store refuses this node's credentials, and
    /// [`Error::Unavailable`] when the store does not answer or fails before it
    /// decides.
    fn get<'a>(&'a self, name: &'a Name) -> Request<'a>;
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
    use std::cell::Cell;
    use std::collections::BTreeMap;
    use std::pin::pin;
    use std::rc::Rc;
    use std::task::{Context, Poll, Waker};

    use super::*;

    /// Holds values in memory and answers after one pending poll, as a store that
    /// waits on I/O does.
    struct Memory {
        values: BTreeMap<Name, Vec<u8>>,
        /// Not `Send`, as a store on one shard may be.
        polls: Rc<Cell<u32>>,
    }

    impl Store for Memory {
        fn get<'a>(&'a self, name: &'a Name) -> Request<'a> {
            Box::pin(std::future::poll_fn(move |cx| {
                self.polls.set(self.polls.get() + 1);
                if self.polls.get() == 1 {
                    cx.waker().wake_by_ref();
                    return Poll::Pending;
                }
                Poll::Ready(
                    self.values
                        .get(name)
                        .map(|v| Value::new(v.clone()))
                        .ok_or(Error::Missing),
                )
            }))
        }
    }

    fn name(s: &str) -> Name {
        s.parse().unwrap()
    }

    /// Polls the request for `name` until it answers, at most twice.
    fn drive(store: &dyn Store, name: &Name) -> Result<Value, Error> {
        let mut request = pin!(store.get(name));
        let mut cx = Context::from_waker(Waker::noop());
        assert!(request.as_mut().poll(&mut cx).is_pending());
        match request.as_mut().poll(&mut cx) {
            Poll::Ready(result) => result,
            Poll::Pending => panic!("the store answers on the second poll"),
        }
    }

    #[test]
    fn drives_a_request_on_one_thread_to_its_answer() {
        let store = Memory {
            values: BTreeMap::from([(
                "site.secrets.token".parse().unwrap(),
                b"t".to_vec(),
            )]),
            polls: Rc::default(),
        };
        assert_eq!(
            drive(&store, &name("site.secrets.token")).unwrap().expose(),
            b"t"
        );
        store.polls.set(0);
        assert_eq!(
            drive(&store, &name("site.secrets.key")).unwrap_err(),
            Error::Missing
        );
    }

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
