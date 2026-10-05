//! The verdict of the pinned HCL version on each text in `texts/`, and what `read` and
//! `write` must do with each text. `README.md` tells how to add a text.

// Lets Clippy treat the helpers as test code.
#![cfg(test)]

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::PathBuf;

use config_hcl::{read, write};
use document::Source;
use document::diagnostic::Diagnostic;

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
    let mut failures = Vec::new();
    let mut seen = BTreeSet::new();
    for (name, _) in verdicts() {
        if !texts.contains_key(&name) {
            failures.push(format!("{name}: has a verdict but no text"));
        }
        if !seen.insert(name.clone()) {
            failures.push(format!("{name}: has two verdicts"));
        }
    }
    for name in texts.keys().filter(|name| !seen.contains(*name)) {
        failures.push(format!("{name}: has no verdict; run `go run .`"));
    }
    let mut listed = BTreeSet::new();
    for (name, _) in differences() {
        if !texts.contains_key(&name) {
            failures.push(format!("{name}: is in differences.txt but has no text"));
        }
        if !listed.insert(name.clone()) {
            failures.push(format!("{name}: is in differences.txt twice"));
        }
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
    let differences: BTreeSet<_> =
        differences().into_iter().map(|(name, _)| name).collect();
    let data: BTreeSet<&str> = verdicts()
        .iter()
        .filter(|(name, verdict)| {
            matches!(verdict, Verdict::Accepted(forms) if forms.is_empty())
                && !differences.contains(name)
        })
        .filter_map(|(name, _)| texts.get(name).map(String::as_str))
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
            Ok(written) if data.contains(written.as_str()) => {}
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
