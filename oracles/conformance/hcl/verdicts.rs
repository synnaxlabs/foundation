//! The verdict of the pinned HCL version on each text in `texts/`, the values it reads
//! from each text with only data, and what `read` and `write` must do with each text.
//! `README.md` tells how to add a text.

// Lets Clippy treat the helpers as test code.
#![cfg(test)]

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::fs;
use std::iter::once;
use std::path::PathBuf;

use config_hcl::{read, write};
use document::diagnostic::Diagnostic;
use document::value::{Kind, Value};
use document::{Attribute, Document, Map, Source};

/// What HCL does with a text.
enum Verdict {
    Refused,
    /// Accepted, with the codes of the forms outside data in the text.
    Accepted(BTreeSet<String>),
}

/// What `read` gives: a Document, or the codes of its errors.
type Outcome = Result<(), BTreeSet<String>>;

fn directory() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../oracles/conformance/hcl")
}

/// Each text by its name, with its exact bytes.
fn texts() -> BTreeMap<String, String> {
    let mut texts = BTreeMap::new();
    for entry in fs::read_dir(directory().join("texts")).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().is_some_and(|extension| extension == "hcl") {
            let name = path.file_stem().unwrap().to_str().unwrap().to_owned();
            let text = fs::read_to_string(&path)
                .unwrap_or_else(|error| panic!("{}: {error}", path.display()));
            texts.insert(name, text);
        }
    }
    texts
}

/// The lines of a file in this directory, without blank lines and `#` comments.
fn lines(file: &str) -> Vec<String> {
    fs::read_to_string(directory().join(file))
        .unwrap()
        .lines()
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(str::to_owned)
        .collect()
}

/// Each line of `verdicts.txt` as a name and a verdict, in file order.
fn verdicts() -> Vec<(String, Verdict)> {
    lines("verdicts.txt")
        .iter()
        .map(|line| {
            let mut words = line.split(' ');
            let name = words.next().unwrap().to_owned();
            let verdict = match words.next() {
                Some("accepted") => {
                    Verdict::Accepted(words.map(str::to_owned).collect())
                }
                Some("refused") if words.next().is_none() => Verdict::Refused,
                _ => panic!("verdicts.txt: {line:?} is not a verdict"),
            };
            (name, verdict)
        })
        .collect()
}

/// Each line of `differences.txt` as a name and the outcome `read` gives, in file
/// order. The decision after the outcome is for people.
fn differences() -> Vec<(String, Outcome)> {
    lines("differences.txt")
        .iter()
        .map(|line| {
            let mut words = line.splitn(3, ' ');
            let (Some(name), Some(outcome), Some(decision)) =
                (words.next(), words.next(), words.next())
            else {
                panic!("differences.txt: {line:?} has no outcome or no decision")
            };
            assert!(
                !decision.trim().is_empty(),
                "differences.txt: {line:?} has no decision"
            );
            let outcome = match outcome {
                "ok" => Ok(()),
                code => Err(BTreeSet::from([code.to_owned()])),
            };
            (name.to_owned(), outcome)
        })
        .collect()
}

/// Each line of `values.txt` as a name and the values HCL reads, in file order.
fn values() -> Vec<(String, String)> {
    lines("values.txt")
        .iter()
        .map(|line| {
            let (name, values) = line
                .split_once(' ')
                .unwrap_or_else(|| panic!("values.txt: {line:?} has no values"));
            (name.to_owned(), values.to_owned())
        })
        .collect()
}

/// The names of the texts that HCL accepts with no code and that are not in
/// `differences.txt`.
fn data() -> BTreeSet<String> {
    let differences: BTreeSet<_> =
        differences().into_iter().map(|(name, _)| name).collect();
    verdicts()
        .into_iter()
        .filter(|(name, verdict)| {
            matches!(verdict, Verdict::Accepted(forms) if forms.is_empty())
                && !differences.contains(name)
        })
        .map(|(name, _)| name)
        .collect()
}

fn outcome(text: &str) -> Outcome {
    read(Source(0), text).map(drop).map_err(|errors| {
        errors
            .iter()
            .map(|error| Diagnostic::from(error).code.as_str().to_owned())
            .collect()
    })
}

/// Whether `outcome` is what `verdict` asks of `read`: a refused text gives errors, a
/// text with only data gives a Document, and a text with forms outside data gives
/// errors for some of those forms only. `Err` with no error never agrees.
fn agrees(verdict: &Verdict, outcome: &Outcome) -> bool {
    match (verdict, outcome) {
        (_, Err(codes)) if codes.is_empty() => false,
        (Verdict::Refused, outcome) => outcome.is_err(),
        (Verdict::Accepted(forms), Ok(())) => forms.is_empty(),
        (Verdict::Accepted(forms), Err(codes)) => {
            !forms.is_empty() && codes.is_subset(forms)
        }
    }
}

fn show(outcome: &Outcome) -> String {
    outcome.as_ref().map_or_else(join, |()| "ok".into())
}

fn join(codes: &BTreeSet<String>) -> String {
    codes
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .join(" ")
}

/// The failures of `read` against `values.txt`: each text in `data()` that `read`
/// reads must give the values HCL reads.
fn value_failures(read: impl Fn(&str) -> Option<Document>) -> Vec<String> {
    let texts = texts();
    let data = data();
    let mut failures = Vec::new();
    for (name, hcl) in values().into_iter().filter(|(name, _)| data.contains(name)) {
        let Some(document) = texts.get(&name).and_then(|text| read(text)) else {
            continue;
        };
        let ours = body(&document);
        if ours != hcl {
            failures.push(format!("{name}: read gives {ours}, and HCL reads {hcl}"));
        }
    }
    failures
}

/// The form of `values.txt` for a document. `README.md` tells the form.
fn body(document: &Document) -> String {
    let attributes = document.attributes.iter().map(attribute);
    let blocks = document.blocks.iter().map(|block| {
        once(quote(&block.keyword))
            .chain(block.labels.iter().map(|label| quote(&label.text)))
            .chain(once(body(&block.body)))
            .collect::<Vec<_>>()
            .join(" ")
    });
    format!(
        "{{{}}}",
        attributes.chain(blocks).collect::<Vec<_>>().join(", ")
    )
}

fn attribute(item: &Attribute) -> String {
    format!("{} = {}", quote(&item.key), value(&item.value))
}

fn value(item: &Value) -> String {
    let items =
        |values: &[Value]| values.iter().map(value).collect::<Vec<_>>().join(", ");
    match &item.kind {
        Kind::Bool(truth) => truth.to_string(),
        Kind::Integer(integer) => integer.to_string(),
        Kind::Float(float) => format!("f{:016x}", float.get().to_bits()),
        Kind::String(text) => quote(text),
        Kind::Reference(name) => format!("${name}"),
        Kind::List(values) => format!("[{}]", items(values)),
        Kind::Map(map) => format!(
            "{{{}}}",
            map.iter().map(attribute).collect::<Vec<_>>().join(", ")
        ),
        Kind::Call(call) => {
            format!("{}({})", quote(&call.function), items(&call.arguments))
        }
    }
}

fn quote(text: &str) -> String {
    let mut quoted = String::from('"');
    for c in text.chars() {
        match c {
            '"' | '\\' => {
                quoted.push('\\');
                quoted.push(c);
            }
            ' '..='~' => quoted.push(c),
            _ => write!(quoted, "\\u{{{:x}}}", u32::from(c)).unwrap(),
        }
    }
    quoted.push('"');
    quoted
}

/// The names in `file`, with a failure for each name that has no text and each name
/// that `file` lists twice.
fn listed(
    file: &str,
    names: impl IntoIterator<Item = String>,
    texts: &BTreeMap<String, String>,
    failures: &mut Vec<String>,
) -> BTreeSet<String> {
    let mut listed = BTreeSet::new();
    for name in names {
        if !texts.contains_key(&name) {
            failures.push(format!("{name}: is in {file} but has no text"));
        }
        if !listed.insert(name.clone()) {
            failures.push(format!("{name}: is in {file} twice"));
        }
    }
    listed
}

fn fail(failures: &[String]) {
    assert!(
        failures.is_empty(),
        "{} failures:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

#[test]
fn each_text_has_one_verdict() {
    let texts = texts();
    let verdicts = verdicts();
    let mut failures = Vec::new();
    let names = verdicts.iter().map(|(name, _)| name.clone());
    let judged = listed("verdicts.txt", names, &texts, &mut failures);
    for name in texts.keys().filter(|name| !judged.contains(*name)) {
        failures.push(format!("{name}: has no verdict; run `go run .`"));
    }
    let names = differences().into_iter().map(|(name, _)| name);
    listed("differences.txt", names, &texts, &mut failures);
    let names = values().into_iter().map(|(name, _)| name);
    let valued = listed("values.txt", names, &texts, &mut failures);
    let data: BTreeSet<String> = verdicts
        .into_iter()
        .filter_map(|(name, verdict)| match verdict {
            Verdict::Accepted(forms) if forms.is_empty() => Some(name),
            _ => None,
        })
        .collect();
    for name in data.symmetric_difference(&valued) {
        failures.push(format!("{name}: values.txt is out of date; run `go run .`"));
    }
    fail(&failures);
}

#[test]
fn reads_as_hcl_does() {
    let texts = texts();
    let differences: BTreeMap<_, _> = differences().into_iter().collect();
    let mut failures = Vec::new();
    for (name, verdict) in verdicts() {
        let Some(text) = texts.get(&name) else {
            continue;
        };
        let got = outcome(text);
        let failure = match differences.get(&name) {
            Some(listed) if agrees(&verdict, listed) => Some(format!(
                "{name}: the difference {} is what HCL asks; remove it",
                show(listed)
            )),
            Some(listed) if got != *listed => Some(format!(
                "{name}: read gives {}, and differences.txt lists {}",
                show(&got),
                show(listed)
            )),
            None if !agrees(&verdict, &got) => Some(format!(
                "{name}: read gives {}, and HCL {}",
                show(&got),
                match &verdict {
                    Verdict::Refused => "refuses it".into(),
                    Verdict::Accepted(forms) if forms.is_empty() => "accepts it".into(),
                    Verdict::Accepted(forms) => format!("finds {}", join(forms)),
                }
            )),
            _ => None,
        };
        failures.extend(failure);
    }
    fail(&failures);
}

#[test]
fn writes_text_hcl_accepts() {
    let texts = texts();
    let accepted: BTreeSet<&str> = data()
        .iter()
        .filter_map(|name| texts.get(name).map(String::as_str))
        .collect();
    let mut failures = Vec::new();
    for (name, text) in &texts {
        let Ok(document) = read(Source(0), text) else {
            continue;
        };
        match write(&document) {
            Ok(written) if read(Source(0), &written).as_ref() != Ok(&document) => {
                failures.push(format!(
                    "{name}: write gives {written:?}, which reads as \
                     another Document"
                ));
            }
            Ok(written) if accepted.contains(written.as_str()) => {}
            Ok(written) => failures.push(format!(
                "{name}: write gives {written:?}, which is not a text with only \
                 data; add it as a text and run `go run .`"
            )),
            Err(errors) => {
                failures.push(format!("{name}: write fails with {errors:?}"));
            }
        }
    }
    fail(&failures);
}

#[test]
fn reads_the_values_hcl_reads() {
    fail(&value_failures(|text| read(Source(0), text).ok()));
}

#[test]
fn finds_a_read_that_gives_empty_documents() {
    let failures =
        value_failures(|text| read(Source(0), text).ok().map(|_| Document::default()));
    let expected = r#"attribute: read gives {}, and HCL reads {"a" = 1}"#;
    assert!(
        failures.iter().any(|failure| failure == expected),
        "{failures:#?}"
    );
}

#[test]
fn finds_a_read_that_takes_an_escape_wrong() {
    let failures =
        value_failures(|text| read(Source(0), &text.replace(r"\t", "t")).ok());
    let ours = r#"{"a" = "\u{a}\u{d}t\"\\\u{e9}\u{1f600}"}"#;
    let hcl = r#"{"a" = "\u{a}\u{d}\u{9}\"\\\u{e9}\u{1f600}"}"#;
    let expected = format!("string-escapes: read gives {ours}, and HCL reads {hcl}");
    assert!(failures.contains(&expected), "{failures:#?}");
}

#[test]
fn finds_a_read_that_gives_references_for_bools() {
    let failures = value_failures(|text| {
        let mut document = read(Source(0), text).ok()?;
        let mut attributes: Vec<Attribute> =
            document.attributes.iter().cloned().collect();
        for item in &mut attributes {
            if let Kind::Bool(truth) = item.value.kind {
                item.value.kind = Kind::Reference(truth.to_string().parse().ok()?);
            }
        }
        document.attributes = Map::new(attributes).ok()?;
        Some(document)
    });
    let ours = r#"{"a" = $true, "b" = $false}"#;
    let hcl = r#"{"a" = true, "b" = false}"#;
    let expected = format!("bool: read gives {ours}, and HCL reads {hcl}");
    assert!(failures.contains(&expected), "{failures:#?}");
}
