//! Resolves a named secret on the node that runs a connector, through store adapters
//! chosen by policy; `node` hands it the sealed ciphertexts it pulls from `mesh`.

use std::collections::BTreeMap;
use std::fmt;
use std::rc::Rc;

use types::name::{Name, Selector, Specificity};

pub mod store;

/// A secret value. `Debug` prints `<secret>`, never the bytes.
pub struct Value(Box<[u8]>);

impl Value {
    /// Wraps the bytes of a secret.
    pub fn new(bytes: impl Into<Box<[u8]>>) -> Self {
        Self(bytes.into())
    }

    /// The bytes. Send them only to the system they unlock.
    #[must_use]
    pub fn expose(&self) -> &[u8] {
        &self.0
    }
}

impl fmt::Debug for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<secret>")
    }
}

/// A place that holds secret values: the sealed store, a node environment variable
/// or file, or an outside secret manager.
pub trait Store {
    /// Reads the value of `name`.
    ///
    /// # Errors
    ///
    /// A [`store::Error`] that tells the caller what to do next.
    fn get<'a>(&'a self, name: &'a Name) -> store::Request<'a>;
}

/// Picks the store for each secret name and reads the value from it.
pub struct Resolver {
    stores: BTreeMap<store::Key, Rc<dyn Store>>,
    rules: Vec<(Selector, store::Key)>,
    default: store::Key,
}

impl Resolver {
    /// Builds a resolver over `stores`. Each rule sends the names its selector matches
    /// to its store. A name that no rule matches goes to `default`.
    ///
    /// # Errors
    ///
    /// [`Error::UnknownStore`] when `default` or a rule names a store not in `stores`.
    pub fn new(
        stores: impl IntoIterator<Item = (store::Key, Rc<dyn Store>)>,
        rules: impl IntoIterator<Item = (Selector, store::Key)>,
        default: store::Key,
    ) -> Result<Self, Error> {
        let stores: BTreeMap<_, _> = stores.into_iter().collect();
        let rules: Vec<_> = rules.into_iter().collect();
        let mut named = rules.iter().map(|(_, key)| key).chain([&default]);
        if let Some(key) = named.find(|key| !stores.contains_key(*key)) {
            return Err(Error::UnknownStore { store: key.clone() });
        }
        Ok(Self {
            stores,
            rules,
            default,
        })
    }

    /// Reads `name` from the store of the most specific rule that matches it.
    ///
    /// # Errors
    ///
    /// [`Error::Store`] with the store's error.
    ///
    /// # Panics
    ///
    /// When two rules of equal specificity match `name`. Plan rejects such rules.
    pub async fn resolve(&self, name: &Name) -> Result<Value, Error> {
        let key = self.pick(name);
        let store = self
            .stores
            .get(key)
            .expect("invariant: `new` checked that every rule's store exists");
        store.get(name).await.map_err(|error| Error::Store {
            name: name.clone(),
            store: key.clone(),
            error,
        })
    }

    fn pick(&self, name: &Name) -> &store::Key {
        let mut best: Option<(Specificity, &store::Key)> = None;
        let mut tied: Option<&store::Key> = None;
        for (select, key) in &self.rules {
            let Some(specificity) = select.matches(name) else {
                continue;
            };
            match best {
                Some((top, _)) if specificity < top => {}
                Some((top, _)) if specificity == top => tied = Some(key),
                _ => {
                    best = Some((specificity, key));
                    tied = None;
                }
            }
        }
        if let (Some((_, first)), Some(second)) = (best, tied) {
            panic!(
                "invariant: plan rejects rules of equal specificity, but stores {first} \
                 and {second} both match secret {name}"
            );
        }
        best.map_or(&self.default, |(_, key)| key)
    }
}

impl fmt::Debug for Resolver {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Resolver")
            .field("stores", &self.stores.keys().collect::<Vec<_>>())
            .field("rules", &self.rules)
            .field("default", &self.default)
            .finish()
    }
}

/// An error from `secret`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Error {
    /// A rule or the default names a store the node does not have.
    UnknownStore {
        /// The store's key.
        store: store::Key,
    },
    /// A store gave no value for a secret.
    Store {
        /// The secret.
        name: Name,
        /// The store that `name` resolved to.
        store: store::Key,
        /// Why the store gave no value.
        error: store::Error,
    },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownStore { store } => write!(
                f,
                "the node has no secret store {store}. Add the store to the node, or \
                 change the secret store policy"
            ),
            Self::Store { name, store, error } => {
                write!(f, "secret {name} from store {store}: {error}")
            }
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::UnknownStore { .. } => None,
            Self::Store { error, .. } => Some(error),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::panic::{AssertUnwindSafe, catch_unwind};
    use std::pin::pin;
    use std::task::{Context, Poll, Waker};

    use proptest::prelude::*;

    use super::*;

    /// Answers every name with the same result.
    struct Fixed(Result<Vec<u8>, store::Error>);

    impl Store for Fixed {
        fn get<'a>(&'a self, _: &'a Name) -> store::Request<'a> {
            let result = self.0.clone().map(Value::new);
            Box::pin(async move { result })
        }
    }

    fn key(s: &str) -> store::Key {
        store::Key::new(s)
    }

    fn name(s: &str) -> Name {
        s.parse().unwrap()
    }

    fn select(patterns: &[&str]) -> Selector {
        Selector::new(patterns.iter().copied()).unwrap()
    }

    /// Stores named by `keys`, each answering with its own key as the value.
    fn stores(keys: &[&str]) -> Vec<(store::Key, Rc<dyn Store>)> {
        keys.iter()
            .map(|k| {
                let store: Rc<dyn Store> = Rc::new(Fixed(Ok(k.as_bytes().to_vec())));
                (key(k), store)
            })
            .collect()
    }

    fn resolve(resolver: &Resolver, secret: &str) -> Result<Vec<u8>, Error> {
        let name = name(secret);
        let mut future = pin!(resolver.resolve(&name));
        match future
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop()))
        {
            Poll::Ready(result) => result.map(|v| v.expose().to_vec()),
            Poll::Pending => panic!("the test stores answer at once"),
        }
    }

    /// The store that answers `secret` under `rules`, with `sealed` as the default.
    fn check(rules: &[(&[&str], &str)], secret: &str) -> String {
        let rules = rules.iter().map(|(p, k)| (select(p), key(k)));
        let resolver =
            Resolver::new(stores(&["sealed", "vault", "env"]), rules, key("sealed"))
                .unwrap();
        String::from_utf8(resolve(&resolver, secret).unwrap()).unwrap()
    }

    #[test]
    fn sends_a_name_no_rule_matches_to_the_default() {
        assert_eq!(check(&[], "site.secrets.token"), "sealed");
        assert_eq!(check(&[(&["plant.**"], "vault")], "site.token"), "sealed");
    }

    #[test]
    fn picks_the_most_specific_rule() {
        let rules: &[(&[&str], &str)] =
            &[(&["site.**"], "vault"), (&["site.secrets.*"], "env")];
        assert_eq!(check(rules, "site.secrets.token"), "env");
        assert_eq!(check(rules, "site.other.token"), "vault");
    }

    #[test]
    fn skips_a_rule_whose_exclusion_matches() {
        let rules: &[(&[&str], &str)] = &[
            (&["site.**"], "vault"),
            (&["site.secrets.*", "!site.secrets.token"], "env"),
        ];
        assert_eq!(check(rules, "site.secrets.token"), "vault");
        assert_eq!(check(rules, "site.secrets.key"), "env");
    }

    #[test]
    #[should_panic(
        expected = "invariant: plan rejects rules of equal specificity, but \
                               stores vault and env both match secret site.token"
    )]
    fn panics_when_two_rules_tie() {
        check(
            &[(&["site.*"], "vault"), (&["*.token"], "env")],
            "site.token",
        );
    }

    #[test]
    fn rejects_a_rule_with_an_unknown_store() {
        let rules = [(select(&["**"]), key("vault"))];
        let error =
            Resolver::new(stores(&["sealed"]), rules, key("sealed")).unwrap_err();
        assert_eq!(
            error,
            Error::UnknownStore {
                store: key("vault")
            }
        );
        assert_eq!(
            error.to_string(),
            "the node has no secret store vault. Add the store to the node, or change \
             the secret store policy"
        );
    }

    #[test]
    fn rejects_an_unknown_default() {
        let error = Resolver::new(stores(&["vault"]), [], key("sealed")).unwrap_err();
        assert_eq!(
            error,
            Error::UnknownStore {
                store: key("sealed")
            }
        );
    }

    #[test]
    fn wraps_each_store_error_with_the_name_and_store() {
        let cases = [
            (
                store::Error::Missing,
                "secret site.token from store vault: the store has no value. Run \
                 `secret set`, or point the secret store policy at the store that \
                 holds it",
            ),
            (
                store::Error::Denied {
                    detail: "403".into(),
                },
                "secret site.token from store vault: the store refused this node: 403. \
                 Grant the node access in the store",
            ),
            (
                store::Error::Unavailable {
                    detail: "timed out".into(),
                },
                "secret site.token from store vault: the store did not answer: timed \
                 out. Retrying may succeed",
            ),
        ];
        for (error, message) in cases {
            let store: Rc<dyn Store> = Rc::new(Fixed(Err(error.clone())));
            let resolver =
                Resolver::new([(key("vault"), store)], [], key("vault")).unwrap();
            let got = resolve(&resolver, "site.token").unwrap_err();
            assert_eq!(
                got,
                Error::Store {
                    name: name("site.token"),
                    store: key("vault"),
                    error,
                }
            );
            assert_eq!(got.to_string(), message);
        }
    }

    fn pattern() -> impl Strategy<Value = String> {
        prop::collection::vec(prop::sample::select(vec!["a", "b", "*", "**"]), 1..4)
            .prop_map(|segments| segments.join("."))
    }

    proptest! {
        #[test]
        fn debug_never_shows_the_bytes(bytes in prop::collection::vec(any::<u8>(), 1..64)) {
            let shown = format!("{:?}", Value::new(bytes.clone()));
            prop_assert_eq!(&shown, "<secret>");
        }

        #[test]
        fn rule_order_does_not_change_the_store(
            rules in prop::collection::vec((pattern(), 0..3_usize), 0..6),
            secret in prop::collection::vec(prop::sample::select(vec!["a", "b"]), 1..4),
        ) {
            const KEYS: [&str; 3] = ["sealed", "vault", "env"];
            let pick = |rules: Vec<(String, usize)>| {
                let rules = rules.into_iter().map(|(p, k)| (select(&[&p]), key(KEYS[k])));
                let resolver = Resolver::new(stores(&KEYS), rules, key("sealed")).unwrap();
                catch_unwind(AssertUnwindSafe(|| {
                    resolve(&resolver, &secret.join(".")).unwrap()
                }))
                .ok()
            };
            let mut reversed = rules.clone();
            reversed.reverse();
            prop_assert_eq!(pick(rules), pick(reversed));
        }
    }
}
