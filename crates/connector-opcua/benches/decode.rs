//! The time to decode and clear a `PublishResponse` of 1, 100, or 1000 values of type
//! Double, which allocates through the global allocator once for each value and 3
//! more times. Also one allocation and free alone, and a `ResponseHeader`, whose decode
//! allocates nothing, as the control.

#![expect(unsafe_code, reason = "the bench calls the C library")]

use std::ffi::c_void;

use connector_opcua as _;
use divan::Bencher;

fn main() {
    divan::main();
}

/// A `UA_ByteString`.
#[repr(C)]
struct Bytes {
    length: usize,
    data: *const u8,
}

/// A `UA_NodeId` with a numeric identifier.
#[repr(C, align(8))]
struct Key {
    namespace: u16,
    kind: u32,
    numeric: u32,
    rest: [u32; 3],
}

unsafe extern "C" {
    fn UA_findDataType(key: *const Key) -> *const c_void;
    fn UA_new(kind: *const c_void) -> *mut c_void;
    fn UA_delete(value: *mut c_void, kind: *const c_void);
    fn UA_clear(value: *mut c_void, kind: *const c_void);
    fn UA_decodeBinary(
        bytes: *const Bytes,
        value: *mut c_void,
        kind: *const c_void,
        options: *const c_void,
    ) -> u32;
}

const UINT32: u32 = 7;
const RESPONSE_HEADER: u32 = 392;
const PUBLISH_RESPONSE: u32 = 827;
/// The binary encoding of a `DataChangeNotification`.
const DATA_CHANGE: u16 = 811;

/// Gives the type of namespace 0 with the key `numeric`.
fn kind(numeric: u32) -> *const c_void {
    let key = Key {
        namespace: 0,
        kind: 0,
        numeric,
        rest: [0; 3],
    };
    // SAFETY: `key` is a valid numeric node key.
    let kind = unsafe { UA_findDataType(&raw const key) };
    assert!(!kind.is_null(), "the copy holds the type {numeric}");
    kind
}

/// The length of an array or a byte string, or -1 for a null one.
fn length(bytes: &mut Vec<u8>, length: Option<usize>) {
    let length = length.map_or(-1, |n| i32::try_from(n).expect("the length fits"));
    bytes.extend(length.to_le_bytes());
}

fn response_header(bytes: &mut Vec<u8>) {
    bytes.extend(1_i64.to_le_bytes());
    bytes.extend(7_u32.to_le_bytes());
    bytes.extend(0_u32.to_le_bytes());
    // No diagnostic info, no string table, and an empty extension object.
    bytes.push(0);
    length(bytes, None);
    bytes.extend([0, 0, 0]);
}

/// A `DataChangeNotification` of `values` values, each with its source and server time.
fn data_change(values: usize) -> Vec<u8> {
    let mut bytes = Vec::new();
    length(&mut bytes, Some(values));
    for handle in 0..values {
        bytes.extend(
            u32::try_from(handle)
                .expect("the handle fits")
                .to_le_bytes(),
        );
        // A value and both times, of the type Double.
        bytes.extend([0x0d, 11]);
        bytes.extend(1.5_f64.to_le_bytes());
        bytes.extend(2_i64.to_le_bytes());
        bytes.extend(3_i64.to_le_bytes());
    }
    length(&mut bytes, None);
    bytes
}

fn publish_response(values: usize) -> Vec<u8> {
    let mut bytes = Vec::new();
    response_header(&mut bytes);
    bytes.extend(1_u32.to_le_bytes());
    length(&mut bytes, None);
    bytes.push(0);
    bytes.extend(1_u32.to_le_bytes());
    bytes.extend(4_i64.to_le_bytes());
    length(&mut bytes, Some(1));
    // An extension object with a four-byte node key and a body.
    bytes.extend([1, 0]);
    bytes.extend(DATA_CHANGE.to_le_bytes());
    bytes.push(1);
    let body = data_change(values);
    length(&mut bytes, Some(body.len()));
    bytes.extend(body);
    length(&mut bytes, None);
    length(&mut bytes, None);
    bytes
}

/// Times the decode and the clear of `bytes` as a value of `kind`.
fn decode(bencher: Bencher<'_, '_>, bytes: &[u8], kind: *const c_void) {
    // SAFETY: `kind` is a type of the copy.
    let value = unsafe { UA_new(kind) };
    assert!(!value.is_null(), "the allocation succeeded");
    let bytes = Bytes {
        length: bytes.len(),
        data: bytes.as_ptr(),
    };
    let once = || {
        // SAFETY: `value` is a cleared value of `kind`, and `bytes` lives.
        let status =
            unsafe { UA_decodeBinary(&raw const bytes, value, kind, std::ptr::null()) };
        // SAFETY: `value` is a value of `kind`.
        unsafe { UA_clear(value, kind) };
        status
    };
    assert_eq!(once(), 0, "the bytes decode");
    bencher.bench_local(once);
    // SAFETY: `value` is a cleared value of `kind`, from `UA_new`.
    unsafe { UA_delete(value, kind) };
}

#[divan::bench(args = [1, 100, 1000])]
fn publish(bencher: Bencher<'_, '_>, values: usize) {
    decode(bencher, &publish_response(values), kind(PUBLISH_RESPONSE));
}

#[divan::bench]
fn header(bencher: Bencher<'_, '_>) {
    let mut bytes = Vec::new();
    response_header(&mut bytes);
    decode(bencher, &bytes, kind(RESPONSE_HEADER));
}

/// One `UA_calloc` and one `UA_free`.
#[divan::bench]
fn allocate(bencher: Bencher<'_, '_>) {
    let kind = kind(UINT32);
    bencher.bench_local(|| {
        // SAFETY: `kind` is a type of the copy.
        let value = unsafe { UA_new(kind) };
        // SAFETY: `value` is NULL or a value of `kind` from `UA_new`.
        unsafe { UA_delete(value, kind) };
    });
}
