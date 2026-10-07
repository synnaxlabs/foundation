//! Which commits a review round covers, read from the history with `git`.

use std::path::Path;
use std::process::Command;

/// The history of a git repository, and the branch that PRs in it merge into.
pub(crate) struct History<'a> {
    root: &'a Path,
    /// The full ref of the base, so a tag of the same short name cannot take its
    /// place.
    base: String,
}

impl<'a> History<'a> {
    /// The history of the repository at `root`, where PRs merge into the branch
    /// `base` (for example `main`) of the remote `origin`.
    pub(crate) fn new(root: &'a Path, base: &str) -> Self {
        let base = format!("refs/remotes/origin/{base}");
        Self { root, base }
    }

    /// Reports whether `end` (a SHA or a prefix of at least 7 digits) is `head`, or
    /// reaches it through first-parent merges of a commit on the base that resolve no
    /// conflict: each merge's tree is the tree that `git merge-tree` makes of its
    /// parents. Text that names no single commit does not reach `head`.
    ///
    /// # Errors
    ///
    /// A failed `git` command.
    pub(crate) fn reaches(&self, end: &str, head: &str) -> Result<bool, String> {
        let Some(end) = self.named(end)? else {
            return Ok(false);
        };
        let mut commit =
            self.git(&["rev-parse", "--verify", &format!("{head}^{{commit}}")])?;
        while commit != end {
            let line = self.git(&["rev-list", "--parents", "-n", "1", &commit])?;
            let [_, first, second] = line.split(' ').collect::<Vec<_>>()[..] else {
                return Ok(false);
            };
            if !self.on_base(second)? || !self.clean(&commit, first, second)? {
                return Ok(false);
            }
            commit = first.to_string();
        }
        Ok(true)
    }

    /// The first line of code that `from..end` adds or removes, as a phrase: "changes
    /// code at `<file>:<line>`". A line of a `.rs` file is code unless, trimmed, it is
    /// empty or starts with `//`; each line of a `Cargo.toml` or `Cargo.lock` is
    /// code. A moved file counts as removed and added. The line number is in `end` for
    /// an added line and in `from` for a removed one. `None` when no line is code.
    /// `from` and `end` are SHAs or prefixes of at least 7 digits; text that names no
    /// single commit gives the phrase "has `<text>`, which names no commit".
    ///
    /// # Errors
    ///
    /// A failed `git` command, or a changed path that is not UTF-8.
    pub(crate) fn code_change(
        &self,
        from: &str,
        end: &str,
    ) -> Result<Option<String>, String> {
        let unnamed = |text: &str| format!("has `{text}`, which names no commit");
        let Some(from_sha) = self.named(from)? else {
            return Ok(Some(unnamed(from)));
        };
        let Some(end_sha) = self.named(end)? else {
            return Ok(Some(unnamed(end)));
        };
        // Each entry is `:<old mode> <new mode> <old blob> <new blob> <status>` and
        // the path, each ended by a NUL.
        let raw = self.output(&[
            "diff",
            "--raw",
            "-z",
            "--no-abbrev",
            "--no-renames",
            &from_sha,
            &end_sha,
            "--",
            ":(glob)**/*.rs",
            ":(glob)**/Cargo.toml",
            ":(glob)**/Cargo.lock",
        ])?;
        let mut fields = raw.split(|&b| b == 0);
        while let (Some(entry), Some(path)) = (fields.next(), fields.next()) {
            let entry = String::from_utf8_lossy(entry);
            let [_, _, old, new, _] = entry.split(' ').collect::<Vec<_>>()[..] else {
                return Err(format!("git diff: a bad raw entry `{entry}`"));
            };
            let path = std::str::from_utf8(path).map_err(|e| {
                format!(
                    "git diff: the path `{}` is not UTF-8: {e}",
                    String::from_utf8_lossy(path)
                )
            })?;
            let rust = Path::new(path).extension().is_some_and(|e| e == "rs");
            if let Some(line) = self.first_code(old, new, rust)? {
                return Ok(Some(format!("changes code at `{path}:{line}`")));
            }
        }
        Ok(None)
    }

    /// The line number of the first line of code that a change from the blob `old` to
    /// the blob `new` adds or removes, in `new` for an added line and in `old` for a
    /// removed one. A blob of zeros is a file that does not exist. `rust` tells
    /// whether the file is a `.rs` file.
    fn first_code(
        &self,
        old: &str,
        new: &str,
        rust: bool,
    ) -> Result<Option<u32>, String> {
        let absent = |blob: &str| blob.bytes().all(|b| b == b'0');
        if absent(old) || absent(new) {
            let blob = if absent(old) { new } else { old };
            let text = self.output(&["cat-file", "blob", blob])?;
            let text = String::from_utf8_lossy(&text);
            return Ok((1..)
                .zip(text.lines())
                .find(|(_, l)| code(rust, l))
                .map(|(n, _)| n));
        }
        let diff = self.git(&[
            "diff",
            "--no-ext-diff",
            "--no-textconv",
            "--no-color",
            "--text",
            "--unified=0",
            "--inter-hunk-context=0",
            old,
            new,
        ])?;
        let (mut old, mut new) = (0, 0);
        let mut hunk = false;
        for line in diff.lines() {
            if let Some(header) = line.strip_prefix("@@ ") {
                hunk = true;
                (old, new) = starts(header)
                    .ok_or_else(|| format!("git diff: a bad hunk header `{line}`"))?;
            } else if hunk && let Some(text) = line.strip_prefix('-') {
                if code(rust, text) {
                    return Ok(Some(old));
                }
                old += 1;
            } else if hunk && let Some(text) = line.strip_prefix('+') {
                if code(rust, text) {
                    return Ok(Some(new));
                }
                new += 1;
            }
        }
        Ok(None)
    }

    /// The full SHA of the commit that `text` names when it is a SHA or a prefix of at
    /// least 7 digits, or `None`. A ref is never read, so a tag named like a prefix
    /// cannot take the commit's place.
    fn named(&self, text: &str) -> Result<Option<String>, String> {
        if !(7..=40).contains(&text.len())
            || !text.bytes().all(|b| b.is_ascii_hexdigit())
        {
            return Ok(None);
        }
        let objects = self.git(&["rev-parse", &format!("--disambiguate={text}")])?;
        let mut commits = Vec::new();
        for object in objects.lines() {
            if self.git(&["cat-file", "-t", object])? == "commit" {
                commits.push(object);
            }
        }
        Ok(match commits[..] {
            [commit] => Some(commit.to_string()),
            _ => None,
        })
    }

    fn on_base(&self, commit: &str) -> Result<bool, String> {
        let status = Command::new("git")
            .current_dir(self.root)
            .args(["merge-base", "--is-ancestor", commit, &self.base])
            .status()
            .map_err(|e| format!("git merge-base: {e}"))?;
        match status.code() {
            Some(0) => Ok(true),
            Some(1) => Ok(false),
            _ => Err(format!(
                "git merge-base --is-ancestor {commit} {}: {status}",
                self.base
            )),
        }
    }

    /// Reports whether `merge` has the tree that a merge of `first` and `second`
    /// makes with no conflict.
    fn clean(&self, merge: &str, first: &str, second: &str) -> Result<bool, String> {
        let output = Command::new("git")
            .current_dir(self.root)
            .args(["merge-tree", "--write-tree", first, second])
            .output()
            .map_err(|e| format!("git merge-tree: {e}"))?;
        match output.status.code() {
            Some(0) => {}
            Some(1) => return Ok(false),
            _ => return Err(failure("merge-tree", &output.stderr)),
        }
        let made = String::from_utf8_lossy(&output.stdout);
        let tree = self.git(&["rev-parse", &format!("{merge}^{{tree}}")])?;
        Ok(made.lines().next() == Some(tree.as_str()))
    }

    /// The trimmed output of a `git` command that must succeed.
    fn git(&self, args: &[&str]) -> Result<String, String> {
        let output = self.output(args)?;
        Ok(String::from_utf8_lossy(&output).trim().to_string())
    }

    /// The output of a `git` command that must succeed.
    fn output(&self, args: &[&str]) -> Result<Vec<u8>, String> {
        let output = Command::new("git")
            .current_dir(self.root)
            .args(args)
            .output()
            .map_err(|e| format!("git {}: {e}", args.join(" ")))?;
        if !output.status.success() {
            return Err(failure(&args.join(" "), &output.stderr));
        }
        Ok(output.stdout)
    }
}

/// Whether `text`, a changed line of a file, is code: any line of a file that is not
/// `.rs`, and a `.rs` line that, trimmed, is not empty or a comment.
fn code(rust: bool, text: &str) -> bool {
    let text = text.trim();
    !rust || !(text.is_empty() || text.starts_with("//"))
}

/// The first old and new line numbers of a hunk header `-<a>[,<n>] +<b>[,<m>] @@`.
fn starts(header: &str) -> Option<(u32, u32)> {
    let mut parts = header.split(' ');
    let mut start = |sign| {
        let part: &str = parts.next()?.strip_prefix(sign)?;
        part.split(',').next()?.parse().ok()
    };
    Some((start('-')?, start('+')?))
}

fn failure(command: &str, stderr: &[u8]) -> String {
    format!("git {command}: {}", String::from_utf8_lossy(stderr).trim())
}

#[cfg(test)]
mod tests;
