//! `secret::store::Sealed::put` never panics, and takes only the one real sealed
//! value: [`SEALED`], for [`NAME`] at [`VERSION`]. It refuses any other bytes, name,
//! or version.
//!
//! Input: an 8-byte big-endian version, a name, a NUL, then the sealed bytes.

#![no_main]

use libfuzzer_sys::fuzz_target;
use secret::seal::{Error, Opener};
use secret::store::Sealed;
use types::name::Name;

/// The private key that [`SEALED`] is sealed to.
const KEY: [u8; 32] = [7; 32];
const NAME: &str = "plc.password";
const VERSION: u64 = 1;
/// `hunter2`, sealed to [`KEY`] for [`NAME`] at [`VERSION`].
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
    let mut store = Sealed::new(Opener::from_bytes(KEY));
    let put = store.put(name, version, sealed.to_vec());
    assert_eq!(put, if real { Ok(()) } else { Err(Error::Refused) });
});
