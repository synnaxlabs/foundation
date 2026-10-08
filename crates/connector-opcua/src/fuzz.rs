//! The round trip of open62541 binary decoding, for the `connector_opcua_decode` fuzz
//! target.

#![expect(unsafe_code, reason = "open62541 is a C library")]

use std::ffi::c_void;

use crate::ffi::{self, BYTE_STRING, Bytes, DataType, DecodeOptions, Status, TYPES};

/// Decodes `data[2..]` as the type of `UA_TYPES` that the little-endian `u16` of
/// `data[..2]` picks, by its remainder by the table length. When the decode gives
/// `Good`, it encodes the value, decodes those bytes, and encodes again. An input of
/// fewer than 2 bytes decodes nothing.
///
/// # Panics
///
/// When a check of the round trip fails, with the type name and the step.
pub fn decode(data: &[u8]) {
    let Some((data_type, input)) = pick(data) else {
        return;
    };
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
        .unwrap_or_else(|e| panic!("{name}: encode gave {}", e.name()));
    assert_eq!(
        value.size(),
        once.len(),
        "{name}: the size is not the encoded size"
    );
    // A `Variant` of `ExtensionObject` values decodes only when 4 bytes follow its
    // length for each value, and the header of the first value fits at each value
    // (#435). So an encoding that does not decode gets zeros after it.
    let (again, read) = Value::decode(data_type, &once)
        .or_else(|_| {
            let mut padded = once.clone();
            padded.resize(once.len() * 2, 0);
            Value::decode(data_type, &padded)
        })
        .unwrap_or_else(|e| {
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
}

/// Splits `data` into the type that its first two bytes pick and the rest.
fn pick(data: &[u8]) -> Option<(&'static DataType, &[u8])> {
    let (&[low, high], input) = data.split_first_chunk()?;
    let at = usize::from(u16::from_le_bytes([low, high])) % TYPES;
    Some((&ffi::types()[at], input))
}

/// A decoded value, cleared when dropped.
struct Value {
    data_type: &'static DataType,
    memory: Vec<u64>,
}

impl Value {
    /// Decodes a value of `data_type` from the start of `bytes`, and gives it with the
    /// number of bytes read.
    fn decode(
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

    fn pointer(&self) -> *const c_void {
        self.memory.as_ptr().cast()
    }

    fn encode(&self) -> Result<Vec<u8>, Status> {
        let mut output = Bytes {
            length: 0,
            data: std::ptr::null_mut(),
        };
        // SAFETY: the value is a decoded value of its type, and an empty output makes
        // the encoder allocate.
        let code = unsafe {
            ffi::UA_encodeBinary(
                self.pointer(),
                self.data_type,
                &raw mut output,
                std::ptr::null_mut(),
            )
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
            [0, BYTE_STRING, TYPES - 1].map(|at| types[at].name()),
            ["Boolean", "ByteString", "PubSubConfiguration2DataType"]
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
