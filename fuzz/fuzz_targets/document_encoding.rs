//! `document::encoding::decode` never panics, and a document it reads encodes to the
//! same bytes, because the encoding is canonical.

#![no_main]

use document::encoding;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|bytes: &[u8]| {
    let Ok(document) = encoding::decode(bytes) else {
        return;
    };
    let encoded = encoding::encode(&document).expect("a decoded document is too deep");
    assert_eq!(encoded, bytes, "two encodings read as one document");
});
