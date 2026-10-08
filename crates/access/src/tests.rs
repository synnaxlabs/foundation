use std::collections::BTreeMap;

use document::Document;
use document::encoding::Checked;
use proptest::prelude::*;
use spec::channel::{Channel, Kind as ChannelKind};
use spec::connector::Connector;
use spec::definition::Kind;
use spec::region::Delegation;
use types::channel;
use types::name::Selector;

use super::*;

fn name(s: &str) -> Name {
    s.parse().unwrap()
}

fn policy(subjects: &str, select: &str, allow: &[Action], authority: u8) -> Policy {
    Policy::new(
        Selector::new([subjects]).unwrap(),
        Selector::new([select]).unwrap(),
        allow.iter().copied().collect(),
        Authority(authority),
    )
}

fn connector() -> Definition {
    let config = Checked::new(Document::default()).unwrap();
    Definition::Connector(Connector::new(name("modbus"), name("gw_1"), config))
}

type Tree = BTreeMap<Name, Definition>;

/// One tree per region, with each policy under its region and each connector in the
/// root tree.
fn rules(policies: &[(&str, Policy)], connectors: &[&str]) -> Rules {
    let mut trees = BTreeMap::<&str, Tree>::new();
    for (i, (region, policy)) in policies.iter().enumerate() {
        let label = match *region {
            "" => format!("p{i}"),
            region => format!("{region}.p{i}"),
        };
        let key = Kind::Access.key(&label).unwrap();
        let tree = trees.entry(region).or_default();
        tree.insert(key, Definition::Access(policy.clone()));
    }
    let root = trees.entry("").or_default();
    for at in connectors {
        root.insert(name(at), connector());
    }
    Rules::new(trees.iter().map(|(r, tree)| (r.parse().unwrap(), tree)))
}

fn grant(rules: &Rules, subject: &str, on: &str) -> Grant {
    rules.grant(&name(subject), &name(on))
}

#[test]
fn denies_every_action_with_no_policy() {
    let rules = rules(&[], &[]);
    let grant = grant(&rules, "ops.ana", "site_a.pt_1");
    assert_eq!(grant.actions(), Actions::NONE);
    assert_eq!(grant.authority(), None);
}

#[test]
fn allows_the_union_of_the_matching_policies() {
    let rules = rules(
        &[
            ("", policy("ops.*", "site_a.**", &[Action::Read], 0)),
            ("", policy("ops.ana", "site_a.**", &[Action::Plan], 0)),
        ],
        &[],
    );
    let actions = grant(&rules, "ops.ana", "site_a.pt_1").actions();
    assert_eq!(actions, [Action::Read, Action::Plan].into_iter().collect());
    let actions = grant(&rules, "ops.ben", "site_a.pt_1").actions();
    assert_eq!(actions, [Action::Read].into_iter().collect());
}

#[test]
fn gives_nothing_from_a_policy_that_does_not_match() {
    let rules = rules(
        &[("", policy("ops.*", "site_a.**", &[Action::Read], 0))],
        &[],
    );
    assert_eq!(
        grant(&rules, "eng.cy", "site_a.pt_1").actions(),
        Actions::NONE
    );
    assert_eq!(
        grant(&rules, "ops.ana", "site_b.pt_1").actions(),
        Actions::NONE
    );
}

#[test]
fn caps_authority_at_the_highest_write_allow() {
    let rules = rules(
        &[
            ("", policy("ops.*", "site_a.**", &[Action::Write], 3)),
            ("", policy("ops.ana", "site_a.**", &[Action::Write], 9)),
            ("", policy("ops.*", "site_a.**", &[Action::Read], 200)),
        ],
        &[],
    );
    assert_eq!(
        grant(&rules, "ops.ana", "site_a.pt_1").authority(),
        Some(Authority(9))
    );
    assert_eq!(
        grant(&rules, "ops.ben", "site_a.pt_1").authority(),
        Some(Authority(3))
    );
}

#[test]
fn gives_no_authority_without_write() {
    let rules = rules(
        &[("", policy("ops.*", "site_a.**", &[Action::Read], 200))],
        &[],
    );
    assert_eq!(grant(&rules, "ops.ana", "site_a.pt_1").authority(), None);
}

#[test]
fn lets_a_connector_write_under_its_own_name() {
    let rules = rules(&[], &["site_a.daq"]);
    let write = [Action::Write].into_iter().collect();
    let under = grant(&rules, "site_a.daq", "site_a.daq.ai_0");
    assert_eq!(under.actions(), write);
    assert_eq!(under.authority(), Some(Authority::ABSOLUTE));
    for on in ["site_a.daq", "site_a.other", "site_a.daq_2.ai_0", "site_a"] {
        assert_eq!(
            grant(&rules, "site_a.daq", on).actions(),
            Actions::NONE,
            "{on}"
        );
    }
    let person = grant(&rules, "ops.ana", "site_a.daq.ai_0");
    assert_eq!(person.actions(), Actions::NONE);
}

#[test]
fn gives_no_default_write_to_a_subject_that_is_not_a_connector() {
    let rules = rules(&[], &["site_a.daq"]);
    let under = grant(&rules, "ops.ana", "ops.ana.pt_1");
    assert_eq!(under.actions(), Actions::NONE);
    assert_eq!(under.authority(), None);
}

#[test]
fn reaches_only_the_region_of_the_policy_and_below() {
    let read = policy("ops.*", "**", &[Action::Read], 0);
    let in_site_a = rules(&[("site_a", read.clone())], &[]);
    let at_root = rules(&[("", read)], &[]);
    let readable = [Action::Read].into_iter().collect();
    for (on, actions) in [
        ("site_a", readable),
        ("site_a.pt_1", readable),
        ("site_a.cell.pt_1", readable),
        ("site_b.pt_1", Actions::NONE),
        ("site_ab.pt_1", Actions::NONE),
        ("pt_1", Actions::NONE),
    ] {
        assert_eq!(grant(&in_site_a, "ops.ana", on).actions(), actions, "{on}");
    }
    assert_eq!(
        grant(&at_root, "ops.ana", "site_b.pt_1").actions(),
        readable
    );
}

#[test]
fn matches_subjects_in_any_region() {
    let rules = rules(
        &[("site_a", policy("site_b.**", "**", &[Action::Read], 0))],
        &[],
    );
    let actions = grant(&rules, "site_b.bot", "site_a.pt_1").actions();
    assert_eq!(actions, [Action::Read].into_iter().collect());
}

#[test]
fn takes_the_connectors_of_each_region_tree() {
    let root = Tree::from([(name("gw.daq"), connector())]);
    let site_a = Tree::from([(name("site_a.daq"), connector())]);
    let rules = Rules::new([
        (Prefix::ROOT, &root),
        (name("site_a").into(), &site_a),
        (name("site_b").into(), &Tree::new()),
    ]);
    let write = [Action::Write].into_iter().collect();
    for (subject, on) in [("gw.daq", "gw.daq.ai_0"), ("site_a.daq", "site_a.daq.ai_0")]
    {
        let under = grant(&rules, subject, on);
        assert_eq!(under.actions(), write, "{on}");
        assert_eq!(under.authority(), Some(Authority::ABSOLUTE), "{on}");
    }
}

#[test]
fn gives_no_grant_from_a_channel_or_a_region_record() {
    let index = ChannelKind::Index {
        error: None,
        control: None,
    };
    let channel = Channel {
        key: channel::Key::from_u128(1),
        kind: index,
    };
    let region = Delegation::new(1, [name("node_1")]).unwrap();
    let region_key = Kind::Region.key("site_a").unwrap();
    let tree = Tree::from([
        (name("site_a.daq"), Definition::Channel(channel)),
        (region_key.clone(), Definition::Region(region)),
    ]);
    let rules = Rules::new([(Prefix::ROOT, &tree)]);
    for subject in [name("site_a.daq"), region_key] {
        let on = format!("{subject}.pt_1");
        let under = grant(&rules, subject.as_str(), &on);
        assert_eq!(under.actions(), Actions::NONE, "{subject}");
    }
}

#[test]
fn keeps_each_policy_with_the_region_of_its_tree() {
    let root = Tree::from([(
        Kind::Access.key("ops").unwrap(),
        Definition::Access(policy("ops.*", "**", &[Action::Plan], 0)),
    )]);
    let site_a = Tree::from([(
        Kind::Access.key("site_a.ops").unwrap(),
        Definition::Access(policy("ops.*", "**", &[Action::Read], 0)),
    )]);
    let rules = Rules::new([(Prefix::ROOT, &root), (name("site_a").into(), &site_a)]);
    let both = [Action::Read, Action::Plan].into_iter().collect();
    let plan = [Action::Plan].into_iter().collect();
    assert_eq!(grant(&rules, "ops.ana", "site_a.pt_1").actions(), both);
    assert_eq!(grant(&rules, "ops.ana", "site_b.pt_1").actions(), plan);
}

fn arbitrary_policy() -> impl Strategy<Value = (&'static str, Policy)> {
    let regions = prop::sample::select(vec!["", "a", "a.b", "b"]);
    let selects = prop::sample::select(vec!["**", "a.**", "b.*", "a.b.**", "*.x"]);
    let subjects = prop::sample::select(vec!["**", "s.*", "s.x", "t.*"]);
    let allow = prop::sample::subsequence(spec_actions(), 0..=6);
    (regions, subjects, selects, allow, any::<u8>())
        .prop_map(|(r, s, sel, a, auth)| (r, policy(s, sel, &a, auth)))
}

fn spec_actions() -> Vec<Action> {
    vec![
        Action::Read,
        Action::Write,
        Action::Plan,
        Action::Apply,
        Action::Secret,
        Action::Admin,
    ]
}

type Placed = Vec<(&'static str, Policy)>;

fn policies_and_shuffle() -> impl Strategy<Value = (Placed, Placed)> {
    prop::collection::vec(arbitrary_policy(), 0..8)
        .prop_flat_map(|p| (Just(p.clone()), Just(p).prop_shuffle()))
}

proptest! {
    #[test]
    fn decides_the_same_for_any_order_of_policies(
        (policies, shuffled) in policies_and_shuffle(),
    ) {
        let (one, two) = (rules(&policies, &["s.x"]), rules(&shuffled, &["s.x"]));
        for subject in ["s.x", "s.y", "t.z"] {
            for on in ["a.x", "a.b.x", "b.x", "s.x.y", "c.x"] {
                prop_assert_eq!(grant(&one, subject, on), grant(&two, subject, on));
            }
        }
    }
}
