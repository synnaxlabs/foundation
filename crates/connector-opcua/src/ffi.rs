//! The C functions of open62541 and `shim.c` that Rust calls, each declared once.

#![expect(unsafe_code, reason = "open62541 is a C library")]

use std::ffi::{CStr, c_char, c_void};

/// An open62541 status code.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) struct Status(pub(crate) u32);

impl Status {
    /// Gives the name of the code, such as `BadNodeIdUnknown`.
    pub(crate) fn name(self) -> &'static str {
        // SAFETY: `UA_StatusCode_name` takes any code and gives a static C string.
        let name = unsafe { UA_StatusCode_name(self.0) };
        // SAFETY: the string is static, and ends with a NUL.
        let name = unsafe { CStr::from_ptr(name) };
        name.to_str().expect("invariant: each status name is ASCII")
    }
}

/// `UA_String` and `UA_ByteString`.
#[repr(C)]
pub(crate) struct Bytes {
    pub(crate) length: usize,
    pub(crate) data: *mut u8,
}

unsafe extern "C" {
    pub(crate) fn UA_StatusCode_name(code: u32) -> *const c_char;
    pub(crate) fn UA_DateTime_now() -> i64;
    pub(crate) fn UA_DateTime_nowMonotonic() -> i64;
    pub(crate) fn UA_DateTime_localTimeUtcOffset() -> i64;

    pub(crate) fn UA_EventLoop_new_POSIX(logger: *const c_void) -> *mut c_void;
    pub(crate) fn UA_ConnectionManager_new_POSIX_TCP(name: Bytes) -> *mut c_void;
    pub(crate) fn UA_ConnectionManager_new_POSIX_UDP(name: Bytes) -> *mut c_void;
    pub(crate) fn UA_ConnectionManager_new_POSIX_Ethernet(name: Bytes) -> *mut c_void;
    pub(crate) fn UA_InterruptManager_new_POSIX(name: Bytes) -> *mut c_void;

}
