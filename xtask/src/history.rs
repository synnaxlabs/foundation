//! Reads the commit history of a repository with `git`.

use std::path::Path;
use std::process::Command;

/// The branch that a merge of `main` takes its second parent from.
const MAIN: &str = "origin/main";

/// The history of the git repository at a path.
pub(crate) struct History<'a> {
    root: &'a Path,
}

impl<'a> History<'a> {
    /// The history of the repository at `root`.
    pub(crate) fn new(root: &'a Path) -> Self {
        Self { root }
    }

    /// Reports whether `end` (a SHA or a prefix of at least 7 digits) is `head`, or
    /// reaches it through first-parent merges of a commit on `origin/main` that
    /// resolve no conflict: each merge's tree is the tree that `git merge-tree` makes
    /// of its parents.
    ///
    /// # Errors
    ///
    /// A failed `git` command, or an `end` that names no single commit.
    pub(crate) fn reaches(&self, end: &str, head: &str) -> Result<bool, String> {
        if end.len() < 7 || !end.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(format!("`{end}` is not a commit SHA of at least 7 digits"));
        }
        let end = self.git(&["rev-parse", "--verify", &format!("{end}^{{commit}}")])?;
        let mut commit =
            self.git(&["rev-parse", "--verify", &format!("{head}^{{commit}}")])?;
        while commit != end {
            let line = self.git(&["rev-list", "--parents", "-n", "1", &commit])?;
            let [_, first, second] = line.split(' ').collect::<Vec<_>>()[..] else {
                return Ok(false);
            };
            if !self.on_main(second)? || !self.clean(&commit, first, second)? {
                return Ok(false);
            }
            commit = first.to_string();
        }
        Ok(true)
    }

    fn on_main(&self, commit: &str) -> Result<bool, String> {
        let status = Command::new("git")
            .current_dir(self.root)
            .args(["merge-base", "--is-ancestor", commit, MAIN])
            .status()
            .map_err(|e| format!("git merge-base: {e}"))?;
        match status.code() {
            Some(0) => Ok(true),
            Some(1) => Ok(false),
            _ => Err(format!(
                "git merge-base --is-ancestor {commit} {MAIN}: {status}"
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
        let output = Command::new("git")
            .current_dir(self.root)
            .args(args)
            .output()
            .map_err(|e| format!("git {}: {e}", args.join(" ")))?;
        if !output.status.success() {
            return Err(failure(&args.join(" "), &output.stderr));
        }
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    }
}

fn failure(command: &str, stderr: &[u8]) -> String {
    format!("git {command}: {}", String::from_utf8_lossy(stderr).trim())
}

#[cfg(test)]
mod tests;
