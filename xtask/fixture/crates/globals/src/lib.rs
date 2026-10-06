//! A library that lifts `disallowed_macros` and holds statics, which the check refuses.

#![expect(
    clippy::disallowed_macros,
    reason = "a library never holds a global allocator"
)]

static NAME: &str = "a constant is a `const`";
pub static mut COUNT: u32 = 0;
#[used] static USED: u8 = 0;
pub(crate) static
    LOCK: std::sync::Mutex<u8> = std::sync::Mutex::new(0);
