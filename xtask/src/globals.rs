//! The globals check. Clippy's `disallowed-macros` refuses `global_allocator` and
//! `thread_local`, and only an attribute at the root of a crate lifts that lint. This
//! check allows the lift only in a test or benchmark target, where COUNTING ALLOCATOR
//! allows one global allocator. Clippy has no lint for a mutable `static`, so this
//! check also reads each `static` that a target other than a test or benchmark
//! compiles.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::{build, field, files, select};

/// Words that, in the type of a `static`, name a type with interior mutability. A
/// word that starts with `Atomic` does too.
const INTERIOR: [&str; 9] = [
    "Mutex",
    "RwLock",
    "OnceLock",
    "LazyLock",
    "Cell",
    "RefCell",
    "OnceCell",
    "LazyCell",
    "UnsafeCell",
];

/// Checks the packages `names` of the workspace at `root`, or each package when
/// `names` is empty. It never reads `xtask`, whose tests hold sample source. In each
/// target other than a test or benchmark target:
///
/// - the root file names `disallowed_macros` only in a `//` comment;
/// - no file that rustc compiles holds a `static mut`, or a `static` whose type names
///   a type with interior mutability. It runs `cargo check` to learn those files.
///
/// It reads text, so a `static` of a type that holds an atomic inside passes, and a
/// `#[cfg(test)]` module in a file it reads is read too.
pub(crate) fn check(root: &Path, names: &[String]) -> Result<(), Vec<String>> {
    let problems = problems(root, names).map_err(|e| vec![e])?;
    if problems.is_empty() {
        Ok(())
    } else {
        Err(problems)
    }
}

fn problems(root: &Path, names: &[String]) -> Result<Vec<String>, String> {
    let metadata = crate::metadata(root)?;
    let workspace = Path::new(field::text(&metadata, "workspace_root")?);
    let packages = packages(&metadata, names)?;
    let mut problems = Vec::new();
    for package in &packages {
        for target in field::list(package, "targets")? {
            let kinds = field::list(target, "kind")?;
            if kinds.iter().any(|kind| kind == "test" || kind == "bench") {
                continue;
            }
            let file = files::normalize(Path::new(field::text(target, "src_path")?));
            let (shown, text) = read(workspace, &file)?;
            problems.extend(lifts(&shown, &text));
        }
    }
    for file in compiled(root, &metadata, &packages)? {
        let (shown, text) = read(workspace, &file)?;
        problems.extend(statics(&shown, &text));
    }
    problems.sort();
    Ok(problems)
}

/// The packages of `metadata` named in `names`, or each package when `names` is
/// empty, without `xtask`.
fn packages<'a>(
    metadata: &'a Value,
    names: &[String],
) -> Result<Vec<&'a Value>, String> {
    let all = field::list(metadata, "packages")?;
    let mut known = BTreeSet::new();
    for package in all {
        known.insert(field::text(package, "name")?);
    }
    if let Some(name) = names.iter().find(|name| !known.contains(name.as_str())) {
        let known: Vec<_> = known.into_iter().collect();
        return Err(format!(
            "no package `{name}` in the workspace. Its packages: {}.",
            known.join(", ")
        ));
    }
    let mut picked = Vec::new();
    for package in all {
        let name = field::text(package, "name")?;
        if name != "xtask" && (names.is_empty() || names.iter().any(|n| n == name)) {
            picked.push(package);
        }
    }
    Ok(picked)
}

/// The Rust files of the workspace that rustc compiles into a target of `packages`
/// other than a test or benchmark target. It runs `cargo check` on `packages`.
fn compiled(
    root: &Path,
    metadata: &Value,
    packages: &[&Value],
) -> Result<BTreeSet<PathBuf>, String> {
    let mut files = BTreeSet::new();
    if packages.is_empty() {
        return Ok(files);
    }
    let workspace = Path::new(field::text(metadata, "workspace_root")?);
    let target = Path::new(field::text(metadata, "target_directory")?);
    let mut ids = BTreeSet::new();
    let mut cargo = crate::cargo();
    cargo.current_dir(root).args([
        "check",
        "--all-targets",
        "--all-features",
        "--message-format=json",
    ]);
    for package in packages {
        let id = field::text(package, "id")?;
        ids.insert(id);
        cargo.args(["-p", id]);
    }
    for unit in build::checked(&mut cargo)? {
        if !ids.contains(unit.package_id.as_str()) {
            continue;
        }
        for file in build::sources(workspace, &unit.info)? {
            if file.starts_with(workspace) && !file.starts_with(target) {
                files.insert(file);
            }
        }
    }
    Ok(files)
}

/// The path of `file` from `workspace`, and the text of `file`.
fn read(workspace: &Path, file: &Path) -> Result<(String, String), String> {
    let text = std::fs::read_to_string(file)
        .map_err(|e| format!("{}: {e}", file.display()))?;
    let shown = file.strip_prefix(workspace).unwrap_or(file);
    Ok((shown.display().to_string(), text))
}

/// A problem for each line of `text`, the root file `shown`, that names
/// `disallowed_macros` outside a `//` comment.
fn lifts(shown: &str, text: &str) -> Vec<String> {
    let mut problems = Vec::new();
    for (index, line) in text.lines().enumerate() {
        if !line.trim_start().starts_with("//")
            && select::has_word(line, "disallowed_macros")
        {
            problems.push(format!(
                "`{shown}:{}` lifts `clippy::disallowed_macros` outside a test or \
                 benchmark target. Fix: remove the lift. Only a test or benchmark \
                 binary may hold a `#[global_allocator]` (COUNTING ALLOCATOR), and no \
                 target holds a `thread_local!`.",
                index + 1
            ));
        }
    }
    problems
}

/// A problem for each `static mut`, and each `static` whose type names a type with
/// interior mutability, in `text`, the file `shown`.
fn statics(shown: &str, text: &str) -> Vec<String> {
    let lines: Vec<&str> = text.lines().collect();
    let mut problems = Vec::new();
    for (index, line) in lines.iter().enumerate() {
        let Some(item) = item(line) else {
            continue;
        };
        // The type can go on to the next lines, up to the `=`, or to the `;` of a
        // `static` with no value.
        let mut item = item.to_string();
        for next in &lines[index + 1..] {
            if item.contains('=') || item.trim_end().ends_with(';') {
                break;
            }
            item.push(' ');
            item.push_str(next.trim());
        }
        let head = item.split_once('=').map_or(item.as_str(), |(head, _)| head);
        let place = format!("{shown}:{}", index + 1);
        if head.split_whitespace().next() == Some("mut") {
            problems.push(format!(
                "`{place}` holds a `static mut`. Fix: build the state where `node` \
                 wires the crate and pass it in. No mutable globals."
            ));
            continue;
        }
        let Some((_, kind)) = head.split_once(':') else {
            continue;
        };
        let kind = kind.trim().trim_end_matches(';').trim_end();
        let ident = |c: char| c.is_alphanumeric() || c == '_';
        if kind
            .split(|c| !ident(c))
            .any(|word| word.starts_with("Atomic") || INTERIOR.contains(&word))
        {
            let kind = kind.split_whitespace().collect::<Vec<_>>().join(" ");
            problems.push(format!(
                "`{place}` holds a `static` of `{kind}`, which has interior \
                 mutability. Fix: build the value where `node` wires the crate and \
                 pass it in. No mutable globals."
            ));
        }
    }
    problems
}

/// The text after `static` when `line` starts a `static` item, after an optional
/// visibility.
fn item(line: &str) -> Option<&str> {
    let mut rest = line.trim_start();
    if let Some(after) = rest.strip_prefix("pub") {
        rest = match after.strip_prefix('(') {
            Some(scope) => scope.split_once(')')?.1,
            None => after,
        }
        .trim_start();
    }
    let rest = rest.strip_prefix("static")?;
    rest.starts_with(char::is_whitespace)
        .then(|| rest.trim_start())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lift(at: &str) -> String {
        format!(
            "`{at}` lifts `clippy::disallowed_macros` outside a test or benchmark \
             target. Fix: remove the lift. Only a test or benchmark binary may hold a \
             `#[global_allocator]` (COUNTING ALLOCATOR), and no target holds a \
             `thread_local!`."
        )
    }

    fn mutable(at: &str) -> String {
        format!(
            "`{at}` holds a `static mut`. Fix: build the state where `node` wires the \
             crate and pass it in. No mutable globals."
        )
    }

    fn interior(at: &str, kind: &str) -> String {
        format!(
            "`{at}` holds a `static` of `{kind}`, which has interior mutability. Fix: \
             build the value where `node` wires the crate and pass it in. No mutable \
             globals."
        )
    }

    fn names(names: &[&str]) -> Vec<String> {
        names.iter().map(ToString::to_string).collect()
    }

    fn refused() -> Vec<String> {
        let mut refused = vec![
            lift("crates/globals/build.rs:1"),
            mutable("crates/globals/build.rs:3"),
            interior("crates/globals/moved/mod.rs:3", "std::sync::OnceLock<u8>"),
            lift("crates/globals/src/lib.rs:4"),
            mutable("crates/globals/src/lib.rs:11"),
            interior(
                "crates/globals/src/lib.rs:12",
                "std::sync::atomic::AtomicU64",
            ),
            interior("crates/globals/src/lib.rs:13", "std::sync::Mutex<u8>"),
            interior("crates/globals/src/lib.rs:15", "std::sync::RwLock<bool>"),
        ];
        refused.sort();
        refused
    }

    #[test]
    fn refuses_lifts_and_mutable_statics_outside_test_and_benchmark_targets() {
        assert_eq!(check(&crate::fixture(), &[]), Err(refused()));
    }

    #[test]
    fn checks_only_the_named_packages() {
        assert_eq!(
            check(&crate::fixture(), &names(&["a", "globals"])),
            Err(refused())
        );
        assert_eq!(check(&crate::fixture(), &names(&["a", "model"])), Ok(()));
    }

    #[test]
    fn names_the_packages_when_one_is_unknown() {
        assert_eq!(
            check(&crate::fixture(), &names(&["a", "nope"])),
            Err(vec![
                "no package `nope` in the workspace. Its packages: a, b, globals, \
                 model."
                    .to_string()
            ])
        );
    }

    #[test]
    fn refuses_each_interior_type_and_no_other() {
        let text = "\
static A: AtomicBool = AtomicBool::new(false);
pub(in crate::x) static B: Wrap<RefCell<u8>> = Wrap::new();
pub static C: [UnsafeCell<u8>; 2] = todo!();
    static D: LazyCell<u8>;
static E: &'static str = \"Mutex\";
static F: Cellar = Cellar;
static G: [u8; 4] = [0; 4];
static ref H: Mutex<u8> = Mutex::new(0);
/// static mut I: u8 = 0;
let static_mut = 0;
";
        assert_eq!(
            statics("f.rs", text),
            [
                interior("f.rs:1", "AtomicBool"),
                interior("f.rs:2", "Wrap<RefCell<u8>>"),
                interior("f.rs:3", "[UnsafeCell<u8>; 2]"),
                interior("f.rs:4", "LazyCell<u8>"),
                interior("f.rs:8", "Mutex<u8>"),
            ]
        );
    }
}
