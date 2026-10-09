//! The reader of ranges never panics, and a range prints as text that reads back to the
//! same range.

#![no_main]
#![expect(clippy::disallowed_methods, reason = "fuzz_target! calls File::create")]

use libfuzzer_sys::fuzz_target;
use types::time::Range;

fuzz_target!(|text: &str| fuzz::check_round_trip::<Range>(text));
