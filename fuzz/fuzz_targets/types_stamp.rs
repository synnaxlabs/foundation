//! The reader of stamps never panics, and a stamp prints as text that reads back to the
//! same stamp.

#![no_main]
#![expect(clippy::disallowed_methods, reason = "fuzz_target! calls File::create")]

use libfuzzer_sys::fuzz_target;
use types::time::Stamp;

fuzz_target!(|text: &str| fuzz::check_round_trip::<Stamp>(text));
