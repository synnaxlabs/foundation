//! The round trip of open62541 binary decoding, and the read of each decoded
//! `DataValue`, for the `connector_opcua_decode` fuzz target, and the read of the
//! certificate of a server, for the `connector_opcua_certificate` fuzz target.

#![expect(unsafe_code, reason = "open62541 is a C library")]

use std::ffi::c_void;

use env::clock::Clock;
use env::rng::Rng;

use crate::client::Credentials;
use crate::event::{self, Loop};
use crate::ffi::{self, BYTE_STRING, Bytes, DATA_VALUE, DataType, DecodeOptions};
use crate::ffi::{Status, TYPES};
use crate::secure::SERVER;
use crate::value::Cause;

/// Reads `data` as a client reads the certificate that a server sends, on a loop of
/// `clock`: the check of a certificate group that trusts nothing, a channel context of
/// each policy with `data` as the certificate of the server, and the compare of `data`
/// with the certificate of a channel. Also reads the application URI of `data`, which
/// the client reads only from its own certificate.
///
/// # Panics
///
/// When the check gives `Good`, or a step that no input changes fails, with the status
/// name.
pub fn certificate(clock: Clock, data: &[u8]) {
    let (verified, _) = read(clock, data);
    assert_ne!(
        verified.name(),
        "Good",
        "a group that trusts nothing trusts it"
    );
}

/// Gives the status of the check of `data` in [`certificate`], and of the read of its
/// application URI.
///
/// # Panics
///
/// When a step that no input changes fails, with the status name.
fn read(clock: Clock, data: &[u8]) -> (Status, Status) {
    let events = Loop::new(
        event::Time::monotonic(clock),
        tracing::Span::none(),
        &mut Rng::from_seed(0),
    );
    let security = Credentials {
        certificate: SERVER.certificate,
        key: SERVER.key,
        trust: &[],
        user: None,
    }
    .raw();
    let (mut verified, mut named) = (0, 0);
    // SAFETY: each pointer is live for the call, which only reads `data` and
    // `security`, and the loop lives past it.
    let status = Status(unsafe {
        ffi::test::shim_read_certificate(
            &Bytes::of(data),
            &raw const security,
            events.raw(),
            &raw mut verified,
            &raw mut named,
        )
    });
    assert_eq!(status.name(), "Good", "a step that no input changes");
    (Status(verified), Status(named))
}

/// Decodes `data[2..]` as the type of `UA_TYPES` that the little-endian `u16` of
/// `data[..2]` picks, by its remainder by the table length. When the decode gives
/// `Good`, it encodes the value, decodes those bytes with as many zeros after them as
/// their length (#435), and encodes again. A `DataValue` gives the same read of its
/// value, or the same fault, and the same time (`value::time`), or the same status,
/// from both decodes. A decode under the calloc of the decode of a Publish answer,
/// with its limit of 128 bytes for each byte of the input, gives the same status,
/// length, and encoding, or `BadOutOfMemory` where the decode without it fails. An
/// input of fewer than 2 bytes decodes nothing.
///
/// # Panics
///
/// When a check of the round trip fails, with the type name and the step.
pub fn decode(data: &[u8]) {
    let Some((data_type, input)) = pick(data) else {
        return;
    };
    let name = data_type.name();
    let arena = arena(data_type, input).map_err(Status::name);
    let (value, decoded) = match Value::decode(data_type, input) {
        Ok(decoded) => decoded,
        Err(status) => {
            let status = Err(status.name());
            let arena = arena.map(drop);
            assert!(
                arena == status || arena == Err("BadOutOfMemory"),
                "{name}: the calloc changes the decode: {arena:?}, {status:?}"
            );
            return;
        }
    };
    assert!(
        decoded <= input.len(),
        "{name}: decoded {decoded} of {} bytes",
        input.len()
    );
    let once = value
        .encode()
        .unwrap_or_else(|e| panic!("{name}: encode gave {}", e.name()));
    assert_eq!(
        value.size(),
        once.len(),
        "{name}: the size is not the encoded size"
    );
    // A `Variant` of `ExtensionObject` values decodes only when 4 bytes follow its
    // length for each value, and the header of the first value fits at each value
    // (#435). Each other end-of-input check only loosens when bytes follow, so the
    // zeros change no other result, and the length check still holds.
    let mut padded = once.clone();
    padded.resize(once.len() * 2, 0);
    let (again, read) = Value::decode(data_type, &padded).unwrap_or_else(|e| {
        panic!("{name}: {once:02x?} does not decode: {}", e.name())
    });
    assert_eq!(
        read,
        once.len(),
        "{name}: {once:02x?} decodes to another length"
    );
    let twice = again
        .encode()
        .unwrap_or_else(|e| panic!("{name}: encode gave {}", e.name()));
    assert_eq!(
        once, twice,
        "{name}: the encoding changes on a second round trip"
    );
    if std::ptr::eq(data_type, &raw const ffi::types()[DATA_VALUE]) {
        assert_eq!(
            read_of(&value),
            read_of(&again),
            "{name}: {once:02x?} reads two ways"
        );
    }
    assert_eq!(
        arena,
        Ok((once, decoded)),
        "{name}: the calloc changes the decode"
    );
}

/// The text of the read of `value`, a `DataValue`, as a sample and as a time: a `NaN`
/// equals itself.
///
/// # Panics
///
/// When `value` is not a `DataValue`.
fn read_of(value: &Value) -> String {
    let data_value = &raw const ffi::types()[DATA_VALUE];
    assert!(std::ptr::eq(value.data_type, data_value), "a DataValue");
    // SAFETY: `value` is a live `DataValue`, as the assert checks.
    let sample = match unsafe { crate::value::Value::of(value.pointer().cast()) } {
        Ok(read) => format!("{read:?}"),
        Err(fault) => match fault.cause() {
            Cause::Status(status) => status.name().to_owned(),
            Cause::Type(opc_type) => opc_type.to_string(),
        },
    };
    // SAFETY: as above.
    let time = unsafe { crate::value::time(value.pointer().cast()) };
    format!("{sample}, {time:?}")
}

/// The binary encoding of the value of `data_type` at `value`.
///
/// # Safety
///
/// `value` points to a decoded value of `data_type`.
unsafe fn encode(
    value: *const c_void,
    data_type: &DataType,
) -> Result<Vec<u8>, Status> {
    let mut output = Bytes {
        length: 0,
        data: std::ptr::null_mut(),
    };
    // SAFETY: the caller gives a decoded value, and an empty output makes the encoder
    // allocate.
    let code = unsafe {
        ffi::UA_encodeBinary(value, data_type, &raw mut output, std::ptr::null_mut())
    };
    if code != 0 {
        return Err(Status(code));
    }
    // SAFETY: the encoder gave `length` bytes at `data`.
    let bytes = unsafe { std::slice::from_raw_parts(output.data, output.length) };
    let bytes = bytes.to_vec();
    let byte_string = &ffi::types()[BYTE_STRING];
    // SAFETY: the encoder allocated the output, a `ByteString`.
    unsafe { ffi::UA_clear((&raw mut output).cast(), byte_string) };
    Ok(bytes)
}

/// `PUBLISH_EXPANSION` of the patched open62541: the most decode memory of a Publish
/// answer for each of its bytes.
const EXPANSION: usize = 128;

/// Splits `data` into the type that its first two bytes pick and the rest.
fn pick(data: &[u8]) -> Option<(&'static DataType, &[u8])> {
    let (&[low, high], input) = data.split_first_chunk()?;
    let at = usize::from(u16::from_le_bytes([low, high])) % TYPES;
    Some((&ffi::types()[at], input))
}

/// Decodes a value of `data_type` from the start of `bytes` under the calloc of the
/// decode of a Publish answer, with the limit that a Publish answer of `bytes` gets,
/// and gives its encoding and the number of bytes read.
fn arena(
    data_type: &'static DataType,
    bytes: &[u8],
) -> Result<(Vec<u8>, usize), Status> {
    let mut memory = vec![0_u64; data_type.size().div_ceil(8)];
    let mut arena = ffi::test::Arena {
        blocks: std::ptr::null_mut(),
        left: bytes.len().saturating_mul(EXPANSION),
    };
    let input = Bytes {
        length: bytes.len(),
        data: bytes.as_ptr().cast_mut(),
    };
    let mut options = DecodeOptions::default();
    options.context = (&raw mut arena).cast();
    options.calloc = Some(ffi::test::__Client_PublishArena_calloc);
    // SAFETY: the memory holds `memSize` bytes with 8-byte alignment, the decoder only
    // reads the input, and the arena lives to the free below.
    let code = unsafe {
        ffi::UA_decodeBinary(
            &raw const input,
            memory.as_mut_ptr().cast(),
            data_type,
            &raw mut options,
        )
    };
    let encoded = if code == 0 {
        // SAFETY: the decode gave a value of `data_type`, in memory that lives.
        unsafe { encode(memory.as_ptr().cast(), data_type) }
            .map(|e| (e, options.decoded))
    } else {
        Err(Status(code))
    };
    // SAFETY: nothing reads the decoded value after this.
    unsafe { ffi::test::shim_arena_free(&raw mut arena) };
    encoded
}

/// A decoded value, cleared when dropped.
pub(crate) struct Value {
    data_type: &'static DataType,
    memory: Vec<u64>,
}

impl Value {
    /// Decodes a value of `data_type` from the start of `bytes`, and gives it with the
    /// number of bytes read.
    pub(crate) fn decode(
        data_type: &'static DataType,
        bytes: &[u8],
    ) -> Result<(Self, usize), Status> {
        let mut memory = vec![0_u64; data_type.size().div_ceil(8)];
        let input = Bytes {
            length: bytes.len(),
            data: bytes.as_ptr().cast_mut(),
        };
        let mut options = DecodeOptions::default();
        // SAFETY: the memory holds `memSize` bytes with 8-byte alignment, the decoder
        // only reads the input, and it clears the value when it fails.
        let code = unsafe {
            ffi::UA_decodeBinary(
                &raw const input,
                memory.as_mut_ptr().cast(),
                data_type,
                &raw mut options,
            )
        };
        if code != 0 {
            return Err(Status(code));
        }
        Ok((Value { data_type, memory }, options.decoded))
    }

    /// Gives the value, live until the drop.
    pub(crate) fn pointer(&self) -> *const c_void {
        self.memory.as_ptr().cast()
    }

    fn encode(&self) -> Result<Vec<u8>, Status> {
        // SAFETY: the value is a decoded value of its type.
        unsafe { encode(self.pointer(), self.data_type) }
    }

    fn size(&self) -> usize {
        // SAFETY: the value is a decoded value of its type.
        unsafe {
            ffi::UA_calcSizeBinary(self.pointer(), self.data_type, std::ptr::null_mut())
        }
    }
}

impl Drop for Value {
    fn drop(&mut self) {
        // SAFETY: the value is a decoded value of its type, and is not used again.
        unsafe { ffi::UA_clear(self.memory.as_mut_ptr().cast(), self.data_type) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ffi::VARIANT;

    #[test]
    fn the_layout_of_data_type_and_the_count_match_the_copy() {
        let types = ffi::types();
        assert_eq!(
            [0, BYTE_STRING, DATA_VALUE, TYPES - 1].map(|at| types[at].name()),
            [
                "Boolean",
                "ByteString",
                "DataValue",
                "PubSubConfiguration2DataType"
            ]
        );
        assert_eq!(
            [0, BYTE_STRING, VARIANT].map(|at| types[at].size()),
            [1, 16, 48]
        );
    }

    #[test]
    fn each_corpus_input_round_trips() {
        let root = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../oracles/fuzz/connector_opcua_decode"
        );
        let mut count = 0;
        for entry in std::fs::read_dir(root).unwrap() {
            let path = entry.unwrap().path();
            #[expect(clippy::disallowed_methods, reason = "a test reads its oracles")]
            let data = std::fs::read(&path).unwrap();
            decode(&data);
            count += 1;
        }
        assert!(count > 0, "{root} holds no input");
    }

    /// Each input of the certificate target reads to its refusal and to the status of
    /// the read of its URI.
    #[test]
    fn each_certificate_input_reads_to_its_refusal() {
        let root = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../oracles/fuzz/connector_opcua_certificate"
        );
        let mut sim = sim::Sim::new(sim::Config::default());
        let clock = sim.node(sim::node::Config::default()).clock();
        let mut read_to = Vec::new();
        for entry in std::fs::read_dir(root).unwrap() {
            let path = entry.unwrap().path();
            #[expect(clippy::disallowed_methods, reason = "a test reads its oracles")]
            let data = std::fs::read(&path).unwrap();
            let name = path.file_name().unwrap().to_str().unwrap().to_owned();
            let (verified, named) = read(Clock::clone(&clock), &data);
            read_to.push((name, verified.name(), named.name()));
        }
        read_to.sort();
        assert_eq!(
            read_to,
            [
                (
                    "anonymous",
                    "BadCertificateUntrusted",
                    "BadCertificateUriInvalid"
                ),
                ("der", "BadCertificateUntrusted", "Good"),
                (
                    "empty_uri",
                    "BadCertificateUntrusted",
                    "BadCertificateUriInvalid"
                ),
                (
                    "encrypted_pem",
                    "BadCertificateInvalid",
                    "BadSecurityChecksFailed"
                ),
                ("pem", "BadCertificateUntrusted", "Good"),
                (
                    "truncated_der",
                    "BadCertificateInvalid",
                    "BadSecurityChecksFailed"
                ),
                ("two_uris", "BadCertificateUntrusted", "Good"),
                (
                    "unnamed",
                    "BadCertificateUntrusted",
                    "BadCertificateUriInvalid"
                ),
            ]
            .map(|(name, verified, named)| (
                name.to_owned(),
                verified,
                named
            ))
        );
    }

    /// A `Variant` of 7 `ExtensionObject` values, each with a null type and no body.
    fn extension_objects() -> Vec<u8> {
        let mut bytes = vec![0x96, 7, 0, 0, 0];
        bytes.resize(bytes.len() + 7 * 3, 0);
        bytes
    }

    #[test]
    fn a_boolean_decodes_any_byte_and_encodes_0_or_1() {
        let (value, read) = Value::decode(&ffi::types()[0], &[2])
            .map_err(Status::name)
            .unwrap();
        assert_eq!(
            (
                value.encode().map_err(Status::name).unwrap(),
                read,
                value.size()
            ),
            (vec![1], 1, 1)
        );
    }

    #[test]
    fn a_short_input_does_not_decode() {
        let error = Value::decode(&ffi::types()[0], &[]).err().unwrap();
        assert_eq!(error.name(), "BadDecodingError");
    }

    // When this fails, #435 is fixed: remove the padding from `decode`.
    #[test]
    fn variant_extension_objects_need_4_bytes_each() {
        let variant = &ffi::types()[VARIANT];
        assert_eq!(variant.name(), "Variant");
        let bytes = extension_objects();
        let error = Value::decode(variant, &bytes).err().unwrap();
        assert_eq!(error.name(), "BadDecodingError");
        let mut padded = bytes.clone();
        padded.resize(bytes.len() + 7, 0);
        let (value, read) = Value::decode(variant, &padded)
            .map_err(Status::name)
            .unwrap();
        assert_eq!(
            (value.encode().map_err(Status::name).unwrap(), read),
            (bytes.clone(), bytes.len())
        );
    }

    #[test]
    fn decode_round_trips_variant_extension_objects() {
        let mut data = vec![u8::try_from(VARIANT).unwrap(), 0];
        data.extend(extension_objects());
        // The input decodes only with the 4 bytes for each value that #435 needs.
        data.extend([0; 7]);
        let (_, read) = Value::decode(&ffi::types()[VARIANT], &data[2..])
            .map_err(Status::name)
            .unwrap();
        assert_eq!(read, extension_objects().len());
        decode(&data);
    }

    /// A `Variant` of a `Range`, whose header is 5 bytes, and a null
    /// `ExtensionObject` of 3 bytes, then 8 bytes it does not read.
    fn range_then_null() -> Vec<u8> {
        let mut bytes = vec![0x96, 2, 0, 0, 0, 0x01, 0, 0x76, 0x03, 0x01, 16, 0, 0, 0];
        bytes.extend([0; 16]);
        bytes.extend([0, 0, 0]);
        bytes.extend([0; 8]);
        bytes
    }

    // When this fails, #435 is fixed: remove the padding from `decode`.
    #[test]
    fn each_extension_object_needs_the_header_of_the_first() {
        let variant = &ffi::types()[VARIANT];
        let bytes = range_then_null();
        let (value, read) = Value::decode(variant, &bytes)
            .map_err(Status::name)
            .unwrap();
        assert_eq!(read, bytes.len() - 8);
        let once = value.encode().map_err(Status::name).unwrap();
        assert_eq!(once, bytes[..read]);
        let error = Value::decode(variant, &once).err().unwrap();
        assert_eq!(error.name(), "BadEncodingLimitsExceeded");
    }

    #[test]
    fn decode_round_trips_a_range_then_a_null_extension_object() {
        let mut data = vec![u8::try_from(VARIANT).unwrap(), 0];
        data.extend(range_then_null());
        let (_, read) = Value::decode(&ffi::types()[VARIANT], &data[2..])
            .map_err(Status::name)
            .unwrap();
        assert_eq!(read, data.len() - 2 - 8);
        decode(&data);
    }

    #[test]
    fn decode_pads_by_the_length_of_the_encoding() {
        let mut data = vec![u8::try_from(VARIANT).unwrap(), 0, 0x96, 100, 0, 0, 0];
        data.resize(data.len() + 100 * 3, 0);
        // 100 zeros: the 4 bytes of each value that #435 needs.
        data.resize(data.len() + 100, 0);
        let (_, read) = Value::decode(&ffi::types()[VARIANT], &data[2..])
            .map_err(Status::name)
            .unwrap();
        assert_eq!(read, 5 + 100 * 3);
        decode(&data);
    }

    /// Under the calloc of the decode of a Publish answer, an `ExtensionObject` of a
    /// type that the decoder does not know keeps its string type id at the start of
    /// the memory of the first block that the decode takes, as nothing frees a copy.
    #[test]
    fn an_unknown_type_id_lives_in_the_memory_of_the_calloc() {
        let object = &ffi::types()[21];
        assert_eq!(object.name(), "ExtensionObject");
        let name = b"Foundation";
        let mut bytes = vec![0x03, 0x01, 0x00];
        bytes.extend(i32::try_from(name.len()).unwrap().to_le_bytes());
        bytes.extend(name);
        bytes.extend([0x01, 2, 0, 0, 0, 7, 7]);
        let input = Bytes {
            length: bytes.len(),
            data: bytes.as_mut_ptr(),
        };
        let mut arena = ffi::test::Arena {
            blocks: std::ptr::null_mut(),
            left: usize::MAX,
        };
        let mut options = DecodeOptions::default();
        options.context = (&raw mut arena).cast();
        options.calloc = Some(ffi::test::__Client_PublishArena_calloc);
        let mut value = [0_usize; 6];
        // SAFETY: the value holds the 48 bytes of an `ExtensionObject`, aligned to 8,
        // and the arena lives to the free below.
        let code = unsafe {
            ffi::UA_decodeBinary(
                &raw const input,
                value.as_mut_ptr().cast(),
                object,
                &raw mut options,
            )
        };
        // The id takes the first block, and the body a second one at the head of the
        // list. `next` is the first field of a block, and its 24-byte header takes 32.
        // SAFETY: the decode left two blocks.
        let first = unsafe { *arena.blocks.cast::<usize>() } + 32;
        // SAFETY: nothing reads the decoded value after this.
        unsafe { ffi::test::shim_arena_free(&raw mut arena) };
        // `content.encoded.typeId.identifier.string`: its length, then its data.
        assert_eq!(
            (Status(code), value[2], value[3].wrapping_sub(first)),
            (Status::GOOD, name.len(), 0)
        );
    }

    /// 50 nested `Variant` arrays, each of the most elements that the array check of the
    /// decoder lets through, take at most 128 bytes for each byte of the input under
    /// the calloc of the decode of a Publish answer, and fail with `BadOutOfMemory`.
    #[test]
    fn nested_arrays_take_at_most_128_bytes_for_each_byte() {
        let mut data = vec![u8::try_from(VARIANT).unwrap(), 0];
        let start = data.len();
        data.extend([0x98, 0, 0, 0, 0].repeat(50));
        data.push(0x3F);
        data.resize(data.len() + 10_000, 0);
        for level in 0..50 {
            let length = start + 5 * level + 1;
            let elements = 128 * ((data.len() - length - 4) / 48);
            let elements = u32::try_from(elements).unwrap().to_le_bytes();
            data[length..length + 4].copy_from_slice(&elements);
        }
        let (code, used) = variant_in_arena(&data[start..]);
        let most = (data.len() - start) * EXPANSION;
        assert_eq!(code.name(), "BadOutOfMemory");
        assert!(used <= most, "{used} of {most}");
        decode(&data);
    }

    /// An array of 1000 empty `DataValue` values takes 80 bytes for each byte and
    /// decodes under the calloc of the decode of a Publish answer.
    #[test]
    fn empty_data_values_decode_at_80_bytes_for_each_byte() {
        let mut data = vec![0x97];
        data.extend(1000_i32.to_le_bytes());
        data.resize(data.len() + 1000, 0);
        assert_eq!(variant_in_arena(&data), (Status::GOOD, 80_000));
    }

    /// Decodes a `Variant` from `bytes` under the calloc of the decode of a Publish
    /// answer of `bytes`, and gives the status and the bytes that its blocks used.
    fn variant_in_arena(bytes: &[u8]) -> (Status, usize) {
        let input = Bytes {
            length: bytes.len(),
            data: bytes.as_ptr().cast_mut(),
        };
        let mut arena = ffi::test::Arena {
            blocks: std::ptr::null_mut(),
            left: bytes.len() * EXPANSION,
        };
        let mut options = DecodeOptions::default();
        options.context = (&raw mut arena).cast();
        options.calloc = Some(ffi::test::__Client_PublishArena_calloc);
        let mut value = [0_u64; 6];
        // SAFETY: the value holds the 48 bytes of a `Variant`, aligned to 8, the decoder
        // only reads the input, and the arena lives to the free below.
        let code = unsafe {
            ffi::UA_decodeBinary(
                &raw const input,
                value.as_mut_ptr().cast(),
                std::ptr::from_ref(&ffi::types()[VARIANT]),
                &raw mut options,
            )
        };
        let mut used = 0;
        let mut block = arena.blocks.cast::<usize>().cast_const();
        while !block.is_null() {
            // SAFETY: a block starts with `next`, `size`, and `used`.
            used += unsafe { block.wrapping_add(2).read() };
            // SAFETY: as above.
            block = unsafe { block.read() } as *const usize;
        }
        // SAFETY: nothing reads the decoded value after this.
        unsafe { ffi::test::shim_arena_free(&raw mut arena) };
        (Status(code), used)
    }

    /// A `Variant` of 127 values and no bytes after the length passes the length check
    /// of the decoder, which then gives `BadDecodingError`, and `BadOutOfMemory` under
    /// the calloc of the decode of a Publish answer.
    #[test]
    fn a_long_array_with_no_bytes_decodes_the_same_under_each_calloc() {
        decode(&[0x17, 0x00, 0x97, 0x7f, 0x00, 0x00, 0x00]);
    }

    #[test]
    fn a_data_value_reads_its_value_or_its_fault_and_its_time() {
        let read = |bytes: &[u8]| {
            let data_value = &ffi::types()[DATA_VALUE];
            let (value, _) = Value::decode(data_value, bytes)
                .map_err(Status::name)
                .unwrap();
            read_of(&value)
        };
        let mut double = vec![0x01, 0x0b];
        double.extend(1.5_f64.to_le_bytes());
        let string = [0x01, 0x0c, 2, 0, 0, 0, b'h', b'i'];
        let doubles = [0x01, 0x8b, 0, 0, 0, 0];
        let bad = [0x02, 0, 0, 0x34, 0x80];
        let mut time = vec![0x01, 0x0d];
        time.extend((ffi::UNIX_EPOCH + 1).to_le_bytes());
        assert_eq!(
            [&double[..], &string, &doubles, &bad, &[0], &time].map(read),
            [
                "F64(1.5), Err(BadTypeMismatch)",
                "String, Err(BadTypeMismatch)",
                "array of Double, Err(BadTypeMismatch)",
                "BadNodeIdUnknown, Err(BadNodeIdUnknown)",
                "BadNoData, Err(BadNoData)",
                "DateTime, Ok(Stamp(100))"
            ]
        );
        let mut data = vec![u8::try_from(DATA_VALUE).unwrap(), 0];
        data.extend(double);
        decode(&data);
        let scalars = [1, 1, 1, 2, 2, 4, 4, 8, 8].into_iter().zip(1..);
        let ones = scalars.map(|(width, kind)| {
            let mut bytes = vec![0x01, kind, 1];
            bytes.resize(2 + width, 0);
            read(&bytes)
        });
        assert_eq!(
            ones.collect::<Vec<_>>(),
            [
                "Bool(true), Err(BadTypeMismatch)",
                "I8(1), Err(BadTypeMismatch)",
                "U8(1), Err(BadTypeMismatch)",
                "I16(1), Err(BadTypeMismatch)",
                "U16(1), Err(BadTypeMismatch)",
                "I32(1), Err(BadTypeMismatch)",
                "U32(1), Err(BadTypeMismatch)",
                "I64(1), Err(BadTypeMismatch)",
                "U64(1), Err(BadTypeMismatch)"
            ]
        );
        let mut float = vec![0x01, 0x0a];
        float.extend(1.5_f32.to_le_bytes());
        let mut range = vec![0x01, 0x16, 0x01, 0, 0x76, 0x03, 0x01, 16, 0, 0, 0];
        range.extend([0; 16]);
        assert_eq!(
            [&float[..], &range, &[0x01, 0]].map(read),
            [
                "F32(1.5), Err(BadTypeMismatch)",
                "Range, Err(BadTypeMismatch)",
                "BadNoData, Err(BadNoData)"
            ]
        );
    }

    #[test]
    fn two_bytes_pick_each_type_by_the_remainder() {
        let name = |data: &[u8]| pick(data).map(|(t, input)| (t.name(), input.len()));
        assert_eq!(name(&[]), None);
        assert_eq!(name(&[23]), None);
        assert_eq!(name(&[23, 0]), Some(("Variant", 0)));
        assert_eq!(
            name(&[0x83, 1, 9]),
            Some(("PubSubConfiguration2DataType", 1))
        );
        assert_eq!(name(&[0x84, 1]), Some(("Boolean", 0)));
        assert_eq!(name(&[0x9b, 1]), Some(("Variant", 0)));
        assert_eq!(name(&[0xae, 0xfe]), Some(("ByteString", 0)));
    }
}
