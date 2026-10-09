//! Picks workspace crates by what their Rust source says.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::{field, files};

/// A workspace package that a task picked.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Package {
    /// The package id from `cargo metadata`. `-p` takes it, and no other package in a
    /// build shares it, as another package may share the name.
    pub(crate) id: String,
    pub(crate) name: String,
}

impl Package {
    /// Reads the id and the name of a package object of `cargo metadata`.
    ///
    /// # Errors
    ///
    /// A missing field.
    pub(crate) fn read(package: &Value) -> Result<Self, String> {
        Ok(Self {
            id: field::text(package, "id")?.to_string(),
            name: field::text(package, "name")?.to_string(),
        })
    }
}

/// The packages in `metadata` with a `.rs` file whose text `matches`. It reads each
/// file in the directory of each target's root, so it also reads oracles under
/// `oracles/`. It never picks `xtask`, whose tests hold sample source.
///
/// # Errors
///
/// A missing metadata field, or a file that cannot be read.
pub(crate) fn packages(
    metadata: &Value,
    matches: impl Fn(&str) -> bool,
) -> Result<Vec<Package>, String> {
    let mut picked = Vec::new();
    for package in field::list(metadata, "packages")? {
        let found = Package::read(package)?;
        if found.name == "xtask" {
            continue;
        }
        let mut dirs = BTreeSet::new();
        for target in field::list(package, "targets")? {
            let root = files::normalize(Path::new(field::text(target, "src_path")?));
            let dir = root
                .parent()
                .ok_or_else(|| format!("{} has no directory", root.display()))?;
            dirs.insert(dir.to_path_buf());
        }
        if any(&dirs, &matches)? {
            picked.push(found);
        }
    }
    Ok(picked)
}

/// Reports whether a `.rs` file under `dirs` has text that `matches`.
fn any(
    dirs: &BTreeSet<PathBuf>,
    matches: &impl Fn(&str) -> bool,
) -> Result<bool, String> {
    for dir in dirs {
        for file in files::rust(dir)? {
            let text = std::fs::read_to_string(&file)
                .map_err(|e| format!("{}: {e}", file.display()))?;
            if matches(&text) {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

/// Reports whether a `cfg(...)`, `cfg!(...)`, or `cfg_attr(...)` in `source` names
/// `name`. An attribute may span lines.
pub(crate) fn names_cfg(source: &str, name: &str) -> bool {
    let text: String = source.split_whitespace().collect();
    ["cfg(", "cfg!(", "cfg_attr("].iter().any(|open| {
        text.match_indices(open).any(|(at, _)| {
            let rest = &text[at + open.len()..];
            has_word(&rest[..closing(rest)], name)
        })
    })
}

/// The index of the `)` that closes a group opened just before `text`, or the
/// length of `text` when no `)` does.
fn closing(text: &str) -> usize {
    let mut depth = 1;
    for (at, c) in text.char_indices() {
        match c {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    return at;
                }
            }
            _ => {}
        }
    }
    text.len()
}

/// Reports whether `text` holds `word` with no identifier character on either side.
pub(crate) fn has_word(text: &str, word: &str) -> bool {
    let ident = |c: char| c.is_alphanumeric() || c == '_';
    text.match_indices(word).any(|(at, _)| {
        let before = text[..at].chars().next_back();
        let after = text[at + word.len()..].chars().next();
        !before.is_some_and(ident) && !after.is_some_and(ident)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    mod packages {
        use super::*;

        fn check(matches: impl Fn(&str) -> bool) -> Vec<String> {
            let metadata = crate::metadata(&crate::fixture(), &["--no-deps"]).unwrap();
            let picked = packages(&metadata, matches).unwrap();
            picked.into_iter().map(|p| p.name).collect()
        }

        #[test]
        fn picks_loom_cfgs_in_oracles_and_across_lines() {
            assert_eq!(check(|s| names_cfg(s, "loom")), ["a", "model"]);
        }

        #[test]
        fn picks_only_crates_with_a_match() {
            assert_eq!(check(|s| has_word(s, "unsafe_code")), ["model"]);
            assert_eq!(check(|s| names_cfg(s, "shuttle")), Vec::<String>::new());
        }

        #[test]
        fn names_a_missing_metadata_field() {
            for (package, error) in [
                (
                    serde_json::json!({ "id": "a-id", "name": "a" }),
                    "JSON has no array field `targets`",
                ),
                (
                    serde_json::json!({ "name": "a", "targets": [] }),
                    "JSON has no string field `id`",
                ),
            ] {
                let metadata = serde_json::json!({ "packages": [package] });
                assert_eq!(packages(&metadata, |_| true).unwrap_err(), error);
            }
        }
    }

    mod names_cfg {
        use super::*;

        #[test]
        fn finds_the_name_in_any_cfg_form() {
            for source in [
                "#[cfg(loom)]",
                "#[cfg(all(\n    test,\n    loom\n))]",
                "#[cfg(all(not(miri), loom))]",
                "if cfg!(loom) {}",
                "#[cfg_attr(loom, ignore)]",
                "#[cfg(not(loom)",
            ] {
                assert!(names_cfg(source, "loom"), "missed loom in {source:?}");
            }
        }

        #[test]
        fn ignores_the_name_outside_a_cfg() {
            for source in [
                "#[cfg(test)]",
                "#[cfg(all(test, miri))] fn f(loom: u8) {}",
                "#[cfg(bloom)]",
                "let loom = 1;",
            ] {
                assert!(!names_cfg(source, "loom"), "found loom in {source:?}");
            }
        }
    }

    #[test]
    fn has_word_needs_a_boundary_on_each_side() {
        assert!(has_word("expect(unsafe_code)", "unsafe_code"));
        assert!(has_word("unsafe_code", "unsafe_code"));
        assert!(!has_word("unsafe_codes", "unsafe_code"));
        assert!(!has_word("forbid_unsafe_code", "unsafe_code"));
        assert!(!has_word("unsafe_code", "unsafe"));
        assert!(!has_word("bloom", "loom"));
        assert!(!has_word("loom1", "loom"));
    }
}
