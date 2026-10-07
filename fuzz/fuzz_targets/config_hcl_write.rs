//! A document that `config_hcl::read` reads has HCL text from `config_hcl::write`,
//! and that text reads back as an equal document.

#![no_main]

use document::Source;
use document::encoding::Checked;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|text: &str| {
    let Ok(document) = config_hcl::read(Source(0), text) else {
        return;
    };
    let checked = Checked::new(document.clone()).expect("a read document is too deep");
    let written = config_hcl::write(&checked).expect("a read document has HCL text");
    assert_eq!(
        config_hcl::read(Source(0), &written).as_ref(),
        Ok(&document),
        "the document changed in its text:\n{written}"
    );
});
