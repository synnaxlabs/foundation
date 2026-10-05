//! `foundation mcp` never panics or fails, and it writes at most one reply for each
//! line it reads.

#![no_main]

use std::io;

use libfuzzer_sys::fuzz_target;

fuzz_target!(|input: &[u8]| {
    let mut output = Vec::new();
    let mut errors = Vec::new();
    let status = ops::cli(
        ["foundation", "mcp"].map(Into::into),
        input,
        &mut output,
        &mut errors,
    );
    assert_eq!((status, errors.as_slice()), (0, b"".as_slice()));
    let lines = input.split(|&byte| byte == b'\n').count();
    assert!(output.iter().filter(|&&byte| byte == b'\n').count() <= lines);
});
