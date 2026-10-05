//! The reader of spans never panics, and a span prints as text that reads back to the
//! same span.

#![no_main]

use libfuzzer_sys::fuzz_target;
use types::time::Span;

fuzz_target!(|text: &str| fuzz::check_round_trip::<Span>(text));
