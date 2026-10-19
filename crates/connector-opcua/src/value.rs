//! The scalar value of an OPC UA variable.

#![expect(unsafe_code, reason = "open62541 is a C library")]

use std::fmt;

use types::sample::{self, Scalar};
use types::time::Stamp;

use crate::ffi::{self, Status};

/// The scalar value of an OPC UA variable, in the sample type of the same width and
/// sign: Boolean as `bool`, `SByte` to `UInt64` as `i8` to `u64`, Float as `f32`, and
/// Double as `f64`.
// In the order of `sample::Value`, so that a map to it keeps the tag.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum Value {
    Bool(bool),
    I8(i8),
    I16(i16),
    I32(i32),
    I64(i64),
    U8(u8),
    U16(u16),
    U32(u32),
    U64(u64),
    F32(f32),
    F64(f64),
}

/// The sample types of a [`Value`], at the index of each OPC UA type in `UA_TYPES`,
/// which is its kind. Each comes from the decode of its kind, so a decode that gives
/// another kind fails the build.
pub(crate) const SCALARS: [Scalar; 11] = {
    let mut scalars = [Scalar::Bool; 11];
    let mut at = 0;
    let mut kind = 0;
    while at < scalars.len() {
        // SAFETY: each field of a zero scalar is zero, a value of its type.
        match unsafe { Value::from_raw(&ffi::Scalar { uint64: 0 }, kind) } {
            Some(value) => {
                assert!(value.raw().1 == kind, "a decode keeps the kind");
                scalars[at] = value.sample().scalar();
            }
            None => panic!("a decode reads each kind from Boolean to Double"),
        }
        at += 1;
        kind += 1;
    }
    scalars
};

impl Value {
    /// Gives the scalar of `value`, an answer of a read. Else gives its status when
    /// that is not Good (a Good with a subcode is Good), `BadNoData` when it holds no
    /// value, or the type of an array or of a scalar that is not one of [`Value`].
    ///
    /// # Safety
    ///
    /// `value` is a live `UA_DataValue`.
    pub(crate) unsafe fn of(value: *const ffi::DataValue) -> Result<Self, Fault> {
        let mut scalar = ffi::Scalar { uint64: 0 };
        let mut kind = 0;
        // SAFETY: `value` is live, as the caller ensures, and the outputs are places
        // to write.
        let status = Status(unsafe {
            ffi::shim_value_scalar(value, &raw mut scalar, &raw mut kind)
        });
        if status != Status::GOOD {
            return Err(Fault::status(status));
        }
        // SAFETY: `shim_value_scalar` wrote the field of `kind` for each kind of
        // [`Value`].
        match unsafe { Self::from_raw(&scalar, kind) } {
            Some(value) => Ok(value),
            // SAFETY: `shim_value_scalar` wrote the type for each other kind.
            None => Err(mistyped(kind, unsafe { scalar.data_type })),
        }
    }

    /// The value that `scalar` holds, of the kind `kind`, or `None` when `kind` is not
    /// one of [`Value`]. It reverses [`Value::raw`].
    ///
    /// # Safety
    ///
    /// When `kind` is one of [`Value`], `scalar` holds its field.
    #[expect(
        clippy::trivially_copy_pass_by_ref,
        reason = "by value, `client::answer` rebuilds the fields with shifts"
    )]
    #[inline]
    const unsafe fn from_raw(scalar: &ffi::Scalar, kind: u16) -> Option<Self> {
        Some(match kind {
            // SAFETY: `scalar` holds the field of `kind`, as the caller ensures.
            ffi::BOOLEAN => Self::Bool(unsafe { scalar.boolean }),
            // SAFETY: `scalar` holds the field of `kind`, as the caller ensures.
            ffi::SBYTE => Self::I8(unsafe { scalar.sbyte }),
            // SAFETY: `scalar` holds the field of `kind`, as the caller ensures.
            ffi::BYTE => Self::U8(unsafe { scalar.byte }),
            // SAFETY: `scalar` holds the field of `kind`, as the caller ensures.
            ffi::INT16 => Self::I16(unsafe { scalar.int16 }),
            // SAFETY: `scalar` holds the field of `kind`, as the caller ensures.
            ffi::UINT16 => Self::U16(unsafe { scalar.uint16 }),
            // SAFETY: `scalar` holds the field of `kind`, as the caller ensures.
            ffi::INT32 => Self::I32(unsafe { scalar.int32 }),
            // SAFETY: `scalar` holds the field of `kind`, as the caller ensures.
            ffi::UINT32 => Self::U32(unsafe { scalar.uint32 }),
            // SAFETY: `scalar` holds the field of `kind`, as the caller ensures.
            ffi::INT64 => Self::I64(unsafe { scalar.int64 }),
            // SAFETY: `scalar` holds the field of `kind`, as the caller ensures.
            ffi::UINT64 => Self::U64(unsafe { scalar.uint64 }),
            // SAFETY: `scalar` holds the field of `kind`, as the caller ensures.
            ffi::FLOAT => Self::F32(unsafe { scalar.float }),
            // SAFETY: `scalar` holds the field of `kind`, as the caller ensures.
            ffi::DOUBLE => Self::F64(unsafe { scalar.double }),
            _ => return None,
        })
    }

    /// The sample type of the value.
    #[inline]
    pub(crate) fn scalar(self) -> Scalar {
        self.sample().scalar()
    }

    /// The sample value of the same type.
    #[inline]
    const fn sample(self) -> sample::Value {
        match self {
            Self::Bool(value) => sample::Value::Bool(value),
            Self::I8(value) => sample::Value::I8(value),
            Self::U8(value) => sample::Value::U8(value),
            Self::I16(value) => sample::Value::I16(value),
            Self::U16(value) => sample::Value::U16(value),
            Self::I32(value) => sample::Value::I32(value),
            Self::U32(value) => sample::Value::U32(value),
            Self::I64(value) => sample::Value::I64(value),
            Self::U64(value) => sample::Value::U64(value),
            Self::F32(value) => sample::Value::F32(value),
            Self::F64(value) => sample::Value::F64(value),
        }
    }

    /// The OPC UA type of the value.
    pub(crate) fn type_of(self) -> Type {
        // The kind of a scalar of `Value` is its index in `UA_TYPES`, as `shim.c`
        // asserts.
        Type {
            at: self.raw().1,
            array: false,
        }
    }

    /// The value 1 of `scalar`, which is `true` for `bool`.
    ///
    /// # Panics
    ///
    /// If `scalar` is not one of [`SCALARS`].
    #[cfg(feature = "sim")]
    pub(crate) fn one(scalar: Scalar) -> Self {
        match scalar {
            Scalar::Bool => Self::Bool(true),
            Scalar::I8 => Self::I8(1),
            Scalar::I16 => Self::I16(1),
            Scalar::I32 => Self::I32(1),
            Scalar::I64 => Self::I64(1),
            Scalar::U8 => Self::U8(1),
            Scalar::U16 => Self::U16(1),
            Scalar::U32 => Self::U32(1),
            Scalar::U64 => Self::U64(1),
            Scalar::F32 => Self::F32(1.0),
            Scalar::F64 => Self::F64(1.0),
            Scalar::Stamp | Scalar::Span | Scalar::Uuid => {
                panic!("invariant: {scalar:?} is not one of `SCALARS`")
            }
        }
    }

    /// Gives the C scalar of the value and its kind.
    pub(crate) const fn raw(self) -> (ffi::Scalar, u16) {
        match self {
            Self::Bool(boolean) => (ffi::Scalar { boolean }, ffi::BOOLEAN),
            Self::I8(sbyte) => (ffi::Scalar { sbyte }, ffi::SBYTE),
            Self::U8(byte) => (ffi::Scalar { byte }, ffi::BYTE),
            Self::I16(int16) => (ffi::Scalar { int16 }, ffi::INT16),
            Self::U16(uint16) => (ffi::Scalar { uint16 }, ffi::UINT16),
            Self::I32(int32) => (ffi::Scalar { int32 }, ffi::INT32),
            Self::U32(uint32) => (ffi::Scalar { uint32 }, ffi::UINT32),
            Self::I64(int64) => (ffi::Scalar { int64 }, ffi::INT64),
            Self::U64(uint64) => (ffi::Scalar { uint64 }, ffi::UINT64),
            Self::F32(float) => (ffi::Scalar { float }, ffi::FLOAT),
            Self::F64(double) => (ffi::Scalar { double }, ffi::DOUBLE),
        }
    }
}

impl From<Value> for sample::Value {
    #[inline]
    fn from(value: Value) -> Self {
        value.sample()
    }
}

/// Gives the time of `value`, an answer of a read that holds a scalar `DateTime`, in
/// ns from the Unix epoch.
///
/// # Errors
///
/// The status of the answer when it is not Good, `BadNoData` when it holds no value or
/// an empty Variant, `BadTypeMismatch` when it holds no scalar `DateTime`, and
/// `BadOutOfRange` when a [`Stamp`] cannot hold its time, as for the null `DateTime`
/// 0.
///
/// # Safety
///
/// `value` is a live `UA_DataValue`.
pub(crate) unsafe fn time(value: *const ffi::DataValue) -> Result<Stamp, Status> {
    let mut ticks = 0;
    // SAFETY: `value` is live, as the caller ensures, and `ticks` is a place to write.
    let status = Status(unsafe { ffi::shim_value_time(value, &raw mut ticks) });
    if status != Status::GOOD {
        return Err(status);
    }
    stamp(ticks).ok_or(Status::BAD_OUT_OF_RANGE)
}

/// The stamp of a `DateTime` of `ticks`, or `None` when a [`Stamp`] cannot hold it.
fn stamp(ticks: i64) -> Option<Stamp> {
    (ticks.checked_sub(ffi::UNIX_EPOCH))
        .and_then(|since| since.checked_mul(100))
        .map(Stamp::from_nanos)
}

/// The fault of a Good answer of the open62541 kind `kind` and the type `data_type`,
/// apart from the Ok path of [`Value::of`].
///
/// # Panics
///
/// When `data_type` is not of `UA_TYPES`. The client registers no type of its own.
#[cold]
#[inline(never)]
fn mistyped(kind: u16, data_type: *const ffi::DataType) -> Fault {
    let types = ffi::types();
    let offset = data_type.addr().wrapping_sub(types.as_ptr().addr());
    let at = offset / size_of::<ffi::DataType>();
    assert!(
        types.get(at).is_some_and(|of| std::ptr::eq(of, data_type)),
        "invariant: each type of a decoded value is of `UA_TYPES`"
    );
    Fault::of_type(Type {
        at: u16::try_from(at).expect("invariant: `UA_TYPES` fits a u16"),
        array: kind & ffi::ARRAY != 0,
    })
}

/// Why an answer of a read gives no [`Value`], in 4 bytes: [`Fault::cause`] gives it.
// A status that is not Good, or a type with the severity bits of Good: the index in
// bits 0 to 15, and the array flag in bit 16. An enum puts the fault at offset 4, over
// the payload of a `Value`, and `client::answer` then stalls on each read.
#[derive(Clone, Copy, PartialEq)]
pub(crate) struct Fault(u32);

/// What a [`Fault`] holds.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum Cause {
    /// The status of the answer, which is not Good, or `BadNoData` when it holds no
    /// value.
    Status(Status),
    /// The type of a Good answer that is an array, or a scalar of another type than
    /// those of [`Value`].
    Type(Type),
}

impl Fault {
    /// The fault of `status`.
    ///
    /// # Panics
    ///
    /// If `status` is Good.
    pub(crate) const fn status(status: Status) -> Self {
        assert!(
            status.0 >> 30 != 0,
            "invariant: the status of a fault is not Good"
        );
        Self(status.0)
    }

    /// The fault of a Good answer of the type `of`.
    pub(crate) fn of_type(of: Type) -> Self {
        Self(u32::from(of.at) | u32::from(of.array) << 16)
    }

    /// What the fault holds.
    pub(crate) fn cause(self) -> Cause {
        if self.0 >> 30 == 0 {
            let at = u16::try_from(self.0 & 0xFFFF).expect("16 bits");
            let array = self.0 >> 16 & 1 != 0;
            Cause::Type(Type { at, array })
        } else {
            Cause::Status(Status(self.0))
        }
    }
}

impl fmt::Debug for Fault {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&self.cause(), f)
    }
}

/// The OPC UA type of a value: a type of `UA_TYPES`, or an array of one. Its text is
/// the `typeName` of open62541, such as `String`, `Range`, or `array of Double`.
#[derive(Clone, Copy, PartialEq)]
pub(crate) struct Type {
    /// The index of the type in `UA_TYPES`.
    at: u16,
    array: bool,
}

// An index, not a reference, keeps each read of a tick at 16 bytes.
const _: () = assert!(
    size_of::<Result<Value, Fault>>() == 16,
    "a read of a tick grew"
);

impl Type {
    /// The type of the text `name`, such as `array of Double`, from `UA_TYPES`.
    ///
    /// # Panics
    ///
    /// If no type of `UA_TYPES` has the name.
    #[cfg(test)]
    pub(crate) fn named(name: &str) -> Self {
        let (array, scalar) = match name.strip_prefix("array of ") {
            Some(scalar) => (true, scalar),
            None => (false, name),
        };
        let mut types = ffi::types().iter();
        let at = types.position(|data_type| data_type.name() == scalar);
        let at = at.unwrap_or_else(|| panic!("no type {name:?}"));
        Self {
            at: u16::try_from(at).expect("a u16"),
            array,
        }
    }
}

impl fmt::Display for Type {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.array {
            f.write_str("array of ")?;
        }
        f.write_str(ffi::types()[usize::from(self.at)].name())
    }
}

impl fmt::Debug for Type {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

#[cfg(test)]
mod tests;
