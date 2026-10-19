use types::sample::{self, Scalar};
use types::time::Stamp;

use super::{Cause, Fault, SCALARS, Type, Value, time};
use crate::ffi::{self, Status, UNIX_EPOCH};
use crate::fuzz;

/// Decodes `bytes`, the binary encoding of a `DataValue`.
fn decoded(bytes: &[u8]) -> fuzz::Value {
    let data_type = &ffi::types()[ffi::DATA_VALUE];
    let (value, read) = fuzz::Value::decode(data_type, bytes).expect("a DataValue");
    assert_eq!(read, bytes.len());
    value
}

#[test]
fn an_answer_with_a_good_status_and_no_value_fails_with_no_data() {
    assert_eq!(ffi::types()[ffi::DATA_VALUE].name(), "DataValue");
    // The mask 0x00 encodes no field, and 0x02 encodes only the status, GoodClamped.
    let answers = [decoded(&[0x00]), decoded(&[0x02, 0x00, 0x00, 0x30, 0x00])];
    // SAFETY: each answer is a decoded `UA_DataValue` that lives to the end.
    let reads = answers
        .each_ref()
        .map(|answer| unsafe { Value::of(answer.pointer().cast()) });
    assert_eq!(reads, [Err(Fault::status(Status::BAD_NO_DATA)); 2]);
}

/// The text of the fault of the answer `bytes`, an encoded `DataValue`.
fn fault(bytes: &[u8]) -> String {
    let answer = decoded(bytes);
    // SAFETY: the answer is a decoded `UA_DataValue` that lives to the end.
    match unsafe { Value::of(answer.pointer().cast()) } {
        Err(fault) => match fault.cause() {
            Cause::Type(opc_type) => opc_type.to_string(),
            Cause::Status(status) => panic!("not a value of another type: {status:?}"),
        },
        Ok(value) => panic!("not a value of another type: {value:?}"),
    }
}

#[test]
fn an_answer_of_another_type_gives_its_opc_ua_type() {
    // The mask 0x01 encodes only the value. A Variant starts with its type id (1 for
    // Boolean), plus 0x80 for an array, which a count of Int32 follows.
    assert_eq!(fault(&[0x01, 0x0C, 0x02, 0, 0, 0, b'a', b'b']), "String");
    assert_eq!(
        fault(&[0x01, 0x8B, 0x01, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
        "array of Double"
    );
    assert_eq!(fault(&[0x01, 0x8C, 0x00, 0, 0, 0]), "array of String");
    assert_eq!(fault(&[&[0x01, 0x0E][..], &[0; 16]].concat()), "Guid");
}

#[test]
fn a_type_equals_only_the_same_type_of_the_same_shape() {
    let [string, strings, double] =
        ["String", "array of String", "Double"].map(Type::named);
    assert_eq!(string, Type::named("String"));
    assert_ne!(string, strings);
    assert_ne!(string, double);
    assert_eq!(format!("{strings:?}"), "array of String");
}

#[test]
fn an_answer_of_a_decoded_structure_gives_the_name_of_the_structure() {
    // An ExtensionObject (22) of the binary encoding of `Range` (ns 0, id 886), with a
    // body of two Doubles.
    let range = [0x01, 0x16, 0x01, 0x00, 0x76, 0x03, 0x01, 16, 0, 0, 0];
    assert_eq!(fault(&[&range[..], &[0; 16]].concat()), "Range");
}

#[test]
fn an_answer_with_an_empty_value_fails_with_no_data() {
    let answer = decoded(&[0x01, 0x00]);
    // SAFETY: the answer is a decoded `UA_DataValue` that lives to the end.
    let read = unsafe { Value::of(answer.pointer().cast()) };
    assert_eq!(read, Err(Fault::status(Status::BAD_NO_DATA)));
}

#[test]
fn an_answer_with_the_status_bad_type_mismatch_gives_that_status() {
    // The mask 0x03 encodes a Double 0, then the status 0x80740000.
    let mut bytes = vec![0x03, 0x0B];
    bytes.extend([0; 8]);
    bytes.extend(0x8074_0000_u32.to_le_bytes());
    let answer = decoded(&bytes);
    // SAFETY: the answer is a decoded `UA_DataValue` that lives to the end.
    let read = unsafe { Value::of(answer.pointer().cast()) };
    assert_eq!(read, Err(Fault::status(Status::BAD_TYPE_MISMATCH)));
}

#[test]
fn each_value_gives_the_opc_ua_name_of_its_type() {
    let values = [
        Value::Bool(false),
        Value::I8(0),
        Value::U8(0),
        Value::I16(0),
        Value::U16(0),
        Value::I32(0),
        Value::U32(0),
        Value::I64(0),
        Value::U64(0),
        Value::F32(0.0),
        Value::F64(0.0),
    ];
    let names = values.map(|value| value.type_of().to_string());
    let expected = [
        "Boolean", "SByte", "Byte", "Int16", "UInt16", "Int32", "UInt32", "Int64",
        "UInt64", "Float", "Double",
    ];
    assert_eq!(names, expected);
}

/// A value of each type, in the order of [`SCALARS`].
const VALUES: [Value; 11] = [
    Value::Bool(true),
    Value::I8(-1),
    Value::U8(1),
    Value::I16(-1),
    Value::U16(1),
    Value::I32(-1),
    Value::U32(1),
    Value::I64(-1),
    Value::U64(1),
    Value::F32(1.0),
    Value::F64(1.0),
];

#[test]
fn the_scalars_are_the_sample_types_of_the_values_in_the_order_of_their_kinds() {
    assert_eq!(VALUES.map(Value::scalar), SCALARS);
    let kinds: Vec<u16> = VALUES.iter().map(|value| value.raw().1).collect();
    assert_eq!(kinds, (ffi::BOOLEAN..=ffi::DOUBLE).collect::<Vec<_>>());
}

#[test]
fn maps_each_value_to_the_sample_value_of_its_type() {
    let expected = [
        sample::Value::Bool(true),
        sample::Value::I8(-1),
        sample::Value::U8(1),
        sample::Value::I16(-1),
        sample::Value::U16(1),
        sample::Value::I32(-1),
        sample::Value::U32(1),
        sample::Value::I64(-1),
        sample::Value::U64(1),
        sample::Value::F32(1.0),
        sample::Value::F64(1.0),
    ];
    assert_eq!(VALUES.map(sample::Value::from), expected);
}

#[cfg(feature = "sim")]
#[test]
fn one_is_1_of_each_scalar_and_panics_for_another_type() {
    let ones = SCALARS.map(Value::one);
    let expected = [
        Value::Bool(true),
        Value::I8(1),
        Value::U8(1),
        Value::I16(1),
        Value::U16(1),
        Value::I32(1),
        Value::U32(1),
        Value::I64(1),
        Value::U64(1),
        Value::F32(1.0),
        Value::F64(1.0),
    ];
    assert_eq!(ones, expected);
    let panic = std::panic::catch_unwind(|| Value::one(Scalar::Stamp));
    let payload = panic.expect_err("a stamp is not one of `SCALARS`");
    assert_eq!(
        payload.downcast_ref::<String>().map(String::as_str),
        Some("invariant: Stamp is not one of `SCALARS`")
    );
}

#[test]
fn a_fault_gives_the_status_or_the_type_that_made_it() {
    let last = Type {
        at: u16::try_from(ffi::types().len() - 1).expect("a u16"),
        array: true,
    };
    let string = Type::named("String");
    let statuses = [
        Status(0x4000_0000),
        Status::BAD_NO_DATA,
        Status(0xFFFF_FFFF),
    ];
    let causes: Vec<_> = statuses
        .iter()
        .map(|&status| Fault::status(status).cause())
        .chain([last, string].map(|of| Fault::of_type(of).cause()))
        .collect();
    let mut expected: Vec<_> = statuses.map(Cause::Status).to_vec();
    expected.extend([Cause::Type(last), Cause::Type(string)]);
    assert_eq!(causes, expected);
    assert_eq!(size_of::<Fault>(), 4);
}

#[test]
#[should_panic(expected = "invariant: the status of a fault is not Good")]
fn a_fault_of_a_good_status_panics() {
    let _: Fault = Fault::status(Status(0x00A0_0000));
}

/// The time of an answer of a `DateTime` of `ticks`.
fn time_of(ticks: i64) -> Result<Stamp, Status> {
    // The mask 0x01 encodes only the value, a `DateTime` (13).
    let answer = decoded(&[&[0x01, 0x0D][..], &ticks.to_le_bytes()].concat());
    // SAFETY: the answer is a decoded `UA_DataValue` that lives to the end.
    unsafe { time(answer.pointer().cast()) }
}

#[test]
fn an_answer_of_a_date_time_gives_its_time_from_the_unix_epoch() {
    let last = i64::MAX / 100;
    let first = i64::MIN / 100;
    let times = [
        UNIX_EPOCH,
        UNIX_EPOCH + 1,
        UNIX_EPOCH - 1,
        UNIX_EPOCH + last,
        UNIX_EPOCH + first,
    ]
    .map(time_of);
    let expected = [0, 100, -100, last * 100, first * 100].map(Stamp::from_nanos);
    assert_eq!(times, expected.map(Ok));
}

/// A `Stamp` holds 1677-09-21 to 2262-04-11, so the null `DateTime` 0 is out of it.
#[test]
fn an_answer_of_a_date_time_past_a_stamp_fails_with_out_of_range() {
    let times = [
        0,
        UNIX_EPOCH + i64::MAX / 100 + 1,
        UNIX_EPOCH + i64::MIN / 100 - 1,
        i64::MAX,
        i64::MIN,
    ]
    .map(time_of);
    assert_eq!(times, [Err(Status::BAD_OUT_OF_RANGE); 5]);
}

#[test]
fn an_answer_that_is_not_a_good_date_time_fails_with_its_status() {
    // A Double 0; an array of one `DateTime`; a `DateTime` with the status
    // BadNotReadable; an empty Variant; no value.
    let mut double = vec![0x01, 0x0B];
    double.extend([0; 8]);
    let mut array = vec![0x01, 0x8D, 0x01, 0, 0, 0];
    array.extend(UNIX_EPOCH.to_le_bytes());
    let mut bad = vec![0x03, 0x0D];
    bad.extend(UNIX_EPOCH.to_le_bytes());
    bad.extend(0x803A_0000_u32.to_le_bytes());
    let answers =
        [double, array, bad, vec![0x01, 0x00], vec![0x00]].map(|bytes| decoded(&bytes));
    // SAFETY: each answer is a decoded `UA_DataValue` that lives to the end.
    let times = answers
        .each_ref()
        .map(|answer| unsafe { time(answer.pointer().cast()) });
    let expected = [
        Status::BAD_TYPE_MISMATCH,
        Status::BAD_TYPE_MISMATCH,
        Status(0x803A_0000),
        Status::BAD_NO_DATA,
        Status::BAD_NO_DATA,
    ];
    assert_eq!(times, expected.map(Err));
}
