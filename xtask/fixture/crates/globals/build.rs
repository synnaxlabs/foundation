#![expect(clippy::disallowed_macros, reason = "a build script is not a test")]

static mut STEPS: u8 = 0;

fn main() {}
