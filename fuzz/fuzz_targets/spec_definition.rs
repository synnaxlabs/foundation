//! `spec::definition::Definition::decode` never panics, and a definition it reads
//! encodes to the same bytes, because the encoding is canonical.

#![no_main]

use libfuzzer_sys::fuzz_target;
use spec::definition::Definition;

fuzz_target!(|bytes: &[u8]| {
    let Ok(definition) = Definition::decode(bytes) else {
        return;
    };
    assert_eq!(
        definition.encode(),
        bytes,
        "two encodings read as one definition"
    );
});
