use std::collections::BTreeMap;
use std::fmt;

use types::name::Name;

use super::{Error, Request, Store};
use crate::seal::Opener;

/// The built-in store: values sealed to this node, which `node` hands it from region
/// state. It keeps the newest version of each name and opens it on each read, so it
/// holds no plain value.
pub struct Sealed {
    opener: Opener,
    newest: BTreeMap<Name, Entry>,
}

struct Entry {
    version: u64,
    /// `None` after a delete.
    sealed: Option<Vec<u8>>,
}

impl Sealed {
    /// Makes an empty store that opens values with `opener`.
    #[must_use]
    pub fn new(opener: Opener) -> Self {
        Self {
            opener,
            newest: BTreeMap::new(),
        }
    }

    /// Keeps `sealed` as the value of `name` at `version`. A value at the newest
    /// version replaces it, as a re-seal to a new key does.
    ///
    /// # Errors
    ///
    /// [`Stale`] when `version` is older than the newest version of `name`, or is
    /// the version of its delete.
    pub fn put(
        &mut self,
        name: Name,
        version: u64,
        sealed: Vec<u8>,
    ) -> Result<(), Stale> {
        self.keep(name, version, Some(sealed))
    }

    /// Removes the value of `name` at `version`.
    ///
    /// # Errors
    ///
    /// [`Stale`] when `version` is older than the newest version of `name`, or is
    /// the version of its value.
    pub fn delete(&mut self, name: Name, version: u64) -> Result<(), Stale> {
        self.keep(name, version, None)
    }

    fn keep(
        &mut self,
        name: Name,
        version: u64,
        sealed: Option<Vec<u8>>,
    ) -> Result<(), Stale> {
        if let Some(newest) = self.newest.get(&name) {
            let reseal = newest.version == version
                && newest.sealed.is_some() == sealed.is_some();
            if version < newest.version || (version == newest.version && !reseal) {
                return Err(Stale {
                    version,
                    newest: newest.version,
                });
            }
        }
        self.newest.insert(name, Entry { version, sealed });
        Ok(())
    }
}

impl Store for Sealed {
    /// Opens the newest value of `name`. [`Error::Missing`] when the store has none
    /// or it was deleted, and [`Error::Denied`] when it does not open with this
    /// node's key.
    fn get<'a>(&'a self, name: &'a Name) -> Request<'a> {
        let value = match self.newest.get(name) {
            Some(Entry {
                version,
                sealed: Some(sealed),
            }) => self.opener.open(name, *version, sealed).map_err(|refused| {
                Error::Denied {
                    detail: refused.to_string(),
                }
            }),
            _ => Err(Error::Missing),
        };
        Box::pin(std::future::ready(value))
    }
}

impl fmt::Debug for Sealed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Sealed").finish_non_exhaustive()
    }
}

/// A sealed value or delete that is not newer than what the store holds for its name.
/// A writer put an old value back.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Stale {
    /// The version given.
    pub version: u64,
    /// The newest version the store holds for the name.
    pub newest: u64,
}

impl fmt::Display for Stale {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "the sealed value has version {}, but this node holds version {} of this \
             secret. A writer put an old value back; set the secret again",
            self.version, self.newest
        )
    }
}

impl std::error::Error for Stale {}

#[cfg(test)]
mod tests {
    use std::pin::pin;
    use std::task::{Context, Poll, Waker};

    use env::entropy::Entropy;
    use proptest::prelude::*;

    use super::*;
    use crate::Value;
    use crate::seal::seal;

    fn entropy(value: u64) -> Entropy {
        let mut sim = sim::Sim::new(sim::Config {
            seed: value,
            ..sim::Config::default()
        });
        sim.node(sim::node::Config::default()).entropy()
    }

    fn token() -> Name {
        "site.secrets.token".parse().unwrap()
    }

    /// A store, and a sealer of values to its key.
    fn store() -> (Sealed, impl Fn(u64, &[u8]) -> Vec<u8>) {
        let opener = Opener::generate(&entropy(1));
        let to = opener.public();
        let sealer = move |version, plain: &[u8]| {
            let value = Value::new(plain.to_vec());
            seal(&to, &token(), version, &value, &entropy(version))
        };
        (Sealed::new(opener), sealer)
    }

    fn get(store: &Sealed, name: &Name) -> Result<Vec<u8>, Error> {
        let mut request = pin!(store.get(name));
        match request
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop()))
        {
            Poll::Ready(value) => value.map(|v| v.expose().to_vec()),
            Poll::Pending => panic!("the sealed store answers on the first poll"),
        }
    }

    #[test]
    fn refuses_an_older_sealed_value_after_a_newer_one() {
        let (mut store, sealer) = store();
        store.put(token(), 2, sealer(2, b"new")).unwrap();
        let stale = store.put(token(), 1, sealer(1, b"old")).unwrap_err();
        assert_eq!(
            stale,
            Stale {
                version: 1,
                newest: 2
            }
        );
        assert_eq!(
            stale.to_string(),
            "the sealed value has version 1, but this node holds version 2 of this \
             secret. A writer put an old value back; set the secret again"
        );
        assert_eq!(get(&store, &token()), Ok(b"new".to_vec()));
    }

    #[test]
    fn refuses_an_older_value_after_a_delete() {
        let (mut store, sealer) = store();
        store.put(token(), 1, sealer(1, b"old")).unwrap();
        store.delete(token(), 2).unwrap();
        assert_eq!(get(&store, &token()), Err(Error::Missing));
        for (version, newest) in [(1, 2), (2, 2)] {
            let refused = store.put(token(), version, sealer(version, b"old"));
            assert_eq!(refused, Err(Stale { version, newest }));
        }
        assert_eq!(get(&store, &token()), Err(Error::Missing));
    }

    #[test]
    fn refuses_a_delete_at_the_version_of_a_value() {
        let (mut store, sealer) = store();
        store.put(token(), 3, sealer(3, b"t")).unwrap();
        let refused = store.delete(token(), 3);
        assert_eq!(
            refused,
            Err(Stale {
                version: 3,
                newest: 3
            })
        );
        assert_eq!(get(&store, &token()), Ok(b"t".to_vec()));
        store.delete(token(), 2).unwrap_err();
    }

    #[test]
    fn keeps_a_reseal_at_the_newest_version() {
        let (mut store, sealer) = store();
        store.put(token(), 4, sealer(4, b"t")).unwrap();
        store.put(token(), 4, sealer(4, b"t")).unwrap();
        store.delete(token(), 5).unwrap();
        store.delete(token(), 5).unwrap();
        assert_eq!(get(&store, &token()), Err(Error::Missing));
    }

    #[test]
    fn denies_a_value_sealed_at_another_version() {
        let (mut store, sealer) = store();
        store.put(token(), 2, sealer(1, b"t")).unwrap();
        assert_eq!(
            get(&store, &token()),
            Err(Error::Denied {
                detail: crate::seal::Error::Refused.to_string()
            })
        );
    }

    #[test]
    fn has_no_value_for_a_name_never_put() {
        let (store, _) = store();
        assert_eq!(get(&store, &token()), Err(Error::Missing));
    }

    #[test]
    fn never_shows_its_values_in_debug() {
        let (mut store, sealer) = store();
        store.put(token(), 1, sealer(1, b"t")).unwrap();
        assert_eq!(format!("{store:?}"), "Sealed { .. }");
    }

    proptest! {
        /// Writes with versions 1 to n, in any order: the store gives the newest.
        #[test]
        fn gives_the_newest_write_in_any_order(
            writes in prop::collection::vec(prop::option::of(any::<u8>()), 1..12)
                .prop_flat_map(|writes| {
                    let n = writes.len();
                    (Just(writes), Just((1..=n as u64).collect::<Vec<_>>()).prop_shuffle())
                }),
        ) {
            let (writes, order) = writes;
            let (mut store, sealer) = store();
            let mut newest = 0;
            for version in order {
                let write = writes[usize::try_from(version).unwrap() - 1];
                let kept = match write {
                    Some(byte) => store.put(token(), version, sealer(version, &[byte])),
                    None => store.delete(token(), version),
                };
                prop_assert_eq!(kept.is_ok(), version > newest);
                newest = newest.max(version);
            }
            let want = writes.last().unwrap().map(|byte| vec![byte]).ok_or(Error::Missing);
            prop_assert_eq!(get(&store, &token()), want);
        }
    }
}
