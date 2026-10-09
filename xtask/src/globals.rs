//! The globals check. Clippy's `disallowed-macros` refuses `global_allocator` and
//! `thread_local`, and only an attribute at the root of a crate lifts that lint. This
//! check allows the lift only in a test or benchmark target, where COUNTING ALLOCATOR
//! allows one global allocator.

use std::path::Path;

use crate::{field, files, select};

/// Checks that no target of the workspace at `root`, other than a test or benchmark
/// target, names `disallowed_macros` in its root file outside a `//` comment. It
/// never reads `xtask`, whose tests hold sample source.
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
    let mut problems = Vec::new();
    for package in field::list(&metadata, "packages")? {
        if field::text(package, "name")? == "xtask" {
            continue;
        }
        for target in field::list(package, "targets")? {
            let kinds = field::list(target, "kind")?;
            if kinds.iter().any(|kind| kind == "test" || kind == "bench") {
                continue;
            }
            let file = files::normalize(Path::new(field::text(target, "src_path")?));
            let text = std::fs::read_to_string(&file)
                .map_err(|e| format!("{}: {e}", file.display()))?;
            let shown = file.strip_prefix(workspace).unwrap_or(&file).display();
            for (index, line) in text.lines().enumerate() {
                if !line.trim_start().starts_with("//")
                    && select::has_word(line, "disallowed_macros")
                {
                    problems.push(format!(
                        "`{shown}:{}` lifts `clippy::disallowed_macros` outside a test \
                         or benchmark target. Fix: remove the lift. Only a test or \
                         benchmark binary may hold a `#[global_allocator]` (COUNTING \
                         ALLOCATOR), and no target holds a `thread_local!`.",
                        index + 1
                    ));
                }
            }
        }
    }
    problems.sort();
    Ok(problems)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn refused(at: &str) -> String {
        format!(
            "`{at}` lifts `clippy::disallowed_macros` outside a test or benchmark \
             target. Fix: remove the lift. Only a test or benchmark binary may hold a \
             `#[global_allocator]` (COUNTING ALLOCATOR), and no target holds a \
             `thread_local!`."
        )
    }

    #[test]
    fn refuses_a_lift_only_outside_test_and_benchmark_roots() {
        assert_eq!(
            check(&crate::fixture()),
            Err(vec![
                refused("crates/globals/build.rs:1"),
                refused("crates/globals/src/lib.rs:4"),
            ])
        );
    }
}
