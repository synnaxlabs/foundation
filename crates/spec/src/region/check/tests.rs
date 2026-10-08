use proptest::prelude::*;
use types::channel::Key;

use super::*;
use crate::channel::Edge;
use crate::region::common::{data, index, map, name, prefix, record, subject};

fn ungoverned(key: &str, region: &str) -> Problem {
    Problem::Ungoverned {
        name: name(key),
        region: prefix(region),
    }
}

#[test]
fn finds_no_problem_in_a_region_with_a_child() {
    let definitions = map(&[
        ("plant.app.@subject", subject()),
        ("plant.child.@region", record()),
        ("plant.time", index(1)),
        ("plant.pressure", data(2, 1)),
    ]);
    assert_eq!(check(&prefix("plant"), &definitions), []);
}

#[test]
fn finds_each_problem_in_tree_key_order() {
    let definitions = map(&[
        ("plant.@region", record()),
        ("plant.app.@access", subject()),
        ("plant.child.@region", record()),
        ("plant.child.grand.@region", record()),
        ("plant.child.x.@subject", subject()),
        ("plant.pressure", data(2, 9)),
        ("plant.@x.y", index(1)),
        ("site.x.@subject", subject()),
    ]);
    assert_eq!(
        check(&prefix("plant"), &definitions),
        [
            ungoverned("plant.@region", "plant"),
            Problem::Misplaced {
                name: name("plant.@x.y"),
                kind: Kind::Channel,
            },
            Problem::Misplaced {
                name: name("plant.app.@access"),
                kind: Kind::Subject,
            },
            ungoverned("plant.child.grand.@region", "plant"),
            ungoverned("plant.child.x.@subject", "plant"),
            Problem::Channel(channel::Problem::Dangling {
                from: name("plant.pressure"),
                edge: Edge::Index,
                to: Key::from_u128(9),
            }),
            ungoverned("site.x.@subject", "plant"),
        ]
    );
}

#[test]
fn finds_an_edge_to_a_channel_of_another_region_dangling() {
    let definitions = map(&[
        ("plant.child.@region", record()),
        ("plant.child.time", index(1)),
        ("plant.pressure", data(2, 1)),
    ]);
    let dangling = channel::Problem::Dangling {
        from: name("plant.pressure"),
        edge: Edge::Index,
        to: Key::from_u128(1),
    };
    assert_eq!(
        check(&prefix("plant"), &definitions),
        [
            ungoverned("plant.child.time", "plant"),
            Problem::Channel(dangling),
        ]
    );
}

#[test]
fn finds_two_channels_with_one_key() {
    let definitions = map(&[("plant.a", index(1)), ("plant.b", index(1))]);
    let duplicate = channel::Problem::Duplicate {
        first: name("plant.a"),
        second: name("plant.b"),
        key: Key::from_u128(1),
    };
    assert_eq!(
        check(&prefix("plant"), &definitions),
        [Problem::Channel(duplicate)]
    );
}

#[test]
fn the_root_region_governs_each_name_under_no_child() {
    let definitions = map(&[
        ("plant.@region", record()),
        ("plant.x.@subject", subject()),
        ("site.x.@subject", subject()),
    ]);
    assert_eq!(
        check(&Prefix::ROOT, &definitions),
        [ungoverned("plant.x.@subject", "")]
    );
}

#[test]
fn gives_each_problem_a_message_and_a_fix() {
    let dangling = channel::Problem::Dangling {
        from: name("plant.pressure"),
        edge: Edge::Index,
        to: Key::from_u128(1),
    };
    for (problem, message, fix) in [
        (
            ungoverned("site.x.@subject", "plant"),
            "the region `plant` does not govern `site.x.@subject`",
            "Apply the definition in the region that governs its name",
        ),
        (
            ungoverned("site.x.@subject", ""),
            "the root region does not govern `site.x.@subject`",
            "Apply the definition in the region that governs its name",
        ),
        (
            Problem::Misplaced {
                name: name("plant.app.@access"),
                kind: Kind::Subject,
            },
            "`plant.app.@access` is not a tree key of a definition of kind `subject`",
            "Put the definition at the key of its kind: `<label>.@<kind>`, or its own \
             name for a connector or a channel",
        ),
        (
            Problem::Channel(dangling.clone()),
            "the index channel of `plant.pressure` points at no channel",
            "Point it at a channel that exists",
        ),
    ] {
        assert_eq!(problem.to_string(), message);
        assert_eq!(problem.fix(), fix);
    }
    assert_eq!(Problem::Channel(dangling.clone()).fix(), dangling.fix());
}

proptest! {
    #[test]
    fn finds_no_problem_in_definitions_at_their_keys_under_the_prefix(
        labels in prop::collection::btree_set("[a-c]{1,2}(\\.[a-c]{1,2}){0,2}", 1..8),
    ) {
        let mut definitions = BTreeMap::new();
        for (at, label) in (1_u128..).zip(&labels) {
            let label = format!("plant.{label}");
            definitions.insert(Kind::Subject.key(&label).unwrap(), subject());
            definitions.insert(
                Kind::Channel.key(&format!("{label}.time")).unwrap(),
                index(at * 2),
            );
            definitions.insert(
                Kind::Channel.key(&format!("{label}.value")).unwrap(),
                data(at * 2 + 1, at * 2),
            );
        }
        prop_assert_eq!(check(&prefix("plant"), &definitions), []);
    }
}
