//! Which commits a review round covers, read from the history with `git`.

use std::path::Path;
use std::process::Command;

/// The empty tree, the source of attributes for a merge, so no `.gitattributes` can
/// pick a merge driver that hides a conflict.
const EMPTY_TREE: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";

/// The history of a git repository, and the branch that PRs in it merge into.
pub(crate) struct History<'a> {
    root: &'a Path,
    /// The full ref of the base, read only with `show-ref --verify`, so no other ref
    /// can take its place.
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
    /// A failed `git` command, also when the base ref does not exist.
    pub(crate) fn reaches(&self, end: &str, head: &str) -> Result<bool, String> {
        let Some(end) = self.named(end)? else {
            return Ok(false);
        };
        let base = self.base()?;
        let mut commit =
            self.git(&["rev-parse", "--verify", &format!("{head}^{{commit}}")])?;
        while commit != end {
            let line = self.git(&["rev-list", "--parents", "-n", "1", &commit])?;
            let Some((first, second)) = self.base_merge(&line, &base)? else {
                return Ok(false);
            };
            if !self.clean(&commit, first, second)? {
                return Ok(false);
            }
            commit = first.to_string();
        }
        Ok(true)
    }

    /// The first line of code that `from..end` adds or removes, as a phrase: "changes
    /// code at `<file>:<line>`". A line of a `.rs` file is code unless, trimmed, it is
    /// empty or starts with `//`; each line of a `Cargo.toml` or `Cargo.lock` is
    /// code. A moved file counts as removed and added.
    ///
    /// A merge of a commit on the base on the first-parent chain of `end` counts only
    /// by its resolution. A `.rs`, `Cargo.toml`, or `Cargo.lock` file that
    /// `git merge-tree` finds a conflict in between its parents gives "resolves a
    /// conflict in `<file>` in `<merge>`". Else the change is read to `end` from the
    /// tree that `git merge-tree` makes of `from` and the base parent of the last such
    /// merge, not from `from`: the base's code does not count, and text of the range
    /// that the base moves into a code file does. A code file that this tree has a
    /// conflict in gives "has a conflict in `<file>` between its start and the
    /// base", so a conflict that leaves no markers fails closed too.
    ///
    /// The line number is in `end` for an added line, and in `from` or that tree for
    /// a removed one.
    ///
    /// `None` when no line is code. `from` and `end` are SHAs or prefixes of at least
    /// 7 digits; text that names no single commit gives the phrase "has `<text>`,
    /// which names no commit".
    ///
    /// # Errors
    ///
    /// A failed `git` command, also when the base ref does not exist, or a changed
    /// path that is not UTF-8.
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
        let base = self.base()?;
        let range = format!("{from_sha}..{end_sha}");
        let chain = self.git(&[
            "rev-list",
            "--reverse",
            "--first-parent",
            "--parents",
            &range,
        ])?;
        let mut last = None;
        for line in chain.lines() {
            let Some((first, second)) = self.base_merge(line, &base)? else {
                continue;
            };
            let merge = line.split(' ').next().unwrap_or_default();
            let merged = self.merged(first, second)?;
            if let Some(path) = merged.conflicts.iter().find(|p| code_path(p)) {
                return Ok(Some(format!(
                    "resolves a conflict in `{path}` in `{merge}`"
                )));
            }
            last = Some(second);
        }
        let Some(second) = last else {
            return self.first_change(&from_sha, &end_sha);
        };
        let merged = self.merged(&from_sha, second)?;
        if let Some(path) = merged.conflicts.iter().find(|p| code_path(p)) {
            return Ok(Some(format!(
                "has a conflict in `{path}` between its start and the base"
            )));
        }
        self.first_change(&merged.tree, &end_sha)
    }

    /// The first line of code that the change from the tree-ish `old` to `new` adds
    /// or removes, as `code_change` gives it.
    ///
    /// # Errors
    ///
    /// A failed `git` command, or a changed path that is not UTF-8.
    fn first_change(&self, old: &str, new: &str) -> Result<Option<String>, String> {
        // Each entry is `:<old mode> <new mode> <old blob> <new blob> <status>` and
        // the path, each ended by a NUL.
        let raw = self.output(&[
            "diff",
            "--raw",
            "-z",
            "--no-abbrev",
            "--no-renames",
            old,
            new,
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
            if let Some(line) = self.first_code(old, new, rust_path(path))? {
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

    /// The SHA of the base, read only with `show-ref --verify`.
    fn base(&self) -> Result<String, String> {
        self.git(&["show-ref", "--verify", "--hash", &self.base])
    }

    /// The two parents of the commit in `line` (`<commit> <parent>...`, from
    /// `rev-list --parents`) when it is a merge of a commit on the base: it has two
    /// parents, and the second is an ancestor of `base`, the SHA of the base.
    fn base_merge<'l>(
        &self,
        line: &'l str,
        base: &str,
    ) -> Result<Option<(&'l str, &'l str)>, String> {
        let [_, first, second] = line.split(' ').collect::<Vec<_>>()[..] else {
            return Ok(None);
        };
        Ok(self.on_base(second, base)?.then_some((first, second)))
    }

    /// Reports whether `commit` is an ancestor of the commit `base`.
    fn on_base(&self, commit: &str, base: &str) -> Result<bool, String> {
        let status = command(self.root)
            .args(["merge-base", "--is-ancestor", commit, base])
            .status()
            .map_err(|e| format!("git merge-base: {e}"))?;
        match status.code() {
            Some(0) => Ok(true),
            Some(1) => Ok(false),
            _ => Err(format!(
                "git merge-base --is-ancestor {commit} {base}: {status}"
            )),
        }
    }

    /// Reports whether `merge` has the tree that a merge of `first` and `second`
    /// makes with no conflict.
    fn clean(&self, merge: &str, first: &str, second: &str) -> Result<bool, String> {
        let merged = self.merged(first, second)?;
        let tree = self.git(&["rev-parse", &format!("{merge}^{{tree}}")])?;
        Ok(merged.clean && merged.tree == tree)
    }

    /// What `git merge-tree` makes of `first` and `second`.
    ///
    /// # Errors
    ///
    /// A failed `git merge-tree`.
    fn merged(&self, first: &str, second: &str) -> Result<Merged, String> {
        let output = command(self.root)
            .arg(format!("--attr-source={EMPTY_TREE}"))
            .args([
                "merge-tree",
                "--write-tree",
                "-z",
                "--name-only",
                first,
                second,
            ])
            .output()
            .map_err(|e| format!("git merge-tree: {e}"))?;
        let clean = match output.status.code() {
            Some(0) => true,
            Some(1) => false,
            _ => return Err(failure("merge-tree", &output.stderr)),
        };
        // `<tree>NUL`, then `<path>NUL` for each conflicted path, then `NUL` and the
        // messages, which are not read: they can hold any bytes.
        let mut fields = output.stdout.split(|&b| b == 0);
        let tree = String::from_utf8_lossy(fields.next().unwrap_or_default());
        let conflicts = fields
            .take_while(|path| !path.is_empty())
            .map(|path| String::from_utf8_lossy(path).into_owned());
        Ok(Merged {
            clean,
            tree: tree.into_owned(),
            conflicts: conflicts.collect(),
        })
    }

    /// The trimmed output of a `git` command that must succeed.
    fn git(&self, args: &[&str]) -> Result<String, String> {
        let output = self.output(args)?;
        Ok(String::from_utf8_lossy(&output).trim().to_string())
    }

    /// The output of a `git` command that must succeed.
    fn output(&self, args: &[&str]) -> Result<Vec<u8>, String> {
        let output = command(self.root)
            .args(args)
            .output()
            .map_err(|e| format!("git {}: {e}", args.join(" ")))?;
        if !output.status.success() {
            return Err(failure(&args.join(" "), &output.stderr));
        }
        Ok(output.stdout)
    }
}

/// What `git merge-tree` makes of two commits.
struct Merged {
    /// Whether no path conflicts.
    clean: bool,
    /// The merged tree, with conflict markers in each file whose content conflicts.
    tree: String,
    /// Each path with a conflict, in the order `git` gives, with each byte that is not
    /// UTF-8 replaced.
    conflicts: Vec<String>,
}

/// Whether a change to `path`, a path as `git` gives it, can change code: a `.rs`
/// file, a `Cargo.toml`, or a `Cargo.lock`.
pub(super) fn code_path(path: &str) -> bool {
    rust_path(path)
        || Path::new(path)
            .file_name()
            .is_some_and(|n| n == "Cargo.toml" || n == "Cargo.lock")
}

/// Whether `path` is a `.rs` file as the pathspec `**/*.rs` matches it: also the file
/// `.rs`, which has no extension to `Path`.
#[expect(
    clippy::case_sensitive_file_extension_comparisons,
    reason = "the pathspec matches case"
)]
fn rust_path(path: &str) -> bool {
    path.ends_with(".rs")
}

/// A `git` command in `root` that reads no inherited `GIT_*` variable and no global
/// or system config or attributes file, so the machine's settings cannot change a
/// verdict and no variable such as `GIT_DIR` can point it elsewhere. It still reads
/// the repository's own `.git/config` and `.git/info/attributes`.
fn command(root: &Path) -> Command {
    let mut command = Command::new("git");
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("GIT_") {
            command.env_remove(key);
        }
    }
    command
        .current_dir(root)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_ATTR_NOSYSTEM", "1")
        .args(["-c", "core.attributesFile=/dev/null"]);
    command
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
