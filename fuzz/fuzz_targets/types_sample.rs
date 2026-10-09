//! The reader of sample types never panics, and a type prints as the text it was read
//! from.

#![no_main]
#![expect(clippy::disallowed_methods, reason = "fuzz_target! calls File::create")]

use libfuzzer_sys::fuzz_target;
use types::sample::Type;

fuzz_target!(|text: &str| {
    if let Ok(sample) = text.parse::<Type>() {
        assert_eq!(sample.to_string(), text, "the type changed");
    }
});
