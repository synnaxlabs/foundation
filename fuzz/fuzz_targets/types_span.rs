//! The reader of spans never panics, and a span prints as text that reads back to the
//! same span.

#![no_main]
#![expect(clippy::disallowed_methods, reason = "fuzz_target! calls File::create")]

use libfuzzer_sys::fuzz_target;
use types::time::Span;

fuzz_target!(|text: &str| fuzz::check_round_trip::<Span>(text));
