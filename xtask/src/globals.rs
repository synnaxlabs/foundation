//! The globals check: a `#[global_allocator]` only in test and benchmark targets.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::{field, files};

/// Checks that `#[global_allocator]` appears only in test and benchmark targets of the
/// workspace at `root`. It reads each `.rs` file in the directory of each library and
/// binary root, except the roots of test and benchmark targets. Comments do not count.
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
    let mut problems = Vec::new();
    for file in sources(&metadata)? {
        let text = std::fs::read_to_string(&file)
            .map_err(|e| format!("{}: {e}", file.display()))?;
        let shown = file.strip_prefix(root).unwrap_or(&file).display();
        for (index, line) in text.lines().enumerate() {
            if line.trim_start().starts_with("#[global_allocator]") {
                problems.push(format!(
                    "`{shown}:{}` declares a `#[global_allocator]` outside a test or \
                     benchmark target. Fix: declare it in a binary under `tests/` or \
                     `benches/`; a library or `node` never holds one (COUNTING \
                     ALLOCATOR).",
                    index + 1
                ));
            }
        }
    }
    Ok(problems)
}

/// Each `.rs` file in the directory of the root of a target that is not a test, a
/// benchmark, or a build script, except the roots of test and benchmark targets.
fn sources(metadata: &Value) -> Result<BTreeSet<PathBuf>, String> {
    let mut dirs = BTreeSet::new();
    let mut roots = BTreeSet::new();
    for package in field::list(metadata, "packages")? {
        for target in field::list(package, "targets")? {
            let root = files::normalize(Path::new(field::text(target, "src_path")?));
            let kinds = field::list(target, "kind")?;
            if kinds.iter().any(|k| k == "test" || k == "bench") {
                roots.insert(root);
            } else if kinds.iter().all(|k| k != "custom-build") {
                let dir = root
                    .parent()
                    .ok_or_else(|| format!("{} has no directory", root.display()))?;
                dirs.insert(dir.to_path_buf());
            }
        }
    }
    let mut sources = BTreeSet::new();
    for dir in &dirs {
        for file in files::rust(dir)? {
            if !roots.contains(&file) {
                sources.insert(file);
            }
        }
    }
    Ok(sources)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refuses_a_global_allocator_only_outside_tests_and_benchmarks() {
        assert_eq!(
            check(&crate::fixture()),
            Err(vec![
                "`crates/a/src/alloc.rs:4` declares a `#[global_allocator]` outside a \
                 test or benchmark target. Fix: declare it in a binary under `tests/` \
                 or `benches/`; a library or `node` never holds one (COUNTING \
                 ALLOCATOR)."
                    .to_string()
            ])
        );
    }

    #[test]
    fn passes_on_the_workspace() {
        assert_eq!(check(&crate::fixture().join("../..")), Ok(()));
    }
}
