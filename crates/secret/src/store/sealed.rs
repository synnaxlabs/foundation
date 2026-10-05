use std::collections::BTreeMap;
use std::fmt;

use types::name::Name;

use super::{Error, Request, Store};
use crate::seal::{self, Opener};

/// The built-in store: values sealed to this node, which `node` hands it from region
/// state. It holds each value sealed and opens it on each read.
///
/// The store does not judge the order of writes: every node takes a secret write
/// only at the newest version plus one, before `node` hands it here. On a key
/// rotation, make a new store with the new opener and put the re-sealed values.
pub struct Sealed {
    opener: Opener,
    values: BTreeMap<Name, Entry>,
}

struct Entry {
    version: u64,
    sealed: Vec<u8>,
}

impl Sealed {
    /// Makes an empty store that opens values with `opener`.
    #[must_use]
    pub fn new(opener: Opener) -> Self {
        Self {
            opener,
            values: BTreeMap::new(),
        }
    }

    /// Keeps `sealed` as the value of `name` at `version`, in place of the value it
    /// held.
    ///
    /// # Errors
    ///
    /// [`seal::Error::Refused`] when `sealed` does not open with this node's key for
    /// `name` at `version`. The store then keeps the value it held.
    pub fn put(
        &mut self,
        name: Name,
        version: u64,
        sealed: Vec<u8>,
    ) -> Result<(), seal::Error> {
        drop(self.opener.open(&name, version, &sealed)?);
        self.values.insert(name, Entry { version, sealed });
        Ok(())
    }

    /// Removes the value of `name`.
    pub fn delete(&mut self, name: &Name) {
        self.values.remove(name);
    }
}

impl Store for Sealed {
    /// Opens the value of `name`. [`Error::Missing`] when the store has none.
    fn get<'a>(&'a self, name: &'a Name) -> Request<'a> {
        let value = self.values.get(name).ok_or(Error::Missing).map(|entry| {
            self.opener
                .open(name, entry.version, &entry.sealed)
                .expect("invariant: put opened these bytes with this key")
        });
        Box::pin(std::future::ready(value))
    }
}

impl fmt::Debug for Sealed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Sealed").finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use std::pin::pin;
    use std::task::{Context, Poll, Waker};

    use env::entropy::Entropy;

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

    fn sealed(opener: &Opener, version: u64, plain: &[u8]) -> Vec<u8> {
        let value = Value::new(plain.to_vec());
        seal(
            &opener.public(),
            &token(),
            version,
            &value,
            &entropy(version),
        )
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

    fn store() -> (Sealed, Opener) {
        let opener = Opener::generate(&entropy(1));
        let again = Opener::from_bytes(*opener.expose());
        (Sealed::new(opener), again)
    }

    #[test]
    fn gives_the_value_of_the_last_put() {
        let (mut store, key) = store();
        store.put(token(), 1, sealed(&key, 1, b"old")).unwrap();
        store.put(token(), 2, sealed(&key, 2, b"new")).unwrap();
        assert_eq!(get(&store, &token()), Ok(b"new".to_vec()));
    }

    #[test]
    fn keeps_the_newest_value_over_an_old_ciphertext_put_as_a_reseal() {
        let (mut store, key) = store();
        let old = sealed(&key, 1, b"old");
        store.put(token(), 1, old.clone()).unwrap();
        store.put(token(), 2, sealed(&key, 2, b"new")).unwrap();
        assert_eq!(store.put(token(), 2, old), Err(seal::Error::Refused));
        assert_eq!(get(&store, &token()), Ok(b"new".to_vec()));
    }

    #[test]
    fn refuses_a_value_sealed_to_another_key() {
        let (mut store, key) = store();
        store.put(token(), 1, sealed(&key, 1, b"t")).unwrap();
        let other = Opener::generate(&entropy(77));
        let refused = store.put(token(), 1, sealed(&other, 1, b"u"));
        assert_eq!(refused, Err(seal::Error::Refused));
        assert_eq!(get(&store, &token()), Ok(b"t".to_vec()));
    }

    #[test]
    fn replaces_the_value_with_a_reseal_at_the_same_version() {
        let (mut store, key) = store();
        store.put(token(), 4, sealed(&key, 4, b"t")).unwrap();
        let value = Value::new(b"u".to_vec());
        let reseal = seal(&key.public(), &token(), 4, &value, &entropy(40));
        store.put(token(), 4, reseal).unwrap();
        assert_eq!(get(&store, &token()), Ok(b"u".to_vec()));
    }

    #[test]
    fn has_no_value_after_a_delete_or_for_a_name_never_put() {
        let (mut store, key) = store();
        assert_eq!(get(&store, &token()), Err(Error::Missing));
        store.put(token(), 1, sealed(&key, 1, b"t")).unwrap();
        store.delete(&token());
        assert_eq!(get(&store, &token()), Err(Error::Missing));
    }

    #[test]
    fn never_shows_its_values_in_debug() {
        let (mut store, key) = store();
        store.put(token(), 1, sealed(&key, 1, b"t")).unwrap();
        assert_eq!(format!("{store:?}"), "Sealed { .. }");
    }
}
