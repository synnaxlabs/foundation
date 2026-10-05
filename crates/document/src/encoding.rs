//! The canonical bytes of a document. Each document has exactly one encoding, with no
//! spans, and `decode` refuses every byte string that `encode` cannot write.
//!
//! Format, with every integer little-endian:
//!
//! ```text
//! document := version:u8 body
//! body     := map count block*
//! block    := keyword:string count label:string* body
//! map      := count (key:string value)*        keys strictly ascend, by bytes
//! value    := 0 false | 1 true | 2 integer:i128 | 3 float:f64
//!           | 4 string | 5 reference:string | 6 list:count value*
//!           | 7 map:map | 8 call:function:string count value*
//! string   := length:u64 UTF-8 bytes
//! count    := u64
//! ```
//!
//! A float is finite and never -0.0.

use std::fmt;
use std::str;

use types::name;

use crate::value::{Call, Float, Kind, Value};
use crate::{Attribute, Block, Document, Label, Map, Span};

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
/// Returns [`TooDeep`] when `document` nests deeper than [`DEPTH_MAX`].
pub fn encode(document: &Document) -> Result<Vec<u8>, TooDeep> {
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
    let [version] = reader.chunk()?;
    if version > VERSION {
        return Err(Error::Newer { found: version });
    }
    if version != VERSION {
        return Err(Error::Version { found: version });
    }
    let document = reader.body(0)?;
    if !reader.rest.is_empty() {
        return Err(Error::TrailingBytes { at: reader.at() });
    }
    Ok(document)
}

/// A document that nests deeper than [`DEPTH_MAX`], so it has no encoding.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TooDeep {
    /// Where the first level past the limit is, when the document has spans.
    pub span: Option<Span>,
}

impl fmt::Display for TooDeep {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "the document nests deeper than {DEPTH_MAX} levels. Make it flatter"
        )
    }
}

impl std::error::Error for TooDeep {}

/// Bytes that are not the encoding of a document. `at` is a byte offset into the
/// bytes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Error {
    /// The bytes have a format version newer than this build reads.
    Newer {
        /// The version in the bytes.
        found: u8,
    },
    /// The bytes have a format version that does not exist.
    Version {
        /// The version in the bytes.
        found: u8,
    },
    /// The bytes end before the document does.
    Truncated {
        /// Where the part that runs past the end starts.
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
    /// The bytes nest deeper than [`DEPTH_MAX`].
    Depth {
        /// Where the first level past the limit starts.
        at: usize,
    },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Newer { found } => write!(
                f,
                "the document has format version {found}, and this node reads only \
                 version {VERSION}. Upgrade the node"
            ),
            Self::Version { found } => write!(
                f,
                "the document has format version {found}, which does not exist. It is \
                 corrupt"
            ),
            Self::Truncated { at } => write!(
                f,
                "the part at byte {at} runs past the end of the document. It is cut"
            ),
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
            Self::Utf8 { at } => {
                write!(f, "the text at byte {at} is not UTF-8. It is corrupt")
            }
            Self::KeyOrder { at, key } => write!(
                f,
                "the key {key:?} at byte {at} is not after the key before it. It is \
                 corrupt"
            ),
            Self::Float { at, bits } => write!(
                f,
                "the float at byte {at} (bits {bits:#018x}) is NaN, an infinity, or \
                 -0.0. It is corrupt"
            ),
            Self::Reference { at, .. } => write!(
                f,
                "the reference at byte {at} is not a valid name. It is corrupt"
            ),
            Self::Depth { at } => write!(
                f,
                "the document nests deeper than {DEPTH_MAX} levels at byte {at}. It is \
                 corrupt"
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

/// The depth inside one more level, or `None` past [`DEPTH_MAX`].
fn enter(depth: usize) -> Option<usize> {
    depth.checked_add(1).filter(|&inner| inner <= DEPTH_MAX)
}

struct Writer {
    out: Vec<u8>,
}

impl Writer {
    fn body(&mut self, document: &Document, depth: usize) -> Result<(), TooDeep> {
        self.map(&document.attributes, depth)?;
        self.count(document.blocks.len());
        for block in &document.blocks {
            let depth = enter(depth).ok_or(TooDeep { span: block.span })?;
            self.string(&block.keyword);
            self.count(block.labels.len());
            for label in &block.labels {
                self.string(&label.text);
            }
            self.body(&block.body, depth)?;
        }
        Ok(())
    }

    fn map(&mut self, map: &Map, depth: usize) -> Result<(), TooDeep> {
        self.count(map.iter().len());
        for attribute in map.iter() {
            self.string(&attribute.key);
            self.value(&attribute.value, depth)?;
        }
        Ok(())
    }

    fn value(&mut self, value: &Value, depth: usize) -> Result<(), TooDeep> {
        let inner = || enter(depth).ok_or(TooDeep { span: value.span });
        match &value.kind {
            Kind::Bool(false) => self.out.push(FALSE),
            Kind::Bool(true) => self.out.push(TRUE),
            Kind::Integer(n) => {
                self.out.push(INTEGER);
                self.out.extend_from_slice(&n.to_le_bytes());
            }
            Kind::Float(float) => {
                self.out.push(FLOAT);
                self.out.extend_from_slice(&float.get().to_le_bytes());
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
                let depth = inner()?;
                self.out.push(LIST);
                self.values(items, depth)?;
            }
            Kind::Map(map) => {
                let depth = inner()?;
                self.out.push(MAP);
                self.map(map, depth)?;
            }
            Kind::Call(call) => {
                let depth = inner()?;
                self.out.push(CALL);
                self.string(&call.function);
                self.values(&call.arguments, depth)?;
            }
        }
        Ok(())
    }

    fn values(&mut self, values: &[Value], depth: usize) -> Result<(), TooDeep> {
        self.count(values.len());
        values.iter().try_for_each(|value| self.value(value, depth))
    }

    fn string(&mut self, text: &str) {
        self.count(text.len());
        self.out.extend_from_slice(text.as_bytes());
    }

    fn count(&mut self, n: usize) {
        let n = u64::try_from(n).expect("invariant: a usize fits in a u64");
        self.out.extend_from_slice(&n.to_le_bytes());
    }
}

struct Reader<'a> {
    rest: &'a [u8],
    len: usize,
}

impl Reader<'_> {
    fn at(&self) -> usize {
        self.len
            .checked_sub(self.rest.len())
            .expect("invariant: the rest is a suffix of the bytes")
    }

    fn body(&mut self, depth: usize) -> Result<Document, Error> {
        let attributes = self.map(depth)?;
        let mut blocks = Vec::new();
        for _ in 0..self.count()? {
            let at = self.at();
            let depth = enter(depth).ok_or(Error::Depth { at })?;
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
        let inner = || enter(depth).ok_or(Error::Depth { at });
        let kind = match self.chunk()? {
            [FALSE] => Kind::Bool(false),
            [TRUE] => Kind::Bool(true),
            [INTEGER] => Kind::Integer(i128::from_le_bytes(self.chunk()?)),
            [FLOAT] => Kind::Float(self.float()?),
            [STRING] => Kind::String(self.string()?),
            [REFERENCE] => {
                let at = self.at();
                let text = self.string()?;
                Kind::Reference(
                    text.parse()
                        .map_err(|error| Error::Reference { at, error })?,
                )
            }
            [LIST] => Kind::List(self.values(inner()?)?),
            [MAP] => Kind::Map(self.map(inner()?)?),
            [CALL] => {
                let depth = inner()?;
                Kind::Call(Call {
                    function: self.string()?,
                    function_span: None,
                    arguments: self.values(depth)?,
                })
            }
            [tag] => return Err(Error::Tag { at, tag }),
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
        let bits = u64::from_le_bytes(self.chunk()?);
        // `Float::new` turns -0.0 into 0.0, so the bits differ.
        Float::new(f64::from_bits(bits))
            .filter(|float| float.get().to_bits() == bits)
            .ok_or(Error::Float { at, bits })
    }

    fn string(&mut self) -> Result<Box<str>, Error> {
        let len = self.count()?;
        let at = self.at();
        let (bytes, rest) = usize::try_from(len)
            .ok()
            .and_then(|len| self.rest.split_at_checked(len))
            .ok_or(Error::Truncated { at })?;
        self.rest = rest;
        str::from_utf8(bytes)
            .map(Box::from)
            .map_err(|error| Error::Utf8 {
                at: at.saturating_add(error.valid_up_to()),
            })
    }

    fn count(&mut self) -> Result<u64, Error> {
        self.chunk().map(u64::from_le_bytes)
    }

    fn chunk<const N: usize>(&mut self) -> Result<[u8; N], Error> {
        let at = self.at();
        let (chunk, rest) = self
            .rest
            .split_first_chunk::<N>()
            .ok_or(Error::Truncated { at })?;
        self.rest = rest;
        Ok(*chunk)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::arbitrary::document;
    use crate::{Position, Source};
    use proptest::prelude::*;

    fn count(n: u64) -> [u8; 8] {
        n.to_le_bytes()
    }

    fn string(text: &str) -> Vec<u8> {
        let len = u64::try_from(text.len()).unwrap();
        [&count(len)[..], text.as_bytes()].concat()
    }

    /// The bytes of a document whose one attribute, `a`, holds `value`. The value
    /// starts at byte 18.
    fn attribute(value: &[u8]) -> Vec<u8> {
        [&[VERSION][..], &count(1), &string("a"), value, &count(0)].concat()
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
            let empty = [&[VERSION][..], &count(0), &count(0)].concat();
            assert_eq!(encode(&Document::default()).unwrap(), empty);
        }

        #[test]
        fn too_deep_names_the_fix() {
            assert_eq!(
                TooDeep { span: None }.to_string(),
                "the document nests deeper than 64 levels. Make it flatter"
            );
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

    mod depth {
        use super::*;

        #[derive(Clone, Copy, Debug, PartialEq, Eq)]
        enum Level {
            Block,
            List,
            Map,
            Call,
        }

        /// `total` levels: some blocks, then lists, maps, and calls in any order.
        fn levels(total: usize) -> impl Strategy<Value = Vec<Level>> {
            (0..=total).prop_flat_map(move |blocks| {
                let value =
                    prop_oneof![Just(Level::List), Just(Level::Map), Just(Level::Call)];
                let values = prop::collection::vec(value, total.saturating_sub(blocks));
                values.prop_map(move |values| {
                    let mut levels = vec![Level::Block; blocks];
                    levels.extend(values);
                    levels
                })
            })
        }

        fn blocks(levels: &[Level]) -> usize {
            levels
                .iter()
                .take_while(|&&level| level == Level::Block)
                .count()
        }

        /// The bytes of a document nested through `levels`, outermost first, with
        /// `true` inside, and where each level starts.
        fn nested_bytes(levels: &[Level]) -> (Vec<u8>, Vec<usize>) {
            let mut bytes = vec![VERSION];
            let mut starts = Vec::new();
            for _ in 0..blocks(levels) {
                bytes.extend(count(0));
                bytes.extend(count(1));
                starts.push(bytes.len());
                bytes.extend(string("b"));
                bytes.extend(count(0));
            }
            bytes.extend(count(1));
            bytes.extend(string("a"));
            for level in levels.iter().skip(blocks(levels)) {
                starts.push(bytes.len());
                match level {
                    Level::List => {
                        bytes.push(LIST);
                        bytes.extend(count(1));
                    }
                    Level::Map => {
                        bytes.push(MAP);
                        bytes.extend(count(1));
                        bytes.extend(string("k"));
                    }
                    Level::Call => {
                        bytes.push(CALL);
                        bytes.extend(string("f"));
                        bytes.extend(count(1));
                    }
                    Level::Block => panic!("a block inside a value: {levels:?}"),
                }
            }
            bytes.push(TRUE);
            bytes.extend(count(0));
            (bytes, starts)
        }

        /// The document of [`nested_bytes`], with a span on each level.
        fn nested_document(levels: &[Level]) -> (Document, Vec<Span>) {
            let spans: Vec<Span> = (0u32..)
                .zip(levels)
                .map(|(i, _)| {
                    let at = Position {
                        offset: i,
                        line: 0,
                        column: i,
                    };
                    Span::new(Source(0), at, at).unwrap()
                })
                .collect();
            let mut value = Value {
                kind: Kind::Bool(true),
                span: None,
            };
            for (level, span) in levels.iter().zip(&spans).skip(blocks(levels)).rev() {
                let kind = match level {
                    Level::List => Kind::List(vec![value]),
                    Level::Map => Kind::Map(
                        Map::new(vec![Attribute {
                            key: "k".into(),
                            key_span: None,
                            value,
                        }])
                        .unwrap(),
                    ),
                    Level::Call => Kind::Call(Call {
                        function: "f".into(),
                        function_span: None,
                        arguments: vec![value],
                    }),
                    Level::Block => panic!("a block inside a value: {levels:?}"),
                };
                value = Value {
                    kind,
                    span: Some(*span),
                };
            }
            let attribute = Attribute {
                key: "a".into(),
                key_span: None,
                value,
            };
            let mut document = Document {
                attributes: Map::new(vec![attribute]).unwrap(),
                blocks: Vec::new(),
            };
            for span in spans.iter().take(blocks(levels)).rev() {
                let block = Block {
                    keyword: "b".into(),
                    keyword_span: None,
                    labels: Vec::new(),
                    body: document,
                    span: Some(*span),
                };
                document = Document {
                    attributes: Map::default(),
                    blocks: vec![block],
                };
            }
            (document, spans)
        }

        /// Both sides count each level and refuse the same one.
        fn check_depth(levels: &[Level]) {
            let (bytes, starts) = nested_bytes(levels);
            let (document, spans) = nested_document(levels);
            if let (Some(&at), Some(&span)) =
                (starts.get(DEPTH_MAX), spans.get(DEPTH_MAX))
            {
                assert_eq!(encode(&document), Err(TooDeep { span: Some(span) }));
                assert_eq!(decode(&bytes), Err(Error::Depth { at }));
            } else {
                assert_eq!(encode(&document).unwrap(), bytes);
                assert_eq!(decode(&bytes).unwrap(), document);
            }
        }

        #[test]
        fn counts_each_kind_of_level() {
            for level in [Level::Block, Level::List, Level::Map, Level::Call] {
                for total in [DEPTH_MAX, DEPTH_MAX.saturating_add(1)] {
                    check_depth(&vec![level; total]);
                }
            }
        }

        proptest! {
            #[test]
            fn counts_any_mix_of_levels(
                levels in prop_oneof![
                    levels(DEPTH_MAX),
                    levels(DEPTH_MAX.saturating_add(1)),
                ],
            ) {
                check_depth(&levels);
            }
        }

        #[test]
        fn refuses_a_hostile_depth_without_recursing() {
            for level in [Level::Block, Level::Map] {
                let (bytes, starts) = nested_bytes(&vec![level; 100_000]);
                assert_eq!(
                    decode(&bytes),
                    Err(Error::Depth {
                        at: starts[DEPTH_MAX]
                    })
                );
            }
        }

        #[test]
        fn depth_says_the_bytes_are_corrupt() {
            let (bytes, starts) = nested_bytes(&[Level::List; 65]);
            let at = starts[DEPTH_MAX];
            check(
                &bytes,
                &Error::Depth { at },
                &format!(
                    "the document nests deeper than 64 levels at byte {at}. It is \
                     corrupt"
                ),
            );
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
                if let Ok(decoded) = decode(&bytes) {
                    prop_assert_eq!(encode(&decoded).unwrap(), bytes);
                }
            }
        }

        #[test]
        fn refuses_a_newer_version() {
            check(
                &[2],
                &Error::Newer { found: 2 },
                "the document has format version 2, and this node reads only version \
                 1. Upgrade the node",
            );
        }

        #[test]
        fn refuses_a_version_that_does_not_exist() {
            check(
                &[0],
                &Error::Version { found: 0 },
                "the document has format version 0, which does not exist. It is \
                 corrupt",
            );
        }

        #[test]
        fn refuses_no_bytes() {
            check(
                &[],
                &Error::Truncated { at: 0 },
                "the part at byte 0 runs past the end of the document. It is cut",
            );
        }

        #[test]
        fn refuses_a_cut_count() {
            assert_eq!(decode(&[VERSION, 0, 0, 0]), Err(Error::Truncated { at: 1 }));
        }

        #[test]
        fn refuses_a_count_past_the_end() {
            let mut bytes = attribute(&[TRUE]);
            bytes.splice(1..9, count(2));
            bytes.truncate(19);
            assert_eq!(decode(&bytes), Err(Error::Truncated { at: 19 }));
        }

        #[test]
        fn refuses_a_string_past_the_end() {
            let bytes = [&[VERSION][..], &count(1), &count(5), b"a"].concat();
            assert_eq!(decode(&bytes), Err(Error::Truncated { at: 17 }));
        }

        #[test]
        fn refuses_a_cut_integer() {
            let bytes = attribute(&[INTEGER, 1]);
            assert_eq!(decode(&bytes[..20]), Err(Error::Truncated { at: 19 }));
        }

        #[test]
        fn refuses_a_cut_float() {
            let bytes = attribute(&[FLOAT, 0, 0, 0, 0]);
            assert_eq!(decode(&bytes[..23]), Err(Error::Truncated { at: 19 }));
        }

        #[test]
        fn refuses_trailing_bytes() {
            let bytes = [&[VERSION][..], &count(0), &count(0), &[0]].concat();
            check(
                &bytes,
                &Error::TrailingBytes { at: 17 },
                "the document ends at byte 17, but more bytes follow. They are corrupt",
            );
        }

        #[test]
        fn refuses_an_unknown_tag() {
            check(
                &attribute(&[9]),
                &Error::Tag { at: 18, tag: 9 },
                "byte 18 holds 9, which is not a value tag. It is corrupt",
            );
        }

        #[test]
        fn refuses_text_that_is_not_utf8() {
            let bytes =
                [&[VERSION][..], &count(1), &count(2), b"a\xff", &[TRUE]].concat();
            check(
                &[&bytes[..], &count(0)].concat(),
                &Error::Utf8 { at: 18 },
                "the text at byte 18 is not UTF-8. It is corrupt",
            );
        }

        #[test]
        fn refuses_keys_out_of_order() {
            let bytes = [
                &[VERSION][..],
                &count(2),
                &string("b"),
                &[TRUE],
                &string("a"),
                &[TRUE],
                &count(0),
            ]
            .concat();
            check(
                &bytes,
                &Error::KeyOrder {
                    at: 19,
                    key: "a".into(),
                },
                "the key \"a\" at byte 19 is not after the key before it. It is \
                 corrupt",
            );
        }

        #[test]
        fn refuses_a_repeated_key() {
            let bytes = [
                &[VERSION][..],
                &count(2),
                &string("a"),
                &[TRUE],
                &string("a"),
                &[FALSE],
                &count(0),
            ]
            .concat();
            assert_eq!(
                decode(&bytes),
                Err(Error::KeyOrder {
                    at: 19,
                    key: "a".into(),
                })
            );
        }

        fn float(bits: u64) -> Vec<u8> {
            attribute(&[&[FLOAT][..], &bits.to_le_bytes()].concat())
        }

        #[test]
        fn refuses_negative_zero() {
            check(
                &float((-0.0f64).to_bits()),
                &Error::Float {
                    at: 19,
                    bits: 0x8000_0000_0000_0000,
                },
                "the float at byte 19 (bits 0x8000000000000000) is NaN, an infinity, \
                 or -0.0. It is corrupt",
            );
        }

        #[test]
        fn refuses_nan_and_the_infinities() {
            for bits in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY].map(f64::to_bits) {
                assert_eq!(decode(&float(bits)), Err(Error::Float { at: 19, bits }));
            }
        }

        #[test]
        fn refuses_a_reference_that_is_not_a_name() {
            let error = "a..b".parse::<types::name::Name>().unwrap_err();
            let bytes = attribute(&[&[REFERENCE][..], &string("a..b")].concat());
            let err = decode(&bytes).unwrap_err();
            assert_eq!(
                err,
                Error::Reference {
                    at: 19,
                    error: error.clone()
                }
            );
            assert_eq!(
                err.to_string(),
                "the reference at byte 19 is not a valid name. It is corrupt"
            );
            let source = std::error::Error::source(&err).unwrap();
            assert_eq!(source.to_string(), error.to_string());
        }
    }
}
