//! `config::check` never panics on the documents of HCL files, gives the same entries
//! or the same problems for the files in either order, and orders its problems as its
//! doc says. Files that pass alone, with keys that differ in more than case and no
//! subject named as a connector in any ASCII case, pass together and give the union
//! of their entries. Each `\x1e` in the input starts the next file, up to three. The
//! kind table has the influx kind.

#![no_main]
#![expect(clippy::disallowed_methods, reason = "fuzz_target! calls File::create")]

use std::collections::{BTreeMap, BTreeSet};

use connector::kind::Table;
use document::diagnostic::Diagnostic;
use document::{Document, Source};
use libfuzzer_sys::fuzz_target;
use spec::definition::Definition;
use types::name::Name;

fuzz_target!(|text: &str| {
    let files: Vec<&str> = text.splitn(3, '\x1e').collect();
    let mut documents = Vec::new();
    for (index, file) in (0..).zip(&files) {
        let Ok(document) = config_hcl::read(Source(index), file) else {
            return;
        };
        documents.push(document);
    }
    let kinds = Table::new().with("influx", connector_influx::Kind::default());
    passing_files_pass_together(&documents, &kinds);
    let result = config::check(&documents, &kinds);
    let mut reversed = documents.clone();
    reversed.reverse();
    let other = config::check(&reversed, &kinds);
    match &result {
        Ok(entries) => {
            assert_eq!(
                other.as_ref(),
                Ok(entries),
                "the order of the files changed it"
            );
            entries_match(&documents, entries, &kinds);
        }
        Err(diagnostics) => {
            assert_eq!(
                other.as_ref(),
                Err(diagnostics),
                "the order of the files changed it"
            );
            diagnostics_in_order(&files, diagnostics);
        }
    }
});

/// Files that each pass alone, with keys that differ in more than case and no subject
/// named as a connector in any ASCII case, pass together.
fn passing_files_pass_together(documents: &[Document], kinds: &Table) {
    let mut passing = Vec::new();
    let mut union = BTreeMap::new();
    let mut keys = BTreeSet::new();
    let mut connectors = BTreeSet::new();
    let mut subjects = BTreeSet::new();
    for document in documents {
        let Ok(entries) = config::check(std::slice::from_ref(document), kinds) else {
            continue;
        };
        for (key, entry) in entries {
            let lower = key.as_str().to_ascii_lowercase();
            match &entry.definition {
                config::Definition::Spec(Definition::Connector(_)) => {
                    connectors.insert(lower.clone());
                }
                config::Definition::Spec(Definition::Subject(_)) => {
                    let label = lower.strip_suffix(".@subject").expect("a subject key");
                    subjects.insert(label.to_owned());
                }
                _ => {}
            }
            if !keys.insert(lower) || !connectors.is_disjoint(&subjects) {
                return;
            }
            union.insert(key, entry);
        }
        passing.push(document.clone());
    }
    assert_eq!(
        config::check(&passing, kinds),
        Ok(union),
        "files that pass alone failed together"
    );
}

/// Each block gives one entry, keyed by its label for a channel or a connector, or
/// `<label>.@<keyword>` for each other block. Each connector also gives the status
/// channels that `connector::status::channels` names, and no other entry is there.
/// Keys are unique in any case. A policy or a connector has a definition that the spec
/// tree reads back, and each edge of a channel names a channel entry.
fn entries_match(
    documents: &[Document],
    entries: &BTreeMap<Name, config::Entry>,
    kinds: &Table,
) {
    let by_key: BTreeMap<String, &config::Entry> = entries
        .iter()
        .map(|(name, entry)| (name.as_str().to_ascii_lowercase(), entry))
        .collect();
    assert_eq!(by_key.len(), entries.len(), "two keys differ only in case");
    let mut keys = BTreeSet::new();
    for block in documents.iter().flat_map(|document| &document.blocks) {
        let [label] = block.labels.as_slice() else {
            panic!("a block with {} labels passed", block.labels.len());
        };
        let key = match &*block.keyword {
            "channel" | "connector" => label.text.to_string(),
            keyword => format!("{}.@{keyword}", label.text),
        };
        let key = key.to_ascii_lowercase();
        assert!(keys.insert(key.clone()), "two blocks have the key {key}");
    }
    for (key, entry) in entries {
        match &entry.definition {
            config::Definition::Spec(definition) => assert_eq!(
                Definition::decode(&definition.encode()).as_ref(),
                Ok(definition),
                "the definition of {key} changed in its encoding"
            ),
            config::Definition::Channel(kind) => {
                for (edge, to) in kind.edges() {
                    assert!(
                        matches!(
                            entries.get(to).map(|entry| &entry.definition),
                            Some(config::Definition::Channel(_))
                        ),
                        "the {edge} of {key} is {to}, which is not a channel entry"
                    );
                }
            }
            _ => panic!("{key} gave a definition that this target does not know"),
        }
        if let config::Definition::Spec(Definition::Connector(connector)) =
            &entry.definition
        {
            let config = connector.config().document();
            let channels = kinds
                .check(connector.kind().as_str(), None, config)
                .expect("the kind of a connector that passed takes its config");
            let (time, status) = connector::status::channels(key, &channels.counts)
                .expect("the status names of a connector that passed fit");
            let status = status.into_iter().map(|(name, _)| name);
            let implied = std::iter::once(time).chain(status);
            for name in implied {
                let name = name.as_str().to_ascii_lowercase();
                assert!(
                    keys.insert(name.clone()),
                    "a block has the key of the status channel {name}"
                );
            }
        }
    }
    assert_eq!(
        by_key.keys().cloned().collect::<BTreeSet<_>>(),
        keys,
        "the entries are not those of the blocks and the status channels"
    );
}

/// Each problem with a span is in its file, in the order of the files, then of the
/// source.
fn diagnostics_in_order(files: &[&str], diagnostics: &[Diagnostic]) {
    assert!(!diagnostics.is_empty(), "an error with no problem");
    let mut last = (0, 0);
    for diagnostic in diagnostics {
        assert!(
            !diagnostic.message.is_empty() && !diagnostic.fix.is_empty(),
            "{diagnostic:?} has no message or no fix"
        );
        let Some(span) = diagnostic.span else {
            continue;
        };
        let at = (span.source().0, span.start().offset);
        let file = files[usize::try_from(at.0).unwrap()];
        assert!(
            usize::try_from(span.end().offset).unwrap() <= file.len(),
            "{diagnostic:?} ends past its file"
        );
        assert!(at >= last, "{diagnostic:?} is out of order");
        last = at;
    }
}
