use proptest::prelude::*;

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
        store.points("m", &[]).collect::<Vec<_>>(),
        [Point {
            time: Stamp::from_nanos(10),
            tags: &map(&[("a", "1".to_owned()), ("b", "2".to_owned())]),
            fields: &map(&[
                ("f", Field::Float(1.5)),
                ("i", Field::Integer(-2)),
                ("u", Field::Unsigned(3)),
                ("t", Field::Boolean(true)),
                ("s", Field::String("x y".into())),
            ]),
        }]
    );
}

#[test]
fn a_later_write_replaces_only_the_fields_it_sets() {
    let store = stored("m v=1 10\nm w=2i 10\nm v=3 10\n");
    let point = store.points("m", &[]).next().unwrap();
    assert_eq!(
        point.fields,
        &map(&[("v", Field::Float(3.0)), ("w", Field::Integer(2))])
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
fn refuses_a_body_that_is_not_utf8_and_stores_none_of_it() {
    let body = [b"m v=1 10\nm v=1 2".as_slice(), &[0xff], b"\n"].concat();
    let mut store = Store::default();
    let error = store.write(&body).unwrap_err();
    assert_eq!(error, Error::Utf8(std::str::from_utf8(&body).unwrap_err()));
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
fn a_type_conflict_in_one_line_stores_no_kind() {
    let mut store = Store::default();
    store.write(b"m v=1,v=1i 10\n").unwrap_err();
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
            field: "v".into(),
            stored: Kind::Float,
            written: Kind::Integer,
        }
    );
    assert_eq!(
        error.to_string(),
        "the line \"m w=1i,v=1i 20\" writes the float field \"v\" as integer"
    );
    assert_eq!(times(&store, "m", &[]), [10, 30, 40]);
    assert_eq!(times(&store, "n", &[]), [50]);
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
            store.points(&measurement, &[]).collect::<Vec<_>>(),
            [Point {
                time: Stamp::from_nanos(time),
                tags: &map(&tags),
                fields: &map(&fields),
            }]
        );
    }
}
