//! `config::profile::decode` never panics on the document of a profile. A profile that
//! decodes names a subject when it names a key file, and the `node` block of each pin
//! decodes alone to that pin. A profile that does not gives each problem with a message
//! and a fix, in source order, and only `config.private-key` when the text holds a
//! private key.

#![no_main]
#![expect(clippy::disallowed_methods, reason = "fuzz_target! calls File::create")]

use std::collections::BTreeMap;

use config::profile;
use document::{Document, Source};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|text: &str| {
    let Ok(document) = config_hcl::read(Source(0), text) else {
        return;
    };
    match profile::decode(&document) {
        Ok(profile) => {
            assert!(
                profile.private_key.is_none() || profile.subject.is_some(),
                "a key file with no subject"
            );
            for (&address, &key) in &profile.nodes {
                let block = Document {
                    blocks: vec![profile::node(address, key)],
                    ..Document::default()
                };
                let nodes = profile::decode(&block).expect("a pin decodes").nodes;
                assert_eq!(nodes, BTreeMap::from([(address, key)]), "{address}");
            }
        }
        Err(diagnostics) => {
            assert!(!diagnostics.is_empty(), "an error with no problem");
            let alarm = diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code.as_str() == "config.private-key");
            let mut last = 0;
            for diagnostic in &diagnostics {
                assert!(
                    !diagnostic.message.is_empty() && !diagnostic.fix.is_empty(),
                    "{diagnostic:?} has no message or no fix"
                );
                assert!(
                    !alarm || diagnostic.code.as_str() == "config.private-key",
                    "{diagnostic:?} beside a private key alarm"
                );
                let Some(span) = diagnostic.span else {
                    continue;
                };
                let at = span.start().offset;
                assert!(
                    usize::try_from(span.end().offset).unwrap() <= text.len(),
                    "{diagnostic:?} ends past the text"
                );
                assert!(at >= last, "{diagnostic:?} is out of order");
                last = at;
            }
        }
    }
});
