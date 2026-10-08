//! Checks that the open62541 copy and `shim.c` compile and link.

#![expect(unsafe_code, reason = "open62541 is a C library")]

use std::ffi::{CStr, c_char};

unsafe extern "C" {
    fn UA_StatusCode_name(code: u32) -> *const c_char;
    fn UA_DateTime_now() -> i64;
    fn UA_DateTime_nowMonotonic() -> i64;
    fn UA_DateTime_localTimeUtcOffset() -> i64;
}

fn name(code: u32) -> &'static str {
    // SAFETY: `UA_StatusCode_name` takes any code and gives a static C string.
    let name = unsafe { UA_StatusCode_name(code) };
    // SAFETY: the string is static, and ends with a NUL.
    unsafe { CStr::from_ptr(name) }.to_str().unwrap()
}

#[test]
fn the_copy_names_a_status_code() {
    assert_eq!(name(0), "Good");
    assert_eq!(name(0x8034_0000), "BadNodeIdUnknown");
}

#[test]
fn the_global_clocks_give_a_fixed_time() {
    // SAFETY: each takes no argument and reads no state.
    let now = unsafe { UA_DateTime_now() };
    // SAFETY: as above.
    let monotonic = unsafe { UA_DateTime_nowMonotonic() };
    // SAFETY: as above.
    let offset = unsafe { UA_DateTime_localTimeUtcOffset() };
    assert_eq!((now, monotonic, offset), (0, 0, 0));
}
