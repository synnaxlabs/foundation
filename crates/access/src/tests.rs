use proptest::prelude::*;
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

fn rules(policies: &[(Option<&str>, Policy)], connectors: &[&str]) -> Rules {
    Rules::new(
        policies.iter().map(|(r, p)| (r.map(name), p.clone())),
        connectors.iter().copied().map(name),
    )
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
            (None, policy("ops.*", "site_a.**", &[Action::Read], 0)),
            (None, policy("ops.ana", "site_a.**", &[Action::Plan], 0)),
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
        &[(None, policy("ops.*", "site_a.**", &[Action::Read], 0))],
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
            (None, policy("ops.*", "site_a.**", &[Action::Write], 3)),
            (None, policy("ops.ana", "site_a.**", &[Action::Write], 9)),
            (None, policy("ops.*", "site_a.**", &[Action::Read], 200)),
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
        &[(None, policy("ops.*", "site_a.**", &[Action::Read], 200))],
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
    let in_site_a = rules(&[(Some("site_a"), read.clone())], &[]);
    let at_root = rules(&[(None, read)], &[]);
    let readable = [Action::Read].into_iter().collect();
    for (on, actions) in [
        ("site_a", readable),
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
        &[(
            Some("site_a"),
            policy("site_b.**", "**", &[Action::Read], 0),
        )],
        &[],
    );
    let actions = grant(&rules, "site_b.bot", "site_a.pt_1").actions();
    assert_eq!(actions, [Action::Read].into_iter().collect());
}

fn arbitrary_policy() -> impl Strategy<Value = (Option<Name>, Policy)> {
    let regions = prop::sample::select(vec![None, Some("a"), Some("a.b"), Some("b")]);
    let selects = prop::sample::select(vec!["**", "a.**", "b.*", "a.b.**", "*.x"]);
    let subjects = prop::sample::select(vec!["**", "s.*", "s.x", "t.*"]);
    let allow = prop::sample::subsequence(spec_actions(), 0..=6);
    (regions, subjects, selects, allow, any::<u8>())
        .prop_map(|(r, s, sel, a, auth)| (r.map(name), policy(s, sel, &a, auth)))
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

type Placed = Vec<(Option<Name>, Policy)>;

fn policies_and_shuffle() -> impl Strategy<Value = (Placed, Placed)> {
    prop::collection::vec(arbitrary_policy(), 0..8)
        .prop_flat_map(|p| (Just(p.clone()), Just(p).prop_shuffle()))
}

proptest! {
    #[test]
    fn decides_the_same_for_any_order_of_policies(
        (policies, shuffled) in policies_and_shuffle(),
    ) {
        let build = |p: Placed| {
            Rules::new(p, [name("s.x")])
        };
        let (one, two) = (build(policies), build(shuffled));
        for subject in ["s.x", "s.y", "t.z"] {
            for on in ["a.x", "a.b.x", "b.x", "s.x.y", "c.x"] {
                prop_assert_eq!(grant(&one, subject, on), grant(&two, subject, on));
            }
        }
    }
}
