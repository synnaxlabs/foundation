#![expect(clippy::arithmetic_side_effects, reason = "a test may panic")]

use proptest::prelude::*;

use super::*;

/// One parsed line: measurement, tags, fields, and time.
type Parsed = (String, Vec<(String, String)>, Vec<(String, Value)>, i64);

/// Splits `text` at each `split` that no backslash escapes, and keeps the escapes.
fn split(text: &str, split: char) -> Vec<&str> {
    let (mut parts, mut from, mut escaped) = (Vec::new(), 0, false);
    for (at, c) in text.char_indices() {
        if !escaped && c == split {
            parts.push(text.get(from..at).unwrap());
            from = at + 1;
        }
        escaped = !escaped && c == '\\';
    }
    parts.push(text.get(from..).unwrap());
    parts
}

/// Drops the backslash of each escape.
fn unescape(text: &str) -> String {
    let (mut out, mut chars) = (String::new(), text.chars());
    while let Some(c) = chars.next() {
        out.push(if c == '\\' { chars.next().unwrap() } else { c });
    }
    out
}

/// Splits `pair` at its first `=` that no backslash escapes.
fn pair(pair: &str) -> (String, &str) {
    let parts = split(pair, '=');
    let key = parts.first().unwrap();
    (unescape(key), pair.get(key.len() + 1..).unwrap())
}

fn value(text: &str) -> Value {
    match text {
        "t" => Value::Boolean(true),
        "f" => Value::Boolean(false),
        _ if text.ends_with('i') => {
            Value::Integer(text.trim_end_matches('i').parse().unwrap())
        }
        _ if text.ends_with('u') => {
            Value::Unsigned(text.trim_end_matches('u').parse().unwrap())
        }
        _ => Value::Float(float(text.parse().unwrap())),
    }
}

/// Parses one line of line protocol, as InfluxDB reads it.
fn parse(line: &str) -> Parsed {
    let line = line.strip_suffix('\n').expect("a line ends with a newline");
    let line = line.trim_start_matches([' ', '\t', '\0']);
    assert!(!line.starts_with('#'), "InfluxDB drops a comment: {line:?}");
    let [head, fields, time] = split(line, ' ')[..] else {
        panic!("not three parts: {line:?}");
    };
    let head = split(head, ',');
    let tags = head.iter().skip(1).map(|tag| {
        let (key, value) = pair(tag);
        (key, unescape(value))
    });
    let fields = fields.trim_start_matches([' ', '\t', '\0']);
    let fields = split(fields, ',').into_iter().map(|field| {
        let (key, text) = pair(field);
        (key, value(text))
    });
    (
        unescape(head.first().unwrap()),
        tags.collect(),
        fields.collect(),
        time.parse().unwrap(),
    )
}

fn float(value: f64) -> Float {
    Float::new(value).unwrap()
}

fn text(out: &[u8]) -> &str {
    std::str::from_utf8(out).unwrap()
}

#[test]
fn writes_one_known_line() {
    let measurement = Measurement::new(
        "plant",
        &[("site", "west"), ("line", "a b")],
        &["temp", "n", "total", "open"],
    )
    .unwrap();
    let mut out = b"before\n".to_vec();
    let values = [
        Some(Value::Float(float(21.5))),
        Some(Value::Integer(-3)),
        Some(Value::Unsigned(7)),
        Some(Value::Boolean(true)),
    ];
    measurement.line(&mut out, &values, Stamp::from_nanos(1_000));
    assert_eq!(
        text(&out),
        "before\nplant,line=a\\ b,site=west temp=2.15e1,n=-3i,total=7u,open=t 1000\n"
    );
}

#[test]
fn escapes_as_the_specification_example() {
    let measurement =
        Measurement::new("my Measurement", &[("tag Key1", "tag Value1")], &["f"])
            .unwrap();
    let mut out = Vec::new();
    measurement.line(&mut out, &[Some(Value::Integer(1))], Stamp::EPOCH);
    assert_eq!(
        text(&out),
        "my\\ Measurement,tag\\ Key1=tag\\ Value1 f=1i 0\n"
    );
}

#[test]
fn escapes_what_line_protocol_reads_as_syntax() {
    let measurement =
        Measurement::new("a,b c=d", &[("k,=  ", "v,= ")], &["f,= "]).unwrap();
    let mut out = Vec::new();
    measurement.line(&mut out, &[Some(Value::Boolean(false))], Stamp::EPOCH);
    assert_eq!(
        text(&out),
        "a\\,b\\ c=d,k\\,\\=\\ \\ =v\\,\\=\\  f\\,\\=\\ =f 0\n"
    );
}

#[test]
fn leaves_out_a_field_with_no_value() {
    let measurement = Measurement::new("m", &[], &["a", "b"]).unwrap();
    let mut out = b"x".to_vec();
    measurement.line(&mut out, &[None, None], Stamp::EPOCH);
    assert_eq!(out, b"x", "no value, so nothing is written");
    measurement.line(&mut out, &[None, Some(Value::Integer(1))], Stamp::EPOCH);
    assert_eq!(text(&out), "xm b=1i 0\n");
}

#[test]
#[should_panic(expected = "one value for each field")]
fn panics_on_a_value_count_that_is_not_the_field_count() {
    let measurement = Measurement::new("m", &[], &["a"]).unwrap();
    measurement.line(&mut Vec::new(), &[], Stamp::EPOCH);
}

#[test]
fn refuses_a_float_that_is_not_finite() {
    for bad in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
        assert_eq!(Float::new(bad), None);
    }
    assert_eq!(Float::new(-0.5).map(Float::get), Some(-0.5));
}

fn refuses(cases: &[(Result<Measurement, Error>, Error, &str)]) {
    for (got, error, message) in cases {
        assert_eq!(got, &Err(error.clone()));
        assert_eq!(error.to_string(), *message);
    }
}

#[test]
fn refuses_a_measurement_line_protocol_cannot_carry() {
    refuses(&[
        (
            Measurement::new("", &[], &["f"]),
            Error::Empty(Part::Measurement),
            "the measurement name is empty",
        ),
        (
            Measurement::new("m", &[("", "v")], &["f"]),
            Error::Empty(Part::TagKey),
            "a tag key is empty",
        ),
        (
            Measurement::new("m", &[("k", "")], &["f"]),
            Error::Empty(Part::TagValue("k".into())),
            "the value of the tag \"k\" is empty",
        ),
        (
            Measurement::new("m", &[], &[""]),
            Error::Empty(Part::FieldKey),
            "a field key is empty",
        ),
        (
            Measurement::new("a\\b", &[], &["f"]),
            Error::Character {
                name: "a\\b".into(),
                character: '\\',
            },
            "the name \"a\\\\b\" holds '\\\\', which line protocol cannot carry",
        ),
        (
            Measurement::new("m", &[("k", "a\nb")], &["f"]),
            Error::Character {
                name: "a\nb".into(),
                character: '\n',
            },
            "the name \"a\\nb\" holds '\\n', which line protocol cannot carry",
        ),
        (
            Measurement::new("m", &[], &["f\r"]),
            Error::Character {
                name: "f\r".into(),
                character: '\r',
            },
            "the name \"f\\r\" holds '\\r', which line protocol cannot carry",
        ),
    ]);
}

#[test]
fn refuses_a_tab_or_nul_in_any_part() {
    refuses(&[
        (
            Measurement::new("\t#m", &[], &["f"]),
            Error::Character {
                name: "\t#m".into(),
                character: '\t',
            },
            "the name \"\\t#m\" holds '\\t', which line protocol cannot carry",
        ),
        (
            Measurement::new("m\0x", &[], &["f"]),
            Error::Character {
                name: "m\0x".into(),
                character: '\0',
            },
            "the name \"m\\0x\" holds '\\0', which line protocol cannot carry",
        ),
        (
            Measurement::new("m", &[("\tk", "v")], &["f"]),
            Error::Character {
                name: "\tk".into(),
                character: '\t',
            },
            "the name \"\\tk\" holds '\\t', which line protocol cannot carry",
        ),
        (
            Measurement::new("m", &[("k", "a\tb")], &["f"]),
            Error::Character {
                name: "a\tb".into(),
                character: '\t',
            },
            "the name \"a\\tb\" holds '\\t', which line protocol cannot carry",
        ),
        (
            Measurement::new("m", &[], &["f", "\tf"]),
            Error::Character {
                name: "\tf".into(),
                character: '\t',
            },
            "the name \"\\tf\" holds '\\t', which line protocol cannot carry",
        ),
        (
            Measurement::new("m", &[], &["f\0g"]),
            Error::Character {
                name: "f\0g".into(),
                character: '\0',
            },
            "the name \"f\\0g\" holds '\\0', which line protocol cannot carry",
        ),
    ]);
}

#[test]
fn refuses_a_name_influxdb_keeps_or_repeats() {
    refuses(&[
        (
            Measurement::new("_m", &[], &["f"]),
            Error::Reserved("_m".into()),
            "InfluxDB keeps the name \"_m\" for itself",
        ),
        (
            Measurement::new("m", &[("_k", "v")], &["f"]),
            Error::Reserved("_k".into()),
            "InfluxDB keeps the name \"_k\" for itself",
        ),
        (
            Measurement::new("m", &[], &["_f"]),
            Error::Reserved("_f".into()),
            "InfluxDB keeps the name \"_f\" for itself",
        ),
        (
            Measurement::new("m", &[("time", "v")], &["f"]),
            Error::Reserved("time".into()),
            "InfluxDB keeps the name \"time\" for itself",
        ),
        (
            Measurement::new("m", &[], &["time"]),
            Error::Reserved("time".into()),
            "InfluxDB keeps the name \"time\" for itself",
        ),
        (
            Measurement::new("#m", &[], &["f"]),
            Error::Comment("#m".into()),
            "the name \"#m\" starts with '#', which makes each line a comment",
        ),
        (
            Measurement::new("m", &[("k", "a"), ("k", "b")], &["f"]),
            Error::Duplicate("k".into()),
            "the key \"k\" comes more than once",
        ),
        (
            Measurement::new("m", &[("k", "a")], &["f", "k"]),
            Error::Duplicate("k".into()),
            "the key \"k\" comes more than once",
        ),
        (
            Measurement::new("m", &[], &["f", "f"]),
            Error::Duplicate("f".into()),
            "the key \"f\" comes more than once",
        ),
        (
            Measurement::new("m", &[], &[]),
            Error::NoField,
            "a measurement needs at least one field",
        ),
    ]);
}

#[test]
fn allows_what_is_reserved_only_elsewhere() {
    let measurement =
        Measurement::new("time", &[("k", "_v"), ("t", "#")], &["#f"]).unwrap();
    let mut out = Vec::new();
    measurement.line(&mut out, &[Some(Value::Integer(0))], Stamp::EPOCH);
    assert_eq!(text(&out), "time,k=_v,t=# #f=0i 0\n");
}

fn name() -> impl Strategy<Value = String> {
    "[^_#\\\\\n\r\t\0][^\\\\\n\r\t\0]{0,8}"
        .prop_filter("not time", |name| name != "time")
}

fn field_value() -> impl Strategy<Value = Value> {
    prop_oneof![
        any::<f64>()
            .prop_filter_map("finite", Float::new)
            .prop_map(Value::Float),
        any::<i64>().prop_map(Value::Integer),
        any::<u64>().prop_map(Value::Unsigned),
        any::<bool>().prop_map(Value::Boolean),
    ]
}

proptest! {
    #[test]
    fn each_line_parses_back(
        measurement in name(),
        keys in proptest::collection::btree_map(
            name(),
            (any::<bool>(), name(), field_value()),
            1..8,
        ),
        time in any::<i64>(),
    ) {
        let mut tags = Vec::new();
        let mut fields = Vec::new();
        let mut keys_of_fields = Vec::new();
        for (key, (tag, value, field)) in &keys {
            if *tag {
                tags.push((key.clone(), value.clone()));
            } else {
                keys_of_fields.push(key.as_str());
                fields.push((key.clone(), *field));
            }
        }
        prop_assume!(!fields.is_empty());
        let borrowed: Vec<(&str, &str)> =
            tags.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
        let line = Measurement::new(&measurement, &borrowed, &keys_of_fields).unwrap();
        let values: Vec<Option<Value>> = fields.iter().map(|&(_, v)| Some(v)).collect();
        let mut out = Vec::new();
        line.line(&mut out, &values, Stamp::from_nanos(time));
        prop_assert_eq!(parse(text(&out)), (measurement, tags, fields, time));
    }

    #[test]
    fn refuses_a_tab_or_nul_anywhere(
        name in name(),
        at in any::<proptest::sample::Index>(),
        character in proptest::sample::select(vec!['\t', '\0']),
        part in 0..4_usize,
    ) {
        let mut chars: Vec<char> = name.chars().collect();
        chars.insert(at.index(chars.len() + 1), character);
        let name: String = chars.into_iter().collect();
        let n = name.as_str();
        let got = match part {
            0 => Measurement::new(n, &[("k", "v")], &["f"]),
            1 => Measurement::new("m", &[(n, "v")], &["f"]),
            2 => Measurement::new("m", &[("k", n)], &["f"]),
            _ => Measurement::new("m", &[("k", "v")], &["f", n]),
        };
        prop_assert_eq!(got, Err(Error::Character { name, character }));
    }
}
