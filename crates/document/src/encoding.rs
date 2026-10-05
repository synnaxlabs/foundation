//! The canonical bytes of a document. Each document has exactly one encoding, with no
//! spans, and `decode` refuses every byte string that `encode` cannot write.
//!
//! Format, little-endian:
//!
//! ```text
//! document := version:u8 body
//! body     := map count block*
//! block    := keyword:string count label:string* body
//! map      := count (key:string value)*        keys strictly ascend, by bytes
//! value    := 0 false | 1 true | 2 integer:zigzag varint | 3 float:8 bytes
//!           | 4 string | 5 reference:string | 6 list:count value*
//!           | 7 map:map | 8 call:function:string count value*
//! string   := length:varint UTF-8 bytes
//! ```
//!
//! A varint is unsigned LEB128 in its shortest form. A float is finite and never
//! -0.0.

use std::fmt;
use std::str;

use types::name;

use crate::value::{Call, Float, Kind, Value};
use crate::{Attribute, Block, Document, Label, Map};

const VERSION: u8 = 1;

/// The deepest nesting that [`encode`] writes and [`decode`] reads. Each block body
/// and each list, map, and call value is one level.
pub const DEPTH_MAX: usize = 64;

const FALSE: u8 = 0;
const TRUE: u8 = 1;
const INTEGER: u8 = 2;
const FLOAT: u8 = 3;
const STRING: u8 = 4;
const REFERENCE: u8 = 5;
const LIST: u8 = 6;
const MAP: u8 = 7;
const CALL: u8 = 8;

/// Writes the canonical bytes of `document`. Spans are not written.
///
/// # Errors
///
/// Returns [`Error::TooDeep`] when `document` nests deeper than [`DEPTH_MAX`].
pub fn encode(document: &Document) -> Result<Vec<u8>, Error> {
    let mut writer = Writer { out: vec![VERSION] };
    writer.body(document, 0)?;
    Ok(writer.out)
}

/// Reads a document from its canonical bytes. The document has no spans.
///
/// # Errors
///
/// Returns an [`Error`] when `bytes` are not the encoding of a document.
pub fn decode(bytes: &[u8]) -> Result<Document, Error> {
    let mut reader = Reader {
        rest: bytes,
        len: bytes.len(),
    };
    let version = reader.byte()?;
    if version != VERSION {
        return Err(Error::Version { found: version });
    }
    let document = reader.body(0)?;
    if !reader.rest.is_empty() {
        return Err(Error::TrailingBytes { at: reader.at() });
    }
    Ok(document)
}

/// Bytes that are not the encoding of a document. `at` is a byte offset into the
/// bytes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Error {
    /// The bytes have a format version that this build cannot read.
    Version {
        /// The version in the bytes.
        found: u8,
    },
    /// The bytes end before the document does, or a count or length is larger than
    /// the bytes left.
    Truncated {
        /// Where more bytes were needed.
        at: usize,
    },
    /// Bytes follow the end of the document.
    TrailingBytes {
        /// Where the extra bytes start.
        at: usize,
    },
    /// A value starts with a byte that is not a value tag.
    Tag {
        /// Where the tag is.
        at: usize,
        /// The tag.
        tag: u8,
    },
    /// A varint is longer than its shortest form, or too large.
    Varint {
        /// Where the varint starts.
        at: usize,
    },
    /// A string is not UTF-8.
    Utf8 {
        /// The first byte that is not UTF-8.
        at: usize,
    },
    /// A map key is not larger than the key before it.
    KeyOrder {
        /// Where the key starts.
        at: usize,
        /// The key.
        key: Box<str>,
    },
    /// A float is NaN, an infinity, or -0.0.
    Float {
        /// Where the float's bytes start.
        at: usize,
        /// The float's bits.
        bits: u64,
    },
    /// A reference is not a valid name.
    Reference {
        /// Where the reference's string starts.
        at: usize,
        /// Why the name is not valid.
        error: name::Error,
    },
    /// The document nests deeper than [`DEPTH_MAX`].
    TooDeep {
        /// Where the level that is too deep starts.
        at: usize,
    },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Version { found } => write!(
                f,
                "the document has format version {found}, and this node reads version \
                 {VERSION}. Upgrade the node"
            ),
            Self::Truncated { at } => {
                write!(f, "the document bytes end early at byte {at}. They are cut")
            }
            Self::TrailingBytes { at } => write!(
                f,
                "the document ends at byte {at}, but more bytes follow. They are \
                 corrupt"
            ),
            Self::Tag { at, tag } => {
                write!(
                    f,
                    "byte {at} holds {tag}, which is not a value tag. It is corrupt"
                )
            }
            Self::Varint { at } => write!(
                f,
                "the number at byte {at} is not in its shortest form, or is too large. \
                 It is corrupt"
            ),
            Self::Utf8 { at } => {
                write!(f, "the text at byte {at} is not UTF-8. It is corrupt")
            }
            Self::KeyOrder { at, key } => write!(
                f,
                "the key {key:?} at byte {at} is not after the key before it. Keys \
                 must strictly ascend"
            ),
            Self::Float { at, bits } => write!(
                f,
                "the float at byte {at} (bits {bits:#018x}) is NaN, an infinity, or \
                 -0.0. It is corrupt"
            ),
            Self::Reference { at, error } => {
                write!(f, "the reference at byte {at}: {error}")
            }
            Self::TooDeep { at } => write!(
                f,
                "the document nests deeper than {DEPTH_MAX} levels at byte {at}. Make \
                 it flatter"
            ),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Reference { error, .. } => Some(error),
            _ => None,
        }
    }
}

/// The depth inside one more level, or [`Error::TooDeep`] at `at`.
fn enter(depth: usize, at: usize) -> Result<usize, Error> {
    depth
        .checked_add(1)
        .filter(|&inner| inner <= DEPTH_MAX)
        .ok_or(Error::TooDeep { at })
}

fn zigzag(n: i128) -> u128 {
    (n.wrapping_shl(1) ^ n.wrapping_shr(127)).cast_unsigned()
}

fn unzigzag(n: u128) -> i128 {
    n.wrapping_shr(1).cast_signed() ^ 0i128.wrapping_sub((n & 1).cast_signed())
}

struct Writer {
    out: Vec<u8>,
}

impl Writer {
    fn body(&mut self, document: &Document, depth: usize) -> Result<(), Error> {
        self.map(&document.attributes, depth)?;
        self.count(document.blocks.len());
        for block in &document.blocks {
            let depth = enter(depth, self.out.len())?;
            self.string(&block.keyword);
            self.count(block.labels.len());
            for label in &block.labels {
                self.string(&label.text);
            }
            self.body(&block.body, depth)?;
        }
        Ok(())
    }

    fn map(&mut self, map: &Map, depth: usize) -> Result<(), Error> {
        self.count(map.iter().len());
        for attribute in map.iter() {
            self.string(&attribute.key);
            self.value(&attribute.value, depth)?;
        }
        Ok(())
    }

    fn value(&mut self, value: &Value, depth: usize) -> Result<(), Error> {
        let at = self.out.len();
        match &value.kind {
            Kind::Bool(false) => self.out.push(FALSE),
            Kind::Bool(true) => self.out.push(TRUE),
            Kind::Integer(n) => {
                self.out.push(INTEGER);
                self.varint(zigzag(*n));
            }
            Kind::Float(float) => {
                self.out.push(FLOAT);
                self.out
                    .extend_from_slice(&float.get().to_bits().to_le_bytes());
            }
            Kind::String(text) => {
                self.out.push(STRING);
                self.string(text);
            }
            Kind::Reference(name) => {
                self.out.push(REFERENCE);
                self.string(name.as_str());
            }
            Kind::List(items) => {
                let depth = enter(depth, at)?;
                self.out.push(LIST);
                self.values(items, depth)?;
            }
            Kind::Map(map) => {
                let depth = enter(depth, at)?;
                self.out.push(MAP);
                self.map(map, depth)?;
            }
            Kind::Call(call) => {
                let depth = enter(depth, at)?;
                self.out.push(CALL);
                self.string(&call.function);
                self.values(&call.arguments, depth)?;
            }
        }
        Ok(())
    }

    fn values(&mut self, values: &[Value], depth: usize) -> Result<(), Error> {
        self.count(values.len());
        values.iter().try_for_each(|value| self.value(value, depth))
    }

    fn string(&mut self, text: &str) {
        self.count(text.len());
        self.out.extend_from_slice(text.as_bytes());
    }

    fn count(&mut self, n: usize) {
        self.varint(u128::try_from(n).expect("invariant: a usize fits in a u128"));
    }

    fn varint(&mut self, mut n: u128) {
        while n >= 0x80 {
            self.out.push(low_seven(n) | 0x80);
            n = n.wrapping_shr(7);
        }
        self.out.push(low_seven(n));
    }
}

fn low_seven(n: u128) -> u8 {
    u8::try_from(n & 0x7f).expect("invariant: seven bits fit in a byte")
}

struct Reader<'a> {
    rest: &'a [u8],
    len: usize,
}

impl Reader<'_> {
    fn at(&self) -> usize {
        self.len.saturating_sub(self.rest.len())
    }

    fn body(&mut self, depth: usize) -> Result<Document, Error> {
        let attributes = self.map(depth)?;
        let mut blocks = Vec::new();
        for _ in 0..self.count()? {
            let depth = enter(depth, self.at())?;
            let keyword = self.string()?;
            let mut labels = Vec::new();
            for _ in 0..self.count()? {
                labels.push(Label {
                    text: self.string()?,
                    span: None,
                });
            }
            blocks.push(Block {
                keyword,
                keyword_span: None,
                labels,
                body: self.body(depth)?,
                span: None,
            });
        }
        Ok(Document { attributes, blocks })
    }

    fn map(&mut self, depth: usize) -> Result<Map, Error> {
        let mut attributes: Vec<Attribute> = Vec::new();
        for _ in 0..self.count()? {
            let at = self.at();
            let key = self.string()?;
            if attributes.last().is_some_and(|last| last.key >= key) {
                return Err(Error::KeyOrder { at, key });
            }
            attributes.push(Attribute {
                key,
                key_span: None,
                value: self.value(depth)?,
            });
        }
        Ok(Map::from_sorted(attributes))
    }

    fn value(&mut self, depth: usize) -> Result<Value, Error> {
        let at = self.at();
        let kind = match self.byte()? {
            FALSE => Kind::Bool(false),
            TRUE => Kind::Bool(true),
            INTEGER => Kind::Integer(unzigzag(self.varint()?)),
            FLOAT => Kind::Float(self.float()?),
            STRING => Kind::String(self.string()?),
            REFERENCE => {
                let at = self.at();
                let text = self.string()?;
                Kind::Reference(
                    text.parse()
                        .map_err(|error| Error::Reference { at, error })?,
                )
            }
            LIST => Kind::List(self.values(enter(depth, at)?)?),
            MAP => Kind::Map(self.map(enter(depth, at)?)?),
            CALL => {
                let depth = enter(depth, at)?;
                Kind::Call(Call {
                    function: self.string()?,
                    function_span: None,
                    arguments: self.values(depth)?,
                })
            }
            tag => return Err(Error::Tag { at, tag }),
        };
        Ok(Value { kind, span: None })
    }

    fn values(&mut self, depth: usize) -> Result<Vec<Value>, Error> {
        let mut values = Vec::new();
        for _ in 0..self.count()? {
            values.push(self.value(depth)?);
        }
        Ok(values)
    }

    fn float(&mut self) -> Result<Float, Error> {
        let at = self.at();
        let (bytes, rest) = self
            .rest
            .split_first_chunk::<8>()
            .ok_or(Error::Truncated { at })?;
        self.rest = rest;
        let bits = u64::from_le_bytes(*bytes);
        // `Float::new` turns -0.0 into 0.0, so the bits differ.
        Float::new(f64::from_bits(bits))
            .filter(|float| float.get().to_bits() == bits)
            .ok_or(Error::Float { at, bits })
    }

    fn string(&mut self) -> Result<Box<str>, Error> {
        let len = self.count()?;
        let at = self.at();
        let (bytes, rest) = self
            .rest
            .split_at_checked(len)
            .ok_or(Error::Truncated { at })?;
        self.rest = rest;
        str::from_utf8(bytes)
            .map(Box::from)
            .map_err(|error| Error::Utf8 {
                at: at.saturating_add(error.valid_up_to()),
            })
    }

    /// Each counted item takes at least one byte, so a count larger than the bytes
    /// left is [`Error::Truncated`].
    fn count(&mut self) -> Result<usize, Error> {
        let at = self.at();
        let n = self.varint()?;
        usize::try_from(n)
            .ok()
            .filter(|&n| n <= self.rest.len())
            .ok_or(Error::Truncated { at })
    }

    fn varint(&mut self) -> Result<u128, Error> {
        let at = self.at();
        let mut n = 0u128;
        for shift in (0..u128::BITS).step_by(7) {
            let byte = self.byte()?;
            let low = u128::from(byte & 0x7f);
            // Refuses bits that would shift past bit 127.
            if low.wrapping_shl(shift).wrapping_shr(shift) != low {
                return Err(Error::Varint { at });
            }
            n |= low.wrapping_shl(shift);
            if byte & 0x80 == 0 {
                if byte == 0 && shift > 0 {
                    return Err(Error::Varint { at });
                }
                return Ok(n);
            }
        }
        Err(Error::Varint { at })
    }

    fn byte(&mut self) -> Result<u8, Error> {
        let (&byte, rest) = self
            .rest
            .split_first()
            .ok_or(Error::Truncated { at: self.at() })?;
        self.rest = rest;
        Ok(byte)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::arbitrary::document;
    use proptest::prelude::*;

    fn value(kind: Kind) -> Value {
        Value { kind, span: None }
    }

    fn attribute(key: &str, kind: Kind) -> Attribute {
        Attribute {
            key: key.into(),
            key_span: None,
            value: value(kind),
        }
    }

    /// A list nested `levels` deep inside attribute `a`.
    fn nested(levels: usize) -> Document {
        let mut kind = Kind::Bool(true);
        for _ in 0..levels {
            kind = Kind::List(vec![value(kind)]);
        }
        Document {
            attributes: Map::new(vec![attribute("a", kind)]).unwrap(),
            blocks: Vec::new(),
        }
    }

    fn check(bytes: &[u8], expected: &Error, message: &str) {
        let err = decode(bytes).unwrap_err();
        assert_eq!(&err, expected);
        assert_eq!(err.to_string(), message);
    }

    mod encode {
        use super::*;

        #[test]
        fn writes_the_version_and_an_empty_body() {
            assert_eq!(encode(&Document::default()).unwrap(), [VERSION, 0, 0]);
        }

        #[test]
        fn writes_up_to_the_deepest_level() {
            let bytes = encode(&nested(DEPTH_MAX)).unwrap();
            assert_eq!(decode(&bytes).unwrap(), nested(DEPTH_MAX));
        }

        #[test]
        fn refuses_a_level_past_the_deepest() {
            let at = 4usize.saturating_add(DEPTH_MAX.saturating_mul(2));
            assert_eq!(
                encode(&nested(DEPTH_MAX.saturating_add(1))),
                Err(Error::TooDeep { at })
            );
        }

        #[test]
        fn refuses_a_block_past_the_deepest() {
            let mut document = Document::default();
            for _ in 0..=DEPTH_MAX {
                document = Document {
                    attributes: Map::default(),
                    blocks: vec![Block {
                        keyword: "b".into(),
                        keyword_span: None,
                        labels: Vec::new(),
                        body: document,
                        span: None,
                    }],
                };
            }
            // Each level: keyword length, keyword, label count, map count, block count.
            let at = 3usize.saturating_add(DEPTH_MAX.saturating_mul(5));
            assert_eq!(encode(&document), Err(Error::TooDeep { at }));
        }

        proptest! {
            #[test]
            fn reads_back_as_the_same_document(document in document()) {
                prop_assert_eq!(decode(&encode(&document).unwrap()).unwrap(), document);
            }

            #[test]
            fn writes_equal_documents_as_equal_bytes(document in document()) {
                let again = decode(&encode(&document).unwrap()).unwrap();
                prop_assert_eq!(encode(&again).unwrap(), encode(&document).unwrap());
            }
        }
    }

    mod decode {
        use super::*;

        #[derive(Clone, Debug)]
        enum Edit {
            Set(prop::sample::Index, u8),
            Insert(prop::sample::Index, u8),
            Remove(prop::sample::Index),
            Cut(prop::sample::Index),
        }

        fn edit() -> impl Strategy<Value = Edit> {
            prop_oneof![
                (any::<prop::sample::Index>(), any::<u8>())
                    .prop_map(|(i, b)| Edit::Set(i, b)),
                (any::<prop::sample::Index>(), any::<u8>())
                    .prop_map(|(i, b)| Edit::Insert(i, b)),
                any::<prop::sample::Index>().prop_map(Edit::Remove),
                any::<prop::sample::Index>().prop_map(Edit::Cut),
            ]
        }

        fn apply(bytes: &mut Vec<u8>, edit: &Edit) {
            let len = bytes.len();
            match edit {
                Edit::Set(i, byte) => bytes[i.index(len)] = *byte,
                Edit::Insert(i, byte) => bytes.insert(i.index(len), *byte),
                Edit::Remove(i) => drop(bytes.remove(i.index(len))),
                Edit::Cut(i) => bytes.truncate(i.index(len)),
            }
        }

        proptest! {
            #[test]
            fn reads_only_bytes_that_encode_writes(
                document in document(),
                edits in prop::collection::vec(edit(), 1..4),
            ) {
                let mut bytes = encode(&document).unwrap();
                for edit in &edits {
                    if !bytes.is_empty() {
                        apply(&mut bytes, edit);
                    }
                }
                if let Ok(decoded) = decode(&bytes) {
                    prop_assert_eq!(encode(&decoded).unwrap(), bytes);
                }
            }

            #[test]
            fn reads_only_random_bytes_that_encode_writes(
                bytes in prop::collection::vec(any::<u8>(), 0..64),
            ) {
                let mut bytes = bytes;
                if let Some(first) = bytes.first_mut() {
                    *first = VERSION;
                }
                if let Ok(decoded) = decode(&bytes) {
                    prop_assert_eq!(encode(&decoded).unwrap(), bytes);
                }
            }
        }

        #[test]
        fn refuses_another_version() {
            check(
                &[2, 0, 0],
                &Error::Version { found: 2 },
                "the document has format version 2, and this node reads version 1. \
                 Upgrade the node",
            );
        }

        #[test]
        fn refuses_no_bytes() {
            check(
                &[],
                &Error::Truncated { at: 0 },
                "the document bytes end early at byte 0. They are cut",
            );
        }

        #[test]
        fn refuses_a_count_larger_than_the_bytes_left() {
            check(
                &[VERSION, 4, 1, b'a', TRUE],
                &Error::Truncated { at: 1 },
                "the document bytes end early at byte 1. They are cut",
            );
        }

        #[test]
        fn refuses_a_cut_float() {
            check(
                &[VERSION, 1, 1, b'a', FLOAT, 0, 0],
                &Error::Truncated { at: 5 },
                "the document bytes end early at byte 5. They are cut",
            );
        }

        #[test]
        fn refuses_trailing_bytes() {
            check(
                &[VERSION, 0, 0, 0],
                &Error::TrailingBytes { at: 3 },
                "the document ends at byte 3, but more bytes follow. They are corrupt",
            );
        }

        #[test]
        fn refuses_an_unknown_tag() {
            check(
                &[VERSION, 1, 1, b'a', 9, 0],
                &Error::Tag { at: 4, tag: 9 },
                "byte 4 holds 9, which is not a value tag. It is corrupt",
            );
        }

        #[test]
        fn refuses_a_varint_longer_than_its_shortest_form() {
            check(
                &[VERSION, 0x80, 0x00, 0],
                &Error::Varint { at: 1 },
                "the number at byte 1 is not in its shortest form, or is too large. It \
                 is corrupt",
            );
        }

        #[test]
        fn refuses_a_varint_past_128_bits() {
            let mut bytes = vec![VERSION, 1, 1, b'a', INTEGER];
            bytes.extend([0xff; 18]);
            bytes.extend([0x04, 0]);
            check(
                &bytes,
                &Error::Varint { at: 5 },
                "the number at byte 5 is not in its shortest form, or is too large. It \
                 is corrupt",
            );
        }

        #[test]
        fn refuses_a_varint_of_twenty_bytes() {
            let mut bytes = vec![VERSION, 1, 1, b'a', INTEGER];
            bytes.extend([0x80; 19]);
            bytes.extend([0x01, 0]);
            check(
                &bytes,
                &Error::Varint { at: 5 },
                "the number at byte 5 is not in its shortest form, or is too large. It \
                 is corrupt",
            );
        }

        #[test]
        fn refuses_text_that_is_not_utf8() {
            check(
                &[VERSION, 1, 2, b'a', 0xff, TRUE, 0],
                &Error::Utf8 { at: 4 },
                "the text at byte 4 is not UTF-8. It is corrupt",
            );
        }

        #[test]
        fn refuses_keys_out_of_order() {
            check(
                &[VERSION, 2, 1, b'b', TRUE, 1, b'a', TRUE, 0],
                &Error::KeyOrder {
                    at: 5,
                    key: "a".into(),
                },
                "the key \"a\" at byte 5 is not after the key before it. Keys must \
                 strictly ascend",
            );
        }

        #[test]
        fn refuses_a_repeated_key() {
            check(
                &[VERSION, 2, 1, b'a', TRUE, 1, b'a', FALSE, 0],
                &Error::KeyOrder {
                    at: 5,
                    key: "a".into(),
                },
                "the key \"a\" at byte 5 is not after the key before it. Keys must \
                 strictly ascend",
            );
        }

        fn float(bits: u64) -> Vec<u8> {
            let mut bytes = vec![VERSION, 1, 1, b'a', FLOAT];
            bytes.extend(bits.to_le_bytes());
            bytes.push(0);
            bytes
        }

        #[test]
        fn refuses_negative_zero() {
            check(
                &float((-0.0f64).to_bits()),
                &Error::Float {
                    at: 5,
                    bits: 0x8000_0000_0000_0000,
                },
                "the float at byte 5 (bits 0x8000000000000000) is NaN, an infinity, or \
                 -0.0. It is corrupt",
            );
        }

        #[test]
        fn refuses_nan_and_the_infinities() {
            for bits in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY].map(f64::to_bits) {
                assert_eq!(decode(&float(bits)), Err(Error::Float { at: 5, bits }));
            }
        }

        #[test]
        fn refuses_a_reference_that_is_not_a_name() {
            let error = "a..b".parse::<types::name::Name>().unwrap_err();
            check(
                &[VERSION, 1, 1, b'x', REFERENCE, 4, b'a', b'.', b'.', b'b', 0],
                &Error::Reference { at: 5, error },
                "the reference at byte 5: \"a..b\" has a segment that is not valid: \
                 \"\". Use letters, digits, `_`, and `-`, separated by dots",
            );
        }

        #[test]
        fn refuses_a_level_past_the_deepest() {
            let mut bytes = vec![VERSION, 1, 1, b'a'];
            bytes.extend([LIST, 1].repeat(DEPTH_MAX.saturating_add(1)));
            bytes.extend([TRUE, 0]);
            let at = 4usize.saturating_add(DEPTH_MAX.saturating_mul(2));
            check(
                &bytes,
                &Error::TooDeep { at },
                &format!(
                    "the document nests deeper than 64 levels at byte {at}. Make it \
                     flatter"
                ),
            );
        }

        #[test]
        fn refuses_a_block_past_the_deepest() {
            let mut bytes = vec![VERSION, 0, 1];
            for _ in 0..DEPTH_MAX {
                bytes.extend([1, b'b', 0, 0, 1]);
            }
            bytes.extend([1, b'b', 0, 0, 0]);
            let at = 3usize.saturating_add(DEPTH_MAX.saturating_mul(5));
            check(
                &bytes,
                &Error::TooDeep { at },
                &format!(
                    "the document nests deeper than 64 levels at byte {at}. Make it \
                     flatter"
                ),
            );
        }
    }
}
