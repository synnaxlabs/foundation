//! The readers of names, patterns, and selectors never panic, a name prints as the
//! text it was read from, and a selector agrees with its patterns.
//!
//! Input: lines. The first line is a name; the others are a selector's patterns.

#![no_main]

use libfuzzer_sys::fuzz_target;
use types::name::{Name, Pattern, Selector};

fuzz_target!(|text: &str| {
    let mut lines = text.lines();
    let name = lines.next().unwrap_or_default();
    let patterns: Vec<&str> = lines.collect();
    let name = name.parse::<Name>().inspect(|read| {
        assert_eq!(read.to_string(), name, "the name changed");
    });
    let selector = Selector::new(patterns.iter().copied());
    let (Ok(name), Ok(selector)) = (name, selector) else {
        return;
    };
    let matches = |pattern: &&str| {
        pattern
            .parse::<Pattern>()
            .expect("a selector read a pattern that does not read")
            .matches(&name)
    };
    let included = patterns.iter().filter(|p| !p.starts_with('!')).any(matches);
    let excluded = patterns
        .iter()
        .filter_map(|p| p.strip_prefix('!'))
        .any(|p| matches(&p));
    assert_eq!(
        selector.matches(&name).is_some(),
        included && !excluded,
        "the selector and its patterns disagree"
    );
});
