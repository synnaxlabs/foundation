use proptest::prelude::*;
use spec::patterns::Patterns;

use super::*;

fn name(s: &str) -> Name {
    s.parse().unwrap()
}

fn policy(subjects: &str, select: &str, allow: &[Action], authority: u8) -> Policy {
    Policy::new(
        Patterns::new([subjects]).unwrap(),
        Patterns::new([select]).unwrap(),
        allow.iter().copied().collect(),
        Authority(authority),
    )
}

fn rules(policies: &[(&str, Policy)], regions: &[&str], connectors: &[&str]) -> Rules {
    Rules::new(
        policies.iter().map(|(n, p)| (name(n), p.clone())),
        regions.iter().copied().map(name),
        connectors.iter().copied().map(name),
    )
}

fn grant(rules: &Rules, subject: &str, on: &str) -> Grant {
    rules.grant(&name(subject), &name(on))
}

#[test]
fn denies_every_action_with_no_policy() {
    let rules = rules(&[], &[], &[]);
    let grant = grant(&rules, "ops.ana", "site_a.pt_1");
    assert_eq!(grant.actions(), Actions::NONE);
    assert_eq!(grant.authority(), None);
}

#[test]
fn allows_the_union_of_the_matching_policies() {
    let rules = rules(
        &[
            ("readers", policy("ops.*", "site_a.**", &[Action::Read], 0)),
            (
                "planners",
                policy("ops.ana", "site_a.**", &[Action::Plan], 0),
            ),
        ],
        &[],
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
        &[("readers", policy("ops.*", "site_a.**", &[Action::Read], 0))],
        &[],
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
            ("low", policy("ops.*", "site_a.**", &[Action::Write], 3)),
            ("high", policy("ops.ana", "site_a.**", &[Action::Write], 9)),
            ("read", policy("ops.*", "site_a.**", &[Action::Read], 200)),
        ],
        &[],
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
        &[("read", policy("ops.*", "site_a.**", &[Action::Read], 200))],
        &[],
        &[],
    );
    assert_eq!(grant(&rules, "ops.ana", "site_a.pt_1").authority(), None);
}

#[test]
fn lets_a_connector_write_under_its_own_name() {
    let rules = rules(&[], &[], &["site_a.daq"]);
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
fn reaches_only_the_region_of_the_policy_and_below() {
    let read = policy("ops.*", "**", &[Action::Read], 0);
    let in_site_a = rules(
        &[("site_a.readers", read.clone())],
        &["site_a", "site_b"],
        &[],
    );
    let at_root = rules(&[("readers", read)], &["site_a", "site_b"], &[]);
    let readable = [Action::Read].into_iter().collect();
    assert_eq!(
        grant(&in_site_a, "ops.ana", "site_a.cell.pt_1").actions(),
        readable
    );
    assert_eq!(
        grant(&in_site_a, "ops.ana", "site_b.pt_1").actions(),
        Actions::NONE
    );
    assert_eq!(
        grant(&in_site_a, "ops.ana", "pt_1").actions(),
        Actions::NONE
    );
    assert_eq!(
        grant(&at_root, "ops.ana", "site_b.pt_1").actions(),
        readable
    );
}

#[test]
fn places_a_policy_in_the_longest_region_that_holds_it() {
    let rules = rules(
        &[(
            "site_a.cell.readers",
            policy("*.*", "**", &[Action::Read], 0),
        )],
        &["site_a", "site_a.cell"],
        &[],
    );
    let in_cell = grant(&rules, "ops.ana", "site_a.cell.pt_1").actions();
    assert_eq!(in_cell, [Action::Read].into_iter().collect());
    let outside = grant(&rules, "ops.ana", "site_a.pt_1").actions();
    assert_eq!(outside, Actions::NONE);
}

#[test]
fn matches_subjects_in_any_region() {
    let rules = rules(
        &[(
            "site_a.readers",
            policy("site_b.**", "**", &[Action::Read], 0),
        )],
        &["site_a", "site_b"],
        &[],
    );
    let actions = grant(&rules, "site_b.bot", "site_a.pt_1").actions();
    assert_eq!(actions, [Action::Read].into_iter().collect());
}

fn arbitrary_policy() -> impl Strategy<Value = (Name, Policy)> {
    let names = prop::sample::select(vec!["a", "a.p", "b.p", "p", "a.b.p"]);
    let selects = prop::sample::select(vec!["**", "a.**", "b.*", "a.b.**", "*.x"]);
    let subjects = prop::sample::select(vec!["**", "s.*", "s.x", "t.*"]);
    let allow = prop::sample::subsequence(spec_actions(), 0..=6);
    (names, subjects, selects, allow, any::<u8>())
        .prop_map(|(n, s, sel, a, auth)| (name(n), policy(s, sel, &a, auth)))
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

type Named = Vec<(Name, Policy)>;

fn policies_and_shuffle() -> impl Strategy<Value = (Named, Named)> {
    prop::collection::vec(arbitrary_policy(), 0..8)
        .prop_flat_map(|p| (Just(p.clone()), Just(p).prop_shuffle()))
}

proptest! {
    #[test]
    fn decides_the_same_for_any_order_of_policies(
        (policies, shuffled) in policies_and_shuffle(),
    ) {
        let build = |p: Named| {
            Rules::new(p, [name("a"), name("a.b")], [name("s.x")])
        };
        let (one, two) = (build(policies), build(shuffled));
        for subject in ["s.x", "s.y", "t.z"] {
            for on in ["a.x", "a.b.x", "b.x", "s.x.y", "c.x"] {
                prop_assert_eq!(grant(&one, subject, on), grant(&two, subject, on));
            }
        }
    }
}
