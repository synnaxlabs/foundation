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
        _ => Value::Float(text.parse().unwrap()),
    }
}

/// Parses one line of line protocol, as InfluxDB reads it.
fn parse(line: &str) -> Parsed {
    let line = line.strip_suffix('\n').expect("a line ends with a newline");
    let [head, fields, time] = split(line, ' ')[..] else {
        panic!("not three parts: {line:?}");
    };
    let head = split(head, ',');
    let tags = head.iter().skip(1).map(|tag| {
        let (key, value) = pair(tag);
        (key, unescape(value))
    });
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

fn key(key: &str) -> Key {
    Key::new(key).unwrap()
}

fn text(out: &[u8]) -> &str {
    std::str::from_utf8(out).unwrap()
}

#[test]
fn writes_one_known_line() {
    let measurement =
        Measurement::new("plant", &[("site", "west"), ("line", "a b")]).unwrap();
    let (temp, count, total, open) = (key("temp"), key("n"), key("total"), key("open"));
    let mut out = b"before\n".to_vec();
    let fields = [
        (&temp, Value::Float(21.5)),
        (&count, Value::Integer(-3)),
        (&total, Value::Unsigned(7)),
        (&open, Value::Boolean(true)),
    ];
    let written = measurement.line(&mut out, fields, Stamp::from_nanos(1_000));
    assert_eq!(written, 4);
    assert_eq!(
        text(&out),
        "before\nplant,line=a\\ b,site=west temp=2.15e1,n=-3i,total=7u,open=t 1000\n"
    );
}

#[test]
fn escapes_what_line_protocol_reads_as_syntax() {
    let measurement = Measurement::new("a,b c=d", &[("k,=  ", "v,= ")]).unwrap();
    let field = key("f,= ");
    let mut out = Vec::new();
    measurement.line(&mut out, [(&field, Value::Boolean(false))], Stamp::EPOCH);
    assert_eq!(
        text(&out),
        "a\\,b\\ c=d,k\\,\\=\\ \\ =v\\,\\=\\  f\\,\\=\\ =f 0\n"
    );
}

#[test]
fn leaves_out_a_float_that_is_not_finite() {
    let measurement = Measurement::new("m", &[]).unwrap();
    let (a, b) = (key("a"), key("b"));
    let mut out = b"x".to_vec();
    for bad in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
        let fields = [(&a, Value::Float(bad)), (&b, Value::Float(bad))];
        assert_eq!(measurement.line(&mut out, fields, Stamp::EPOCH), 0);
        assert_eq!(out, b"x", "no field left, so nothing is written");
        let fields = [(&a, Value::Float(bad)), (&b, Value::Integer(1))];
        assert_eq!(measurement.line(&mut out, fields, Stamp::EPOCH), 1);
        assert_eq!(text(&out), "xm b=1i 0\n");
        out.truncate(1);
    }
    assert_eq!(measurement.line(&mut out, [], Stamp::EPOCH), 0);
    assert_eq!(out, b"x");
}

#[test]
fn refuses_a_name_line_protocol_cannot_carry() {
    let cases = [
        (
            Measurement::new("", &[]).err(),
            Error::Empty,
            "a name is empty",
        ),
        (
            Measurement::new("a\\b", &[]).err(),
            Error::Character {
                name: "a\\b".into(),
                character: '\\',
            },
            "the name \"a\\\\b\" holds '\\\\', which line protocol cannot carry",
        ),
        (
            Measurement::new("m", &[("k", "a\nb")]).err(),
            Error::Character {
                name: "a\nb".into(),
                character: '\n',
            },
            "the name \"a\\nb\" holds '\\n', which line protocol cannot carry",
        ),
        (
            Measurement::new("m", &[("k\r", "v")]).err(),
            Error::Character {
                name: "k\r".into(),
                character: '\r',
            },
            "the name \"k\\r\" holds '\\r', which line protocol cannot carry",
        ),
        (
            Measurement::new("m", &[("k", "")]).err(),
            Error::Empty,
            "a name is empty",
        ),
        (
            Measurement::new("_m", &[]).err(),
            Error::Reserved("_m".into()),
            "the name \"_m\" starts with '_', which InfluxDB keeps for itself",
        ),
        (
            Measurement::new("m", &[("_k", "v")]).err(),
            Error::Reserved("_k".into()),
            "the name \"_k\" starts with '_', which InfluxDB keeps for itself",
        ),
        (
            Measurement::new("m", &[("k", "a"), ("k", "b")]).err(),
            Error::Duplicate("k".into()),
            "the tag key \"k\" comes more than once",
        ),
    ];
    for (got, error, message) in cases {
        assert_eq!(got, Some(error.clone()));
        assert_eq!(error.to_string(), message);
    }
}

#[test]
fn checks_a_field_key_as_a_tag_key() {
    assert_eq!(Key::new("").err(), Some(Error::Empty));
    assert_eq!(Key::new("_f").err(), Some(Error::Reserved("_f".into())));
    assert_eq!(
        Key::new("f\\").err(),
        Some(Error::Character {
            name: "f\\".into(),
            character: '\\'
        })
    );
    let tag = Measurement::new("m", &[("k", "_v")]).unwrap();
    let mut out = Vec::new();
    tag.line(&mut out, [(&key("f"), Value::Integer(0))], Stamp::EPOCH);
    assert_eq!(
        text(&out),
        "m,k=_v f=0i 0\n",
        "a tag value may start with '_'"
    );
}

fn name() -> impl Strategy<Value = String> {
    "[^_\\\\\n\r][^\\\\\n\r]{0,8}"
}

fn field_value() -> impl Strategy<Value = Value> {
    prop_oneof![
        any::<f64>()
            .prop_filter("finite", |f| f.is_finite())
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
        tags in proptest::collection::btree_map(name(), name(), 0..4),
        fields in proptest::collection::vec((name(), field_value()), 1..5),
        time in any::<i64>(),
    ) {
        let tags: Vec<(String, String)> = tags.into_iter().collect();
        let borrowed: Vec<(&str, &str)> =
            tags.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
        let line = Measurement::new(&measurement, &borrowed).unwrap();
        let keys: Vec<Key> = fields.iter().map(|(k, _)| key(k)).collect();
        let mut out = Vec::new();
        let pairs = keys.iter().zip(fields.iter().map(|&(_, v)| v));
        let written = line.line(&mut out, pairs, Stamp::from_nanos(time));
        prop_assert_eq!(written, fields.len());
        prop_assert_eq!(parse(text(&out)), (measurement, tags, fields, time));
    }
}
