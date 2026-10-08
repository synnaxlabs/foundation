use config::plan::{Error, Plan};
use proptest::prelude::*;
use spec::channel::{Edge, Problem};
use spec::region;
use types::channel::Key;
use types::digest::Digest;
use types::name::Prefix;

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
        .iter_mut()
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
        let found = plan.definitions(&spec.definitions(), keys(spec.made));
        spec.apply(&plan);
        assert_eq!(found, spec.definitions());
    }
}

#[test]
fn gives_a_dangling_edge_a_key_that_the_region_check_refuses() {
    let spec = Spec::create_empty();
    let mut plan = spec.plan(&[PLANT], &["n"]).expect("no problems");
    plan.changes.retain(|change| change.name != name("a.time"));
    let definitions = plan.definitions(&spec.definitions(), keys(0));
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
    let removal = removed
        .changes
        .into_iter()
        .find(|change| change.name == name("a.time"));
    plan.changes
        .insert(0, removal.expect("the removal of a.time"));
    let definitions = plan.definitions(&spec.definitions(), keys(spec.made));
    let problems = region::check(&Prefix::ROOT, &definitions);
    let dangling = Problem::Dangling {
        from: name("a.value"),
        edge: Edge::Index,
        to: Key::from_u128((7 << 76) | (spec.made + 1)),
    };
    assert_eq!(problems, [region::Problem::Channel(dangling)]);
}

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
    assert_eq!(plan.changes, []);
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
    let definitions = plan.definitions(&spec.definitions(), keys(0));
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

/// Each plan of [`plans`], with a part of its changes and homes, and another base
/// and old digests.
fn changed() -> impl Strategy<Value = Plan> {
    let plans = plans();
    let plan = (0..plans.len()).prop_flat_map(move |at| {
        let plan = plans[at].clone();
        let changes =
            proptest::sample::subsequence(plan.changes.clone(), 0..=plan.changes.len());
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
        for (change, old) in changes.iter_mut().zip(olds) {
            if change.new.is_some() {
                change.old = old.map(Digest);
            }
        }
        plan.changes = changes;
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
