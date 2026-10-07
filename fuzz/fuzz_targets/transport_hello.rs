//! `transport::fuzzing::Hello::decode` never panics, and a hello it reads keeps the
//! limit order and decodes from its own encoding.

#![no_main]

use libfuzzer_sys::fuzz_target;
use transport::fuzzing::Hello;

fuzz_target!(|bytes: &[u8]| {
    let Ok(hello) = Hello::decode(bytes) else {
        return;
    };
    assert!(hello.message_bytes_max >= 1472, "a message_bytes_max below 1472");
    assert!(
        hello.window_bytes >= hello.message_bytes_max,
        "a window_bytes below the message_bytes_max"
    );
    assert_eq!(Hello::decode(&hello.encode()), Ok(hello), "the hello changed");
});
