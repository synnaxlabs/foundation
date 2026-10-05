//! The readers of patterns and selectors never panic, and they match the names that
//! a second matcher, written from the text of the patterns, matches.
//!
//! Input: lines. The first line is a name; the others are a selector's patterns.

#![no_main]

use libfuzzer_sys::fuzz_target;
use types::name::{Name, Pattern, Selector};

fuzz_target!(|text: &str| {
    let mut lines = text.lines();
    let Ok(name) = lines.next().unwrap_or_default().parse::<Name>() else {
        return;
    };
    let lines: Vec<&str> = lines.collect();
    let mut expected = None;
    let mut excluded = false;
    for line in &lines {
        let body = line.strip_prefix('!').unwrap_or(line);
        let Ok(pattern) = body.parse::<Pattern>() else {
            continue;
        };
        let matched = matches(body, &name);
        assert_eq!(pattern.matches(&name), matched, "{body:?} on {name}");
        if !matched {
            continue;
        }
        if body.len() < line.len() {
            excluded = true;
        } else {
            expected = expected.max(Some(pattern.specificity()));
        }
    }
    if let Ok(selector) = Selector::new(lines.iter().copied()) {
        let expected = if excluded { None } else { expected };
        assert_eq!(selector.matches(&name), expected, "the selector on {name}");
    }
});

/// Reports whether `pattern` matches `name`. `reach[i]` is true when the pattern
/// segments read so far can match the first `i` segments of the name.
fn matches(pattern: &str, name: &Name) -> bool {
    let name: Vec<&str> = name.segments().collect();
    let mut reach = vec![false; name.len() + 1];
    reach[0] = true;
    for segment in pattern.split('.') {
        let mut next = vec![false; reach.len()];
        match segment {
            "**" => {
                let mut on = false;
                for (next, reach) in next.iter_mut().zip(&reach) {
                    on |= reach;
                    *next = on;
                }
            }
            "*" => next[1..].copy_from_slice(&reach[..name.len()]),
            literal => {
                for (i, part) in name.iter().enumerate() {
                    next[i + 1] = reach[i] && *part == literal;
                }
            }
        }
        reach = next;
    }
    reach[name.len()]
}
