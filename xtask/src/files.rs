//! Paths and Rust source files.

use std::path::{Component, Path, PathBuf};

/// Every `.rs` file under `dir`, sorted. It skips hidden directories and each cargo
/// target directory, which holds a `CACHEDIR.TAG`.
///
/// # Errors
///
/// A directory that cannot be read.
pub(crate) fn rust(dir: &Path) -> Result<Vec<PathBuf>, String> {
    let mut files = Vec::new();
    let mut dirs = vec![dir.to_path_buf()];
    while let Some(dir) = dirs.pop() {
        let entries =
            std::fs::read_dir(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        for entry in entries {
            let path = entry.map_err(|e| format!("{}: {e}", dir.display()))?.path();
            if path.is_dir() {
                let hidden = entry_name(&path).starts_with('.');
                if !hidden && !path.join("CACHEDIR.TAG").exists() {
                    dirs.push(path);
                }
            } else if path.extension().is_some_and(|e| e == "rs") {
                files.push(path);
            }
        }
    }
    files.sort();
    Ok(files)
}

fn entry_name(path: &Path) -> &str {
    path.file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default()
}

/// `source` with each comment and each string and character literal replaced by
/// spaces. Line breaks stay, so a line of the result is the same line of `source`.
pub(crate) fn code(source: &str) -> String {
    let chars: Vec<char> = source.chars().collect();
    let mut out = String::with_capacity(source.len());
    let mut at = 0;
    while at < chars.len() {
        if let Some(end) = skip(&chars, at) {
            out.extend(
                chars[at..end]
                    .iter()
                    .map(|&c| if c == '\n' { c } else { ' ' }),
            );
            at = end;
        } else {
            out.push(chars[at]);
            at += 1;
        }
    }
    out
}

/// The end of the comment or literal that starts at `at`, or `None` when none does.
fn skip(chars: &[char], at: usize) -> Option<usize> {
    let word = at > 0 && (chars[at - 1].is_alphanumeric() || chars[at - 1] == '_');
    match (chars[at], chars.get(at + 1)) {
        ('/', Some('/')) => Some(find(chars, at, '\n').unwrap_or(chars.len())),
        ('/', Some('*')) => Some(block(chars, at)),
        ('"', _) => Some(string(chars, at)),
        ('\'', _) => character(chars, at),
        ('b' | 'c' | 'r', _) if !word => prefixed(chars, at),
        _ => None,
    }
}

/// The index of the first `target` after `at`.
fn find(chars: &[char], at: usize, target: char) -> Option<usize> {
    let rest = chars.get(at + 1..)?;
    rest.iter().position(|&c| c == target).map(|n| at + 1 + n)
}

/// The end of the block comment that starts at `at`. Block comments nest.
fn block(chars: &[char], mut at: usize) -> usize {
    let mut depth = 0_usize;
    while at < chars.len() {
        match (chars[at], chars.get(at + 1)) {
            ('/', Some('*')) => {
                depth += 1;
                at += 2;
            }
            ('*', Some('/')) => {
                depth -= 1;
                at += 2;
                if depth == 0 {
                    return at;
                }
            }
            _ => at += 1,
        }
    }
    chars.len()
}

/// The end of the string whose opening `"` is at `at`.
fn string(chars: &[char], mut at: usize) -> usize {
    at += 1;
    while at < chars.len() {
        match chars[at] {
            '\\' => at += 2,
            '"' => return at + 1,
            _ => at += 1,
        }
    }
    chars.len()
}

/// The end of the character literal that starts at `at`, or `None` for a lifetime or
/// a label.
fn character(chars: &[char], at: usize) -> Option<usize> {
    match (chars.get(at + 1), chars.get(at + 2)) {
        (Some('\\'), _) => {
            Some(find(chars, at + 2, '\'').map_or(chars.len(), |n| n + 1))
        }
        (Some(_), Some('\'')) => Some(at + 3),
        _ => None,
    }
}

/// The end of the literal with a `b`, `c`, or `r` prefix at `at`, or `None` when the
/// letter starts a name.
fn prefixed(chars: &[char], at: usize) -> Option<usize> {
    let after = if matches!(chars[at], 'b' | 'c') {
        at + 1
    } else {
        at
    };
    match chars.get(after) {
        Some('r') => raw(chars, after),
        Some('"') if after > at => Some(string(chars, after)),
        _ => None,
    }
}

/// The end of the raw string whose `r` is at `at`, or `None` when no `"` follows the
/// `#` marks.
fn raw(chars: &[char], at: usize) -> Option<usize> {
    let hashes = chars[at + 1..].iter().take_while(|&&c| c == '#').count();
    let open = at + 1 + hashes;
    if chars.get(open) != Some(&'"') {
        return None;
    }
    let closes = |&i: &usize| {
        chars[i] == '"'
            && chars[i + 1..]
                .iter()
                .take(hashes)
                .filter(|&&c| c == '#')
                .count()
                == hashes
    };
    Some(
        (open + 1..chars.len())
            .find(closes)
            .map_or(chars.len(), |i| i + 1 + hashes),
    )
}

/// Removes `.` and `..` components without reading the disk. Cargo reports a
/// `[[test]] path` such as `../../oracles/x.rs` joined to the manifest directory.
pub(crate) fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lists_rust_files_in_subdirectories() {
        let oracles = crate::fixture().join("oracles");
        let found = rust(&oracles).unwrap();
        let names: Vec<_> = found.iter().map(|f| f.strip_prefix(&oracles)).collect();
        assert_eq!(
            names,
            [
                Ok(Path::new("a/common.rs")),
                Ok(Path::new("a/main.rs")),
                Ok(Path::new("ignored.rs")),
                Ok(Path::new("orphan.rs")),
            ]
        );
    }

    #[test]
    fn code_blanks_comments_and_literals_and_keeps_line_breaks() {
        let source = r##"a // b
c /* d /* e */ f */ g
"h \" i" j 'k' '\'' '"' 'µ' l
r#"m " n"# br"o" b"p" c"q" r
&'s t r#u b'v'
/* w
x */ y"##;
        let blanked = code(source);
        let words: Vec<Vec<&str>> = blanked
            .lines()
            .map(|line| line.split_whitespace().collect())
            .collect();
        let none: [&str; 0] = [];
        assert_eq!(
            words,
            [
                &["a"][..],
                &["c", "g"],
                &["j", "l"],
                &["r"],
                &["&'s", "t", "r#u", "b"],
                &none,
                &["y"],
            ]
        );
    }

    #[test]
    fn normalize_removes_current_and_parent_components() {
        let path = Path::new("/w/crates/raft/./../../oracles/x.rs");
        assert_eq!(normalize(path), PathBuf::from("/w/oracles/x.rs"));
    }
}
