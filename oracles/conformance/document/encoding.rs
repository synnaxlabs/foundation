//! Pinned bytes of the canonical Document encoding, and bytes it must refuse. `spec`
//! stores and hashes these bytes, so they never change: a new format takes a new
//! version byte.

// Lets Clippy treat the helpers as test code.
#![cfg(test)]

use document::encoding::{Error, decode, encode};
use document::value::{Call, Float, Kind, Value};
use document::{Attribute, Block, Document, Label, Map};

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

fn map(attributes: Vec<Attribute>) -> Map {
    Map::new(attributes).unwrap()
}

fn label(text: &str) -> Label {
    Label {
        text: text.into(),
        span: None,
    }
}

fn count(n: u64) -> [u8; 8] {
    n.to_le_bytes()
}

fn string(text: &str) -> Vec<u8> {
    let len = u64::try_from(text.len()).unwrap();
    [&count(len)[..], text.as_bytes()].concat()
}

fn check(document: &Document, bytes: &[u8]) {
    assert_eq!(encode(document).unwrap(), bytes);
    assert_eq!(&decode(bytes).unwrap(), document);
}

#[test]
fn empty() {
    check(
        &Document::default(),
        &[1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
    );
}

#[test]
fn every_value_kind() {
    let document = Document {
        attributes: map(vec![
            attribute("b", Kind::Bool(true)),
            attribute("f", Kind::Float(Float::new(1.5).unwrap())),
            attribute("i", Kind::Integer(-3)),
            attribute("l", Kind::List(vec![value(Kind::Integer(300))])),
            attribute("m", Kind::Map(map(vec![attribute("k", Kind::Bool(false))]))),
            attribute("n", Kind::Integer(i128::MIN)),
            attribute("r", Kind::Reference("site_a.pt_1".parse().unwrap())),
            attribute("s", Kind::String("kPa".into())),
            attribute("u", Kind::String("°C".into())),
            attribute(
                "x",
                Kind::Call(Call {
                    function: "secret".into(),
                    function_span: None,
                    arguments: vec![value(Kind::String("pw".into()))],
                }),
            ),
        ]),
        blocks: Vec::new(),
    };
    let bytes: Vec<u8> = [
        &[1][..],
        &count(10),
        &string("b"),
        &[1],
        &string("f"),
        &[3, 0, 0, 0, 0, 0, 0, 0xf8, 0x3f],
        &string("i"),
        &[2, 0xfd],
        &[0xff; 15],
        &string("l"),
        &[6],
        &count(1),
        &[2, 0x2c, 0x01],
        &[0; 14],
        &string("m"),
        &[7],
        &count(1),
        &string("k"),
        &[0],
        &string("n"),
        &[2],
        &[0; 15],
        &[0x80],
        &string("r"),
        &[5],
        &string("site_a.pt_1"),
        &string("s"),
        &[4],
        &string("kPa"),
        &string("u"),
        &[4, 3, 0, 0, 0, 0, 0, 0, 0, 0xc2, 0xb0, b'C'],
        &string("x"),
        &[8],
        &string("secret"),
        &count(1),
        &[4],
        &string("pw"),
        &count(0),
    ]
    .concat();
    check(&document, &bytes);
}

#[test]
fn connector_block() {
    let input = Block {
        keyword: "in".into(),
        keyword_span: None,
        labels: Vec::new(),
        body: Document {
            attributes: map(vec![attribute(
                "channel",
                Kind::Reference("site_a.pt_1".parse().unwrap()),
            )]),
            blocks: Vec::new(),
        },
        span: None,
    };
    let connector = Block {
        keyword: "connector".into(),
        keyword_span: None,
        labels: vec![label("opcua"), label("site_a.plc_7")],
        body: Document {
            attributes: map(vec![attribute("url", Kind::String("opc.tcp://x".into()))]),
            blocks: vec![input],
        },
        span: None,
    };
    let document = Document {
        attributes: Map::default(),
        blocks: vec![connector],
    };
    let bytes: Vec<u8> = [
        &[1][..],
        &count(0),
        &count(1),
        &string("connector"),
        &count(2),
        &string("opcua"),
        &string("site_a.plc_7"),
        &count(1),
        &string("url"),
        &[4],
        &string("opc.tcp://x"),
        &count(1),
        &string("in"),
        &count(0),
        &count(1),
        &string("channel"),
        &[5],
        &string("site_a.pt_1"),
        &count(0),
    ]
    .concat();
    check(&document, &bytes);
}

#[test]
fn blocks_keep_their_order() {
    let block = |text: &str| Block {
        keyword: "channel".into(),
        keyword_span: None,
        labels: vec![label(text)],
        body: Document::default(),
        span: None,
    };
    let document = Document {
        attributes: Map::default(),
        blocks: vec![block("b"), block("a"), block("b")],
    };
    let block = |text: &str| {
        [
            &string("channel")[..],
            &count(1),
            &string(text),
            &count(0),
            &count(0),
        ]
        .concat()
    };
    let bytes: Vec<u8> = [
        &[1][..],
        &count(0),
        &count(3),
        &block("b"),
        &block("a"),
        &block("b"),
    ]
    .concat();
    check(&document, &bytes);
}

/// The bytes of a document whose one attribute, `a`, holds `value`. The value starts
/// at byte 18.
fn one(value: &[u8]) -> Vec<u8> {
    [&[1][..], &count(1), &string("a"), value, &count(0)].concat()
}

#[test]
fn refuses_another_version() {
    let bytes = [&[2][..], &count(0), &count(0)].concat();
    assert_eq!(decode(&bytes), Err(Error::Newer { found: 2 }));
    let bytes = [&[0][..], &count(0), &count(0)].concat();
    assert_eq!(decode(&bytes), Err(Error::Version { found: 0 }));
}

#[test]
fn refuses_trailing_bytes() {
    let bytes = [&[1][..], &count(0), &count(0), &[0]].concat();
    assert_eq!(decode(&bytes), Err(Error::TrailingBytes { at: 17 }));
}

#[test]
fn refuses_negative_zero_nan_and_the_infinities() {
    for bits in [
        0x8000_0000_0000_0000,
        0x7ff8_0000_0000_0000,
        0x7ff0_0000_0000_0000,
    ] {
        let bytes = one(&[&[3][..], &u64::to_le_bytes(bits)].concat());
        assert_eq!(decode(&bytes), Err(Error::Float { at: 19, bits }));
    }
}

#[test]
fn refuses_keys_that_do_not_strictly_ascend() {
    for (first, second) in [("b", "a"), ("a", "a")] {
        let bytes = [
            &[1][..],
            &count(2),
            &string(first),
            &[1],
            &string(second),
            &[1],
            &count(0),
        ]
        .concat();
        let key = second.into();
        assert_eq!(decode(&bytes), Err(Error::KeyOrder { at: 19, key }));
    }
}

#[test]
fn refuses_an_unknown_tag() {
    assert_eq!(decode(&one(&[9])), Err(Error::Tag { at: 18, tag: 9 }));
}

#[derive(Clone, Copy)]
enum Level {
    Block,
    List,
    Map,
    Call,
}

/// The bytes of a document nested `levels` deep through `level`, with `true` inside,
/// and where the level after the 64th starts.
fn nested(level: Level, levels: usize) -> (Vec<u8>, Option<usize>) {
    let mut bytes = vec![1];
    let mut starts = Vec::new();
    let blocks = if let Level::Block = level { levels } else { 0 };
    for _ in 0..blocks {
        bytes.extend(count(0));
        bytes.extend(count(1));
        starts.push(bytes.len());
        bytes.extend(string("b"));
        bytes.extend(count(0));
    }
    bytes.extend(count(1));
    bytes.extend(string("a"));
    for _ in blocks..levels {
        starts.push(bytes.len());
        match level {
            Level::Block => {}
            Level::List => bytes.extend([&[6][..], &count(1)].concat()),
            Level::Map => bytes.extend([&[7][..], &count(1), &string("k")].concat()),
            Level::Call => bytes.extend([&[8][..], &string("f"), &count(1)].concat()),
        }
    }
    bytes.push(1);
    bytes.extend(count(0));
    (bytes, starts.get(64).copied())
}

#[test]
fn reads_64_levels_and_refuses_65() {
    for level in [Level::Block, Level::List, Level::Map, Level::Call] {
        let (bytes, _) = nested(level, 64);
        assert_eq!(encode(&decode(&bytes).unwrap()).unwrap(), bytes);
        let (bytes, at) = nested(level, 65);
        assert_eq!(decode(&bytes), Err(Error::Depth { at: at.unwrap() }));
    }
}
