//! `config::check` never panics on the documents of HCL files, gives the same entries
//! for the files in either order or problems in both, and orders its problems as its
//! doc says. Each `\x1e` in the input starts the next file, up to three.

#![no_main]

use std::collections::{BTreeMap, BTreeSet};

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
    passing_files_pass_together(&documents);
    let result = config::check(&documents);
    let mut reversed = documents.clone();
    reversed.reverse();
    let other = config::check(&reversed);
    match &result {
        Ok(entries) => {
            assert_eq!(
                other.as_ref(),
                Ok(entries),
                "the order of the files changed it"
            );
            entries_match(&documents, entries);
        }
        Err(diagnostics) => {
            assert!(other.is_err(), "the order of the files made it pass");
            diagnostics_in_order(&files, diagnostics);
        }
    }
});

/// Files that each pass alone, with keys that differ in more than case, pass together.
fn passing_files_pass_together(documents: &[Document]) {
    let mut passing = Vec::new();
    let mut union = BTreeMap::new();
    let mut keys = BTreeSet::new();
    for document in documents {
        let Ok(entries) = config::check(std::slice::from_ref(document)) else {
            continue;
        };
        for (key, entry) in entries {
            if !keys.insert(key.as_str().to_ascii_lowercase()) {
                return;
            }
            union.insert(key, entry);
        }
        passing.push(document.clone());
    }
    assert_eq!(
        config::check(&passing),
        Ok(union),
        "files that pass alone failed together"
    );
}

/// Each block gives one entry, keyed `<label>.@<keyword>` and unique in any case, with
/// a definition that the spec tree reads back.
fn entries_match(documents: &[Document], entries: &BTreeMap<Name, config::Entry>) {
    let by_key: BTreeMap<String, &config::Entry> = entries
        .iter()
        .map(|(name, entry)| (name.as_str().to_ascii_lowercase(), entry))
        .collect();
    assert_eq!(by_key.len(), entries.len(), "two keys differ only in case");
    let blocks: usize = documents.iter().map(|document| document.blocks.len()).sum();
    assert_eq!(
        entries.len(),
        blocks,
        "a block with no problem gave no entry"
    );
    for block in documents.iter().flat_map(|document| &document.blocks) {
        let [label] = block.labels.as_slice() else {
            panic!("a block with {} labels passed", block.labels.len());
        };
        let key = format!("{}.@{}", label.text, block.keyword).to_ascii_lowercase();
        let definition = &by_key
            .get(&key)
            .unwrap_or_else(|| panic!("no entry for {key}"))
            .definition;
        assert_eq!(
            Definition::decode(&definition.encode()).as_ref(),
            Ok(definition),
            "the definition of {key} changed in its encoding"
        );
    }
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
