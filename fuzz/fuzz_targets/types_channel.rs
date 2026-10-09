//! The reader of channel keys never panics, and a key it reads prints as text that
//! reads back to the same key.

#![no_main]
#![expect(clippy::disallowed_methods, reason = "fuzz_target! calls File::create")]

use libfuzzer_sys::fuzz_target;
use types::channel::Key;

fuzz_target!(|text: &str| {
    let Ok(key) = text.parse::<Key>() else {
        return;
    };
    let printed = key.to_string();
    assert_eq!(
        printed.parse::<Key>().as_ref(),
        Ok(&key),
        "{printed:?} does not read back"
    );
});
