//! `wire::clock::decode` never panics, and a message it reads encodes to the same
//! bytes.

#![no_main]
#![expect(clippy::disallowed_methods, reason = "fuzz_target! calls File::create")]

use libfuzzer_sys::fuzz_target;
use wire::clock;

fuzz_target!(|bytes: &[u8]| {
    let Ok(message) = clock::decode(bytes) else {
        return;
    };
    let mut out = [0; clock::MAX_LEN];
    assert_eq!(
        clock::encode(&message, &mut out),
        bytes,
        "the message changed"
    );
});
