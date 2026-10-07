//! `config_hcl::update` never panics. When `read` takes both texts of an input, the
//! update of the first to the document of the second reads as that document, and
//! the update of a text to its own document keeps every byte. A second
//! update to the same document changes nothing. When `read` refuses the first
//! text, `update` gives the same problems.
//!
//! An input is two HCL texts with a NUL between them. With no NUL, the one text is
//! both.

#![no_main]

use document::Source;
use document::encoding::Checked;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|input: &str| {
    let (old, new) = input.split_once('\0').unwrap_or((input, input));
    let Ok(document) = config_hcl::read(Source(0), new) else {
        return;
    };
    let checked = Checked::new(document.clone()).expect("a read document is too deep");
    let updated = config_hcl::update(Source(0), old, &checked);
    let own = match config_hcl::read(Source(0), old) {
        Ok(own) => own,
        Err(problems) => {
            assert_eq!(
                updated,
                Err(config_hcl::Refusal::Text(problems)),
                "not the problems `read` gives"
            );
            return;
        }
    };
    let updated = updated.expect("a read text takes a read document");
    assert_eq!(
        config_hcl::read(Source(0), &updated).as_ref(),
        Ok(&document),
        "the update does not read as its document:\n{updated}"
    );
    assert_eq!(
        config_hcl::update(Source(0), &updated, &checked).as_deref(),
        Ok(updated.as_str()),
        "a second update to the same document changed the text"
    );
    assert_eq!(
        config_hcl::update(Source(0), old, &Checked::new(own).expect("a read document is too deep")).as_deref(),
        Ok(old),
        "the update to its own document changed the text"
    );
});
