//! The C functions of open62541, `shim.c`, and `server.c` that Rust calls, each
//! declared once. `shim.c` asserts the size of each C enum that this module mirrors,
//! and each value of such an enum that it names.

#![expect(unsafe_code, reason = "open62541 is a C library")]

use std::ffi::{CStr, c_char, c_int, c_void};
use std::marker::PhantomData;
use std::mem::offset_of;
use std::ptr;

/// `shim_node`: a node that borrows its name from a [`crate::node::Node`].
#[repr(C)]
pub(crate) struct Node<'a> {
    namespace: u16,
    number: u32,
    /// NULL for a node by number.
    name: *const u8,
    length: usize,
    borrowed: PhantomData<&'a str>,
}

impl<'a> From<&'a crate::node::Node> for Node<'a> {
    fn from(node: &'a crate::node::Node) -> Self {
        let (number, name, length) = match &node.key {
            crate::node::Key::Number(number) => (*number, ptr::null(), 0),
            crate::node::Key::Name(name) => {
                (0, name.as_str().as_ptr(), name.as_str().len())
            }
        };
        Self {
            namespace: node.namespace,
            number,
            name,
            length,
            borrowed: PhantomData,
        }
    }
}

impl Node<'_> {
    /// Gives the node, or `None` for a name that is empty or not UTF-8.
    ///
    /// # Safety
    ///
    /// The name of `self`, if any, points at its `length` live bytes.
    pub(crate) unsafe fn owned(&self) -> Option<crate::node::Node> {
        let key = if self.name.is_null() {
            crate::node::Key::Number(self.number)
        } else {
            // SAFETY: the caller ensures it.
            let name = unsafe { text(self.name, self.length) }?;
            crate::node::Key::Name(crate::node::Name::new(name)?)
        };
        Some(crate::node::Node {
            namespace: self.namespace,
            key,
        })
    }
}

impl Default for Node<'_> {
    /// The node `i=0`.
    fn default() -> Self {
        Self {
            namespace: 0,
            number: 0,
            name: ptr::null(),
            length: 0,
            borrowed: PhantomData,
        }
    }
}

/// Gives the `length` bytes at `data` as text, or `None` when they are not UTF-8.
///
/// # Safety
///
/// `data` points at `length` live bytes, or `length` is 0.
unsafe fn text<'a>(data: *const u8, length: usize) -> Option<&'a str> {
    if length == 0 {
        return Some("");
    }
    // SAFETY: the caller ensures it.
    std::str::from_utf8(unsafe { std::slice::from_raw_parts(data, length) }).ok()
}

/// An open62541 status code.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) struct Status(pub(crate) u32);

impl Status {
    pub(crate) const GOOD: Self = Self(0);
    pub(crate) const BAD_NOT_FOUND: Self = Self(0x803E_0000);
    #[cfg(test)]
    pub(crate) const BAD_INVALID_ARGUMENT: Self = Self(0x80AB_0000);
    pub(crate) const BAD_CONNECTION_CLOSED: Self = Self(0x80AE_0000);
    #[cfg(test)]
    pub(crate) const BAD_TCP_ENDPOINT_URL_INVALID: Self = Self(0x8083_0000);
    pub(crate) const BAD_TIMEOUT: Self = Self(0x800A_0000);
    pub(crate) const BAD_REQUEST_TOO_LARGE: Self = Self(0x80B8_0000);
    pub(crate) const BAD_TOO_MANY_OPERATIONS: Self = Self(0x8010_0000);
    pub(crate) const BAD_UNEXPECTED_ERROR: Self = Self(0x8001_0000);
    #[cfg(test)]
    pub(crate) const BAD_INTERNAL_ERROR: Self = Self(0x8002_0000);
    pub(crate) const BAD_OUT_OF_RANGE: Self = Self(0x803C_0000);
    #[cfg(test)]
    pub(crate) const BAD_NOT_SUPPORTED: Self = Self(0x803D_0000);
    pub(crate) const BAD_NODE_ID_UNKNOWN: Self = Self(0x8034_0000);
    pub(crate) const BAD_CONTINUATION_POINT_INVALID: Self = Self(0x804A_0000);
    pub(crate) const BAD_NO_CONTINUATION_POINTS: Self = Self(0x804B_0000);
    #[cfg(test)]
    pub(crate) const BAD_USER_ACCESS_DENIED: Self = Self(0x801F_0000);
    #[cfg(test)]
    pub(crate) const BAD_TYPE_MISMATCH: Self = Self(0x8074_0000);
    #[cfg(test)]
    pub(crate) const BAD_NO_DATA: Self = Self(0x809B_0000);
    pub(crate) const BAD_SESSION_CLOSED: Self = Self(0x8026_0000);
    #[cfg(test)]
    pub(crate) const BAD_SESSION_ID_INVALID: Self = Self(0x8025_0000);
    #[cfg(test)]
    pub(crate) const BAD_SECURE_CHANNEL_CLOSED: Self = Self(0x8086_0000);
    #[cfg(test)]
    pub(crate) const BAD_SECURE_CHANNEL_ID_INVALID: Self = Self(0x8022_0000);
    #[cfg(test)]
    pub(crate) const BAD_RESPONSE_TOO_LARGE: Self = Self(0x80B9_0000);
    #[cfg(test)]
    pub(crate) const BAD_TCP_MESSAGE_TOO_LARGE: Self = Self(0x8080_0000);
    #[cfg(test)]
    pub(crate) const BAD_CONNECTION_REJECTED: Self = Self(0x80AC_0000);
    #[cfg(test)]
    pub(crate) const BAD_TOO_MANY_SESSIONS: Self = Self(0x8056_0000);
    #[cfg(test)]
    pub(crate) const BAD_SERVER_NOT_CONNECTED: Self = Self(0x800D_0000);
    #[cfg(test)]
    pub(crate) const BAD_SECURITY_MODE_REJECTED: Self = Self(0x8054_0000);
    #[cfg(test)]
    pub(crate) const BAD_DECODING_ERROR: Self = Self(0x8007_0000);
    #[cfg(test)]
    pub(crate) const BAD_CERTIFICATE_TIME_INVALID: Self = Self(0x8014_0000);
    #[cfg(test)]
    pub(crate) const BAD_CERTIFICATE_UNTRUSTED: Self = Self(0x801A_0000);
    #[cfg(test)]
    pub(crate) const BAD_CERTIFICATE_INVALID: Self = Self(0x8012_0000);
    #[cfg(test)]
    pub(crate) const BAD_SECURITY_CHECKS_FAILED: Self = Self(0x8013_0000);
    #[cfg(test)]
    pub(crate) const BAD_CERTIFICATE_URI_INVALID: Self = Self(0x8017_0000);

    /// Whether the code is of the severity Uncertain.
    pub(crate) fn uncertain(self) -> bool {
        self.0 >> 30 == 0b01
    }

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

impl Bytes {
    /// Gives a `UA_ByteString` that borrows `bytes`, which C must not free or keep
    /// past the borrow.
    pub(crate) fn of(bytes: &[u8]) -> Self {
        Self {
            length: bytes.len(),
            data: bytes.as_ptr().cast_mut(),
        }
    }

    /// Gives the bytes as text, with each sequence that is not UTF-8 as U+FFFD.
    ///
    /// # Safety
    ///
    /// `data` points at `length` live bytes, or `length` is 0.
    pub(crate) unsafe fn lossy(&self) -> String {
        if self.length == 0 {
            return String::new();
        }
        // SAFETY: the caller ensures it.
        let bytes = unsafe { std::slice::from_raw_parts(self.data, self.length) };
        String::from_utf8_lossy(bytes).into_owned()
    }

    /// Gives a copy of the bytes.
    ///
    /// # Safety
    ///
    /// As [`Bytes::lossy`].
    pub(crate) unsafe fn to_vec(&self) -> Vec<u8> {
        if self.length == 0 {
            return Vec::new();
        }
        // SAFETY: the caller ensures it.
        unsafe { std::slice::from_raw_parts(self.data, self.length) }.to_vec()
    }
}

impl Default for Bytes {
    /// No bytes.
    fn default() -> Self {
        Self {
            length: 0,
            data: ptr::null_mut(),
        }
    }
}

/// `UA_NodeId`.
#[repr(C)]
pub(crate) struct Key {
    namespace: u16,
    kind: u32,
    identifier: [u64; 2],
}

/// `UA_DataType`, with `UA_ENABLE_TYPEDESCRIPTION`.
#[repr(C)]
pub(crate) struct DataType {
    name: *const c_char,
    keys: [Key; 3],
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

/// The kind of each type of [`Scalar`], which is also its index in `UA_TYPES`.
pub(crate) const BOOLEAN: u16 = 0;
pub(crate) const SBYTE: u16 = 1;
pub(crate) const BYTE: u16 = 2;
pub(crate) const INT16: u16 = 3;
pub(crate) const UINT16: u16 = 4;
pub(crate) const INT32: u16 = 5;
pub(crate) const UINT32: u16 = 6;
pub(crate) const INT64: u16 = 7;
pub(crate) const UINT64: u16 = 8;
pub(crate) const FLOAT: u16 = 9;
pub(crate) const DOUBLE: u16 = 10;

/// The bit that `shim_value_scalar` adds to the kind of an array, `SHIM_ARRAY`.
pub(crate) const ARRAY: u16 = 0x8000;

/// The index of `ByteString` in `UA_TYPES`.
pub(crate) const BYTE_STRING: usize = 14;

/// The index of `DataValue` in `UA_TYPES`.
pub(crate) const DATA_VALUE: usize = 22;

/// The index of `Variant` in `UA_TYPES`.
#[cfg(test)]
pub(crate) const VARIANT: usize = 23;

// `shim.c` asserts the same sizes.
const _: () = {
    assert!(size_of::<Key>() == 24, "UA_NodeId changed");
    assert!(size_of::<DataType>() == 96, "UA_DataType changed");
    assert!(
        size_of::<DecodeOptions>() == 40,
        "UA_DecodeBinaryOptions changed"
    );
    assert!(
        offset_of!(DecodeOptions, decoded) == 32,
        "decodedLength moved"
    );
    assert!(size_of::<Node<'_>>() == 24, "shim_node changed");
    assert!(offset_of!(Node<'_>, name) == 8, "shim_node.name moved");
    assert!(size_of::<Scalar>() == 8, "shim_scalar changed");
    assert!(size_of::<Reference>() == 88, "shim_reference changed");
    assert!(
        offset_of!(Reference, class) == 80,
        "shim_reference.class moved"
    );
    assert!(
        offset_of!(Reference, held) == 84,
        "shim_reference.held moved"
    );
    assert!(
        offset_of!(Reference, typed) == 85,
        "shim_reference.typed moved"
    );
};

/// Gives `UA_TYPES`, the table of built-in types.
pub(crate) fn types() -> &'static [DataType; TYPES] {
    // SAFETY: the table is initialized at compile time, and open62541 never writes it.
    unsafe { &UA_TYPES }
}

/// `UA_NS0ID_SERVER_SERVERCAPABILITIES_OPERATIONLIMITS_MAXNODESPERREAD`, the node of
/// the most nodes of one Read request of a server, a `UInt32`. `shim.c` asserts it.
pub(crate) const MAX_NODES_PER_READ: u32 = 11705;

/// `UA_DATETIME_UNIX_EPOCH`, the 100 ns ticks of a `DateTime` from 1601 to the Unix
/// epoch. `shim.c` asserts it.
pub(crate) const UNIX_EPOCH: i64 = 116_444_736_000_000_000;

/// `UA_NS0ID_SERVER_SERVERSTATUS_CURRENTTIME`, the node of the current time of a
/// server. `shim.c` asserts it.
pub(crate) const TIME: u32 = 2258;

/// `UA_NS0ID_SERVER_SERVERSTATUS_STATE`, the node of the state of a server. `shim.c`
/// asserts it.
pub(crate) const SERVER_STATE: u32 = 2259;

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

/// `UA_KeyValueMap`.
#[repr(C)]
pub(crate) struct KeyValueMap {
    pub(crate) size: usize,
    pub(crate) map: *mut c_void,
}

/// `UA_ConnectionState`.
pub(crate) type ConnectionState = c_int;
pub(crate) const OPENING: ConnectionState = 1;
pub(crate) const ESTABLISHED: ConnectionState = 2;
pub(crate) const CLOSING: ConnectionState = 3;

/// `UA_ConnectionManager_connectionCallback`.
pub(crate) type ConnectionCallback = unsafe extern "C" fn(
    cm: *mut ConnectionManager,
    id: usize,
    application: *mut c_void,
    context: *mut *mut c_void,
    state: ConnectionState,
    params: *const KeyValueMap,
    message: Bytes,
);

/// `UA_ConnectionManager`, which Rust holds only by pointer.
#[repr(C)]
pub(crate) struct ConnectionManager([u8; 0]);

/// The hooks of a manager of `shim_cm_new`. Each takes its `state`.
#[repr(C)]
pub(crate) struct Hooks {
    pub(crate) open: unsafe extern "C" fn(
        state: *mut c_void,
        host: Bytes,
        port: u16,
        application: *mut c_void,
        context: *mut c_void,
        callback: ConnectionCallback,
    ) -> u32,
    pub(crate) listen: unsafe extern "C" fn(
        state: *mut c_void,
        host: Bytes,
        port: u16,
        application: *mut c_void,
        context: *mut c_void,
        callback: ConnectionCallback,
    ) -> u32,
    pub(crate) send:
        unsafe extern "C" fn(state: *mut c_void, id: usize, buffer: *mut Bytes) -> u32,
    pub(crate) close: unsafe extern "C" fn(state: *mut c_void, id: usize) -> u32,
}

/// `UA_Client`, which Rust holds only by pointer.
#[repr(C)]
pub(crate) struct Client([u8; 0]);

/// `shim_security` of `shim.h`, which borrows each of its byte strings.
#[repr(C)]
pub(crate) struct Security<'a> {
    pub(crate) certificate: Bytes,
    pub(crate) key: Bytes,
    pub(crate) trust: Bytes,
    pub(crate) borrowed: PhantomData<&'a [u8]>,
}

/// `UA_MessageSecurityMode`, the modes of a channel that signs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(transparent)]
pub(crate) struct Mode(pub(crate) c_int);

impl Mode {
    pub(crate) const SIGN: Self = Self(2);
    pub(crate) const SIGN_AND_ENCRYPT: Self = Self(3);
}

/// `UA_ReadRequest`, which Rust holds only by pointer.
#[repr(C)]
pub(crate) struct ReadRequest([u8; 0]);

/// `UA_ReadResponse`, which Rust holds only by pointer.
#[repr(C)]
pub(crate) struct ReadResponse([u8; 0]);

/// `shim_scalar`: a scalar of a type from [`BOOLEAN`] to [`DOUBLE`], or the type of
/// another value. Its kind tells which field holds it.
#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) union Scalar {
    pub(crate) boolean: bool,
    pub(crate) sbyte: i8,
    pub(crate) byte: u8,
    pub(crate) int16: i16,
    pub(crate) uint16: u16,
    pub(crate) int32: i32,
    pub(crate) uint32: u32,
    pub(crate) int64: i64,
    pub(crate) uint64: u64,
    pub(crate) float: f32,
    pub(crate) double: f64,
    pub(crate) data_type: *const DataType,
}

/// `UA_DataValue`, which Rust holds only by pointer.
#[repr(C)]
pub(crate) struct DataValue([u8; 0]);

/// `UA_SecureChannelState` of a client.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(transparent)]
pub(crate) struct Channel(pub(crate) c_int);

impl Channel {
    /// `UA_SECURECHANNELSTATE_CLOSED`.
    pub(crate) const CLOSED: Self = Self(0);
    /// `UA_SECURECHANNELSTATE_OPEN`.
    pub(crate) const OPEN: Self = Self(11);
}

/// `UA_SessionState` of a client.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(transparent)]
pub(crate) struct Session(pub(crate) c_int);

impl Session {
    /// `UA_SESSIONSTATE_CREATE_REQUESTED`.
    pub(crate) const CREATE_REQUESTED: Self = Self(1);
    /// `UA_SESSIONSTATE_ACTIVATED`.
    pub(crate) const ACTIVATED: Self = Self(4);
}

/// A service of `shim_client_service`: an index of `SERVICES` in `shim.c`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(transparent)]
pub(crate) struct Service(usize);

impl Service {
    pub(crate) const BROWSE: Self = Self(0);
    pub(crate) const BROWSE_NEXT: Self = Self(1);
    pub(crate) const TRANSLATE: Self = Self(2);
    pub(crate) const READ: Self = Self(3);
}

/// `UA_ClientAsyncServiceCallback`.
pub(crate) type Served = unsafe extern "C" fn(
    client: *mut Client,
    data: *mut c_void,
    request: u32,
    response: *mut c_void,
);

/// The values of [`Reference::held`], which `shim.c` asserts.
pub(crate) mod held {
    /// A node of this server by a number or a name.
    pub(crate) const NODE: u8 = 0;
    /// A node of this server by a GUID or opaque key.
    pub(crate) const KEY: u8 = 1;
    /// A node of another server, or of a namespace by its URI.
    pub(crate) const REMOTE: u8 = 2;
}

/// `shim_reference`: a reference of a browse, which borrows from its answer.
#[repr(C)]
#[derive(Default)]
pub(crate) struct Reference {
    /// The node, valid only when `held` is 0.
    pub(crate) node: Node<'static>,
    /// The reference type, valid only when `typed`.
    pub(crate) via: Node<'static>,
    /// The text of the browse name.
    pub(crate) name: Bytes,
    /// The text of the display name.
    pub(crate) display: Bytes,
    /// The `UA_NodeClass`.
    pub(crate) class: u32,
    /// One of the values of [`held`].
    pub(crate) held: u8,
    /// Whether the reference type is by a number or a name.
    pub(crate) typed: bool,
}

/// `UA_ClientAsyncReadCallback`.
pub(crate) type Read = unsafe extern "C" fn(
    client: *mut Client,
    data: *mut c_void,
    request: u32,
    response: *mut ReadResponse,
);

/// The `shim_now` of `shim.c`.
pub(crate) type Now = unsafe extern "C" fn(time: *mut c_void) -> i64;

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
    pub(crate) fn UA_parseEndpointUrl(
        url: *const Bytes,
        host: *mut Bytes,
        port: *mut u16,
        path: *mut Bytes,
    ) -> u32;
    pub(crate) fn UA_random_seed_deterministic(value: u64);

    pub(crate) fn shim_loop_new(
        now: Now,
        wall: Now,
        time: *mut c_void,
        log: *mut c_void,
    ) -> *mut EventLoop;
    pub(crate) fn shim_loop_free(el: *mut EventLoop);
    pub(crate) fn shim_loop_delayed(el: *mut EventLoop) -> bool;
    pub(crate) static shim_free_passes: c_int;

    pub(crate) fn shim_client_new(
        el: *mut EventLoop,
        chunks: u32,
        queued: u32,
    ) -> *mut Client;
    pub(crate) fn UA_Client_run_iterate(client: *mut Client, timeout_ms: u32) -> u32;
    pub(crate) fn UA_Client_delete(client: *mut Client);
    pub(crate) fn shim_client_free(client: *mut Client);
    pub(crate) fn shim_client_secure(
        client: *mut Client,
        mode: Mode,
        security: *const Security<'_>,
    ) -> u32;
    pub(crate) fn shim_client_timeout(client: *mut Client) -> u32;
    pub(crate) fn shim_client_limits(
        client: *mut Client,
        limits: *mut [usize; 5],
    ) -> u32;
    pub(crate) fn UA_Client_disconnectAsync(client: *mut Client) -> u32;
    pub(crate) fn UA_Client_disconnectSecureChannelAsync(client: *mut Client) -> u32;
    pub(crate) fn UA_Client_connectAsync(
        client: *mut Client,
        url: *const c_char,
    ) -> u32;
    pub(crate) fn UA_Client_getState(
        client: *mut Client,
        channel: *mut Channel,
        session: *mut Session,
        status: *mut u32,
    );
    pub(crate) fn shim_read_new(
        nodes: *const Node<'_>,
        count: usize,
    ) -> *mut ReadRequest;
    pub(crate) fn shim_read_free(request: *mut ReadRequest);
    pub(crate) fn shim_client_read(
        client: *mut Client,
        request: *mut ReadRequest,
        callback: Read,
        data: *mut c_void,
        id: *mut u32,
    ) -> u32;
    pub(crate) fn shim_response_result(response: *const c_void) -> u32;
    pub(crate) fn shim_read_count(response: *const ReadResponse) -> usize;
    pub(crate) fn shim_read_result(
        response: *const ReadResponse,
        index: usize,
    ) -> *const DataValue;
    pub(crate) fn shim_value_scalar(
        value: *const DataValue,
        out: *mut Scalar,
        kind: *mut u16,
    ) -> u32;

    pub(crate) fn shim_client_service(
        client: *mut Client,
        request: *mut c_void,
        service: Service,
        callback: Served,
        data: *mut c_void,
        id: *mut u32,
    ) -> u32;
    pub(crate) fn shim_answer_move(
        answer: *mut c_void,
        service: Service,
    ) -> *mut c_void;
    pub(crate) fn shim_answer_count(answer: *const c_void) -> usize;
    pub(crate) fn shim_answer_free(answer: *mut c_void, service: Service);
    pub(crate) fn shim_request_free(request: *mut c_void, service: Service);
    pub(crate) fn shim_request_bytes(
        request: *const c_void,
        service: Service,
        token: usize,
    ) -> usize;
    pub(crate) fn shim_answer_bytes(service: Service) -> usize;
    // Each `shim_item_*` takes nothing and reads no state.
    pub(crate) safe fn shim_item_value() -> usize;
    pub(crate) safe fn shim_item_fact() -> usize;
    pub(crate) safe fn shim_item_unit() -> usize;
    pub(crate) safe fn shim_item_path() -> usize;
    pub(crate) safe fn shim_item_result() -> usize;
    pub(crate) safe fn shim_item_reference() -> usize;
    #[cfg(test)]
    pub(crate) fn shim_service_type(service: Service, answer: bool) -> *const DataType;
    pub(crate) fn shim_browse_new(
        nodes: *const Node<'_>,
        count: usize,
        reference: u32,
        inverse: bool,
        classes: u32,
        most: u32,
    ) -> *mut c_void;
    pub(crate) fn shim_browse_next_new(
        points: *const Bytes,
        count: usize,
        release: bool,
    ) -> *mut c_void;
    pub(crate) fn shim_browse_result(
        answer: *const c_void,
        index: usize,
        references: *mut usize,
        point: *mut Bytes,
    ) -> u32;
    pub(crate) fn shim_browse_reference(
        answer: *const c_void,
        result: usize,
        index: usize,
        out: *mut Reference,
    );
    pub(crate) fn shim_browse_text(
        answer: *const c_void,
        result: usize,
        index: usize,
        out: *mut Bytes,
    ) -> u32;
    pub(crate) fn shim_units_new(nodes: *const Node<'_>, count: usize) -> *mut c_void;
    pub(crate) fn shim_units_target(
        answer: *const c_void,
        index: usize,
        out: *mut Node<'_>,
    ) -> bool;
    pub(crate) fn shim_read_attributes_new(
        nodes: *const Node<'_>,
        attributes: *const u32,
        count: usize,
    ) -> *mut c_void;
    pub(crate) fn shim_value_node(value: *const DataValue, out: *mut Node<'_>) -> u32;
    pub(crate) fn shim_value_time(value: *const DataValue, out: *mut i64) -> u32;
    pub(crate) fn shim_value_unit(
        value: *const DataValue,
        uri: *mut Bytes,
        code: *mut i32,
        text: *mut Bytes,
    ) -> u32;

    pub(crate) fn shim_cm_new(
        el: *mut EventLoop,
        hooks: *const Hooks,
        state: *mut c_void,
    ) -> *mut ConnectionManager;
    pub(crate) fn shim_cm_free(cm: *mut ConnectionManager);
    pub(crate) fn shim_buffer_free(buffer: *mut Bytes);
    pub(crate) fn shim_establish(
        cm: *mut ConnectionManager,
        id: usize,
        application: *mut c_void,
        context: *mut *mut c_void,
        callback: ConnectionCallback,
        address: *const u8,
        length: usize,
        port: *const u16,
    );
}

/// Only tests and `bench` use these.
#[cfg(any(test, feature = "sim"))]
#[cfg_attr(not(test), expect(dead_code, reason = "`bench` uses only some"))]
pub(crate) mod test;
