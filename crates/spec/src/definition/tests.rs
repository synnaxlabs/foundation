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
    for tag in (0..=u8::MAX).filter(|t| ![ACCESS, CONNECTOR, REGION].contains(t)) {
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
    let error = Error::Excluded { at: 10, found: 2 };
    assert_eq!(Definition::decode(&bytes), Err(error.clone()));
    assert_eq!(
        error.to_string(),
        "the exclusion flag 2 at byte 10 is not 0 or 1"
    );
}

#[test]
fn refuses_an_included_pattern_that_starts_with_a_bang() {
    let mut bytes = vec![VERSION, ACCESS];
    bytes.extend_from_slice(&1_u64.to_le_bytes());
    bytes.push(0);
    bytes.extend_from_slice(&2_u64.to_le_bytes());
    bytes.extend_from_slice(b"!a");
    let error = Error::Include { at: 19 };
    assert_eq!(Definition::decode(&bytes), Err(error.clone()));
    assert_eq!(
        error.to_string(),
        "the included pattern at byte 19 starts with `!`"
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
fn config(pairs: &[(&str, i128)]) -> Document {
    let attributes = pairs.iter().map(|&(key, n)| Attribute {
        key: key.into(),
        key_span: None,
        value: Value {
            kind: Kind::Integer(n),
            span: None,
        },
    });
    Document {
        attributes: Map::new(attributes.collect()).unwrap(),
        blocks: Vec::new(),
    }
}

fn connector() -> Definition {
    let config = config(&[("port", 502)]);
    Definition::Connector(Connector::new(name("modbus"), name("gw_1"), config).unwrap())
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
    let config = encoding::encode(&config(&[("port", 502)])).unwrap();
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
    let config = encoding::encode(&Document::default()).unwrap();
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
        r#"the name at byte 16 does not read: "gw 1" has a segment that is not valid: "gw 1""#
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
    let mut config = encoding::encode(&Document::default()).unwrap();
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
        "the voter at byte 29 is not after the voter before it"
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

fn policy_strategy() -> impl Strategy<Value = Definition> {
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
        Definition::Connector(Connector::new(kind, node, config(&pairs)).unwrap())
    })
}

fn region_strategy() -> impl Strategy<Value = Definition> {
    let voters = prop::collection::vec(name_strategy(), 1..5);
    (any::<u64>(), voters).prop_map(|(epoch, voters)| {
        Definition::Region(Delegation::new(epoch, voters).unwrap())
    })
}

fn definition() -> impl Strategy<Value = Definition> {
    prop_oneof![policy_strategy(), connector_strategy(), region_strategy()]
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
