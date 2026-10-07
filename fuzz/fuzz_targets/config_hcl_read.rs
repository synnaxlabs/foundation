//! `config_hcl::read` never panics, and a document it reads has an encoding that
//! decodes to an equal document.

#![no_main]

use document::Source;
use document::encoding::{Checked, decode};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|text: &str| {
    let Ok(document) = config_hcl::read(Source(0), text) else {
        return;
    };
    let checked = Checked::new(document).expect("a read document is too deep");
    assert_eq!(
        decode(&checked.encode()).as_ref(),
        Ok(&checked),
        "the document changed in its encoding"
    );
});
