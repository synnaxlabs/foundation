use std::collections::BTreeMap;

use config::Definition;
use config::plan::{Error, Plan};
use proptest::prelude::*;
use spec::access::{self, Action};
use spec::channel::{Edge, Problem};
use spec::definition::{Definition as Stored, Kind};
use spec::region;
use spec::time::{self, Peers};
use types::authority::Authority;
use types::channel::Key;
use types::digest::Digest;
use types::ed25519::PrivateKey;
use types::name::{Prefix, Selector};

use super::{EDGE, INFLUX, PLANT, Spec, name};

/// Channels with each edge and a unit, placed on `n`.
const EACH: &str = "\
channel \"b.time\" {
  kind = \"index\"
  error = \"b.error\"
  control = \"c.control\"
}
channel \"b.error\" {
  data_type = \"u64\"
  index = \"b.time\"
}
channel \"b.quality\" {
  data_type = \"quality\"
  index = \"b.time\"
}
channel \"b.value\" {
  data_type = \"f64\"
  index = \"b.time\"
  quality = \"b.quality\"
  unit = \"m/s\"
}
channel \"c.time\" {
  kind = \"index\"
}
channel \"c.control\" {
  data_type = \"string\"
  index = \"c.time\"
}
placement \"b\" {
  select = \"b.*\"
  home = \"n\"
}
placement \"c\" {
  select = \"c.*\"
  home = \"n\"
}
";

/// The members of each plan here.
const MEMBERS: [&str; 3] = ["cloud", "edge", "n"];

/// `plan` with each span cleared, as [`Plan::decode`] gives it.
fn spanless(mut plan: Plan) -> Plan {
    for entry in plan
        .changes
        .values_mut()
        .filter_map(|change| change.new.as_mut())
    {
        entry.label_span = None;
    }
    plan
}

/// The plans of the fixtures: each addition to the empty spec, and then a change, a
/// removal, and an addition after that apply.
fn plans() -> [Plan; 2] {
    let mut spec = Spec::create_empty();
    let added = spec.plan(&[EDGE, INFLUX, EACH], &MEMBERS);
    let added = added.expect("no problems");
    spec.apply(&added);
    let edge = EDGE.replace("\"edge.time\"", "\"edge.clock\"");
    let changed = spec.plan(&[&edge, PLANT], &MEMBERS).expect("no problems");
    [added, changed]
}

/// The key rule of the test spec's apply: a new v7 key after `made`.
fn keys(made: u128) -> impl FnMut() -> Key {
    let mut made = made;
    move || {
        made += 1;
        Key::from_u128((7 << 76) | made)
    }
}

#[test]
fn decodes_each_kind_of_change_that_it_encodes() {
    for plan in plans() {
        let decoded = Plan::decode(&plan.encode());
        assert_eq!(decoded, Ok(spanless(plan)));
    }
}

#[test]
fn gives_the_definitions_that_an_apply_of_the_plan_stores() {
    let mut spec = Spec::create_empty();
    // The last step changes `edge.value`, whose index stays.
    let edge = EDGE.replace("\"f64\"", "\"f32\"");
    for texts in [&[EDGE, INFLUX, EACH][..], &[EDGE, PLANT], &[&edge, PLANT]] {
        let plan = spec.plan(texts, &MEMBERS).expect("no problems");
        let found = plan
            .definitions(&spec.definitions(), keys(spec.made))
            .expect("a plan of the applied spec");
        spec.apply(&plan);
        assert_eq!(found, spec.definitions());
    }
}

#[test]
fn gives_a_dangling_edge_a_key_that_the_region_check_refuses() {
    let spec = Spec::create_empty();
    let mut plan = spec.plan(&[PLANT], &["n"]).expect("no problems");
    plan.changes.remove(&name("a.time"));
    let definitions = plan
        .definitions(&spec.definitions(), keys(0))
        .expect("a plan of the applied spec");
    let problems = region::check(&Prefix::ROOT, &definitions);
    let dangling = Problem::Dangling {
        from: name("a.value"),
        edge: Edge::Index,
        to: Key::from_u128((7 << 76) | 2),
    };
    assert_eq!(problems, [region::Problem::Channel(dangling)]);
}

#[test]
fn gives_an_edge_to_a_channel_that_the_plan_removes_a_new_key() {
    let mut spec = Spec::create_empty();
    spec.apply(&spec.plan(&[PLANT], &["n"]).expect("no problems"));
    let plant = PLANT.replace("\"f64\"", "\"f32\"");
    let mut plan = spec.plan(&[&plant], &["n"]).expect("no problems");
    let removed = spec.plan(&[], &["n"]).expect("no problems");
    let time = name("a.time");
    let removal = removed.changes[&time].clone();
    plan.changes.insert(time, removal);
    let definitions = plan
        .definitions(&spec.definitions(), keys(spec.made))
        .expect("a plan of the applied spec");
    let problems = region::check(&Prefix::ROOT, &definitions);
    let dangling = Problem::Dangling {
        from: name("a.value"),
        edge: Edge::Index,
        to: Key::from_u128((7 << 76) | (spec.made + 1)),
    };
    assert_eq!(problems, [region::Problem::Channel(dangling)]);
}

#[test]
fn refuses_a_change_that_states_another_stored_definition() {
    let mut spec = Spec::create_empty();
    spec.apply(&spec.plan(&[PLANT], &["n"]).expect("no problems"));
    let plant = PLANT.replace("\"f64\"", "\"f32\"");
    let plan = spec.plan(&[&plant], &["n"]).expect("no problems");
    let value = name("a.value");
    let mismatch = Err(Error::Mismatch {
        name: value.clone(),
    });
    for old in [None, Some(Digest::of(b"another definition"))] {
        let mut stated = plan.clone();
        let change = stated.changes.get_mut(&value);
        change.expect("the change of a.value").old = old;
        let found = stated.definitions(&spec.definitions(), keys(spec.made));
        assert_eq!(found, mismatch);
    }
    let empty = Spec::create_empty();
    let mut added = empty.plan(&[PLANT], &["n"]).expect("no problems");
    let change = added.changes.get_mut(&value);
    change.expect("the addition of a.value").old = Some(Digest::of(b""));
    let error = added
        .definitions(&empty.definitions(), keys(0))
        .expect_err("an old digest of nothing");
    assert_eq!(error, Error::Mismatch { name: value });
    assert_eq!(
        error.to_string(),
        "the plan holds a change at a.value that a plan of the applied spec cannot \
         make: plan again"
    );
}

/// A plan of [`PLANT`] on the empty spec with one change: its change of
/// `a.@placement` at `at`, with the digest of `old` and with `new` as its definition.
fn one(at: &str, old: Option<&Stored>, new: Option<Definition>) -> Plan {
    let mut plan = Spec::create_empty()
        .plan(&[PLANT], &["n"])
        .expect("no problems");
    let (_, mut change) = plan.changes.pop_first().expect("a change");
    change.old = old.map(|old| Digest::of(&old.encode()));
    let entry = change.new.take().expect("the new placement");
    change.new = new.map(|definition| {
        let mut entry = entry;
        entry.definition = definition;
        entry
    });
    plan.changes = BTreeMap::from([(name(at), change)]);
    plan.homes.clear();
    plan
}

#[test]
fn refuses_a_change_that_plan_cannot_make() {
    let admin = spec::founding::create(PrivateKey([7; 32]).public());
    let subject = &admin[&name("@admin.@subject")];
    let select = Selector::new(["a.**"]).expect("a selector");
    let time = Stored::Time(time::Policy::new(select, Peers::Voters));
    let blockless = BTreeMap::from([(name("a.@time"), time.clone())]);
    let placement = Spec::create_empty()
        .plan(&[PLANT], &["n"])
        .expect("no problems");
    let placement = placement.changes[&name("a.@placement")]
        .new
        .clone()
        .expect("a placement")
        .definition;
    let reserved = Plan::decode(&bytes(&[data("@a.value", "f64", None)]));
    let empty = BTreeMap::new();
    let cases = [
        (one("@admin.@subject", Some(subject), None), &admin),
        (one("a.@time", Some(&time), None), &blockless),
        (
            one("a.@time", None, Some(Definition::Spec(time.clone()))),
            &empty,
        ),
        (one("@a.@placement", None, Some(placement.clone())), &empty),
        (one("a.other", None, Some(placement)), &empty),
        (reserved.expect("a plan"), &empty),
    ];
    for (plan, applied) in cases {
        let at = plan.changes.keys().next().expect("a change").clone();
        let found = plan.definitions(applied, keys(0));
        assert_eq!(found, Err(Error::Mismatch { name: at }));
    }
}

#[test]
fn a_change_with_no_definition_changes_nothing_only_at_a_name_with_none() {
    let mut spec = Spec::create_empty();
    spec.apply(&spec.plan(&[PLANT], &["n"]).expect("no problems"));
    let applied = spec.definitions();
    let found = one("b.@placement", None, None).definitions(&applied, keys(spec.made));
    assert_eq!(found, Ok(applied.clone()));
    let found = one("a.@placement", None, None).definitions(&applied, keys(spec.made));
    let at = name("a.@placement");
    assert_eq!(found, Err(Error::Mismatch { name: at }));
}

#[test]
fn accepts_a_change_whose_new_bytes_equal_the_stored_bytes() {
    let mut spec = Spec::create_empty();
    spec.apply(&spec.plan(&[PLANT], &["n"]).expect("no problems"));
    let applied = spec.definitions();
    let stored = &applied[&name("a.@placement")];
    let same = Some(Definition::Spec(stored.clone()));
    let plan = one("a.@placement", Some(stored), same);
    let found = plan.definitions(&applied, keys(spec.made));
    assert_eq!(found, Ok(applied.clone()));
}

/// The fixture texts that [`plan_then_definitions_never_refuses`] applies and plans.
const TEXTS: [&str; 4] = [EDGE, INFLUX, EACH, PLANT];

/// The bytes of a plan at version 0 of the empty spec with `changes`, each one
/// already encoded, and no home.
fn bytes(changes: &[Vec<u8>]) -> Vec<u8> {
    let mut out = vec![1];
    out.extend_from_slice(&0_u64.to_le_bytes());
    out.extend_from_slice(&spec::tree::empty().0);
    out.extend_from_slice(&(changes.len() as u64).to_le_bytes());
    out.extend(changes.iter().flatten());
    out.extend_from_slice(&0_u64.to_le_bytes());
    out
}

/// The offset of the first change in [`bytes`].
const CHANGES: usize = 1 + 8 + 32 + 8;

/// A count and the bytes of `text`.
fn text(text: &str) -> Vec<u8> {
    let mut out = (text.len() as u64).to_le_bytes().to_vec();
    out.extend_from_slice(text.as_bytes());
    out
}

/// A change at `name` with no old digest and a new data channel on `a.time`, with
/// `data_type` as its text and `unit` as its unit.
fn data(name: &str, data_type: &str, unit: Option<&str>) -> Vec<u8> {
    let mut out = text(name);
    out.extend_from_slice(&[0, 1, 1, 1]);
    out.extend(text("a.time"));
    out.push(0);
    out.extend(text(data_type));
    match unit {
        None => out.push(0),
        Some(unit) => {
            out.push(1);
            out.extend(text(unit));
        }
    }
    out
}

/// The offsets in a [`data`] change of `name`: its old flag, its definition, its data
/// type, and its unit flag.
fn offsets(name: &str, data_type: &str) -> [usize; 4] {
    let old = 8 + name.len();
    let data_type_at = old + 4 + 8 + "a.time".len() + 1;
    [
        old,
        old + 2,
        data_type_at,
        data_type_at + 8 + data_type.len(),
    ]
}

#[test]
fn decodes_a_plan_with_no_change() {
    let plan = Plan::decode(&bytes(&[])).expect("a plan");
    assert_eq!(plan.encode(), bytes(&[]));
    assert_eq!(plan.changes, BTreeMap::new());
}

#[test]
fn refuses_a_plan_of_another_format_version() {
    let mut found = bytes(&[]);
    found[0] = 2;
    let error = Plan::decode(&found).expect_err("version 2");
    assert_eq!(error, Error::Version { found: 2 });
    assert_eq!(
        error.to_string(),
        "the plan has format version 2, and this build reads only version 1; plan \
         again with this build"
    );
}

#[test]
fn refuses_bytes_that_end_in_a_field() {
    let mut found = bytes(&[data("a.value", "f64", None)]);
    found.pop();
    let error = Plan::decode(&found).expect_err("cut");
    assert_eq!(
        error,
        Error::Malformed {
            at: found.len() - 7
        }
    );
    let error = Plan::decode(&[]).expect_err("empty");
    assert_eq!(error, Error::Malformed { at: 0 });
    assert_eq!(error.to_string(), "the bytes are not a plan, from byte 0");
}

#[test]
fn refuses_bytes_after_the_plan() {
    let mut found = bytes(&[]);
    found.push(0);
    let error = Plan::decode(&found).expect_err("one more byte");
    assert_eq!(error, Error::Malformed { at: CHANGES + 8 });
}

#[test]
fn refuses_changes_out_of_name_order() {
    for names in [["a.value", "a.value"], ["a.value", "a.min"]] {
        let changes = names.map(|name| data(name, "f64", None));
        let found = bytes(&changes);
        let error = Plan::decode(&found).expect_err("out of order");
        let second = CHANGES + changes[0].len();
        assert_eq!(error, Error::Malformed { at: second });
    }
}

#[test]
fn refuses_homes_out_of_name_order() {
    let mut found = bytes(&[]);
    found.truncate(found.len() - 8);
    found.extend_from_slice(&2_u64.to_le_bytes());
    let home = [text("b.time"), text("n")].concat();
    found.extend([home.clone(), home].concat());
    let error = Plan::decode(&found).expect_err("out of order");
    assert_eq!(
        error,
        Error::Malformed {
            at: CHANGES + 8 + 8 + 6 + 8 + 1
        }
    );
}

#[test]
fn refuses_homes_in_falling_name_order() {
    let mut found = bytes(&[]);
    found.truncate(found.len() - 8);
    found.extend_from_slice(&2_u64.to_le_bytes());
    let first = [text("b.time"), text("n")].concat();
    found.extend([first.clone(), text("a.time"), text("n")].concat());
    let error = Plan::decode(&found).expect_err("out of order");
    assert_eq!(
        error,
        Error::Malformed {
            at: CHANGES + 8 + first.len()
        }
    );
}

#[test]
fn refuses_a_flag_that_is_not_0_or_1() {
    let mut change = data("a.value", "f64", None);
    let [old, ..] = offsets("a.value", "f64");
    change[old] = 2;
    let error = Plan::decode(&bytes(&[change])).expect_err("flag 2");
    assert_eq!(error, Error::Malformed { at: CHANGES + old });
}

#[test]
fn refuses_a_change_with_no_old_and_no_new_definition() {
    let change = [text("a.value"), vec![0, 0]].concat();
    let error = Plan::decode(&bytes(&[change])).expect_err("no change");
    assert_eq!(
        error,
        Error::Malformed {
            at: CHANGES + 8 + 7 + 1
        }
    );
}

#[test]
fn refuses_a_definition_tag_that_it_does_not_write() {
    let [_, definition, ..] = offsets("a.value", "f64");
    for at in [definition, definition + 1] {
        let mut change = data("a.value", "f64", None);
        change[at] = 2;
        let error = Plan::decode(&bytes(&[change])).expect_err("tag 2");
        assert_eq!(error, Error::Malformed { at: CHANGES + at });
    }
}

#[test]
fn refuses_text_that_is_not_the_text_it_writes() {
    let [_, _, data_type, unit] = offsets("a.value", "F64");
    let error = Plan::decode(&bytes(&[data("a.value", "F64", None)]));
    assert_eq!(
        error,
        Err(Error::Malformed {
            at: CHANGES + data_type
        })
    );
    let error = Plan::decode(&bytes(&[data("a.value", "f64", Some("m s"))]));
    assert_eq!(
        error,
        Err(Error::Malformed {
            at: CHANGES + unit + 1
        })
    );
    let error = Plan::decode(&bytes(&[data("a..b", "f64", None)]));
    assert_eq!(error, Err(Error::Malformed { at: CHANGES }));
    let mut change = data("a.value", "f64", None);
    change[8] = 0xff;
    let error = Plan::decode(&bytes(&[change]));
    assert_eq!(error, Err(Error::Malformed { at: CHANGES }));
}

#[test]
fn refuses_a_unit_on_a_data_type_with_no_unit() {
    let [.., unit] = offsets("a.value", "string");
    let change = data("a.value", "string", Some("m"));
    let error = Plan::decode(&bytes(&[change])).expect_err("a unit on text");
    assert_eq!(error, Error::Malformed { at: CHANGES + unit });
}

#[test]
fn refuses_a_spec_definition_that_is_a_channel() {
    let spec = Spec::create_empty();
    let plan = spec.plan(&[PLANT], &["n"]).expect("no problems");
    let definitions = plan
        .definitions(&spec.definitions(), keys(0))
        .expect("a plan of the applied spec");
    let channel = definitions[&name("a.time")].encode();
    let mut change = [text("a.time"), vec![0, 1, 0]].concat();
    change.extend_from_slice(&(channel.len() as u64).to_le_bytes());
    change.extend(channel);
    let error = Plan::decode(&bytes(&[change])).expect_err("a stored channel");
    assert_eq!(
        error,
        Error::Malformed {
            at: CHANGES + 8 + 6 + 3
        }
    );
}

#[test]
fn refuses_a_wrong_byte_inside_a_spec_definition_at_its_count() {
    let [added, _] = plans();
    let inner = added
        .changes
        .values()
        .find_map(|change| match &change.new.as_ref()?.definition {
            Definition::Spec(definition) => Some(definition.encode()),
            _ => None,
        })
        .expect("a spec definition");
    let mut found = added.encode();
    let start = found
        .windows(inner.len())
        .position(|window| window == inner.as_slice())
        .expect("the spec bytes");
    // The tag of the definition, after its format version.
    found[start + 1] = 0xff;
    let error = Plan::decode(&found).expect_err("an unknown tag");
    assert_eq!(error, Error::Malformed { at: start - 8 });
}

#[test]
fn refuses_an_access_change_that_allows_nothing_at_its_count() {
    let subjects = Selector::new(["s.*"]).expect("a selector");
    let select = Selector::new(["x.**"]).expect("a selector");
    let allow = [Action::Read].into_iter().collect();
    let policy = access::Policy::new(subjects, select, allow, Authority(0));
    let mut inner = Stored::Access(policy.expect("a policy")).encode();
    let actions = inner.len() - 2;
    inner[actions] = 0;
    let name = Kind::Access.key("x").expect("a tree key");
    let mut change = [text(name.as_str()), vec![0, 1, 0]].concat();
    let count = change.len();
    change.extend_from_slice(&(inner.len() as u64).to_le_bytes());
    change.extend(inner);
    let found = bytes(&[change]);
    let input = include_bytes!("../../../../../oracles/fuzz/config_plan/access_empty");
    assert_eq!(found, input);
    let error = Plan::decode(&found);
    assert_eq!(
        error,
        Err(Error::Malformed {
            at: CHANGES + count
        })
    );
}

/// Each plan of [`plans`], with a part of its changes and homes, and another base
/// and old digests.
fn changed() -> impl Strategy<Value = Plan> {
    let plans = plans();
    let plan = (0..plans.len()).prop_flat_map(move |at| {
        let plan = plans[at].clone();
        let changes: Vec<_> = plan.changes.clone().into_iter().collect();
        let changes = proptest::sample::subsequence(changes.clone(), 0..=changes.len());
        let homes: Vec<_> = plan.homes.clone().into_iter().collect();
        let homes = proptest::sample::subsequence(homes.clone(), 0..=homes.len());
        let olds =
            proptest::collection::vec(any::<Option<[u8; 32]>>(), plan.changes.len());
        (
            Just(plan),
            changes,
            homes,
            olds,
            any::<u64>(),
            any::<[u8; 32]>(),
        )
    });
    plan.prop_map(|(mut plan, mut changes, homes, olds, version, root)| {
        for ((_, change), old) in changes.iter_mut().zip(olds) {
            if change.new.is_some() {
                change.old = old.map(Digest);
            }
        }
        plan.changes = changes.into_iter().collect();
        plan.homes = homes.into_iter().collect();
        plan.base = spec::Pointer {
            version,
            root: Digest(root),
        };
        spanless(plan)
    })
}

proptest! {
    #[test]
    fn plan_then_definitions_never_refuses(
        applied in proptest::sample::subsequence(TEXTS.to_vec(), 0..=TEXTS.len()),
        planned in proptest::sample::subsequence(TEXTS.to_vec(), 0..=TEXTS.len()),
    ) {
        let mut spec = Spec::create_empty();
        if let Ok(plan) = spec.plan(&applied, &MEMBERS) {
            spec.apply(&plan);
        }
        if let Ok(plan) = spec.plan(&planned, &MEMBERS) {
            let found = plan.definitions(&spec.definitions(), keys(spec.made));
            prop_assert!(found.is_ok(), "{found:?}");
        }
    }

    #[test]
    fn decodes_each_plan_that_it_encodes(plan in changed()) {
        prop_assert_eq!(Plan::decode(&plan.encode()), Ok(plan));
    }

    #[test]
    fn decodes_any_bytes_with_no_panic(
        found in proptest::collection::vec(any::<u8>(), 0..256),
    ) {
        if let Ok(plan) = Plan::decode(&found) {
            prop_assert_eq!(plan.encode(), found);
        }
    }

    /// Bytes near a plan reach each field of the decode.
    #[test]
    fn decodes_a_changed_plan_with_no_panic(
        plan in changed(),
        at in any::<proptest::sample::Index>(),
        byte in any::<u8>(),
        cut in any::<bool>(),
    ) {
        let mut found = plan.encode();
        let at = at.index(found.len());
        if cut {
            found.truncate(at);
        } else {
            found[at] = byte;
        }
        if let Ok(plan) = Plan::decode(&found) {
            prop_assert_eq!(plan.encode(), found);
        }
    }
}

#[test]
fn decodes_the_fuzz_inputs_to_the_fixture_plans() {
    let added = include_bytes!("../../../../../oracles/fuzz/config_plan/added_status");
    let changed =
        include_bytes!("../../../../../oracles/fuzz/config_plan/changed_status");
    let empty = include_bytes!("../../../../../oracles/fuzz/config_plan/empty");
    let found = [added.as_slice(), changed].map(Plan::decode);
    assert_eq!(found, plans().map(|plan| Ok(spanless(plan))));
    assert_eq!(
        Plan::decode(empty).map(|plan| plan.encode()),
        Ok(bytes(&[]))
    );
}

/// The fixture plans from before connectors had status channels still decode.
#[test]
fn decodes_the_fuzz_inputs_from_before_the_status_channels() {
    let added = include_bytes!("../../../../../oracles/fuzz/config_plan/added");
    let changed = include_bytes!("../../../../../oracles/fuzz/config_plan/changed");
    let [mut plan, _] = plans();
    let kept = |name: &types::name::Name| !name.as_str().contains(".status.");
    plan.changes.retain(|name, _| kept(name));
    plan.homes.retain(|name, _| kept(name));
    assert_eq!(Plan::decode(added), Ok(spanless(plan)));
    let decoded = Plan::decode(changed).map(|plan| plan.encode());
    assert_eq!(decoded.as_deref(), Ok(changed.as_slice()));
}
