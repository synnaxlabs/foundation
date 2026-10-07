use document::encoding::Checked;
use document::value::{Kind, Value};
use document::{Attribute, Document, Map};
use proptest::prelude::*;
use proptest::sample::Index;

use super::*;

fn selector(texts: &[&str]) -> Selector {
    Selector::new(texts.iter().copied()).unwrap()
}

fn policy() -> Definition {
    let allow = [Action::Read, Action::Write].into_iter().collect();
    Definition::Access(Policy::new(
        selector(&["ops.*"]),
        selector(&["site_a.**", "!site_a.@secrets.**"]),
        allow,
        Authority(9),
    ))
}

/// Writes a pattern as a file writes it: a leading `!` is the exclusion flag.
fn text(bytes: &mut Vec<u8>, text: &[u8]) {
    let (excluded, text) = text.strip_prefix(b"!").map_or((0, text), |t| (1, t));
    bytes.push(excluded);
    bytes.extend_from_slice(&u64::try_from(text.len()).unwrap().to_le_bytes());
    bytes.extend_from_slice(text);
}

/// The bytes of an access policy, from its parts.
fn access(subjects: &[&[u8]], select: &[&[u8]], allow: u8, authority: u8) -> Vec<u8> {
    let mut bytes = vec![VERSION, ACCESS];
    for list in [subjects, select] {
        bytes.extend_from_slice(&u64::try_from(list.len()).unwrap().to_le_bytes());
        for t in list {
            text(&mut bytes, t);
        }
    }
    bytes.extend_from_slice(&[allow, authority]);
    bytes
}

#[test]
fn writes_the_documented_layout() {
    let expected = access(
        &[b"ops.*"],
        &[b"site_a.**", b"!site_a.@secrets.**"],
        0b11,
        9,
    );
    assert_eq!(policy().encode(), expected);
}

#[test]
fn reads_what_it_writes() {
    assert_eq!(Definition::decode(&policy().encode()), Ok(policy()));
}

#[test]
fn writes_no_authority_without_write() {
    let read = [Action::Read].into_iter().collect();
    let policy = Policy::new(selector(&["a"]), selector(&["b"]), read, Authority(9));
    let bytes = Definition::Access(policy).encode();
    assert_eq!(bytes, access(&[b"a"], &[b"b"], 0b1, 0));
}

#[test]
fn refuses_a_newer_or_unknown_version() {
    let mut bytes = policy().encode();
    bytes[0] = 2;
    assert_eq!(Definition::decode(&bytes), Err(Error::Newer { found: 2 }));
    assert_eq!(
        Error::Newer { found: 2 }.to_string(),
        "the definition has format version 2, newer than 1"
    );
    bytes[0] = 0;
    assert_eq!(Definition::decode(&bytes), Err(Error::Version { found: 0 }));
    assert_eq!(
        Error::Version { found: 0 }.to_string(),
        "the definition has format version 0, which does not exist"
    );
    assert_eq!(Definition::decode(&[]), Err(Error::Truncated { at: 0 }));
}

#[test]
fn refuses_an_unknown_kind() {
    for tag in (0..=u8::MAX).filter(|t| {
        ![
            ACCESS,
            CONNECTOR,
            REGION,
            NODE_SETTINGS,
            COMPRESSION,
            PLACEMENT,
            TIME,
            CHANNEL,
        ]
        .contains(t)
    }) {
        assert_eq!(
            Definition::decode(&[VERSION, tag]),
            Err(Error::Kind { at: 1, tag })
        );
    }
}

#[test]
fn refuses_bytes_that_end_early_or_run_on() {
    let bytes = policy().encode();
    let end = bytes.len();
    assert_eq!(
        Definition::decode(&bytes[..end - 1]),
        Err(Error::Truncated { at: end - 1 })
    );
    let mut longer = bytes;
    longer.push(0);
    assert_eq!(
        Definition::decode(&longer),
        Err(Error::TrailingBytes { at: end })
    );
    assert_eq!(
        Error::Truncated { at: 4 }.to_string(),
        "the definition ends early at byte 4"
    );
    assert_eq!(
        Error::TrailingBytes { at: 4 }.to_string(),
        "bytes follow the definition at byte 4"
    );
}

#[test]
fn refuses_a_count_larger_than_the_bytes_left() {
    let mut bytes = vec![VERSION, ACCESS];
    bytes.extend_from_slice(&u64::MAX.to_le_bytes());
    assert_eq!(Definition::decode(&bytes), Err(Error::Truncated { at: 2 }));
}

#[test]
fn refuses_more_patterns_than_the_bytes_left_can_hold() {
    let mut bytes = vec![VERSION, ACCESS];
    bytes.extend_from_slice(&8_u64.to_le_bytes());
    bytes.extend_from_slice(&[0; 8]);
    assert_eq!(Definition::decode(&bytes), Err(Error::Truncated { at: 2 }));
}

#[test]
fn refuses_a_pattern_that_is_not_utf8() {
    let bytes = access(&[b"ab\xff"], &[b"b"], 1, 0);
    assert_eq!(Definition::decode(&bytes), Err(Error::Utf8 { at: 21 }));
    assert_eq!(
        Error::Utf8 { at: 21 }.to_string(),
        "a text is not UTF-8 at byte 21"
    );
}

#[test]
fn refuses_patterns_that_do_not_read() {
    let bytes = access(&[b"a"], &[b"!b"], 1, 0);
    let error = Error::Pattern {
        at: 20,
        error: name::Error::NoInclude,
    };
    assert_eq!(Definition::decode(&bytes), Err(error));
    assert_eq!(
        Definition::decode(&bytes).unwrap_err().to_string(),
        "the patterns at byte 20 do not read: a selector includes no names"
    );
    let bytes = access(&[b"a*"], &[b"b"], 1, 0);
    let error = Error::Pattern {
        at: 2,
        error: name::Error::Wildcard { input: "a*".into() },
    };
    assert_eq!(Definition::decode(&bytes), Err(error));
}

#[test]
fn refuses_an_exclusion_flag_that_is_not_0_or_1() {
    let mut bytes = access(&[b"a"], &[b"b"], 1, 0);
    bytes[10] = 2;
    let error = Error::Flag { at: 10, found: 2 };
    assert_eq!(Definition::decode(&bytes), Err(error.clone()));
    assert_eq!(error.to_string(), "the flag 2 at byte 10 is not 0 or 1");
}

#[test]
fn refuses_a_bad_flag_before_a_short_text() {
    let mut bytes = vec![VERSION, ACCESS];
    bytes.extend_from_slice(&1_u64.to_le_bytes());
    bytes.push(2);
    bytes.extend_from_slice(&255_u64.to_le_bytes());
    assert_eq!(
        Definition::decode(&bytes),
        Err(Error::Flag { at: 10, found: 2 })
    );
}

#[test]
fn refuses_an_included_pattern_that_starts_with_a_bang() {
    let mut bytes = vec![VERSION, ACCESS];
    bytes.extend_from_slice(&1_u64.to_le_bytes());
    bytes.push(0);
    bytes.extend_from_slice(&2_u64.to_le_bytes());
    bytes.extend_from_slice(b"!a");
    let error = Error::Pattern {
        at: 2,
        error: name::Error::Segment {
            input: "!a".into(),
            segment: "!a".into(),
        },
    };
    assert_eq!(Definition::decode(&bytes), Err(error.clone()));
    assert_eq!(
        error.to_string(),
        "the patterns at byte 2 do not read: a segment is not valid: \"!a\" in \"!a\""
    );
}

#[test]
fn refuses_an_excluded_pattern_that_starts_with_a_bang() {
    let bytes = access(&[b"a"], &[b"b", b"!!c"], 1, 0);
    let error = Error::Pattern {
        at: 20,
        error: name::Error::Segment {
            input: "!!c".into(),
            segment: "!c".into(),
        },
    };
    assert_eq!(Definition::decode(&bytes), Err(error.clone()));
    assert_eq!(
        error.to_string(),
        "the patterns at byte 20 do not read: a segment is not valid: \"!c\" in \"!!c\""
    );
}

#[test]
fn stores_an_exclusion_of_the_longest_name() {
    let longest = "a".repeat(Name::MAX_BYTES);
    let excluded = format!("!{longest}");
    let select = Selector::new(["**", excluded.as_str()]).unwrap();
    let policy = Policy::new(selector(&["a"]), select, Actions::NONE, Authority(0));
    let definition = Definition::Access(policy);
    let bytes = definition.encode();
    let stored = [
        &[1][..],
        &255_u64.to_le_bytes(),
        longest.as_bytes(),
        &[0, 0],
    ]
    .concat();
    assert!(bytes.ends_with(&stored));
    assert_eq!(Definition::decode(&bytes), Ok(definition));
}

#[test]
fn refuses_bits_that_name_no_action() {
    let bytes = access(&[b"a"], &[b"b"], 0b100_0001, 0);
    let error = Error::Actions {
        at: 38,
        bits: 0b100_0001,
    };
    assert_eq!(Definition::decode(&bytes), Err(error.clone()));
    assert_eq!(
        error.to_string(),
        "the actions 0b01000001 at byte 38 name no action"
    );
}

#[test]
fn refuses_an_authority_without_write() {
    let bytes = access(&[b"a"], &[b"b"], 0b1, 3);
    let error = Error::Authority {
        at: 39,
        found: Authority(3),
    };
    assert_eq!(Definition::decode(&bytes), Err(error.clone()));
    assert_eq!(
        error.to_string(),
        "authority 3 at byte 39 is on a policy without write"
    );
}

fn name(text: &str) -> Name {
    text.parse().unwrap()
}

/// A config with one attribute per pair.
fn config(pairs: &[(&str, i128)]) -> Checked {
    let attributes = pairs.iter().map(|&(key, n)| Attribute {
        key: key.into(),
        key_span: None,
        value: Value {
            kind: Kind::Integer(n),
            span: None,
        },
    });
    Checked::new(Document {
        attributes: Map::new(attributes.collect()).unwrap(),
        blocks: Vec::new(),
    })
    .unwrap()
}

fn connector() -> Definition {
    let config = config(&[("port", 502)]);
    Definition::Connector(Connector::new(name("modbus"), name("gw_1"), config))
}

fn length(bytes: &mut Vec<u8>, n: usize) {
    bytes.extend_from_slice(&u64::try_from(n).unwrap().to_le_bytes());
}

/// The bytes of a connector, from its parts.
fn connector_bytes(kind: &[u8], node: &[u8], config: &[u8]) -> Vec<u8> {
    let mut bytes = vec![VERSION, CONNECTOR];
    for part in [kind, node, config] {
        length(&mut bytes, part.len());
        bytes.extend_from_slice(part);
    }
    bytes
}

/// The bytes of a region record, from its parts.
fn region_bytes(epoch: u64, voters: &[&[u8]]) -> Vec<u8> {
    let mut bytes = vec![VERSION, REGION];
    bytes.extend_from_slice(&epoch.to_le_bytes());
    length(&mut bytes, voters.len());
    for voter in voters {
        length(&mut bytes, voter.len());
        bytes.extend_from_slice(voter);
    }
    bytes
}

fn region() -> Definition {
    let voters = [name("n_3"), name("n_1"), name("n_2"), name("n_1")];
    Definition::Region(Delegation::new(7, voters).unwrap())
}

#[test]
fn writes_the_documented_connector_layout() {
    let config = config(&[("port", 502)]).encode();
    let expected = connector_bytes(b"modbus", b"gw_1", &config);
    assert_eq!(connector().encode(), expected);
    assert_eq!(Definition::decode(&expected), Ok(connector()));
}

#[test]
fn writes_the_documented_region_layout_with_voters_in_order() {
    let expected = region_bytes(7, &[b"n_1", b"n_2", b"n_3"]);
    assert_eq!(region().encode(), expected);
    assert_eq!(Definition::decode(&expected), Ok(region()));
}

#[test]
fn refuses_a_name_that_does_not_read() {
    let config = Checked::new(Document::default()).unwrap().encode();
    let bytes = connector_bytes(b"modbus", b"gw 1", &config);
    let error = Error::Name {
        at: 16,
        error: name::Error::Segment {
            input: "gw 1".into(),
            segment: "gw 1".into(),
        },
    };
    assert_eq!(Definition::decode(&bytes), Err(error.clone()));
    assert_eq!(
        error.to_string(),
        "the name at byte 16 does not read: a segment is not valid: \"gw 1\" in \
         \"gw 1\""
    );
    let bytes = region_bytes(1, &[b"n.*"]);
    let error = Error::Name {
        at: 18,
        error: name::Error::Wildcard {
            input: "n.*".into(),
        },
    };
    assert_eq!(Definition::decode(&bytes), Err(error));
}

#[test]
fn refuses_a_config_that_is_not_a_document() {
    let bytes = connector_bytes(b"modbus", b"gw_1", &[9]);
    let error = Error::Config {
        at: 36,
        error: encoding::Error::Newer { found: 9 },
    };
    assert_eq!(Definition::decode(&bytes), Err(error.clone()));
    assert_eq!(
        error.to_string(),
        "the connector config at byte 36 does not read: the document has format \
         version 9, and this node reads only version 1. Upgrade the node"
    );
    let mut config = Checked::new(Document::default()).unwrap().encode();
    config.push(0);
    let bytes = connector_bytes(b"modbus", b"gw_1", &config);
    let at = config.len() - 1;
    let error = Error::Config {
        at: 36,
        error: encoding::Error::TrailingBytes { at },
    };
    assert_eq!(Definition::decode(&bytes), Err(error));
}

#[test]
fn refuses_a_config_that_runs_past_the_end() {
    let mut bytes = connector_bytes(b"modbus", b"gw_1", &[]);
    let at = bytes.len() - 8;
    bytes.truncate(at);
    length(&mut bytes, 1);
    assert_eq!(Definition::decode(&bytes), Err(Error::Truncated { at }));
}

#[test]
fn refuses_voters_out_of_order_or_repeated() {
    let bytes = region_bytes(1, &[b"n_2", b"n_1"]);
    let error = Error::Order { at: 29 };
    assert_eq!(Definition::decode(&bytes), Err(error.clone()));
    assert_eq!(
        error.to_string(),
        "the name at byte 29 is not after the name before it"
    );
    let bytes = region_bytes(1, &[b"n_1", b"n_1"]);
    assert_eq!(Definition::decode(&bytes), Err(error));
}

#[test]
fn refuses_a_region_with_no_voter() {
    let bytes = region_bytes(1, &[]);
    let error = Error::NoVoters { at: 10 };
    assert_eq!(Definition::decode(&bytes), Err(error.clone()));
    assert_eq!(error.to_string(), "the region at byte 10 has no voter");
}

#[test]
fn refuses_more_voters_than_the_bytes_left_can_hold() {
    let mut bytes = region_bytes(1, &[]);
    bytes.truncate(10);
    length(&mut bytes, 2);
    bytes.extend_from_slice(&[0; 15]);
    assert_eq!(Definition::decode(&bytes), Err(Error::Truncated { at: 10 }));
}

fn settings(disk: Option<u64>, pool: Option<u64>) -> Definition {
    let size = |b: Option<u64>| b.map(byte::Size::from_bytes);
    let policy = node_settings::Policy::new(
        selector(&["site_a.**", "!site_a.gw"]),
        size(disk),
        size(pool),
    );
    Definition::NodeSettings(policy.unwrap())
}

/// The bytes of a node settings policy, from its parts.
fn settings_bytes(disk: u64, pool: u64) -> Vec<u8> {
    let mut bytes = vec![VERSION, NODE_SETTINGS];
    bytes.extend_from_slice(&2_u64.to_le_bytes());
    text(&mut bytes, b"site_a.**");
    text(&mut bytes, b"!site_a.gw");
    bytes.extend_from_slice(&disk.to_le_bytes());
    bytes.extend_from_slice(&pool.to_le_bytes());
    bytes
}

#[test]
fn writes_the_documented_node_settings_layout() {
    let both = settings(Some(1 << 30), Some(1 << 20));
    assert_eq!(both.encode(), settings_bytes(1 << 30, 1 << 20));
    assert_eq!(Definition::decode(&both.encode()), Ok(both));
}

#[test]
fn writes_and_reads_no_budget_as_zero() {
    let disk = settings(Some(7), None);
    assert_eq!(disk.encode(), settings_bytes(7, 0));
    assert_eq!(Definition::decode(&settings_bytes(7, 0)), Ok(disk));
    let extremes = settings(Some(1), Some(u64::MAX));
    assert_eq!(extremes.encode(), settings_bytes(1, u64::MAX));
    assert_eq!(
        Definition::decode(&settings_bytes(1, u64::MAX)),
        Ok(extremes)
    );
}

#[test]
fn refuses_node_settings_with_no_budget() {
    let bytes = settings_bytes(0, 0);
    let error = Definition::decode(&bytes).unwrap_err();
    let at = bytes.len() - 16;
    assert_eq!(
        error,
        Error::Budget {
            at,
            error: node_settings::Error::NoBudget
        }
    );
    assert_eq!(
        error.to_string(),
        format!("the budgets at byte {at}: the policy sets no budget")
    );
}

#[test]
fn refuses_node_settings_that_end_early() {
    let bytes = settings_bytes(7, 9);
    let end = bytes.len();
    assert_eq!(
        Definition::decode(&bytes[..end - 1]),
        Err(Error::Truncated { at: end - 8 })
    );
}

/// The bytes of a policy with one selector, from its parts.
fn select_bytes(tag: u8, rest: &[u8]) -> Vec<u8> {
    let mut bytes = vec![VERSION, tag];
    length(&mut bytes, 2);
    text(&mut bytes, b"site_a.**");
    text(&mut bytes, b"!site_a.gw");
    bytes.extend_from_slice(rest);
    bytes
}

fn select() -> Selector {
    selector(&["site_a.**", "!site_a.gw"])
}

#[test]
fn writes_the_documented_compression_layout() {
    for (mode, byte) in [(Mode::Auto, 0), (Mode::Raw, 1), (Mode::Max, 2)] {
        let definition = Definition::Compression(compression::Policy {
            select: select(),
            mode,
        });
        let expected = select_bytes(COMPRESSION, &[byte]);
        assert_eq!(definition.encode(), expected);
        assert_eq!(Definition::decode(&expected), Ok(definition));
    }
}

#[test]
fn refuses_an_unknown_compression_mode() {
    let bytes = select_bytes(COMPRESSION, &[3]);
    let at = bytes.len() - 1;
    let error = Error::Mode { at, found: 3 };
    assert_eq!(Definition::decode(&bytes), Err(error.clone()));
    assert_eq!(
        error.to_string(),
        format!("compression mode 3 at byte {at} is not a known mode")
    );
}

#[test]
fn refuses_a_compression_that_ends_early() {
    let bytes = select_bytes(COMPRESSION, &[]);
    assert_eq!(
        Definition::decode(&bytes),
        Err(Error::Truncated { at: bytes.len() })
    );
}

#[test]
fn defaults_to_the_auto_mode() {
    assert_eq!(Mode::default(), Mode::Auto);
}

/// Writes a count, then each name as a text.
fn names(bytes: &mut Vec<u8>, names: &[&[u8]]) {
    length(bytes, names.len());
    for name in names {
        length(bytes, name.len());
        bytes.extend_from_slice(name);
    }
}

/// The bytes of a placement, from its parts.
fn placement_bytes(standby: Option<&[u8]>, copies: &[&[u8]]) -> Vec<u8> {
    let mut rest = Vec::new();
    match standby {
        None => rest.push(0),
        Some(node) => {
            rest.push(1);
            length(&mut rest, node.len());
            rest.extend_from_slice(node);
        }
    }
    names(&mut rest, copies);
    select_bytes(PLACEMENT, &rest)
}

fn placement(standby: Option<&str>, copies: &[&str]) -> Definition {
    let copies = copies.iter().map(|c| name(c));
    let policy = placement::Policy::new(select(), standby.map(name), copies);
    Definition::Placement(policy.unwrap())
}

#[test]
fn writes_the_documented_placement_layout() {
    for (definition, expected) in [
        (
            placement(Some("n_1"), &["n_3", "n_2"]),
            placement_bytes(Some(b"n_1"), &[b"n_2", b"n_3"]),
        ),
        (placement(None, &["n_2"]), placement_bytes(None, &[b"n_2"])),
        (
            placement(Some("n_1"), &[]),
            placement_bytes(Some(b"n_1"), &[]),
        ),
    ] {
        assert_eq!(definition.encode(), expected);
        assert_eq!(Definition::decode(&expected), Ok(definition));
    }
}

#[test]
fn refuses_a_presence_flag_that_is_not_0_or_1() {
    let mut bytes = placement_bytes(None, &[b"n_2"]);
    let at = select_bytes(PLACEMENT, &[]).len();
    bytes[at] = 2;
    assert_eq!(
        Definition::decode(&bytes),
        Err(Error::Flag { at, found: 2 })
    );
}

#[test]
fn refuses_a_time_presence_flag_that_is_not_0_or_1() {
    let mut bytes = time_bytes(Some(&[b"n_1"]));
    let at = select_bytes(TIME, &[]).len();
    bytes[at] = 2;
    assert_eq!(
        Definition::decode(&bytes),
        Err(Error::Flag { at, found: 2 })
    );
}

#[test]
fn refuses_copies_out_of_order_or_repeated() {
    let at = select_bytes(PLACEMENT, &[]).len() + 1 + 8 + 11;
    for copies in [[b"n_3", b"n_2"], [b"n_2", b"n_2"]] {
        let bytes = placement_bytes(None, &copies.map(|c| &c[..]));
        assert_eq!(Definition::decode(&bytes), Err(Error::Order { at }));
    }
}

#[test]
fn refuses_a_placement_the_policy_refuses() {
    let at = select_bytes(PLACEMENT, &[]).len();
    let bytes = placement_bytes(None, &[]);
    let error = Error::Placement {
        at,
        error: placement::Error::Empty,
    };
    assert_eq!(Definition::decode(&bytes), Err(error.clone()));
    assert_eq!(
        error.to_string(),
        format!("the placement at byte {at}: a placement has no standby and no copy")
    );
    let bytes = placement_bytes(Some(b"n_2"), &[b"n_1", b"n_2"]);
    let error = Error::Placement {
        at,
        error: placement::Error::Overlap(name("n_2")),
    };
    assert_eq!(Definition::decode(&bytes), Err(error));
}

#[test]
fn refuses_a_placement_that_ends_early() {
    let bytes = placement_bytes(Some(b"n_1"), &[]);
    let end = bytes.len();
    assert_eq!(
        Definition::decode(&bytes[..end - 1]),
        Err(Error::Truncated { at: end - 8 })
    );
}

/// The bytes of a time policy, from its parts.
fn time_bytes(peers: Option<&[&[u8]]>) -> Vec<u8> {
    let mut rest = Vec::new();
    match peers {
        None => rest.push(0),
        Some(peers) => {
            rest.push(1);
            names(&mut rest, peers);
        }
    }
    select_bytes(TIME, &rest)
}

fn time_policy(peers: Option<&[&str]>) -> Definition {
    let peers = peers.map_or(time::Peers::Voters, |p| {
        time::Peers::Listed(p.iter().map(|p| name(p)).collect())
    });
    Definition::Time(time::Policy::new(select(), peers))
}

#[test]
fn writes_the_documented_time_layout() {
    for (definition, expected) in [
        (time_policy(None), time_bytes(None)),
        (time_policy(Some(&[])), time_bytes(Some(&[]))),
        (
            time_policy(Some(&["n_2", "n_1"])),
            time_bytes(Some(&[b"n_1", b"n_2"])),
        ),
    ] {
        assert_eq!(definition.encode(), expected);
        assert_eq!(Definition::decode(&expected), Ok(definition));
    }
}

#[test]
fn refuses_peers_out_of_order() {
    let bytes = time_bytes(Some(&[b"n_2", b"n_1"]));
    let at = select_bytes(TIME, &[]).len() + 1 + 8 + 11;
    assert_eq!(Definition::decode(&bytes), Err(Error::Order { at }));
}

fn key(n: u128) -> Key {
    Key::from_u128(n)
}

/// Each scalar with its code.
const SCALARS: [(Scalar, u8); 14] = [
    (Scalar::Bool, 0),
    (Scalar::I8, 1),
    (Scalar::I16, 2),
    (Scalar::I32, 3),
    (Scalar::I64, 4),
    (Scalar::U8, 5),
    (Scalar::U16, 6),
    (Scalar::U32, 7),
    (Scalar::U64, 8),
    (Scalar::F32, 9),
    (Scalar::F64, 10),
    (Scalar::Stamp, 11),
    (Scalar::Span, 12),
    (Scalar::Uuid, 13),
];

#[test]
fn codes_each_scalar_by_its_documented_byte() {
    for (each, byte) in SCALARS {
        assert_eq!(code(each), byte);
        assert_eq!(scalar(byte), Some(each));
    }
    for byte in 14..=u8::MAX {
        assert_eq!(scalar(byte), None);
    }
}

/// The bytes of a channel, from its parts after the key and the kind byte.
fn channel_bytes(kind: u8, rest: &[u8]) -> Vec<u8> {
    let mut bytes = vec![VERSION, CHANNEL];
    bytes.extend_from_slice(&7_u128.to_le_bytes());
    bytes.push(kind);
    bytes.extend_from_slice(rest);
    bytes
}

/// The bytes of a data channel on index 9 with no quality, from its data type on.
fn data_bytes(rest: &[u8]) -> Vec<u8> {
    let mut tail = 9_u128.to_le_bytes().to_vec();
    tail.push(0);
    tail.extend_from_slice(rest);
    channel_bytes(1, &tail)
}

/// Where the data type of [`data_bytes`] starts.
const DATA_TYPE_AT: usize = 2 + 16 + 1 + 16 + 1;

/// A data channel with key 7 on index 9, with quality 10.
fn data(data_type: DataType, unit: Option<&str>) -> Definition {
    let unit = unit.map(|u| Unit::new(u).unwrap());
    Definition::Channel(Channel {
        key: key(7),
        kind: channel::Kind::Data(
            Data::new(key(9), Some(key(10)), data_type, unit).unwrap(),
        ),
    })
}

fn f64s(len: u32) -> DataType {
    DataType::Sample(sample::Type::Array {
        element: Scalar::F64,
        len,
    })
}

#[test]
fn writes_the_documented_channel_layout() {
    let index = Definition::Channel(Channel {
        key: key(7),
        kind: channel::Kind::Index {
            error: Some(key(8)),
            control: None,
        },
    });
    let mut rest = vec![1];
    rest.extend_from_slice(&8_u128.to_le_bytes());
    rest.push(0);
    let mut tail = 9_u128.to_le_bytes().to_vec();
    tail.push(1);
    tail.extend_from_slice(&10_u128.to_le_bytes());
    tail.extend_from_slice(&[1, 10, 3, 0, 0, 0, 1]);
    tail.extend_from_slice(&3_u64.to_le_bytes());
    tail.extend_from_slice(b"kPa");
    for (definition, expected) in [
        (index, channel_bytes(0, &rest)),
        (data(f64s(3), Some("kPa")), channel_bytes(1, &tail)),
    ] {
        assert_eq!(definition.encode(), expected);
        assert_eq!(Definition::decode(&expected), Ok(definition));
    }
}

#[test]
fn writes_each_data_type_by_its_documented_bytes() {
    let list = sample::Type::List {
        element: Scalar::Uuid,
        max: 0x0102_0304,
    };
    for (data_type, expected) in [
        (
            DataType::Sample(sample::Type::Scalar(Scalar::Bool)),
            &[0, 0][..],
        ),
        (DataType::Sample(sample::Type::Scalar(Scalar::U32)), &[0, 7]),
        (DataType::Sample(list), &[2, 13, 4, 3, 2, 1]),
        (DataType::Sample(sample::Type::String), &[3]),
        (DataType::Sample(sample::Type::Bytes), &[4]),
        (DataType::Quality, &[5]),
    ] {
        let bytes = data(data_type, None).encode();
        // `data` has a quality key.
        let at = DATA_TYPE_AT + 16;
        let end = at + expected.len();
        assert_eq!(&bytes[at..end], expected);
        assert_eq!(&bytes[end..], &[0]);
    }
}

#[test]
fn refuses_an_unknown_channel_kind() {
    let error = Error::ChannelKind { at: 18, found: 2 };
    assert_eq!(
        error.to_string(),
        "the channel kind 2 at byte 18 is not 0 or 1"
    );
    assert_eq!(Definition::decode(&channel_bytes(2, &[])), Err(error));
}

#[test]
fn refuses_a_presence_flag_of_a_channel_that_is_not_0_or_1() {
    let mut control = vec![1];
    control.extend_from_slice(&8_u128.to_le_bytes());
    control.push(2);
    let mut quality = 9_u128.to_le_bytes().to_vec();
    quality.push(2);
    let mut unit = 9_u128.to_le_bytes().to_vec();
    unit.extend_from_slice(&[0, 0, 3, 2]);
    for (bytes, at) in [
        (channel_bytes(0, &[2, 0]), 19),
        (channel_bytes(0, &control), 36),
        (channel_bytes(1, &quality), 35),
        (channel_bytes(1, &unit), DATA_TYPE_AT + 2),
    ] {
        assert_eq!(
            Definition::decode(&bytes),
            Err(Error::Flag { at, found: 2 })
        );
    }
}

#[test]
fn refuses_an_unknown_data_type_or_scalar() {
    let at = DATA_TYPE_AT;
    let scalar_at = at + 1;
    for (rest, error, message) in [
        (
            &[6][..],
            Error::DataType { at, found: 6 },
            format!("the data type 6 at byte {at} is not a known type"),
        ),
        (
            &[0, 14],
            Error::Scalar {
                at: scalar_at,
                found: 14,
            },
            format!("the scalar 14 at byte {scalar_at} is not a known scalar"),
        ),
        (
            &[1, 255],
            Error::Scalar {
                at: scalar_at,
                found: 255,
            },
            format!("the scalar 255 at byte {scalar_at} is not a known scalar"),
        ),
        (
            &[2, 14],
            Error::Scalar {
                at: scalar_at,
                found: 14,
            },
            format!("the scalar 14 at byte {scalar_at} is not a known scalar"),
        ),
    ] {
        assert_eq!(error.to_string(), message);
        assert_eq!(Definition::decode(&data_bytes(rest)), Err(error));
    }
}

#[test]
fn refuses_a_unit_that_does_not_read() {
    let at = DATA_TYPE_AT + 3;
    for (text, error) in [
        (&b""[..], unit::Error::Empty),
        (&[b'a'; 33], unit::Error::Long { len: 33 }),
        (b"k Pa", unit::Error::Character { at: 1, found: ' ' }),
    ] {
        let mut rest = vec![0, 10, 1];
        rest.extend_from_slice(&u64::try_from(text.len()).unwrap().to_le_bytes());
        rest.extend_from_slice(text);
        let error = Error::Unit { at, error };
        assert_eq!(Definition::decode(&data_bytes(&rest)), Err(error));
    }
    let error = Error::Unit {
        at,
        error: unit::Error::Empty,
    };
    assert_eq!(
        error.to_string(),
        format!("the unit at byte {at} does not read: a unit is empty")
    );
}

#[test]
fn refuses_a_data_channel_that_cannot_exist() {
    let mut string = vec![3, 1];
    string.extend_from_slice(&1_u64.to_le_bytes());
    string.push(b'V');
    let at = DATA_TYPE_AT;
    for (rest, error) in [
        (
            string,
            channel::Error::Unit {
                data_type: DataType::Sample(sample::Type::String),
            },
        ),
        (
            vec![1, 10, 0, 0, 0, 0, 0],
            channel::Error::Empty { data_type: f64s(0) },
        ),
        (
            vec![2, 10, 0, 0, 0, 0, 0],
            channel::Error::Empty {
                data_type: DataType::Sample(sample::Type::List {
                    element: Scalar::F64,
                    max: 0,
                }),
            },
        ),
    ] {
        let error = Error::Channel { at, error };
        assert_eq!(Definition::decode(&data_bytes(&rest)), Err(error));
    }
    let error = Error::Channel {
        at,
        error: channel::Error::Empty { data_type: f64s(0) },
    };
    assert_eq!(
        error.to_string(),
        format!("the data channel at byte {at}: an array or a list holds no element")
    );
}

#[test]
fn refuses_a_channel_that_ends_early() {
    let bytes = data(f64s(3), Some("kPa")).encode();
    assert_eq!(bytes.len(), 70);
    for (end, at) in [
        (2, 2),
        (17, 2),
        (18, 18),
        (19, 19),
        (34, 19),
        (35, 35),
        (36, 36),
        (51, 36),
        (52, 52),
        (53, 53),
        (54, 54),
        (57, 54),
        (58, 58),
        (59, 59),
        (66, 59),
        (67, 59),
        (69, 59),
    ] {
        assert_eq!(
            Definition::decode(&bytes[..end]),
            Err(Error::Truncated { at }),
            "{end}"
        );
    }
}

fn pattern() -> impl Strategy<Value = String> {
    let segment = prop_oneof![
        Just("*".to_owned()),
        Just("**".to_owned()),
        "[a-c]{1,2}",
        "@[a-c]",
    ];
    (any::<bool>(), prop::collection::vec(segment, 1..4)).prop_map(|(excluded, s)| {
        let body = s.join(".");
        if excluded { format!("!{body}") } else { body }
    })
}

fn selectors() -> impl Strategy<Value = Selector> {
    ("[a-c]{1,3}", prop::collection::vec(pattern(), 0..4)).prop_map(|(first, rest)| {
        Selector::new(std::iter::once(first.as_str()).chain(rest.iter().map(|s| &**s)))
            .unwrap()
    })
}

fn settings_strategy() -> impl Strategy<Value = Definition> {
    let budget = prop::option::of(prop_oneof![Just(0), Just(1), any::<u64>()]);
    (selectors(), budget.clone(), budget).prop_filter_map(
        "a policy refuses a zero budget or none",
        |(select, disk, pool)| {
            let size = |b: Option<u64>| b.map(byte::Size::from_bytes);
            let policy = node_settings::Policy::new(select, size(disk), size(pool));
            policy.ok().map(Definition::NodeSettings)
        },
    )
}

fn access_strategy() -> impl Strategy<Value = Definition> {
    (
        selectors(),
        selectors(),
        prop::sample::subsequence(Action::ALL.to_vec(), 0..=6),
        any::<u8>(),
    )
        .prop_map(|(subjects, select, allow, authority)| {
            let allow = allow.into_iter().collect();
            Definition::Access(Policy::new(
                subjects,
                select,
                allow,
                Authority(authority),
            ))
        })
}

fn name_strategy() -> impl Strategy<Value = Name> {
    "[a-c]{1,2}(\\.@?[a-c]{1,2}){0,2}".prop_map(|text| text.parse().unwrap())
}

fn connector_strategy() -> impl Strategy<Value = Definition> {
    let pairs = prop::collection::btree_map("[a-z]{1,4}", any::<i64>(), 0..4);
    (name_strategy(), name_strategy(), pairs).prop_map(|(kind, node, pairs)| {
        let pairs = pairs
            .iter()
            .map(|(k, &v)| (k.as_str(), i128::from(v)))
            .collect::<Vec<_>>();
        Definition::Connector(Connector::new(kind, node, config(&pairs)))
    })
}

fn region_strategy() -> impl Strategy<Value = Definition> {
    let voters = prop::collection::vec(name_strategy(), 1..5);
    (any::<u64>(), voters).prop_map(|(epoch, voters)| {
        Definition::Region(Delegation::new(epoch, voters).unwrap())
    })
}

fn compression_strategy() -> impl Strategy<Value = Definition> {
    let mode = prop_oneof![Just(Mode::Auto), Just(Mode::Raw), Just(Mode::Max)];
    (selectors(), mode).prop_map(|(select, mode)| {
        Definition::Compression(compression::Policy { select, mode })
    })
}

fn placement_strategy() -> impl Strategy<Value = Definition> {
    let copies = prop::collection::vec(name_strategy(), 0..4);
    (selectors(), prop::option::of(name_strategy()), copies).prop_filter_map(
        "a policy refuses no standby and no copy, or a standby that is a copy",
        |(select, standby, copies)| {
            let policy = placement::Policy::new(select, standby, copies);
            policy.ok().map(Definition::Placement)
        },
    )
}

fn time_strategy() -> impl Strategy<Value = Definition> {
    let peers = prop::option::of(prop::collection::vec(name_strategy(), 0..4))
        .prop_map(|peers| peers.map_or(time::Peers::Voters, time::Peers::Listed));
    (selectors(), peers)
        .prop_map(|(select, peers)| Definition::Time(time::Policy::new(select, peers)))
}

fn data_type_strategy() -> impl Strategy<Value = DataType> {
    let scalar = prop::sample::select(SCALARS.map(|(each, _)| each).to_vec());
    prop_oneof![
        scalar
            .clone()
            .prop_map(|element| DataType::Sample(sample::Type::Scalar(element))),
        (scalar.clone(), any::<u32>()).prop_map(|(element, len)| {
            DataType::Sample(sample::Type::Array { element, len })
        }),
        (scalar, any::<u32>()).prop_map(|(element, max)| {
            DataType::Sample(sample::Type::List { element, max })
        }),
        Just(DataType::Sample(sample::Type::String)),
        Just(DataType::Sample(sample::Type::Bytes)),
        Just(DataType::Quality),
    ]
}

fn channel_strategy() -> impl Strategy<Value = Definition> {
    let key = any::<u128>().prop_map(Key::from_u128);
    let optional = prop::option::of(key.clone());
    let index = (optional.clone(), optional.clone())
        .prop_map(|(error, control)| channel::Kind::Index { error, control });
    let unit = prop::option::of("[!-~]{1,32}".prop_map(|u| Unit::new(&u).unwrap()));
    let data = (key.clone(), optional, data_type_strategy(), unit).prop_filter_map(
        "a unit on a type that holds no number",
        |(index, quality, data_type, unit)| {
            let data = Data::new(index, quality, data_type, unit);
            data.ok().map(channel::Kind::Data)
        },
    );
    (key, prop_oneof![index, data])
        .prop_map(|(key, kind)| Definition::Channel(Channel { key, kind }))
}

fn definition() -> impl Strategy<Value = Definition> {
    prop_oneof![
        access_strategy(),
        connector_strategy(),
        region_strategy(),
        settings_strategy(),
        compression_strategy(),
        placement_strategy(),
        time_strategy(),
        channel_strategy(),
    ]
}

proptest! {
    #[test]
    fn decodes_each_encoding_to_its_definition(definition in definition()) {
        prop_assert_eq!(Definition::decode(&definition.encode()), Ok(definition));
    }

    #[test]
    fn encodes_each_decoded_byte_string_to_the_same_bytes(
        definition in definition(),
        flips in prop::collection::vec((any::<Index>(), any::<u8>()), 1..4),
    ) {
        let mut bytes = definition.encode();
        for (at, byte) in flips {
            let at = at.index(bytes.len());
            bytes[at] = byte;
        }
        if let Ok(decoded) = Definition::decode(&bytes) {
            prop_assert_eq!(decoded.encode(), bytes);
        }
    }
}
