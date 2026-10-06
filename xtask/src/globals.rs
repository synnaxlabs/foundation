//! The globals check. Foundation holds no global state, so the only `static` item is
//! the `#[global_allocator]` that COUNTING ALLOCATOR allows in a test or benchmark
//! binary. Clippy's `disallowed-macros` refuses `global_allocator` and `thread_local`,
//! and only an attribute at the root of a crate lifts that lint, so this check allows
//! the lift only in the root file of a test or benchmark target.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::{field, files, select};

/// Checks every Rust file of the workspace at `root` but the sample workspace in
/// `xtask/fixture`. A file may hold a `static` item only right after
/// `#[global_allocator]`, and may name `disallowed_macros` only when it is the root
/// file of a test or benchmark target. Comments and literals do not count.
pub(crate) fn check(root: &Path) -> Result<(), Vec<String>> {
    let problems = problems(root).map_err(|e| vec![e])?;
    if problems.is_empty() {
        Ok(())
    } else {
        Err(problems)
    }
}

fn problems(root: &Path) -> Result<Vec<String>, String> {
    let metadata = crate::metadata(root)?;
    let workspace = Path::new(field::text(&metadata, "workspace_root")?);
    let roots = roots(&metadata)?;
    let mut problems = Vec::new();
    for file in files::rust(workspace)? {
        if file.starts_with(workspace.join("xtask/fixture")) {
            continue;
        }
        let text = std::fs::read_to_string(&file)
            .map_err(|e| format!("{}: {e}", file.display()))?;
        let code = files::code(&text);
        let shown = file.strip_prefix(workspace).unwrap_or(&file).display();
        for line in statics(&code) {
            problems.push(format!(
                "`{shown}:{line}` holds a `static` item. Fix: use a `const`, and pass \
                 state in as an input. Only a `#[global_allocator]` may be a `static` \
                 (COUNTING ALLOCATOR)."
            ));
        }
        if roots.contains(&file) {
            continue;
        }
        for (index, line) in code.lines().enumerate() {
            if select::has_word(line, "disallowed_macros") {
                problems.push(format!(
                    "`{shown}:{}` lifts `clippy::disallowed_macros` outside the root \
                     file of a test or benchmark target. Fix: remove the lift. Only a \
                     test or benchmark binary may hold a `#[global_allocator]` \
                     (COUNTING ALLOCATOR), and no target holds a `thread_local!`.",
                    index + 1
                ));
            }
        }
    }
    problems.sort();
    Ok(problems)
}

/// The root file of each test and benchmark target.
fn roots(metadata: &Value) -> Result<BTreeSet<PathBuf>, String> {
    let mut roots = BTreeSet::new();
    for package in field::list(metadata, "packages")? {
        for target in field::list(package, "targets")? {
            let kinds = field::list(target, "kind")?;
            if kinds.iter().any(|kind| kind == "test" || kind == "bench") {
                let file = Path::new(field::text(target, "src_path")?);
                roots.insert(files::normalize(file));
            }
        }
    }
    Ok(roots)
}

/// The line of each `static` keyword in `code` that `#[global_allocator]` does not
/// come right before. The `static` of a `'static` lifetime is not a keyword.
fn statics(code: &str) -> impl Iterator<Item = usize> {
    let ident = |c: char| c.is_alphanumeric() || c == '_';
    code.match_indices("static").filter_map(move |(at, word)| {
        let before = code[..at].chars().next_back();
        let after = code[at + word.len()..].chars().next();
        let keyword =
            !before.is_some_and(|c| ident(c) || c == '\'') && !after.is_some_and(ident);
        let allocator = code[..at].trim_end().ends_with("#[global_allocator]");
        (keyword && !allocator).then(|| code[..at].matches('\n').count() + 1)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lifted(at: &str) -> String {
        format!(
            "`{at}` lifts `clippy::disallowed_macros` outside the root file of a test \
             or benchmark target. Fix: remove the lift. Only a test or benchmark \
             binary may hold a `#[global_allocator]` (COUNTING ALLOCATOR), and no \
             target holds a `thread_local!`."
        )
    }

    fn held(at: &str) -> String {
        format!(
            "`{at}` holds a `static` item. Fix: use a `const`, and pass state in as an \
             input. Only a `#[global_allocator]` may be a `static` (COUNTING \
             ALLOCATOR)."
        )
    }

    #[test]
    fn refuses_each_static_but_an_allocator_and_each_lift_outside_a_test_root() {
        let mut refused = vec![
            lifted("crates/globals/build.rs:1"),
            held("crates/globals/build.rs:3"),
            lifted("crates/globals/src/lib.rs:4"),
            held("crates/globals/src/lib.rs:8"),
            held("crates/globals/src/lib.rs:9"),
            held("crates/globals/src/lib.rs:10"),
            held("crates/globals/src/lib.rs:11"),
            held("crates/globals/src/main.rs:16"),
            held("crates/globals/src/unread.rs:3"),
            held("crates/globals/tests/alloc.rs:10"),
            lifted("crates/globals/tests/lift/inner.rs:3"),
            held("xtask/src/main.rs:3"),
        ];
        refused.sort();
        assert_eq!(check(&crate::fixture()), Err(refused));
    }

    #[test]
    fn finds_each_static_keyword_that_no_allocator_attribute_comes_before() {
        let code = "static A: u8 = 0;\n\
                    fn f(x: &'static str) {}\n\
                    let is_static = 1;\n\
                    #[global_allocator]\n  static B: S = S;\n\
                    #[used] static C: u8 = 0;\n\
                    pub static\n\
                    D: u8 = 0;";
        assert_eq!(statics(code).collect::<Vec<_>>(), [1, 6, 7]);
    }
}
