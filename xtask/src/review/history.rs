//! Which commits a review round covers, read from the history with `git`.

use std::ffi::OsStr;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

/// The empty tree, the source of attributes for a merge, so no `.gitattributes` can
/// pick a merge driver that hides a conflict.
const EMPTY_TREE: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";

/// What [`History::change`] reads as a change.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Kind {
    /// Code: a `.rs` line that, trimmed, is not empty and does not start with `//`,
    /// and each line of a `Cargo.toml` or `Cargo.lock`.
    Code,
    /// A public item or a decision: each line of a file under `docs/decisions/`, and,
    /// in a `.rs` file under `crates/<crate>/src/`, a line that starts with `pub `
    /// after its indent, or a `///` line of the item after it when that item does.
    Public,
}

impl Kind {
    /// Whether a change to `path`, a path as `git` gives it, can be of this kind.
    fn path(self, path: &str) -> bool {
        match self {
            Kind::Code => code_path(path),
            Kind::Public => {
                path.starts_with("docs/decisions/")
                    || path
                        .strip_prefix("crates/")
                        .and_then(|p| p.split_once('/'))
                        .is_some_and(|(_, p)| p.starts_with("src/") && rust_path(p))
            }
        }
    }

    /// The pathspecs of the paths that `path` takes.
    fn globs(self) -> &'static [&'static str] {
        match self {
            Kind::Code => &[
                ":(glob)**/*.rs",
                ":(glob)**/Cargo.toml",
                ":(glob)**/Cargo.lock",
            ],
            Kind::Public => {
                &[":(glob)docs/decisions/**", ":(glob)crates/*/src/**/*.rs"]
            }
        }
    }

    /// Whether line `index` of `lines`, the text of the file `path`, is of this kind.
    fn counts(self, path: &str, lines: &[&str], index: usize) -> bool {
        let rust = rust_path(path);
        match self {
            Kind::Code => code(rust, lines[index]),
            Kind::Public => !rust || public(lines, index),
        }
    }

    /// The lines of this kind, as a phrase names them.
    fn lines(self) -> &'static str {
        match self {
            Kind::Code => "code",
            Kind::Public => "a public item or a decision",
        }
    }

    /// A file of this kind, as a phrase names it.
    fn file(self) -> &'static str {
        match self {
            Kind::Code => "code file",
            Kind::Public => "public file",
        }
    }
}

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
    /// parents. A merge whose base side moves text of its first parent into a code file
    /// does not count, as in `code_change`. Text that names no single commit does not
    /// reach `head`.
    ///
    /// # Errors
    ///
    /// A failed `git` command, also when the base ref does not exist.
    pub(crate) fn reaches(&self, end: &str, head: &str) -> Result<bool, String> {
        let Some(end) = self.sha(end)? else {
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

    /// The first line of `kind` that `from..end` adds or removes, as a phrase:
    /// "changes code at `<file>:<line>`" or "changes a public item or a decision at
    /// `<file>:<line>`". A moved file counts as removed and added.
    ///
    /// A merge of a commit on the base on the first-parent chain of `end` counts only
    /// by its resolution: a file of `kind` that `git merge-tree` finds a conflict in
    /// between its parents gives "resolves a conflict in `<file>` in `<merge>`". Else
    /// the change is read to `end` from the tree that `git merge-tree` makes of `from`
    /// and the newest base commit that `end` holds, not from `from`: the base's lines
    /// do not count, and text of the range that the base moves into a file of `kind`
    /// does. A file of `kind` that this tree has a conflict in gives "has a conflict in
    /// `<file>` between its start and the base", so a conflict that leaves no markers
    /// fails closed too. A path not of `kind`, that `from` changes since its merge base
    /// with that base commit, as the merge reads it, and that the merge that makes this
    /// tree moves into a file of `kind` by its own rename detection gives "the base
    /// moves `<old>`, which the PR changes, into the code file `<new>`", or "the public
    /// file" for [`Kind::Public`]. An `end` that holds more than one newest base commit
    /// gives "holds the base at more than one newest commit: `<commit>`, `<commit>`".
    ///
    /// The line number is in `end` for an added line, and in `from` or that tree for
    /// a removed one.
    ///
    /// `None` when no line is code. `from` and `end` are SHAs or prefixes of at least
    /// 7 digits, and `from` may end in one `^` for its first parent; text that names
    /// no single commit gives the phrase "has `<text>`, which names no commit".
    ///
    /// # Errors
    ///
    /// A failed `git` command, also when the base ref does not exist, or a changed
    /// path that is not UTF-8.
    pub(crate) fn change(
        &self,
        kind: Kind,
        from: &str,
        end: &str,
    ) -> Result<Option<String>, String> {
        let unnamed = |text: &str| format!("has `{text}`, which names no commit");
        let Some(from_sha) = self.named(from)? else {
            return Ok(Some(unnamed(from)));
        };
        let Some(end_sha) = self.sha(end)? else {
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
        for line in chain.lines() {
            let Some((first, second)) = self.base_merge(line, &base)? else {
                continue;
            };
            let merge = line.split(' ').next().unwrap_or_default();
            let merged = self.merged(first, second)?;
            if let Some(path) = merged.conflict(kind) {
                return Ok(Some(format!(
                    "resolves a conflict in `{path}` in `{merge}`"
                )));
            }
        }
        let bases = self.git(&["merge-base", "--all", &end_sha, &base])?;
        let mut bases: Vec<_> = bases.lines().collect();
        bases.sort_unstable();
        let [newest] = bases[..] else {
            return Ok(Some(format!(
                "holds the base at more than one newest commit: `{}`",
                bases.join("`, `")
            )));
        };
        let merged = self.merged(&from_sha, newest)?;
        if let Some(path) = merged.conflict(kind) {
            return Ok(Some(format!(
                "has a conflict in `{path}` between its start and the base"
            )));
        }
        if let Some(moved) = self.moved_into(kind, &from_sha, newest, &merged.tree)? {
            return Ok(Some(moved));
        }
        self.first_change(kind, &merged.tree, &end_sha)
    }

    /// The first line of `kind` that the change from the tree-ish `old` to `new` adds
    /// or removes, as `change` gives it.
    ///
    /// # Errors
    ///
    /// A failed `git` command, or a changed path that is not UTF-8.
    fn first_change(
        &self,
        kind: Kind,
        old: &str,
        new: &str,
    ) -> Result<Option<String>, String> {
        for entry in self.diff(old, new, &[&["--"], kind.globs()].concat())? {
            let path = std::str::from_utf8(&entry.path).map_err(|e| {
                format!(
                    "git diff: the path `{}` is not UTF-8: {e}",
                    String::from_utf8_lossy(&entry.path)
                )
            })?;
            if let Some(line) = self.first_line(kind, &entry.old, &entry.new, path)? {
                return Ok(Some(format!(
                    "changes {} at `{path}:{line}`",
                    kind.lines()
                )));
            }
        }
        Ok(None)
    }

    /// The line number of the first line of `kind` that a change from the blob `old`
    /// to the blob `new` of the file `path` adds or removes, in `new` for an added line
    /// and in `old` for a removed one. A blob of zeros is a file that does not exist.
    fn first_line(
        &self,
        kind: Kind,
        old: &str,
        new: &str,
        path: &str,
    ) -> Result<Option<u32>, String> {
        let absent = |blob: &str| blob.bytes().all(|b| b == b'0');
        let text = |blob: &str| -> Result<String, String> {
            if absent(blob) {
                return Ok(String::new());
            }
            let text = self.output(&["cat-file", "blob", blob])?;
            Ok(String::from_utf8_lossy(&text).into_owned())
        };
        let (old_text, new_text) = (text(old)?, text(new)?);
        let old_lines: Vec<_> = old_text.lines().collect();
        let new_lines: Vec<_> = new_text.lines().collect();
        let counts = |lines: &[&str], n: u32| kind.counts(path, lines, n as usize - 1);
        if absent(old) || absent(new) {
            let lines = if absent(old) { &new_lines } else { &old_lines };
            return Ok((1..)
                .zip(0..lines.len())
                .find(|&(_, i)| kind.counts(path, lines, i))
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
            } else if hunk && line.starts_with('-') {
                if counts(&old_lines, old) {
                    return Ok(Some(old));
                }
                old += 1;
            } else if hunk && line.starts_with('+') {
                if counts(&new_lines, new) {
                    return Ok(Some(new));
                }
                new += 1;
            }
        }
        Ok(None)
    }

    /// The full SHA of the commit that `text` names when it is a SHA or a prefix of at
    /// least 7 digits, or `None`. One final `^` names the first parent of that commit,
    /// as the form `<first-fix>^..<head>` of a later round needs. A ref is never read,
    /// so a tag named like a prefix cannot take the commit's place.
    fn named(&self, text: &str) -> Result<Option<String>, String> {
        if let Some(child) = text.strip_suffix('^') {
            let Some(child) = self.sha(child)? else {
                return Ok(None);
            };
            let line = self.git(&["rev-list", "--parents", "-n", "1", &child])?;
            return Ok(line.split(' ').nth(1).map(str::to_string));
        }
        self.sha(text)
    }

    /// The full SHA of the commit that `text` names when it is a SHA or a prefix of at
    /// least 7 digits, or `None`.
    fn sha(&self, text: &str) -> Result<Option<String>, String> {
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
        Ok(self.ancestor(second, base)?.then_some((first, second)))
    }

    /// Reports whether `commit` is an ancestor of the commit `of`, or is `of`.
    fn ancestor(&self, commit: &str, of: &str) -> Result<bool, String> {
        let status = command(self.root)
            .args(["merge-base", "--is-ancestor", commit, of])
            .status()
            .map_err(|e| format!("git merge-base: {e}"))?;
        match status.code() {
            Some(0) => Ok(true),
            Some(1) => Ok(false),
            _ => Err(format!(
                "git merge-base --is-ancestor {commit} {of}: {status}"
            )),
        }
    }

    /// Reports whether `merge` has the tree that a merge of `first` and `second`
    /// makes with no conflict, and the merge moves no text of `first` into a code
    /// file (`moved_into`).
    fn clean(&self, merge: &str, first: &str, second: &str) -> Result<bool, String> {
        let merged = self.merged(first, second)?;
        let tree = self.git(&["rev-parse", &format!("{merge}^{{tree}}")])?;
        Ok(merged.clean
            && merged.tree == tree
            && self
                .moved_into(Kind::Code, first, second, &merged.tree)?
                .is_none())
    }

    /// The phrase "the base moves `<old>`, which the PR changes, into the code file
    /// `<new>`", as `change` gives it for `kind`, for the first path `<old>` of
    /// `changed(kind, first, second)` that their merge, with the tree `tree`, moves into
    /// the file `<new>` of `kind`. The moves are the
    /// merge's own: `second` is merged again with a copy of `first` that gives each
    /// such path a probe text of its own, and the probe text is read in the code files
    /// that the two merges make differently.
    ///
    /// # Errors
    ///
    /// A failed `git` command.
    fn moved_into(
        &self,
        kind: Kind,
        first: &str,
        second: &str,
        tree: &str,
    ) -> Result<Option<String>, String> {
        // The merge is `first`, and moves nothing.
        if self.ancestor(second, first)? {
            return Ok(None);
        }
        let Some(probe) = self.probe(kind, first, second)? else {
            return Ok(None);
        };
        let probed = self.merged(&probe.commit, second)?.tree;
        for entry in self.diff(tree, &probed, &[])? {
            let new = String::from_utf8_lossy(&entry.path);
            if !kind.path(&new) {
                continue;
            }
            let text = self.output(&["cat-file", "blob", &entry.new])?;
            let old = String::from_utf8_lossy(&text).lines().find_map(|line| {
                let n: usize =
                    line.strip_prefix(PROBE)?.strip_prefix(' ')?.parse().ok()?;
                probe
                    .paths
                    .get(n)
                    .map(|old| String::from_utf8_lossy(old).into_owned())
            });
            if let Some(old) = old {
                return Ok(Some(format!(
                    "the base moves `{old}`, which the PR changes, into the {} \
                     `{new}`",
                    kind.file()
                )));
            }
        }
        Ok(None)
    }

    /// A copy of `first` in which each path of `changed(kind, first, second)` holds the
    /// probe text `<PROBE> <n>`, where `paths[n]` is that path. `None` when no such
    /// path exists.
    ///
    /// # Errors
    ///
    /// A failed `git` command.
    fn probe(
        &self,
        kind: Kind,
        first: &str,
        second: &str,
    ) -> Result<Option<Probe>, String> {
        let changed = self.changed(kind, first, second)?;
        if changed.is_empty() {
            return Ok(None);
        }
        let files: Vec<_> = changed
            .iter()
            .enumerate()
            .map(|(n, (mode, path))| File {
                mode,
                path,
                text: format!("{PROBE} {n}\n"),
            })
            .collect();
        Ok(Some(Probe {
            commit: self.copy(first, &files)?,
            paths: changed.into_iter().map(|(_, path)| path).collect(),
        }))
    }

    /// The mode and path of each file not of `kind` that `first` changes since
    /// its merge base with `second`, as a merge of the two reads that base. A file
    /// that `first` moves since that base is in too, which changes no result: no file
    /// of the base has its path, so the base cannot move it into a code file.
    ///
    /// # Errors
    ///
    /// A failed `git` command.
    fn changed(
        &self,
        kind: Kind,
        first: &str,
        second: &str,
    ) -> Result<Vec<(String, Vec<u8>)>, String> {
        // A merge of `first` with a copy of `second` that has no files builds the same
        // base. It gives a conflict with `first` as stage 2 for each file that the base
        // and `first` both hold with a difference, and for each file that `first`
        // moves.
        let gone = self.commit(second, EMPTY_TREE)?;
        let (_, output) = self.merge_tree(&[first, &gone])?;
        // `<tree>NUL`, then `<mode> <blob> <stage>TAB<path>NUL` for each stage of each
        // conflicted path, then `NUL` and the messages.
        let mut changed = Vec::new();
        for field in output
            .split(|&b| b == 0)
            .skip(1)
            .take_while(|field| !field.is_empty())
        {
            let mut parts = field.splitn(2, |&b| b == b'\t');
            let stage = String::from_utf8_lossy(parts.next().unwrap_or_default());
            let path = parts.next().unwrap_or_default();
            if let [mode, _, "2"] = stage.split(' ').collect::<Vec<_>>()[..]
                && !kind.path(&String::from_utf8_lossy(path))
            {
                changed.push((mode.to_string(), path.to_vec()));
            }
        }
        Ok(changed)
    }

    /// A copy of `first` (`commit`) with its tree with `files` in place of its own.
    /// It builds the tree in an index file of its own, which it removes.
    ///
    /// # Errors
    ///
    /// A failed `git` command.
    fn copy(&self, first: &str, files: &[File<'_>]) -> Result<String, String> {
        // Each entry is `<mode> <blob>TAB<path>NUL`.
        let mut entries = Vec::new();
        for file in files {
            let blob =
                self.run(&["hash-object", "-w", "--stdin"], file.text.as_bytes(), &[])?;
            entries.extend_from_slice(file.mode.as_bytes());
            entries.push(b' ');
            entries.extend_from_slice(String::from_utf8_lossy(&blob).trim().as_bytes());
            entries.push(b'\t');
            entries.extend_from_slice(file.path);
            entries.push(0);
        }
        let name = format!("xtask-review-probe-{}", std::process::id());
        let index = self
            .root
            .join(self.git(&["rev-parse", "--git-path", &name])?);
        let env = [("GIT_INDEX_FILE", index.as_os_str())];
        let tree = self
            .run(&["read-tree", first], b"", &env)
            .and_then(|_| {
                self.run(&["update-index", "-z", "--index-info"], &entries, &env)
            })
            .and_then(|_| self.run(&["write-tree"], b"", &env));
        let removed = std::fs::remove_file(&index)
            .map_err(|e| format!("{}: {e}", index.display()));
        let tree = tree?;
        removed?;
        self.commit(first, String::from_utf8_lossy(&tree).trim())
    }

    /// A commit of `tree` with the parents and the commit time of `commit`, by a
    /// fixed author. A merge with it finds the same merge bases in the same order as a
    /// merge with `commit`, since the walk reads only parents and commit times, and
    /// so builds the same base from more than one.
    ///
    /// # Errors
    ///
    /// A failed `git` command.
    fn commit(&self, commit: &str, tree: &str) -> Result<String, String> {
        let line =
            self.git(&["show", "-s", "--date=raw", "--format=%cd%x00%P", commit])?;
        let (when, parents) = line.split_once('\0').unwrap_or_default();
        let mut args = vec!["commit-tree", tree, "-m", "probe"];
        for parent in parents.split_whitespace() {
            args.extend(["-p", parent]);
        }
        let who = OsStr::new("xtask");
        let when = format!("@{when}");
        let when = OsStr::new(&when);
        let commit = self.run(
            &args,
            b"",
            &[
                ("GIT_AUTHOR_NAME", who),
                ("GIT_AUTHOR_EMAIL", who),
                ("GIT_AUTHOR_DATE", when),
                ("GIT_COMMITTER_NAME", who),
                ("GIT_COMMITTER_EMAIL", who),
                ("GIT_COMMITTER_DATE", when),
            ],
        )?;
        Ok(String::from_utf8_lossy(&commit).trim().to_string())
    }

    /// The entries of `git diff --raw` from the tree-ish `old` to `new`, with no
    /// renames, given the further arguments `args`.
    ///
    /// # Errors
    ///
    /// A failed `git diff`, or an entry not in the raw format.
    fn diff(&self, old: &str, new: &str, args: &[&str]) -> Result<Vec<Entry>, String> {
        let raw = self.output(
            &[
                &[
                    "diff",
                    "--raw",
                    "-z",
                    "--no-abbrev",
                    "--no-renames",
                    old,
                    new,
                ],
                args,
            ]
            .concat(),
        )?;
        // Each entry is `:<old mode> <new mode> <old blob> <new blob> <status>` and
        // the path, each ended by a NUL.
        let mut entries = Vec::new();
        let mut fields = raw.split(|&b| b == 0);
        while let (Some(entry), Some(path)) = (fields.next(), fields.next()) {
            let entry = String::from_utf8_lossy(entry);
            let [_, _, old, new, _] = entry.split(' ').collect::<Vec<_>>()[..] else {
                return Err(format!("git diff: a bad raw entry `{entry}`"));
            };
            entries.push(Entry {
                old: old.to_string(),
                new: new.to_string(),
                path: path.to_vec(),
            });
        }
        Ok(entries)
    }

    /// What `git merge-tree` makes of `first` and `second`.
    ///
    /// # Errors
    ///
    /// A failed `git merge-tree`.
    fn merged(&self, first: &str, second: &str) -> Result<Merged, String> {
        let (clean, output) = self.merge_tree(&["--name-only", first, second])?;
        // `<tree>NUL`, then `<path>NUL` for each conflicted path, then `NUL` and the
        // messages, which are not read: they can hold any bytes.
        let mut fields = output.split(|&b| b == 0);
        let tree = String::from_utf8_lossy(fields.next().unwrap_or_default());
        let conflicts = fields
            .take_while(|path| !path.is_empty())
            .map(|path| unrenamed(&String::from_utf8_lossy(path), [first, second]));
        Ok(Merged {
            clean,
            tree: tree.into_owned(),
            conflicts: conflicts.collect(),
        })
    }

    /// Whether `git merge-tree --write-tree -z` with `args` finds no conflict, and its
    /// output.
    ///
    /// # Errors
    ///
    /// A failed `git merge-tree`.
    fn merge_tree(&self, args: &[&str]) -> Result<(bool, Vec<u8>), String> {
        let output = command(self.root)
            .arg(format!("--attr-source={EMPTY_TREE}"))
            .args(["merge-tree", "--write-tree", "-z"])
            .args(args)
            .output()
            .map_err(|e| format!("git merge-tree: {e}"))?;
        match output.status.code() {
            Some(0) => Ok((true, output.stdout)),
            Some(1) => Ok((false, output.stdout)),
            _ => Err(failure("merge-tree", &output.stderr)),
        }
    }

    /// The trimmed output of a `git` command that must succeed.
    fn git(&self, args: &[&str]) -> Result<String, String> {
        let output = self.output(args)?;
        Ok(String::from_utf8_lossy(&output).trim().to_string())
    }

    /// The output of a `git` command that must succeed.
    fn output(&self, args: &[&str]) -> Result<Vec<u8>, String> {
        self.run(args, b"", &[])
    }

    /// The output of a `git` command that must succeed, given `stdin` and the
    /// variables `env`.
    fn run(
        &self,
        args: &[&str],
        stdin: &[u8],
        env: &[(&str, &OsStr)],
    ) -> Result<Vec<u8>, String> {
        let spawned = |e| format!("git {}: {e}", args.join(" "));
        let mut child = command(self.root)
            .args(args)
            .envs(env.iter().copied())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(spawned)?;
        child
            .stdin
            .take()
            .expect("invariant: stdin is piped")
            .write_all(stdin)
            .map_err(spawned)?;
        let output = child.wait_with_output().map_err(spawned)?;
        if !output.status.success() {
            return Err(failure(&args.join(" "), &output.stderr));
        }
        Ok(output.stdout)
    }
}

/// The first word of each line of probe text that `History::moved_into` writes.
const PROBE: &str = "xtask-review-probe";

/// One entry of `git diff --raw`.
struct Entry {
    /// The blob in the old tree, all zeros when the path is added.
    old: String,
    /// The blob in the new tree, all zeros when the path is removed.
    new: String,
    path: Vec<u8>,
}

/// A file to write in a tree.
struct File<'a> {
    mode: &'a str,
    path: &'a [u8],
    text: String,
}

/// A copy of a commit, with a probe text in each path that is not code and that
/// the commit changes.
struct Probe {
    commit: String,
    /// The probed paths, by the number in their probe text.
    paths: Vec<Vec<u8>>,
}

/// What `git merge-tree` makes of two commits.
struct Merged {
    /// Whether no path conflicts.
    clean: bool,
    /// The merged tree, with conflict markers in each file whose content conflicts.
    tree: String,
    /// Each path with a conflict, in the order `git` gives, with each byte that is not
    /// UTF-8 replaced. A file that `git` moves aside has its path before the move.
    conflicts: Vec<String>,
}

impl Merged {
    /// The first conflicted path of `kind`.
    fn conflict(&self, kind: Kind) -> Option<&str> {
        self.conflicts
            .iter()
            .find(|p| kind.path(p))
            .map(String::as_str)
    }
}

/// `path` without the suffix `~<side>` or `~<side>_<n>` that `git merge-tree` adds to
/// a file that it moves aside in a file/directory conflict, where `<side>` is one of
/// `sides`, the two commits as given to it.
fn unrenamed(path: &str, sides: [&str; 2]) -> String {
    let moved = path.rsplit_once('~').filter(|(_, label)| {
        sides.iter().any(|side| {
            label.strip_prefix(side).is_some_and(|n| {
                n.is_empty()
                    || n.strip_prefix('_')
                        .is_some_and(|n| n.bytes().all(|b| b.is_ascii_digit()))
            })
        })
    });
    moved.map_or(path, |(path, _)| path).to_string()
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
        .args(["-c", "core.attributesFile=/dev/null"])
        // So a walk for merge bases reads only parents and commit times
        // (`History::commit`).
        .args(["-c", "core.commitGraph=false"]);
    command
}

/// Whether `text`, a changed line of a file, is code: any line of a file that is not
/// `.rs`, and a `.rs` line that, trimmed, is not empty or a comment.
fn code(rust: bool, text: &str) -> bool {
    let text = text.trim();
    !rust || !(text.is_empty() || text.starts_with("//"))
}

/// Whether line `index` of `lines`, the text of a `.rs` file, starts with `pub `
/// after its indent, or is a `///` line of the item after it when that item does.
/// Attributes, comments, and blank lines between them do not end the doc.
fn public(lines: &[&str], index: usize) -> bool {
    let item = |line: &str| line.trim_start().starts_with("pub ");
    if !lines[index].trim_start().starts_with("///") {
        return item(lines[index]);
    }
    // The brackets that an attribute left open, so its later lines are skipped.
    let mut open = 0_i64;
    for line in &lines[index + 1..] {
        let line = line.trim();
        if open > 0 || line.starts_with("#[") {
            for c in line.chars() {
                match c {
                    '[' => open += 1,
                    ']' => open -= 1,
                    _ => {}
                }
            }
        } else if !(line.is_empty() || line.starts_with("//")) {
            return item(line);
        }
    }
    false
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
