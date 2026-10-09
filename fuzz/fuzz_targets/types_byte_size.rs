//! The reader of byte sizes never panics, and a size prints as text that reads back to
//! the same size.

#![no_main]
#![expect(clippy::disallowed_methods, reason = "fuzz_target! calls File::create")]

use libfuzzer_sys::fuzz_target;
use types::byte::Size;

fuzz_target!(|text: &str| fuzz::check_round_trip::<Size>(text));
