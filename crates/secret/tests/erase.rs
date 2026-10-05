//! Asserts that `secret` erases a key or a value before it frees the memory that
//! held it. The scan covers every thread, so this binary has no test harness.

#![expect(clippy::disallowed_macros, reason = "COUNTING ALLOCATOR")]

use std::hint::black_box;
use std::pin::pin;
use std::task::{Context, Poll, Waker};

use secret::Value;
use secret::seal::{Opener, seal};
use secret::store::{Sealed, Store};
use types::name::Name;

#[global_allocator]
static ALLOCATOR: counting::Allocator = counting::Allocator::new();

const PLAIN: [u8; 32] = [0x5a; 32];

fn main() {
    let mut sim = sim::Sim::new(sim::Config::default());
    let entropy = sim.node(sim::node::Config::default()).entropy();
    let name: Name = "site.secrets.token".parse().expect("a valid name");

    let opener = Opener::generate(&entropy);
    let key = *opener.expose();
    let ((), found) = ALLOCATOR.freed_holding(&key, || drop(black_box(opener)));
    assert_eq!(found, 0, "a dropped opener erases its key");

    let ((), found) =
        ALLOCATOR.freed_holding(&PLAIN, || drop(black_box(Value::new(PLAIN.to_vec()))));
    assert_eq!(found, 0, "a dropped value erases its bytes");

    let opener = Opener::generate(&entropy);
    let to = opener.public();
    let mut store = Sealed::new(opener);
    let value = Value::new(PLAIN.to_vec());
    let (sealed, found) =
        ALLOCATOR.freed_holding(&PLAIN, || seal(&to, &name, 1, &value, &entropy));
    assert_eq!(found, 0, "seal leaves the value in no freed block");
    drop(value);

    let ((), found) = ALLOCATOR.freed_holding(&PLAIN, || {
        store.put(name.clone(), 1, sealed).expect("the value opens");
        let mut request = pin!(store.get(&name));
        let Poll::Ready(answer) = request
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop()))
        else {
            panic!("the sealed store answers on the first poll");
        };
        let value = answer.expect("the store has the value");
        assert_eq!(value.expose(), PLAIN, "the store opens the value put");
    });
    assert_eq!(found, 0, "put and get leave the value in no freed block");
}
