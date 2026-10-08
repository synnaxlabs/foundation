//! `config::plan::Plan::decode` never panics, and a plan it reads encodes to the same
//! bytes, because the encoding is canonical.

#![no_main]

use config::plan::Plan;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|bytes: &[u8]| {
    let Ok(plan) = Plan::decode(bytes) else {
        return;
    };
    assert_eq!(plan.encode(), bytes, "two encodings read as one plan");
});
