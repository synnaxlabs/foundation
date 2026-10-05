//! Pinned bytes of the canonical Document encoding. `spec` hashes these bytes, so a
//! change here changes every region's root hash.

// Lets Clippy treat the helpers as test code.
#![cfg(test)]

use document::encoding::{decode, encode};
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

fn check(document: &Document, bytes: &[u8]) {
    assert_eq!(encode(document).unwrap(), bytes);
    assert_eq!(&decode(bytes).unwrap(), document);
}

#[test]
fn empty() {
    check(&Document::default(), &[1, 0, 0]);
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
        &[1, 9][..],
        &[1, b'b', 1],
        &[1, b'f', 3, 0, 0, 0, 0, 0, 0, 0xf8, 0x3f],
        &[1, b'i', 2, 5],
        &[1, b'l', 6, 1, 2, 0xd8, 0x04],
        &[1, b'm', 7, 1, 1, b'k', 0],
        &[1, b'n', 2],
        &[0xff; 18],
        &[0x03],
        &[1, b'r', 5, 11],
        b"site_a.pt_1",
        &[1, b's', 4, 3],
        b"kPa",
        &[1, b'x', 8, 6],
        b"secret",
        &[1, 4, 2],
        b"pw",
        &[0],
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
        &[1, 0, 1, 9][..],
        b"connector",
        &[2, 5],
        b"opcua",
        &[12],
        b"site_a.plc_7",
        &[1, 3],
        b"url",
        &[4, 11],
        b"opc.tcp://x",
        &[1, 2],
        b"in",
        &[0, 1, 7],
        b"channel",
        &[5, 11],
        b"site_a.pt_1",
        &[0],
    ]
    .concat();
    check(&document, &bytes);
}
