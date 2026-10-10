use std::collections::BTreeMap;

use document::Label;
use document::diagnostic::{Code, Diagnostic, Note};
use spec::definition::{Definition, Kind};
use types::name::Name;

const DUPLICATE_NAME: Code = Code::new("config.duplicate-name");

/// Each label of each tree key, with the kind of its block, by the key in ASCII lower
/// case, so that keys that differ only in case collide.
pub(crate) type Labels<'a> = BTreeMap<Box<str>, Vec<(&'a Label, Kind)>>;

/// Adds `key`, whose label is `label`, to `labels`.
pub(crate) fn add<'a>(
    labels: &mut Labels<'a>,
    key: &Name,
    label: &'a Label,
    kind: Kind,
) {
    let lower = key.as_str().to_ascii_lowercase().into();
    labels.entry(lower).or_default().push((label, kind));
}

/// `config.duplicate-name` at each label of a tree key of `labels` after the first in
/// span order, no span first. Labels with no span keep the order that `labels` holds
/// them in.
pub(crate) fn in_labels(labels: &mut Labels<'_>) -> Vec<Diagnostic> {
    let mut diagnostics = Vec::new();
    for labels in labels.values_mut() {
        labels.sort_by_key(|(label, _)| label.span);
        let (first, earlier) = labels[0];
        for &(label, later) in &labels[1..] {
            diagnostics.push(repeats((label, later), (first, earlier)));
        }
    }
    diagnostics
}

/// `config.duplicate-name` at each tree key of `definitions` that repeats an earlier
/// one in name order, in another ASCII case, with no span.
pub(crate) fn in_definitions(
    definitions: &BTreeMap<Name, Definition>,
) -> Vec<Diagnostic> {
    let found: Vec<_> = definitions
        .iter()
        .map(|(key, definition)| {
            let kind = definition.kind();
            let text = crate::label(kind, key).as_str().into();
            (key, Label { text, span: None }, kind)
        })
        .collect();
    let mut labels = Labels::new();
    for (key, label, kind) in &found {
        add(&mut labels, key, label, *kind);
    }
    in_labels(&mut labels)
}

/// `config.duplicate-name` at `label`, whose tree key repeats the earlier `first` in
/// another ASCII case.
fn repeats(
    (label, later): (&Label, Kind),
    (first, earlier): (&Label, Kind),
) -> Diagnostic {
    let (earlier, keyword) = (earlier.as_str(), later.as_str());
    let blocks = if earlier == keyword {
        format!("`{keyword}`")
    } else {
        format!("`{earlier}` and `{keyword}`")
    };
    let mut diagnostic = Diagnostic::new(
        DUPLICATE_NAME,
        label.span,
        format!(
            "the name {:?} repeats the earlier `{earlier}` name {:?}",
            label.text, first.text
        ),
        format!("Give each {blocks} block a name that differs by more than case"),
    );
    diagnostic.notes.extend(first.span.map(|span| Note {
        span,
        text: "the earlier name".into(),
    }));
    diagnostic
}
