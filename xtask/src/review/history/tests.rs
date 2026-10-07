use std::path::PathBuf;

use super::*;

/// A git repository in a fresh temporary directory, removed on drop.
struct Repo {
    dir: PathBuf,
}

impl Repo {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir()
            .join(format!("xtask-history-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let repo = Self { dir };
        repo.git(&["init", "--quiet", "--initial-branch", "main"]);
        repo
    }

    fn git(&self, args: &[&str]) -> String {
        let output = Command::new("git")
            .current_dir(&self.dir)
            .args(["-c", "user.name=t", "-c", "user.email=t@t"])
            .args(args)
            .output()
            .unwrap();
        assert!(output.status.success(), "git {args:?}: {output:?}");
        String::from_utf8(output.stdout).unwrap().trim().to_string()
    }

    /// Writes `text` to `file` and commits it on the current branch.
    fn commit(&self, file: &str, text: &str) -> String {
        std::fs::write(self.dir.join(file), text).unwrap();
        self.git(&["add", file]);
        self.git(&["commit", "--quiet", "-m", file]);
        self.head()
    }

    fn head(&self) -> String {
        self.git(&["rev-parse", "HEAD"])
    }

    /// Commits `text` to `file` on `main` and moves `origin/main` to it.
    fn advance_main(&self, file: &str, text: &str) -> String {
        self.git(&["switch", "--quiet", "main"]);
        let sha = self.commit(file, text);
        self.git(&["update-ref", "refs/remotes/origin/main", &sha]);
        self.git(&["switch", "--quiet", "pr"]);
        sha
    }

    /// A repository with `main` at one commit and the branch `pr` one commit after
    /// it. Returns the repository and the commit on `pr`.
    fn with_pr(name: &str) -> (Self, String) {
        let repo = Self::new(name);
        let base = repo.commit("a.txt", "base\n");
        repo.git(&["update-ref", "refs/remotes/origin/main", &base]);
        repo.git(&["switch", "--quiet", "-c", "pr"]);
        let end = repo.commit("b.txt", "pr\n");
        (repo, end)
    }

    fn reaches(&self, end: &str, head: &str) -> Result<bool, String> {
        History::new(&self.dir, "origin/main").reaches(end, head)
    }
}

impl Drop for Repo {
    fn drop(&mut self) {
        drop(std::fs::remove_dir_all(&self.dir));
    }
}

#[test]
fn reaches_itself_by_full_sha_and_by_prefix() {
    let (repo, end) = Repo::with_pr("itself");
    assert_eq!(repo.reaches(&end, &end), Ok(true));
    assert_eq!(repo.reaches(&end[..7], &end), Ok(true));
}

#[test]
fn reaches_through_clean_merges_of_main() {
    let (repo, end) = Repo::with_pr("clean");
    let main = repo.advance_main("c.txt", "main\n");
    repo.git(&["merge", "--quiet", "--no-edit", &main]);
    repo.advance_main("d.txt", "main again\n");
    repo.git(&["merge", "--quiet", "--no-edit", "origin/main"]);
    assert_eq!(repo.reaches(&end, &repo.head()), Ok(true));
}

#[test]
fn refuses_a_commit_after_the_end() {
    let (repo, end) = Repo::with_pr("after");
    let head = repo.commit("b.txt", "changed\n");
    assert_eq!(repo.reaches(&end, &head), Ok(false));
}

#[test]
fn refuses_a_merge_that_resolves_a_conflict() {
    let (repo, _) = Repo::with_pr("conflict");
    let end = repo.commit("a.txt", "pr side\n");
    repo.advance_main("a.txt", "main side\n");
    let merge = Command::new("git")
        .current_dir(&repo.dir)
        .args(["merge", "--quiet", "--no-edit", "origin/main"])
        .output()
        .unwrap();
    assert!(!merge.status.success(), "the merge has a conflict");
    std::fs::write(repo.dir.join("a.txt"), "resolved\n").unwrap();
    repo.git(&["add", "a.txt"]);
    repo.git(&["commit", "--quiet", "--no-edit"]);
    assert_eq!(repo.reaches(&end, &repo.head()), Ok(false));
}

#[test]
fn refuses_a_clean_merge_with_an_added_change() {
    let (repo, end) = Repo::with_pr("amended");
    repo.advance_main("c.txt", "main\n");
    repo.git(&["merge", "--quiet", "--no-edit", "origin/main"]);
    std::fs::write(repo.dir.join("b.txt"), "slipped in\n").unwrap();
    repo.git(&["commit", "--quiet", "--all", "--amend", "--no-edit"]);
    assert_eq!(repo.reaches(&end, &repo.head()), Ok(false));
}

#[test]
fn refuses_a_merge_of_a_branch_that_is_not_main() {
    let (repo, end) = Repo::with_pr("other");
    repo.git(&["switch", "--quiet", "-c", "other", "main"]);
    let other = repo.commit("c.txt", "other\n");
    repo.git(&["switch", "--quiet", "pr"]);
    repo.git(&["merge", "--quiet", "--no-edit", &other]);
    assert_eq!(repo.reaches(&end, &repo.head()), Ok(false));
}

#[test]
fn text_that_names_no_commit_does_not_reach() {
    let (repo, end) = Repo::with_pr("unknown");
    assert_eq!(repo.reaches("abc12", &end), Ok(false));
    assert_eq!(repo.reaches("HEAD~1x", &end), Ok(false));
    assert_eq!(repo.reaches("deadbeef", &end), Ok(false));
    assert_eq!(repo.reaches(&end[..6], &end), Ok(false));
    assert_eq!(repo.reaches(&format!("{end}^{{commit}}"), &end), Ok(false));
}

#[test]
fn names_the_git_failure_for_an_unknown_head() {
    let (repo, end) = Repo::with_pr("head");
    assert_eq!(
        repo.reaches(&end, "deadbeef"),
        Err(
            "git rev-parse --verify deadbeef^{commit}: fatal: Needed a single revision"
                .to_string()
        )
    );
}
