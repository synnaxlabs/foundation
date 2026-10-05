//! `config_hcl::read` never panics, and a document it reads has an encoding that
//! decodes to an equal document.

#![no_main]

use document::{Source, encoding};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|text: &str| {
    let Ok(document) = config_hcl::read(Source(0), text) else {
        return;
    };
    let encoded = encoding::encode(&document).expect("a read document is too deep");
    assert_eq!(
        encoding::decode(&encoded).as_ref(),
        Ok(&document),
        "the document changed in its encoding"
    );
});
