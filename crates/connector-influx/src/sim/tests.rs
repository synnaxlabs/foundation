use proptest::prelude::*;
use std::fmt::Write as _;

use super::*;
use crate::line::{self, Value};

fn stored(body: &str) -> Store {
    let mut store = Store::default();
    store.write(body.as_bytes()).unwrap();
    store
}

fn refused(body: &str) -> (Store, Error) {
    let mut store = Store::default();
    let error = store.write(body.as_bytes()).unwrap_err();
    (store, error)
}

fn times(store: &Store, measurement: &str, tags: &[(&str, &str)]) -> Vec<i64> {
    store
        .points(measurement, tags)
        .map(|point| point.time.nanos())
        .collect()
}

/// The fields as a map that owns its keys.
fn owned(fields: Fields<'_>) -> BTreeMap<String, Field> {
    fields
        .iter()
        .map(|(key, field)| (key.to_owned(), field))
        .collect()
}

/// Each point of `measurement`, with its tags and fields as maps.
fn read(
    store: &Store,
    measurement: &str,
) -> Vec<(Stamp, Tags, BTreeMap<String, Field>)> {
    store
        .points(measurement, &[])
        .map(|point| (point.time, point.tags.clone(), owned(point.fields)))
        .collect()
}

fn map<V: Clone>(pairs: &[(&str, V)]) -> BTreeMap<String, V> {
    pairs
        .iter()
        .map(|(key, value)| ((*key).into(), value.clone()))
        .collect()
}

#[test]
fn stores_each_type_and_tag() {
    let store = stored("m,b=2,a=1 f=1.5,i=-2i,u=3u,t=t,s=\"x y\" 10\n");
    assert_eq!(
        read(&store, "m"),
        [(
            Stamp::from_nanos(10),
            map(&[("a", "1".to_owned()), ("b", "2".to_owned())]),
            map(&[
                ("f", Field::Float(1.5)),
                ("i", Field::Integer(-2)),
                ("u", Field::Unsigned(3)),
                ("t", Field::Boolean(true)),
                ("s", Field::String("x y".into())),
            ]),
        )]
    );
}

#[test]
fn a_later_write_replaces_only_the_fields_it_sets() {
    let store = stored("m v=1 10\nm w=2i 10\nm v=3 10\n");
    let point = store.points("m", &[]).next().unwrap();
    assert_eq!(
        owned(point.fields),
        map(&[("v", Field::Float(3.0)), ("w", Field::Integer(2))])
    );
}

#[test]
fn names_a_point_by_measurement_tags_and_time() {
    let store = stored("m,a=1 v=1 10\nm,a=2 v=2 10\nn,a=1 v=3 10\nm,a=1 v=4 11\n");
    assert_eq!(store.points("m", &[]).count(), 3);
    assert_eq!(times(&store, "m", &[("a", "1")]), [10, 11]);
    assert_eq!(times(&store, "n", &[("a", "1")]), [10]);
}

#[test]
fn gives_the_points_in_time_order() {
    let store = stored("m v=1 30\nm v=1 -10\nm v=1 20\n");
    assert_eq!(times(&store, "m", &[]), [-10, 20, 30]);
}

#[test]
fn a_point_outlives_the_tags_that_found_it() {
    let store = stored("m,a=b v=1 10\n");
    let points: Vec<Point<'_>> = store
        .points("m", &[("a", String::from("b").as_str())])
        .collect();
    assert_eq!(points[0].tags, &BTreeMap::from([("a".into(), "b".into())]));
}

#[test]
fn gives_only_the_points_with_each_tag() {
    let store = stored("m,a=1,b=2 v=1 10\nm,a=1 v=1 20\nm v=1 30\n");
    assert_eq!(times(&store, "m", &[("a", "1"), ("b", "2")]), [10]);
    assert_eq!(times(&store, "m", &[("a", "1")]), [10, 20]);
    assert_eq!(times(&store, "m", &[("a", "2")]), Vec::<i64>::new());
    assert_eq!(times(&store, "x", &[]), Vec::<i64>::new());
}

#[test]
fn skips_empty_lines_and_comments() {
    let store = stored("\n# a comment\n   \nm v=1 10\n\n");
    assert_eq!(times(&store, "m", &[]), [10]);
}

#[test]
fn skips_a_comment_after_spaces_and_tabs() {
    let store = stored("  #m v=1 10\n\t #m v=1 10\nm v=1 20\n");
    assert_eq!(times(&store, "m", &[]), [20]);
    assert_eq!(store.points("#m", &[]).count(), 0);
}

#[test]
fn a_quote_in_a_comment_runs_across_the_next_lines() {
    let body = "# a=\"b\nm v=1 10\nm v=1 20\n";
    let (store, error) = refused(body);
    assert_eq!(
        error,
        Error::Parse {
            line: body.into(),
            message:
                "Could not parse entire line. Found trailing content: '\nm v=1 20\n'"
                    .into(),
        }
    );
    assert_eq!(store.points("m", &[]).count(), 0);
}

#[test]
fn skips_a_comment_with_a_quote_that_opens_no_string() {
    for body in [
        "#a=\"b\nm v=1 10\n",
        "# a,b=\"c\nm v=1 10\n",
        "# \"b\nm v=1 10\n",
        "# a\"b\nm v=1 10\n",
        "#\"\nm v=1 10\n",
    ] {
        assert_eq!(times(&stored(body), "m", &[]), [10]);
    }
}

#[test]
fn stores_a_leading_tab_and_a_tab_in_a_string() {
    let store = stored("\tm v=1 10\nm s=\"a\tb\" 20\n");
    assert_eq!(times(&store, "m", &[]), [10, 20]);
    let point = store.points("m", &[]).last().unwrap();
    assert_eq!(
        owned(point.fields),
        map(&[("s", Field::String("a\tb".into()))])
    );
}

#[test]
fn refuses_a_bare_tab_or_nul_in_a_name() {
    const TAKE: &str = "A generic parsing error occurred: TakeWhile1";
    for (line, message) in [
        ("m\tx v=1 10", TAKE),
        ("m,a\tb=c v=1 10", "Tag Set Malformed"),
        ("m,a=b\tc v=1 10", TAKE),
        ("m a\tb=1 10", "No fields were provided"),
        ("m\0x v=1 10", TAKE),
    ] {
        let (store, error) = refused(line);
        assert_eq!(
            error,
            Error::Parse {
                line: line.into(),
                message: message.into(),
            }
        );
        assert_eq!(store.points("m", &[]).count(), 0);
    }
}

#[test]
fn stores_a_backslash_or_nul_that_the_writer_refuses() {
    let store = stored("m\\x,a\\b=c,d\0e=f,g=h\\i,j=k\0l v\\w=1,x\0y=2 10\n");
    let point = store.points("m\\x", &[]).next().unwrap();
    assert_eq!(
        point.tags,
        &map(&[
            ("a\\b", "c".to_owned()),
            ("d\0e", "f".to_owned()),
            ("g", "h\\i".to_owned()),
            ("j", "k\0l".to_owned()),
        ])
    );
    assert_eq!(
        owned(point.fields),
        map(&[("v\\w", Field::Float(1.0)), ("x\0y", Field::Float(2.0))])
    );
}

#[test]
fn refuses_a_body_that_is_not_utf8_and_stores_none_of_it() {
    let body = [b"m v=1 10\nm v=1 2".as_slice(), &[0xff], b"\n"].concat();
    let mut store = Store::default();
    let error = store.write(&body).unwrap_err();
    assert_eq!(
        error,
        Error::Utf8 {
            error: std::str::from_utf8(&body).unwrap_err(),
        }
    );
    assert_eq!(
        error.to_string(),
        "the body is not UTF-8: invalid utf-8 sequence of 1 bytes from index 16"
    );
    assert_eq!(store.points("m", &[]).count(), 0);
}

#[test]
fn refuses_a_line_that_does_not_parse() {
    let (store, error) = refused("m v=1 10\nm v= 20\nm v=1 30\n");
    assert_eq!(
        error,
        Error::Parse {
            line: "m v= 20".into(),
            message: "No fields were provided".into(),
        }
    );
    assert_eq!(
        error.to_string(),
        "the line \"m v= 20\" does not parse: No fields were provided"
    );
    assert_eq!(times(&store, "m", &[]), [10, 30]);
}

#[test]
fn refuses_a_line_with_no_time() {
    let (store, error) = refused("m v=1");
    assert_eq!(
        error,
        Error::Time {
            line: "m v=1".into(),
            time: None,
        }
    );
    assert_eq!(error.to_string(), "the line \"m v=1\" has no time");
    assert_eq!(store.points("m", &[]).count(), 0);
}

#[test]
fn refuses_a_time_that_influxdb_does_not_store() {
    for time in [i64::MIN, i64::MIN + 1, i64::MAX] {
        let line = format!("m v=1 {time}");
        let (store, error) = refused(&line);
        assert_eq!(
            error,
            Error::Time {
                line: line.clone(),
                time: Some(time),
            }
        );
        assert_eq!(
            error.to_string(),
            format!("the line {line:?} has time {time}, which InfluxDB does not store")
        );
        assert_eq!(store.points("m", &[]).count(), 0);
    }
}

#[test]
fn stores_the_first_and_last_time_influxdb_stores() {
    let store = stored("m v=1 -9223372036854775806\nm v=1 9223372036854775806\n");
    assert_eq!(times(&store, "m", &[]), [i64::MIN + 2, i64::MAX - 1]);
}

#[test]
fn refuses_a_reserved_name_or_key() {
    for (line, name) in [
        ("m time=1 10", "time"),
        ("m,time=x v=1 10", "time"),
        ("m,_field=x v=1 10", "_field"),
        ("m _measurement=1 10", "_measurement"),
        ("_m v=1 10", "_m"),
    ] {
        let (store, error) = refused(line);
        assert_eq!(
            error,
            Error::Reserved {
                line: line.into(),
                name: name.into(),
            }
        );
        assert_eq!(
            error.to_string(),
            format!("the line {line:?} uses the reserved name {name:?}")
        );
        assert!(store.points("m", &[]).next().is_none());
        assert!(store.points("_m", &[]).next().is_none());
    }
}

#[test]
fn stores_a_measurement_named_time() {
    let store = stored("time,a=1 v=1 10\n");
    assert_eq!(times(&store, "time", &[]), [10]);
}

#[test]
fn refuses_a_key_that_comes_twice() {
    for line in ["m,v=1,v=2 w=1 10", "m,v=x v=1 10", "m v=1,v=1i 10"] {
        let (store, error) = refused(line);
        assert_eq!(
            error,
            Error::Duplicate {
                line: line.into(),
                key: "v".into(),
            }
        );
        assert_eq!(
            error.to_string(),
            format!("the line {line:?} has the key \"v\" more than once")
        );
        assert_eq!(store.points("m", &[]).count(), 0);
    }
}

#[test]
fn a_key_twice_with_two_types_stores_no_kind() {
    let (mut store, error) = refused("m v=1,v=1i 10\n");
    assert_eq!(
        error,
        Error::Duplicate {
            line: "m v=1,v=1i 10".into(),
            key: "v".into(),
        }
    );
    assert_eq!(
        error.to_string(),
        r#"the line "m v=1,v=1i 10" has the key "v" more than once"#
    );
    store.write(b"m v=1 20\n").unwrap();
    assert_eq!(times(&store, "m", &[]), [20]);
}

#[test]
fn refuses_an_infinite_float() {
    for line in ["m v=1e400 10", "m w=1,v=-1e400 10"] {
        let (store, error) = refused(line);
        assert_eq!(
            error,
            Error::Infinite {
                line: line.into(),
                field: "v".into(),
            }
        );
        assert_eq!(
            error.to_string(),
            format!("the line {line:?} gives the field \"v\" an infinite float")
        );
        assert_eq!(store.points("m", &[]).count(), 0);
    }
}

#[test]
fn refuses_a_field_of_another_type_and_stores_no_field_of_its_line() {
    let (store, error) =
        refused("m v=1 10\nm w=1i,v=1i 20\nm w=t 30\nm v=2 40\nn v=1i 50\n");
    assert_eq!(
        error,
        Error::Conflict {
            line: "m w=1i,v=1i 20".into(),
            key: "v".into(),
            stored: Kind::Float,
            written: Kind::Integer,
        }
    );
    assert_eq!(
        error.to_string(),
        "the line \"m w=1i,v=1i 20\" writes the float column \"v\" as integer"
    );
    assert_eq!(times(&store, "m", &[]), [10, 30, 40]);
    assert_eq!(times(&store, "n", &[]), [50]);
}

#[test]
fn refuses_a_field_with_the_key_of_a_stored_tag() {
    let (store, error) = refused("m,a=x v=1 10\nm a=1 20\n");
    assert_eq!(
        error,
        Error::Conflict {
            line: "m a=1 20".into(),
            key: "a".into(),
            stored: Kind::Tag,
            written: Kind::Float,
        }
    );
    assert_eq!(
        error.to_string(),
        "the line \"m a=1 20\" writes the tag column \"a\" as float"
    );
    assert_eq!(times(&store, "m", &[]), [10]);
}

#[test]
fn refuses_a_tag_with_the_key_of_a_stored_field() {
    let (store, error) = refused("m a=1 10\nm,a=x v=1 20\n");
    assert_eq!(
        error,
        Error::Conflict {
            line: "m,a=x v=1 20".into(),
            key: "a".into(),
            stored: Kind::Float,
            written: Kind::Tag,
        }
    );
    assert_eq!(
        error.to_string(),
        "the line \"m,a=x v=1 20\" writes the float column \"a\" as tag"
    );
    assert_eq!(times(&store, "m", &[]), [10]);
}

#[test]
fn a_refused_line_stores_no_tag_kind() {
    let (store, _) = refused("m v=1 10\nm,a=x v=1i 20\nm a=1 30\n");
    assert_eq!(times(&store, "m", &[]), [10, 30]);
}

#[test]
fn names_each_kind() {
    let names: Vec<String> = [
        Field::Float(0.0),
        Field::Integer(0),
        Field::Unsigned(0),
        Field::Boolean(false),
        Field::String(String::new()),
    ]
    .iter()
    .map(|field| field.kind().to_string())
    .collect();
    assert_eq!(names, ["float", "integer", "unsigned", "boolean", "string"]);
    assert_eq!(Kind::Tag.to_string(), "tag");
}

#[test]
fn gives_the_error_of_the_first_line_that_is_not_valid() {
    let (_, error) = refused("m v=1\nm v=1 10\nm v=1i 20\n");
    assert_eq!(
        error,
        Error::Time {
            line: "m v=1".into(),
            time: None,
        }
    );
}

fn name() -> impl Strategy<Value = String> {
    prop_oneof![
        Just("time".to_owned()),
        "[^_#\\\\\n\r\t\0][^\\\\\n\r\t\0]{0,8}",
    ]
}

fn key() -> impl Strategy<Value = String> {
    name().prop_filter("not time", |key| key != "time")
}

fn value() -> impl Strategy<Value = Value> {
    prop_oneof![
        any::<f64>()
            .prop_filter_map("finite", line::Float::new)
            .prop_map(Value::Float),
        any::<i64>().prop_map(Value::Integer),
        any::<u64>().prop_map(Value::Unsigned),
        any::<bool>().prop_map(Value::Boolean),
    ]
}

fn field(value: Value) -> Field {
    match value {
        Value::Float(float) => Field::Float(float.get()),
        Value::Integer(integer) => Field::Integer(integer),
        Value::Unsigned(unsigned) => Field::Unsigned(unsigned),
        Value::Boolean(boolean) => Field::Boolean(boolean),
    }
}

proptest! {
    #[test]
    fn stores_each_line_that_the_writer_writes(
        measurement in name(),
        keys in proptest::collection::btree_map(
            key(),
            (any::<bool>(), name(), value()),
            1..8,
        ),
        time in TIMES,
    ) {
        let mut tags = Vec::new();
        let mut fields = Vec::new();
        for (key, (tag, text, value)) in &keys {
            if *tag {
                tags.push((key.as_str(), text.as_str()));
            } else {
                fields.push((key.as_str(), *value));
            }
        }
        prop_assume!(!fields.is_empty());
        let keys: Vec<&str> = fields.iter().map(|&(key, _)| key).collect();
        let values: Vec<Option<Value>> =
            fields.iter().map(|&(_, value)| Some(value)).collect();
        let mut out = Vec::new();
        line::Measurement::new(&measurement, &tags, &keys)
            .unwrap()
            .line(&mut out, &values, Stamp::from_nanos(time));
        let mut store = Store::default();
        store.write(&out).unwrap();
        let fields: Vec<(&str, Field)> =
            fields.iter().map(|&(key, value)| (key, field(value))).collect();
        let tags: Vec<(&str, String)> =
            tags.iter().map(|&(key, text)| (key, text.to_owned())).collect();
        prop_assert_eq!(
            read(&store, &measurement),
            [(Stamp::from_nanos(time), map(&tags), map(&fields))]
        );
    }
}

/// Many points of one measurement, at `start`, `start + step`, and so on, with the
/// fields that `fields` selects and a tag `t` when `tag` is set.
#[derive(Clone, Debug)]
struct Run {
    start: i64,
    step: i64,
    count: i64,
    tag: Option<&'static str>,
    fields: u8,
}

fn run() -> impl Strategy<Value = Run> {
    (
        0..20_000_i64,
        1..4_i64,
        1..6_000_i64,
        prop_oneof![Just(None), Just(Some("a")), Just(Some("b"))],
        1..16_u8,
    )
        .prop_map(|(start, step, count, tag, fields)| Run {
            start,
            step,
            count,
            tag,
            fields,
        })
}

/// The fields of the point at `time` in run `run`, of the kinds that `mask` selects.
#[expect(clippy::arithmetic_side_effects, reason = "times are below 40_000")]
fn fields_of(run: usize, time: i64, mask: u8) -> Vec<(&'static str, Field)> {
    let run = i64::try_from(run).unwrap();
    [
        ("b", Field::Boolean((time + run) % 2 == 0)),
        (
            "f",
            Field::Float(f64::from(i32::try_from(time).unwrap()) * 0.5),
        ),
        ("i", Field::Integer(time * 10 + run)),
        ("s", Field::String(format!("r{run}"))),
    ]
    .into_iter()
    .enumerate()
    .filter(|(bit, _)| mask & (1 << bit) != 0)
    .map(|(_, field)| field)
    .collect()
}

fn text(field: &Field) -> String {
    match field {
        Field::Boolean(value) => value.to_string(),
        Field::Float(value) => format!("{value:?}"),
        Field::Integer(value) => format!("{value}i"),
        Field::String(value) => format!("{value:?}"),
        Field::Unsigned(value) => format!("{value}u"),
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(24))]

    #[test]
    fn reads_back_what_a_map_of_each_point_holds(
        runs in proptest::collection::vec(run(), 1..6),
    ) {
        type Model = BTreeMap<(Stamp, Tags), BTreeMap<String, Field>>;
        let mut store = Store::default();
        let mut model = Model::new();
        for (at, run) in runs.iter().enumerate() {
            let mut body = String::new();
            let tags = run.tag.map(|tag| map(&[("t", tag.to_owned())])).unwrap_or_default();
            for k in 0..run.count {
                #[expect(clippy::arithmetic_side_effects, reason = "below 40_000")]
                let time = run.start + k * run.step;
                let fields = fields_of(at, time, run.fields);
                let set: Vec<String> = fields
                    .iter()
                    .map(|(key, field)| format!("{key}={}", text(field)))
                    .collect();
                let tag = run.tag.map(|tag| format!(",t={tag}")).unwrap_or_default();
                writeln!(body, "m{tag} {} {time}", set.join(",")).unwrap();
                model
                    .entry((Stamp::from_nanos(time), tags.clone()))
                    .or_default()
                    .extend(fields.into_iter().map(|(key, field)| (key.to_owned(), field)));
            }
            store.write(body.as_bytes()).unwrap();
        }
        let expected: Vec<_> = model
            .iter()
            .map(|((time, tags), fields)| (*time, tags.clone(), fields.clone()))
            .collect();
        prop_assert_eq!(read(&store, "m"), expected.clone());
        for series in store.measurements["m"].series.values() {
            for (first, chunk) in &series.chunks {
                prop_assert!((1..=CHUNK).contains(&chunk.times.len()));
                prop_assert_eq!(*first, chunk.times[0]);
            }
        }
        let tagged: Vec<_> = expected
            .into_iter()
            .filter(|(_, tags, _)| tags.get("t").is_some_and(|tag| tag == "a"))
            .collect();
        let filtered: Vec<_> = store
            .points("m", &[("t", "a")])
            .map(|point| {
                let fields = point
                    .fields
                    .iter()
                    .map(|(key, field)| (key.to_owned(), field))
                    .collect();
                (point.time, point.tags.clone(), fields)
            })
            .collect();
        prop_assert_eq!(filtered, tagged);
    }
}

/// The point count of each chunk of the series of `m` with no tags.
fn chunks(store: &Store) -> Vec<usize> {
    store.measurements["m"].series[&Tags::new()]
        .chunks
        .values()
        .map(|chunk| chunk.times.len())
        .collect()
}

fn lines(times: impl Iterator<Item = usize>) -> String {
    times.fold(String::new(), |mut text, time| {
        writeln!(text, "m v={time} {time}").unwrap();
        text
    })
}

#[test]
fn appends_in_time_order_fill_each_chunk() {
    let store = stored(&lines(0..=3 * CHUNK));
    assert_eq!(chunks(&store), [CHUNK, CHUNK, CHUNK, 1]);
}

#[test]
fn a_time_inside_a_full_chunk_splits_it() {
    let mut store = stored(&lines((0..CHUNK).map(|k| 2 * k)));
    store.write(lines(std::iter::once(5)).as_bytes()).unwrap();
    assert_eq!(chunks(&store), [CHUNK / 2 + 1, CHUNK / 2]);
    let times: Vec<i64> = times(&store, "m", &[]);
    assert!(times.is_sorted() && times.len() == CHUNK + 1, "{times:?}");
}

#[test]
fn gets_only_the_fields_that_the_point_sets() {
    let store = stored("m v=1,s=\"a\" 10\nm w=2i 20\n");
    let point = store.points("m", &[]).next().unwrap();
    assert_eq!(point.fields.get("v"), Some(Field::Float(1.0)));
    assert_eq!(point.fields.get("s"), Some(Field::String("a".into())));
    assert_eq!(point.fields.get("w"), None, "set by another point only");
    assert_eq!(point.fields.get("x"), None);
}

#[test]
fn fields_are_equal_when_they_set_the_same_keys_to_equal_values() {
    let store = stored("m v=1 10\nm v=1 20\nm v=2 30\nm v=1,w=1 40\n");
    let fields: Vec<Fields<'_>> =
        store.points("m", &[]).map(|point| point.fields).collect();
    assert_eq!(fields[0], fields[1]);
    assert_ne!(fields[0], fields[2]);
    assert_ne!(fields[0], fields[3]);
}

#[test]
fn prints_fields_as_a_map() {
    let store = stored("m v=1,i=2i 10\n");
    let point = store.points("m", &[]).next().unwrap();
    assert_eq!(
        format!("{:?}", point.fields),
        "{\"i\": Integer(2), \"v\": Float(1.0)}"
    );
}
