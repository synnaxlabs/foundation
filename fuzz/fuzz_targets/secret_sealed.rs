//! `secret::store::Sealed::put` never panics, and takes only the one real sealed
//! value: [`SEALED`], for [`NAME`] at [`VERSION`]. It refuses any other bytes, name,
//! or version, on an empty store and on a store that holds the real value, and a
//! refused `put` leaves the store as it was.
//!
//! Input: an 8-byte big-endian version, a name, a NUL, then the sealed bytes.

#![no_main]
#![expect(clippy::disallowed_methods, reason = "fuzz_target! calls File::create")]

use std::pin::pin;
use std::task::{Context, Poll, Waker};

use libfuzzer_sys::fuzz_target;
use secret::seal::{self, Opener};
use secret::store::{self, Sealed, Store};
use types::name::Name;

/// The private key that [`SEALED`] is sealed to.
const KEY: [u8; 32] = [7; 32];
const NAME: &str = "plc.password";
const VERSION: u64 = 1;
const VALUE: &[u8] = b"hunter2";
/// [`VALUE`], from `seal::seal` to `Opener::from_bytes(KEY).public()` for [`NAME`]
/// at [`VERSION`], with the entropy of a default node of a `sim::Sim` with seed 1. A
/// change to the seal scheme needs new bytes here and new files in
/// `oracles/fuzz/secret_sealed/`.
const SEALED: [u8; 55] = [
    96, 213, 78, 245, 136, 184, 219, 125, 83, 58, 47, 19, 85, 132, 44, 105, 159, 148,
    140, 179, 47, 21, 147, 98, 87, 131, 119, 45, 182, 164, 189, 43, 221, 60, 229, 76,
    140, 112, 209, 171, 207, 90, 236, 81, 200, 138, 221, 85, 2, 152, 97, 159, 56, 60,
    214,
];

fuzz_target!(|input: &[u8]| {
    let Some((version, rest)) = input.split_first_chunk::<8>() else {
        return;
    };
    let version = u64::from_be_bytes(*version);
    let Some(nul) = rest.iter().position(|&byte| byte == 0) else {
        return;
    };
    let (name, sealed) = (&rest[..nul], &rest[nul + 1..]);
    let Some(name) = str::from_utf8(name)
        .ok()
        .and_then(|n| n.parse::<Name>().ok())
    else {
        return;
    };
    let real = name.as_str() == NAME && version == VERSION && sealed == SEALED;
    let held: Name = NAME.parse().expect("a valid name");

    let mut empty = Sealed::new(Opener::from_bytes(KEY));
    let mut full = Sealed::new(Opener::from_bytes(KEY));
    full.put(held.clone(), VERSION, SEALED.to_vec())
        .expect("the real value opens");
    let want = if real {
        Ok(())
    } else {
        Err(seal::Error::Refused)
    };
    assert_eq!(empty.put(name.clone(), version, sealed.to_vec()), want);
    assert_eq!(full.put(name.clone(), version, sealed.to_vec()), want);

    let value = if real {
        Ok(VALUE.to_vec())
    } else {
        Err(store::Error::Missing)
    };
    assert_eq!(get(&empty, &name), value, "the empty store");
    assert_eq!(
        get(&full, &held),
        Ok(VALUE.to_vec()),
        "the store of the real value"
    );
});

/// The value of `name` in `store`, which answers on the first poll.
fn get(store: &Sealed, name: &Name) -> Result<Vec<u8>, store::Error> {
    let mut request = pin!(store.get(name));
    match request
        .as_mut()
        .poll(&mut Context::from_waker(Waker::noop()))
    {
        Poll::Ready(value) => value.map(|value| value.expose().to_vec()),
        Poll::Pending => panic!("the sealed store answers on the first poll"),
    }
}
