//! open62541 decodes any input as each of its built-in types with no memory fault or
//! leak, and the encoding of each value that it decodes decodes to a value with the
//! same encoding.
//!
//! Input: a byte that picks the type by its index in `UA_TYPES`, then the encoded
//! value. A new open62541 copy can change the type of an input.
//!
//! The C code gets no coverage or AddressSanitizer flags from cargo-fuzz. Build with
//! `CC=clang CFLAGS="-fsanitize=fuzzer-no-link,address"`.

#![no_main]
#![expect(unsafe_code, reason = "open62541 is a C library")]

use std::ffi::{CStr, c_char, c_void};

use connector_opcua as _;
use libfuzzer_sys::fuzz_target;

/// `UA_NodeId`.
#[repr(C)]
struct NodeId {
    namespace: u16,
    kind: u32,
    identifier: [u64; 2],
}

/// `UA_DataType`, with `UA_ENABLE_TYPEDESCRIPTION`.
#[repr(C)]
struct DataType {
    name: *const c_char,
    ids: [NodeId; 3],
    /// `memSize` in the low 16 bits, then `typeKind`, `pointerFree`, `overlayable`,
    /// and `membersSize`.
    bits: u32,
    members: *const c_void,
}

impl DataType {
    fn size(&self) -> usize {
        usize::try_from(self.bits & 0xffff).expect("a u16 fits a usize")
    }

    fn name(&self) -> &str {
        // SAFETY: each type in `UA_TYPES` has a static name that ends with a NUL.
        let name = unsafe { CStr::from_ptr(self.name) };
        name.to_str().expect("each type name is ASCII")
    }
}

/// `UA_DecodeBinaryOptions`, all null but the length that the decoder sets.
#[repr(C)]
#[derive(Default)]
struct Options {
    pointers: [usize; 4],
    decoded: usize,
}

/// `UA_ByteString`.
#[repr(C)]
struct Bytes {
    length: usize,
    data: *mut u8,
}

/// `UA_TYPES_COUNT`.
const COUNT: usize = 388;

/// The index of `ByteString` in `UA_TYPES`.
const BYTE_STRING: usize = 14;

unsafe extern "C" {
    static UA_TYPES: [DataType; COUNT];
    fn UA_decodeBinary(
        input: *const Bytes,
        value: *mut c_void,
        data_type: *const DataType,
        options: *mut Options,
    ) -> u32;
    fn UA_encodeBinary(
        value: *const c_void,
        data_type: *const DataType,
        output: *mut Bytes,
        options: *mut c_void,
    ) -> u32;
    fn UA_calcSizeBinary(
        value: *const c_void,
        data_type: *const DataType,
        options: *mut c_void,
    ) -> usize;
    fn UA_clear(value: *mut c_void, data_type: *const DataType);
    fn UA_StatusCode_name(code: u32) -> *const c_char;
}

/// The table of built-in types.
///
/// # Panics
///
/// When `DataType` or `COUNT` does not match the copy.
fn types() -> &'static [DataType; COUNT] {
    // SAFETY: the table is initialized at compile time, and open62541 never writes it.
    let types = unsafe { &UA_TYPES };
    assert_eq!(
        [0, BYTE_STRING, COUNT - 1].map(|at| types[at].name()),
        ["Boolean", "ByteString", "PubSubConfiguration2DataType"],
        "DataType or COUNT does not match the copy"
    );
    types
}

/// The name of a status code, such as `BadDecodingError`.
fn status(code: u32) -> &'static str {
    // SAFETY: `UA_StatusCode_name` takes any code and gives a static C string.
    let name = unsafe { UA_StatusCode_name(code) };
    // SAFETY: the string is static, and ends with a NUL.
    let name = unsafe { CStr::from_ptr(name) };
    name.to_str().expect("each status name is ASCII")
}

/// A decoded value, cleared when dropped.
struct Value {
    data_type: &'static DataType,
    memory: Vec<u64>,
}

impl Value {
    /// Decodes a value of `data_type` from the start of `bytes`, and gives it with the
    /// number of bytes read, or gives the status name.
    fn decode(
        data_type: &'static DataType,
        bytes: &[u8],
    ) -> Result<(Self, usize), &'static str> {
        let mut memory = vec![0_u64; data_type.size().div_ceil(8)];
        let input = Bytes {
            length: bytes.len(),
            data: bytes.as_ptr().cast_mut(),
        };
        let mut options = Options::default();
        // SAFETY: the memory holds `memSize` bytes with 8-byte alignment, the decoder
        // only reads the input, and it clears the value when it fails.
        let code = unsafe {
            UA_decodeBinary(&input, memory.as_mut_ptr().cast(), data_type, &mut options)
        };
        if code != 0 {
            return Err(status(code));
        }
        Ok((Value { data_type, memory }, options.decoded))
    }

    fn pointer(&self) -> *const c_void {
        self.memory.as_ptr().cast()
    }

    /// Encodes the value, or gives the status name.
    fn encode(&self) -> Result<Vec<u8>, &'static str> {
        let mut output = Bytes {
            length: 0,
            data: std::ptr::null_mut(),
        };
        // SAFETY: the value is a decoded value of its type, and an empty output makes
        // the encoder allocate.
        let code = unsafe {
            UA_encodeBinary(
                self.pointer(),
                self.data_type,
                &mut output,
                std::ptr::null_mut(),
            )
        };
        if code != 0 {
            return Err(status(code));
        }
        // SAFETY: the encoder gave `length` bytes at `data`.
        let bytes = unsafe { std::slice::from_raw_parts(output.data, output.length) };
        let bytes = bytes.to_vec();
        // SAFETY: the encoder allocated the output, a `ByteString`.
        unsafe { UA_clear((&raw mut output).cast(), &types()[BYTE_STRING]) };
        Ok(bytes)
    }

    fn size(&self) -> usize {
        // SAFETY: the value is a decoded value of its type.
        unsafe {
            UA_calcSizeBinary(self.pointer(), self.data_type, std::ptr::null_mut())
        }
    }
}

impl Drop for Value {
    fn drop(&mut self) {
        // SAFETY: the value is a decoded value of its type, and is not used again.
        unsafe { UA_clear(self.memory.as_mut_ptr().cast(), self.data_type) };
    }
}

fuzz_target!(|bytes: &[u8]| {
    let Some((&at, input)) = bytes.split_first() else {
        return;
    };
    let data_type = &types()[usize::from(at) % COUNT];
    let name = data_type.name();
    let Ok((value, read)) = Value::decode(data_type, input) else {
        return;
    };
    assert!(
        read <= input.len(),
        "{name}: read {read} of {} bytes",
        input.len()
    );
    let once = value
        .encode()
        .unwrap_or_else(|e| panic!("{name}: encode gave {e}"));
    assert_eq!(
        value.size(),
        once.len(),
        "{name}: the size is not the encoded size"
    );
    // The zeros make room for the item on #435: a Variant of N ExtensionObjects decodes
    // only when 4N bytes follow its length.
    let mut padded = once.clone();
    padded.resize(once.len() * 2, 0);
    let (again, read) = Value::decode(data_type, &padded)
        .unwrap_or_else(|e| panic!("{name}: {once:02x?} does not decode: {e}"));
    assert_eq!(
        read,
        once.len(),
        "{name}: {once:02x?} decodes to another length"
    );
    let twice = again
        .encode()
        .unwrap_or_else(|e| panic!("{name}: encode gave {e}"));
    assert_eq!(
        once, twice,
        "{name}: the encoding changes on a second round trip"
    );
});
