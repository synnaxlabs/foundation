//! `document::encoding::decode` never panics, and a document it reads encodes to the
//! same bytes, because the encoding is canonical.

#![no_main]

use document::encoding::{self, Checked};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|bytes: &[u8]| {
    let Ok(document) = encoding::decode(bytes) else {
        return;
    };
    assert_eq!(
        Checked::new(document.clone().into_document()).as_ref(),
        Ok(&document),
        "the encoding refuses a decoded document"
    );
    assert_eq!(
        document.encode(),
        bytes,
        "two encodings read as one document"
    );
});
