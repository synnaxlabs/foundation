//! The C functions of open62541 and `shim.c` that Rust calls, each declared once.

#![expect(unsafe_code, reason = "open62541 is a C library")]

use std::ffi::{CStr, c_char, c_int, c_void};
use std::mem::offset_of;

/// An open62541 status code.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) struct Status(pub(crate) u32);

impl Status {
    pub(crate) const GOOD: Self = Self(0);
    #[cfg(test)]
    pub(crate) const BAD_INTERNAL_ERROR: Self = Self(0x8002_0000);

    /// Gives the name of the code, such as `BadNodeIdUnknown`.
    pub(crate) fn name(self) -> &'static str {
        // SAFETY: `UA_StatusCode_name` takes any code and gives a static C string.
        let name = unsafe { UA_StatusCode_name(self.0) };
        // SAFETY: the string is static, and ends with a NUL.
        let name = unsafe { CStr::from_ptr(name) };
        name.to_str().expect("invariant: each status name is ASCII")
    }
}

impl std::fmt::Debug for Status {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name())
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

/// `UA_Callback`.
pub(crate) type Callback =
    unsafe extern "C" fn(application: *mut c_void, data: *mut c_void);

/// `UA_TimerPolicy`.
pub(crate) type Policy = c_int;
#[cfg(test)]
pub(crate) const ONCE: Policy = 0;
pub(crate) const CURRENT_TIME: Policy = 1;
#[cfg(test)]
pub(crate) const BASE_TIME: Policy = 2;

/// `UA_EventLoopState`.
#[cfg(test)]
pub(crate) const FRESH: c_int = 0;
#[cfg(test)]
pub(crate) const STARTED: c_int = 2;

/// `UA_DelayedCallback`.
#[repr(C)]
pub(crate) struct DelayedCallback {
    pub(crate) next: *mut DelayedCallback,
    pub(crate) callback: Callback,
    pub(crate) application: *mut c_void,
    pub(crate) context: *mut c_void,
}

/// `UA_EventLoop`. `shim.c` asserts its size and the same offsets. The shim sets each
/// function but `lock` and `unlock`.
#[repr(C)]
pub(crate) struct EventLoop {
    pub(crate) logger: *const c_void,
    pub(crate) params: [usize; 2],
    pub(crate) state: c_int,
    pub(crate) start: unsafe extern "C" fn(el: *mut EventLoop) -> u32,
    pub(crate) stop: unsafe extern "C" fn(el: *mut EventLoop),
    pub(crate) free: unsafe extern "C" fn(el: *mut EventLoop) -> u32,
    pub(crate) run: unsafe extern "C" fn(el: *mut EventLoop, timeout_ms: u32) -> u32,
    pub(crate) cancel: unsafe extern "C" fn(el: *mut EventLoop),
    pub(crate) now: unsafe extern "C" fn(el: *mut EventLoop) -> i64,
    pub(crate) now_monotonic: unsafe extern "C" fn(el: *mut EventLoop) -> i64,
    pub(crate) utc_offset: unsafe extern "C" fn(el: *mut EventLoop) -> i64,
    pub(crate) next_timer: unsafe extern "C" fn(el: *mut EventLoop) -> i64,
    pub(crate) add_timer: unsafe extern "C" fn(
        el: *mut EventLoop,
        callback: Callback,
        application: *mut c_void,
        data: *mut c_void,
        interval_ms: f64,
        base: *mut i64,
        policy: Policy,
        key: *mut u64,
    ) -> u32,
    pub(crate) modify_timer: unsafe extern "C" fn(
        el: *mut EventLoop,
        key: u64,
        interval_ms: f64,
        base: *mut i64,
        policy: Policy,
    ) -> u32,
    pub(crate) remove_timer: unsafe extern "C" fn(el: *mut EventLoop, key: u64),
    pub(crate) add_delayed:
        unsafe extern "C" fn(el: *mut EventLoop, callback: *mut DelayedCallback),
    pub(crate) remove_delayed:
        unsafe extern "C" fn(el: *mut EventLoop, callback: *mut DelayedCallback),
    pub(crate) sources: *mut c_void,
    pub(crate) register:
        unsafe extern "C" fn(el: *mut EventLoop, es: *mut c_void) -> u32,
    pub(crate) deregister:
        unsafe extern "C" fn(el: *mut EventLoop, es: *mut c_void) -> u32,
    pub(crate) lock: Option<unsafe extern "C" fn(el: *mut EventLoop)>,
    pub(crate) unlock: Option<unsafe extern "C" fn(el: *mut EventLoop)>,
}

/// Gives the offset of the word `word`, of the size of a pointer.
const fn at(word: usize) -> usize {
    word * size_of::<usize>()
}

const _: () = {
    assert!(size_of::<EventLoop>() == at(23), "UA_EventLoop changed");
    assert!(offset_of!(EventLoop, logger) == at(0), "logger moved");
    assert!(offset_of!(EventLoop, params) == at(1), "params moved");
    assert!(offset_of!(EventLoop, state) == at(3), "state moved");
    assert!(offset_of!(EventLoop, start) == at(4), "start moved");
    assert!(offset_of!(EventLoop, stop) == at(5), "stop moved");
    assert!(offset_of!(EventLoop, free) == at(6), "free moved");
    assert!(offset_of!(EventLoop, run) == at(7), "run moved");
    assert!(offset_of!(EventLoop, cancel) == at(8), "cancel moved");
    assert!(offset_of!(EventLoop, now) == at(9), "now moved");
    assert!(
        offset_of!(EventLoop, now_monotonic) == at(10),
        "now_monotonic moved"
    );
    assert!(
        offset_of!(EventLoop, utc_offset) == at(11),
        "utc_offset moved"
    );
    assert!(
        offset_of!(EventLoop, next_timer) == at(12),
        "next_timer moved"
    );
    assert!(
        offset_of!(EventLoop, add_timer) == at(13),
        "add_timer moved"
    );
    assert!(
        offset_of!(EventLoop, modify_timer) == at(14),
        "modify_timer moved"
    );
    assert!(
        offset_of!(EventLoop, remove_timer) == at(15),
        "remove_timer moved"
    );
    assert!(
        offset_of!(EventLoop, add_delayed) == at(16),
        "add_delayed moved"
    );
    assert!(
        offset_of!(EventLoop, remove_delayed) == at(17),
        "remove_delayed moved"
    );
    assert!(offset_of!(EventLoop, sources) == at(18), "sources moved");
    assert!(offset_of!(EventLoop, register) == at(19), "register moved");
    assert!(
        offset_of!(EventLoop, deregister) == at(20),
        "deregister moved"
    );
    assert!(offset_of!(EventLoop, lock) == at(21), "lock moved");
    assert!(offset_of!(EventLoop, unlock) == at(22), "unlock moved");
};

/// `UA_Client`, which Rust holds only by pointer.
#[repr(C)]
pub(crate) struct Client([u8; 0]);

/// The `shim_now` of `shim.c`.
pub(crate) type Now = unsafe extern "C" fn(clock: *mut c_void) -> i64;

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
    pub(crate) fn UA_random_seed_deterministic(value: u64);

    pub(crate) fn shim_loop_new(now: Now, clock: *mut c_void) -> *mut EventLoop;
    pub(crate) fn shim_loop_free(el: *mut EventLoop);

    pub(crate) fn shim_client_new(el: *mut EventLoop) -> *mut Client;
    pub(crate) fn UA_Client_run_iterate(client: *mut Client, timeout_ms: u32) -> u32;
    pub(crate) fn UA_Client_delete(client: *mut Client);
}

/// Only `link` calls these.
#[cfg(test)]
pub(crate) mod test {
    use std::ffi::c_void;

    use super::Bytes;

    unsafe extern "C" {
        pub(crate) fn UA_DateTime_now() -> i64;
        pub(crate) fn UA_DateTime_nowMonotonic() -> i64;
        pub(crate) fn UA_DateTime_localTimeUtcOffset() -> i64;

        pub(crate) fn UA_EventLoop_new_POSIX(logger: *const c_void) -> *mut c_void;
        pub(crate) fn UA_ConnectionManager_new_POSIX_TCP(name: Bytes) -> *mut c_void;
        pub(crate) fn UA_ConnectionManager_new_POSIX_UDP(name: Bytes) -> *mut c_void;
        pub(crate) fn UA_ConnectionManager_new_POSIX_Ethernet(
            name: Bytes,
        ) -> *mut c_void;
        pub(crate) fn UA_InterruptManager_new_POSIX(name: Bytes) -> *mut c_void;

        pub(crate) fn UA_Client_connectAsync(
            client: *mut super::Client,
            url: *const std::ffi::c_char,
        ) -> u32;
    }
}
