//! `config_hcl::update` never panics. When `read` takes both texts of an input, the
//! update of the first to the document of the second reads as that document, and
//! the update of a text to its own document keeps every byte.
//!
//! An input is two HCL texts with a NUL between them. With no NUL, the one text is
//! both.

#![no_main]

use document::Source;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|input: &str| {
    let (old, new) = input.split_once('\0').unwrap_or((input, input));
    let Ok(document) = config_hcl::read(Source(0), new) else {
        return;
    };
    let updated = config_hcl::update(Source(0), old, &document);
    let Ok(same) = config_hcl::read(Source(0), old) else {
        assert!(
            updated.is_err(),
            "a text that does not read was updated:\n{old}"
        );
        return;
    };
    let updated = updated.expect("a read text takes a read document");
    assert_eq!(
        config_hcl::read(Source(0), &updated).as_ref(),
        Ok(&document),
        "the update does not read as its document:\n{updated}"
    );
    assert_eq!(
        config_hcl::update(Source(0), old, &same).as_deref(),
        Ok(old),
        "the update to its own document changed the text"
    );
});
