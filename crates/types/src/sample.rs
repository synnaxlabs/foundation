//! The byte layout of samples.

/// A fixed-width sample type.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Scalar {
    /// One byte: 0 or 1.
    Bool,
    /// Signed 8-bit integer.
    I8,
    /// Signed 16-bit integer.
    I16,
    /// Signed 32-bit integer.
    I32,
    /// Signed 64-bit integer.
    I64,
    /// Unsigned 8-bit integer.
    U8,
    /// Unsigned 16-bit integer.
    U16,
    /// Unsigned 32-bit integer.
    U32,
    /// Unsigned 64-bit integer.
    U64,
    /// 32-bit float.
    F32,
    /// 64-bit float.
    F64,
    /// A [`crate::time::Stamp`].
    Stamp,
    /// A [`crate::time::Span`].
    Span,
    /// A 128-bit UUID.
    Uuid,
}

impl Scalar {
    /// Bytes per sample.
    #[must_use]
    pub const fn width(self) -> usize {
        match self {
            Self::Bool | Self::I8 | Self::U8 => 1,
            Self::I16 | Self::U16 => 2,
            Self::I32 | Self::U32 | Self::F32 => 4,
            Self::I64 | Self::U64 | Self::F64 | Self::Stamp | Self::Span => 8,
            Self::Uuid => 16,
        }
    }
}

/// The byte layout of one channel's samples.
///
/// Enums and flags use an integer layout, and quality uses `U32`; their meaning is in
/// `spec`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Type {
    /// One fixed-width value per sample.
    Scalar(Scalar),
    /// A fixed array per sample, row-major for more than one dimension.
    Array {
        /// The element type.
        element: Scalar,
        /// Elements per sample.
        len: u32,
    },
    /// A list of at most `max` elements per sample.
    List {
        /// The element type.
        element: Scalar,
        /// The most elements one sample may hold.
        max: u32,
    },
    /// UTF-8 text per sample.
    String,
    /// Unlabeled bytes per sample.
    Bytes,
}

impl Type {
    /// Bytes per sample, or `None` when samples vary in size.
    #[must_use]
    pub const fn width(self) -> Option<usize> {
        match self {
            Self::Scalar(s) => Some(s.width()),
            #[allow(
                clippy::cast_possible_truncation,
                reason = "array lengths fit in usize"
            )]
            Self::Array { element, len } => Some(element.width() * len as usize),
            Self::List { .. } | Self::String | Self::Bytes => None,
        }
    }
}
