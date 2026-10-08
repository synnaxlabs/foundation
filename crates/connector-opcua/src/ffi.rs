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

/// `UA_NodeId`.
#[repr(C)]
pub(crate) struct NodeId {
    namespace: u16,
    kind: u32,
    identifier: [u64; 2],
}

/// `UA_DataType`, with `UA_ENABLE_TYPEDESCRIPTION`.
#[repr(C)]
pub(crate) struct DataType {
    name: *const c_char,
    ids: [NodeId; 3],
    /// `memSize` in the low 16 bits, then `typeKind`, `pointerFree`, `overlayable`,
    /// and `membersSize`.
    bits: u32,
    members: *const c_void,
}

impl DataType {
    /// Gives the size of a value in memory, in bytes.
    pub(crate) fn size(&self) -> usize {
        usize::try_from(self.bits & 0xffff).expect("invariant: a u16 fits a usize")
    }

    /// Gives the name of the type, such as `Variant`.
    pub(crate) fn name(&self) -> &'static str {
        // SAFETY: each type of `UA_TYPES` has a static name that ends with a NUL.
        let name = unsafe { CStr::from_ptr(self.name) };
        name.to_str().expect("invariant: each type name is ASCII")
    }
}

/// `UA_DecodeBinaryOptions`, all null but the length that the decoder sets.
#[repr(C)]
#[derive(Default)]
pub(crate) struct DecodeOptions {
    pointers: [usize; 4],
    pub(crate) decoded: usize,
}

/// `UA_TYPES_COUNT`.
pub(crate) const TYPES: usize = 388;

/// The index of `ByteString` in `UA_TYPES`.
pub(crate) const BYTE_STRING: usize = 14;

/// Gives `UA_TYPES`, the table of built-in types.
pub(crate) fn types() -> &'static [DataType; TYPES] {
    // SAFETY: the table is initialized at compile time, and open62541 never writes it.
    unsafe { &UA_TYPES }
}

unsafe extern "C" {
    static UA_TYPES: [DataType; TYPES];

    pub(crate) fn UA_decodeBinary(
        input: *const Bytes,
        value: *mut c_void,
        data_type: *const DataType,
        options: *mut DecodeOptions,
    ) -> u32;
    pub(crate) fn UA_encodeBinary(
        value: *const c_void,
        data_type: *const DataType,
        output: *mut Bytes,
        options: *mut c_void,
    ) -> u32;
    pub(crate) fn UA_calcSizeBinary(
        value: *const c_void,
        data_type: *const DataType,
        options: *mut c_void,
    ) -> usize;
    pub(crate) fn UA_clear(value: *mut c_void, data_type: *const DataType);

    pub(crate) fn UA_StatusCode_name(code: u32) -> *const c_char;
}

#[cfg(test)]
unsafe extern "C" {
    pub(crate) fn UA_DateTime_now() -> i64;
    pub(crate) fn UA_DateTime_nowMonotonic() -> i64;
    pub(crate) fn UA_DateTime_localTimeUtcOffset() -> i64;

    pub(crate) fn UA_EventLoop_new_POSIX(logger: *const c_void) -> *mut c_void;
    pub(crate) fn UA_ConnectionManager_new_POSIX_TCP(name: Bytes) -> *mut c_void;
    pub(crate) fn UA_ConnectionManager_new_POSIX_UDP(name: Bytes) -> *mut c_void;
    pub(crate) fn UA_ConnectionManager_new_POSIX_Ethernet(name: Bytes) -> *mut c_void;
    pub(crate) fn UA_InterruptManager_new_POSIX(name: Bytes) -> *mut c_void;

}
