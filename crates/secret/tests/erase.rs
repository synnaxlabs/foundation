//! Asserts that `secret` erases a key or a value before it frees the memory that
//! held it. The scan covers every thread, so this binary has no test harness.

#![expect(clippy::disallowed_macros, reason = "COUNTING ALLOCATOR")]

use std::hint::black_box;
use std::path::Path;
use std::pin::pin;
use std::sync::Arc;
use std::task::{Context, Poll, Waker};

use aws_lc_rs::agreement::{self, PrivateKey, UnparsedPublicKey, X25519};
use env::files::Mode;
use secret::seal::{Opener, seal};
use secret::store::{Sealed, Store};
use secret::{Error, Resolver, Source, Value};
use types::name::Name;

#[global_allocator]
static ALLOCATOR: counting::Allocator = counting::Allocator::new();

/// The bytes that a scan looks for.
type Needle = [u8; 32];

/// The needle `scan`. No other value of this test and no fill of one byte gives it,
/// so a scan finds no copy of another value and no fill of the allocator. `SEALED` is
/// the needle of two scans: of `seal`, and of the put and get that open its value.
const fn needle(scan: u8) -> Needle {
    let mut needle = *b"?a needle that no fill can give.";
    needle[0] = scan;
    needle
}

/// The value that a dropped `Value` holds.
const DROPPED: Needle = needle(0);
/// The value that the sealed store holds.
const SEALED: Needle = needle(1);
/// The value of the file that the file store refuses.
const REFUSED: Needle = needle(2);

fn main() {
    let mut sim = sim::Sim::new(sim::Config::default());
    let entropy = sim.node(sim::node::Config::default()).entropy();
    let name: Name = "site.secrets.token".parse().expect("a valid name");

    let opener = Opener::generate(&entropy);
    let key = *opener.expose();
    let ((), found) = ALLOCATOR.freed_holding(&key, || drop(black_box(opener)));
    assert_eq!(found, 0, "a dropped opener erases its key");

    let ((), found) = ALLOCATOR
        .freed_holding(&DROPPED, || drop(black_box(Value::new(DROPPED.to_vec()))));
    assert_eq!(found, 0, "a dropped value erases its bytes");

    let opener = Opener::generate(&entropy);
    let key = *opener.expose();
    let to = opener.public();
    let mut store = Sealed::new(opener);
    let value = Value::new(SEALED.to_vec());
    let (sealed, found) =
        ALLOCATOR.freed_holding(&SEALED, || seal(&to, &name, 1, &value, &entropy));
    assert_eq!(found, 0, "seal leaves the value in no freed block");
    let again = seal(&to, &name, 2, &value, &entropy);
    let last = seal(&to, &name, 3, &value, &entropy);
    let shared = shared_secret(&key, &last);
    drop(value);

    let mut round = |version, sealed| {
        store
            .put(name.clone(), version, sealed)
            .expect("the value opens");
        let mut request = pin!(store.get(&name));
        let Poll::Ready(answer) = request
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop()))
        else {
            panic!("the sealed store answers on the first poll");
        };
        let value = answer.expect("the store has the value");
        assert_eq!(value.expose(), SEALED, "the store opens the value put");
    };
    let ((), found) = ALLOCATOR.freed_holding(&SEALED, || round(1, sealed));
    assert_eq!(found, 0, "put and get leave the value in no freed block");
    let ((), found) = ALLOCATOR.freed_holding(&key, || round(2, again));
    assert_eq!(found, 0, "put and get leave the key in no freed block");
    let ((), found) = ALLOCATOR.freed_holding(&shared, || round(3, last));
    assert_eq!(
        found, 0,
        "put and get leave the shared secret in no freed block"
    );

    let (refused, found) = refused();
    assert_eq!(found, 0, "a refused get leaves the value in no freed block");
    let exposed = "other users can read the file /data/secrets/site.token. Make it \
                   unreadable by other users: chmod o-r /data/secrets/site.token";
    assert_eq!(
        refused,
        Err(Error::Unavailable {
            detail: exposed.into()
        }),
        "the store refuses a file that others can read"
    );
}

/// A get of the file store, on the sim disk, of a file that holds `REFUSED` and that
/// other users can read. Gives what it gave and the count of freed blocks that hold
/// `REFUSED` during the get.
fn refused() -> (Result<(), Error>, u64) {
    let mut sim = sim::Sim::new(sim::Config::default());
    let node = sim.node(sim::node::Config::default());
    let path = Path::new("secrets/site.token");
    let put = sim.run_on(&node, move |node, _| async move {
        let files = node.files();
        files
            .create_dir(Path::new("secrets"))
            .await
            .expect("makes it");
        let len = u64::try_from(REFUSED.len()).expect("fits");
        let file = files.open(path, Mode::Create { len }).await;
        let file = file.expect("makes it");
        let pool = block::Pool::heap(block::Config::new(1 << 18).expect("fits"));
        let block = pool.copy(&REFUSED).expect("holds it");
        file.write_at(0, &[block]).await.expect("writes it");
        file.close().await;
        node.expose_file(path);
    });
    put.expect("the run ends");
    ALLOCATOR.freed_holding(&REFUSED, || {
        let get = sim.run_on(&node, |node, _| async move {
            let resolver =
                Resolver::new(Arc::default(), node.files(), Path::new("secrets"));
            let source = Source::Secret("site.token".parse().expect("a valid name"));
            resolver.get(&source).await.map(drop)
        });
        get.expect("the run ends")
    })
}

/// The X25519 shared secret of the node key `own` and the encapsulated key that
/// starts `sealed`.
fn shared_secret(own: &[u8; 32], sealed: &[u8]) -> Vec<u8> {
    let own = PrivateKey::from_private_key(&X25519, own).expect("a valid key");
    let peer = UnparsedPublicKey::new(&X25519, &sealed[..32]);
    agreement::agree(&own, peer, (), |dh| Ok(dh.to_vec())).expect("the keys agree")
}
