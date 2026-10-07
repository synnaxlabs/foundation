use proptest::prelude::*;
use types::sample::{self, Scalar};

use super::*;
use crate::channel::Data;

fn name(text: &str) -> Name {
    text.parse().unwrap()
}

fn key(n: u128) -> channel::Key {
    channel::Key::from_u128(n)
}

fn index(n: u128, error: Option<u128>, control: Option<u128>) -> Channel {
    Channel {
        key: key(n),
        kind: Kind::Index {
            error: error.map(key),
            control: control.map(key),
        },
    }
}

fn data(n: u128, index: u128, quality: Option<u128>, data_type: DataType) -> Channel {
    let data = Data::new(key(index), quality.map(key), data_type, None).unwrap();
    Channel {
        key: key(n),
        kind: Kind::Data(data),
    }
}

fn f64() -> DataType {
    DataType::Sample(sample::Type::Scalar(Scalar::F64))
}

fn check_all(channels: &[(&str, Channel)]) -> Vec<Problem> {
    let names: Vec<_> = channels.iter().map(|(text, _)| name(text)).collect();
    check(
        names
            .iter()
            .zip(channels.iter().map(|(_, channel)| channel)),
    )
}

#[test]
fn accepts_channels_with_correct_edges() {
    let channels = [
        ("a.time", index(1, Some(2), Some(3))),
        ("a.error", data(2, 1, None, f64())),
        ("a.control", data(3, 1, None, f64())),
        ("a.quality", data(4, 1, None, DataType::Quality)),
        ("a.pressure", data(5, 1, Some(4), f64())),
    ];
    assert_eq!(check_all(&channels), []);
}

#[test]
fn refuses_an_edge_to_the_wrong_kind() {
    let channels = [
        ("a.time", index(1, Some(6), Some(6))),
        ("a.other", index(6, None, None)),
        ("a.pressure", data(2, 3, Some(3), f64())),
        ("a.temperature", data(3, 1, Some(6), f64())),
    ];
    let wrong = |from: &str, edge, to: &str| Problem::Wrong {
        from: name(from),
        edge,
        to: name(to),
    };
    assert_eq!(
        check_all(&channels),
        [
            wrong("a.pressure", Edge::Index, "a.temperature"),
            wrong("a.pressure", Edge::Quality, "a.temperature"),
            wrong("a.temperature", Edge::Quality, "a.other"),
            wrong("a.time", Edge::Error, "a.other"),
            wrong("a.time", Edge::Control, "a.other"),
        ]
    );
}

#[test]
fn refuses_an_edge_to_a_key_that_no_channel_has() {
    let channels = [
        ("a.time", index(1, Some(7), Some(8))),
        ("a.pressure", data(2, 9, Some(10), f64())),
    ];
    let dangling = |from: &str, edge, to| Problem::Dangling {
        from: name(from),
        edge,
        to: key(to),
    };
    assert_eq!(
        check_all(&channels),
        [
            dangling("a.pressure", Edge::Index, 9),
            dangling("a.pressure", Edge::Quality, 10),
            dangling("a.time", Edge::Error, 7),
            dangling("a.time", Edge::Control, 8),
        ]
    );
}

#[test]
fn refuses_two_channels_with_one_key() {
    let channels = [
        ("b.time", index(1, None, None)),
        ("a.time", index(1, None, None)),
        ("c.time", index(1, None, None)),
    ];
    let shared = |second: &str| Problem::Shared {
        key: key(1),
        first: name("a.time"),
        second: name(second),
    };
    assert_eq!(check_all(&channels), [shared("b.time"), shared("c.time")]);
}

#[test]
fn refuses_one_name_given_twice_with_one_key() {
    let channels = [
        ("a.time", index(1, None, None)),
        ("a.time", index(1, None, None)),
    ];
    let shared = Problem::Shared {
        key: key(1),
        first: name("a.time"),
        second: name("a.time"),
    };
    assert_eq!(check_all(&channels), [shared]);
}

#[test]
fn gives_only_the_shared_key_for_an_edge_to_it() {
    let channels = [
        ("c.time", index(1, None, None)),
        ("a.x", data(1, 1, None, f64())),
        ("b.p", data(2, 1, None, f64())),
    ];
    let shared = Problem::Shared {
        key: key(1),
        first: name("a.x"),
        second: name("c.time"),
    };
    assert_eq!(check_all(&channels), [shared]);
}

#[test]
fn checks_the_edges_of_each_channel_with_a_shared_key() {
    let channels = [
        ("a.time", index(1, None, None)),
        ("b.x", data(1, 99, None, f64())),
    ];
    let shared = Problem::Shared {
        key: key(1),
        first: name("a.time"),
        second: name("b.x"),
    };
    let dangling = Problem::Dangling {
        from: name("b.x"),
        edge: Edge::Index,
        to: key(99),
    };
    assert_eq!(check_all(&channels), [shared, dangling]);
}

#[test]
fn finds_no_problem_in_no_channels() {
    assert_eq!(check_all(&[]), []);
}

#[test]
fn gives_each_problem_a_message_and_a_fix() {
    let from = name("a.pressure");
    let to = name("a.time");
    let wrong = |edge| Problem::Wrong {
        from: from.clone(),
        edge,
        to: to.clone(),
    };
    for (problem, message, fix) in [
        (
            Problem::Dangling {
                from: from.clone(),
                edge: Edge::Index,
                to: key(1),
            },
            "the index channel of `a.pressure` is 00000000-0000-0000-0000-000000000001, \
             which no channel has",
            "Point it at a channel that exists",
        ),
        (
            wrong(Edge::Index),
            "the index channel of `a.pressure` is `a.time`, which is not an index channel",
            "Point it at an index channel",
        ),
        (
            wrong(Edge::Quality),
            "the quality channel of `a.pressure` is `a.time`, which is not a data \
             channel of type quality",
            "Point it at a data channel of type quality",
        ),
        (
            wrong(Edge::Error),
            "the error channel of `a.pressure` is `a.time`, which is not a data channel",
            "Point it at a data channel",
        ),
        (
            wrong(Edge::Control),
            "the control channel of `a.pressure` is `a.time`, which is not a data \
             channel",
            "Point it at a data channel",
        ),
        (
            Problem::Shared {
                key: key(1),
                first: to.clone(),
                second: from.clone(),
            },
            "`a.time` and `a.pressure` have the same key \
             00000000-0000-0000-0000-000000000001",
            "Give each channel its own key",
        ),
    ] {
        assert_eq!(problem.to_string(), message);
        assert_eq!(problem.fix(), fix);
    }
}

/// An optional edge to one of the `len` channels from key `from`.
fn edge(from: u128, len: u128) -> BoxedStrategy<Option<u128>> {
    if len == 0 {
        Just(None).boxed()
    } else {
        prop::option::of(from..from + len).boxed()
    }
}

/// A set of channels with correct edges: indexes, then quality channels, then other
/// data channels, each edge drawn from the channels of the kind it needs.
fn correct() -> impl Strategy<Value = Vec<Channel>> {
    (1_u128..4, 0_u128..3, 1_u128..5)
        .prop_flat_map(|(indexes, qualities, others)| {
            let data = indexes + qualities + others;
            (
                prop::collection::vec(
                    (edge(indexes, data - indexes), edge(indexes, data - indexes)),
                    usize::try_from(indexes).unwrap(),
                ),
                prop::collection::vec(
                    (0..indexes, edge(indexes, qualities)),
                    usize::try_from(data - indexes).unwrap(),
                ),
                Just((indexes, qualities)),
            )
        })
        .prop_map(|(index_edges, data_edges, (indexes, qualities))| {
            let mut channels: Vec<_> = (0..)
                .zip(index_edges)
                .map(|(n, (error, control))| index(n, error, control))
                .collect();
            for (n, (to, quality)) in (indexes..).zip(data_edges) {
                if n < indexes + qualities {
                    channels.push(data(n, to, None, DataType::Quality));
                } else {
                    channels.push(data(n, to, quality, f64()));
                }
            }
            channels
        })
        .prop_shuffle()
}

proptest! {
    #[test]
    fn finds_no_problem_in_correct_edges(channels in correct()) {
        let names: Vec<_> = (0..channels.len()).map(|n| name(&format!("c{n}"))).collect();
        prop_assert_eq!(check(names.iter().zip(&channels)), []);
    }
}
