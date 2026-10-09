//! The reader of names never panics, and a name prints as the text it was read from.

#![no_main]
#![expect(clippy::disallowed_methods, reason = "fuzz_target! calls File::create")]

use libfuzzer_sys::fuzz_target;
use types::name::Name;

fuzz_target!(|text: &str| {
    if let Ok(name) = text.parse::<Name>() {
        assert_eq!(name.to_string(), text, "the name changed");
    }
});
