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
        // A killed run with the same PID can leave the directory behind.
        match std::fs::remove_dir_all(&dir) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => panic!("{e}"),
            _ => {}
        }
        std::fs::create_dir_all(&dir).unwrap();
        let repo = Self { dir };
        repo.git(&["init", "--quiet", "--initial-branch", "main"]);
        repo
    }

    /// A `git` command in the repository as `History` makes it, with an identity and
    /// no global ignore file, so the machine's settings cannot change a test.
    fn command(&self) -> Command {
        let mut command = command(&self.dir);
        command
            .args(["-c", "user.name=t", "-c", "user.email=t@t"])
            .args(["-c", "core.excludesFile=/dev/null"]);
        command
    }

    fn git(&self, args: &[&str]) -> String {
        let output = self.command().args(args).output().unwrap();
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
        History::new(&self.dir, "main").reaches(end, head)
    }

    fn code_change(&self, from: &str, end: &str) -> Result<Option<String>, String> {
        History::new(&self.dir, "main").code_change(from, end)
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
    let merge = repo
        .command()
        .args(["merge", "--no-edit", "origin/main"])
        .output()
        .unwrap();
    let out = String::from_utf8_lossy(&merge.stdout);
    assert!(
        out.contains("CONFLICT (content): Merge conflict in a.txt"),
        "{merge:?}"
    );
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

#[test]
fn a_range_of_comment_and_blank_lines_changes_no_code() {
    let (repo, from) = Repo::with_pr("comments");
    let code = repo.commit("a.rs", "/// A.\nfn a() {}\n");
    let docs = repo.commit("a.rs", "/// A, wrapped.\n\n  // B.\nfn a() {}\n\n");
    assert_eq!(repo.code_change(&code, &docs), Ok(None));
    let text = repo.commit("b.txt", "fn b() {}\n");
    assert_eq!(repo.code_change(&code[..7], &text[..7]), Ok(None));
    let removed = repo.commit("a.rs", "fn a() {}\n");
    assert_eq!(repo.code_change(&text, &removed), Ok(None));
    assert_eq!(repo.code_change(&removed, &removed), Ok(None));
    assert_eq!(
        repo.code_change(&from, &code),
        Ok(Some("changes code at `a.rs:2`".to_string()))
    );
}

#[test]
fn names_the_first_line_of_code_that_a_range_changes() {
    let (repo, _) = Repo::with_pr("code");
    let from = repo.commit("a.rs", "// A.\nfn a() {}\n\nfn b() {}\n");
    repo.commit("c.rs", "fn c() {}\n");
    let end = repo.commit("a.rs", "// A, B.\nfn a() {}\n\nfn d() {}\n");
    assert_eq!(
        repo.code_change(&from, &end),
        Ok(Some("changes code at `a.rs:4`".to_string()))
    );
    let deleted = repo.commit("a.rs", "// A, B.\nfn a() {}\n\n");
    assert_eq!(
        repo.code_change(&end, &deleted),
        Ok(Some("changes code at `a.rs:4`".to_string()))
    );
    std::fs::remove_file(repo.dir.join("c.rs")).unwrap();
    repo.git(&["commit", "--quiet", "-am", "rm"]);
    assert_eq!(
        repo.code_change(&deleted, &repo.head()),
        Ok(Some("changes code at `c.rs:1`".to_string()))
    );
}

#[test]
fn a_range_that_names_no_commit_has_that_text() {
    let (repo, end) = Repo::with_pr("unnamed");
    assert_eq!(
        repo.code_change("deadbeef", &end),
        Ok(Some("has `deadbeef`, which names no commit".to_string()))
    );
    assert_eq!(
        repo.code_change(&end, "HEAD"),
        Ok(Some("has `HEAD`, which names no commit".to_string()))
    );
}

#[test]
fn counts_lines_to_the_first_line_of_code() {
    let (repo, _) = Repo::with_pr("count");
    let empty = repo.commit("a.rs", "");
    let added = repo.commit("a.rs", "// A.\n\nfn a() {}\n");
    assert_eq!(
        repo.code_change(&empty, &added),
        Ok(Some("changes code at `a.rs:3`".to_string()))
    );
    assert_eq!(
        repo.code_change(&added, &empty),
        Ok(Some("changes code at `a.rs:3`".to_string()))
    );
}

#[test]
fn a_moved_file_and_a_manifest_change_code() {
    let (repo, _) = Repo::with_pr("moved");
    std::fs::create_dir_all(repo.dir.join("tests/data")).unwrap();
    let from = repo.commit("tests/oracle.rs", "#[test]\nfn holds() {}\n");
    repo.git(&["mv", "tests/oracle.rs", "tests/data/oracle.rs"]);
    repo.git(&["commit", "--quiet", "-m", "move"]);
    assert_eq!(
        repo.code_change(&from, &repo.head()),
        Ok(Some("changes code at `tests/data/oracle.rs:1`".to_string()))
    );
    let from = repo.head();
    std::fs::create_dir_all(repo.dir.join("crates/a")).unwrap();
    let toml = repo.commit("crates/a/Cargo.toml", "# A comment.\n");
    assert_eq!(
        repo.code_change(&from, &toml),
        Ok(Some("changes code at `crates/a/Cargo.toml:1`".to_string()))
    );
    let lock = repo.commit("Cargo.lock", "\n");
    assert_eq!(
        repo.code_change(&toml, &lock),
        Ok(Some("changes code at `Cargo.lock:1`".to_string()))
    );
}

#[test]
fn a_removed_line_that_looks_like_a_header_is_code() {
    let (repo, _) = Repo::with_pr("header");
    let from = repo.commit("a.rs", "const Q: &str = \"\n-- a\n++ b\n\";\n");
    let end = repo.commit("a.rs", "const Q: &str = \"\n\";\n");
    assert_eq!(
        repo.code_change(&from, &end),
        Ok(Some("changes code at `a.rs:2`".to_string()))
    );
}

#[test]
fn a_nul_byte_does_not_hide_code() {
    let (repo, from) = Repo::with_pr("nul");
    let added = repo.commit("a.rs", "// A \0 byte.\nfn a() {}\n");
    let changed = repo.commit("a.rs", "// A \0 byte.\nfn b() {}\n");
    let found = Ok(Some("changes code at `a.rs:2`".to_string()));
    assert_eq!(repo.code_change(&from, &added), found);
    assert_eq!(repo.code_change(&added, &changed), found);
}

#[test]
fn reads_a_name_that_git_quotes() {
    for name in ["caf\u{e9}.rs", "a b.rs", "a\tb.rs", "\"a\".rs"] {
        let (repo, _) = Repo::with_pr("quoted");
        let from = repo.commit(name, "fn a() {}\n");
        let docs = repo.commit(name, "// A.\nfn a() {}\n");
        assert_eq!(repo.code_change(&from, &docs), Ok(None), "{name:?}");
        let code = repo.commit(name, "// A.\nfn b() {}\n");
        assert_eq!(
            repo.code_change(&docs, &code),
            Ok(Some(format!("changes code at `{name}:2`"))),
            "{name:?}"
        );
    }
}

#[test]
fn a_move_does_not_hide_a_removed_line_of_code() {
    let (repo, _) = Repo::with_pr("rename");
    let comments = "// c\n".repeat(10);
    let from = repo.commit("a.rs", &format!("{comments}fn a() {{}}\n"));
    repo.git(&["rm", "--quiet", "a.rs"]);
    let end = repo.commit("b.rs", &comments);
    assert_eq!(
        repo.code_change(&from, &end),
        Ok(Some("changes code at `a.rs:11`".to_string()))
    );
}

#[test]
fn a_diff_driver_config_does_not_hide_code() {
    for config in [["diff.x.textconv", "true"], ["diff.external", "true"]] {
        let (repo, _) = Repo::with_pr("driver");
        repo.git(&["config", config[0], config[1]]);
        repo.commit(".gitattributes", "* diff=x\n");
        let from = repo.commit("a.rs", "// a\n");
        let code = repo.commit("a.rs", "fn a() {}\n");
        assert_eq!(
            repo.code_change(&from, &code),
            Ok(Some("changes code at `a.rs:1`".to_string())),
            "{config:?}"
        );
    }
}

#[test]
fn a_colored_diff_config_does_not_hide_code() {
    let (repo, _) = Repo::with_pr("color");
    repo.git(&["config", "color.diff", "always"]);
    repo.git(&["config", "diff.noprefix", "false"]);
    repo.git(&["config", "diff.renames", "copies"]);
    let from = repo.commit("a.rs", "// a\n");
    let code = repo.commit("a.rs", "fn a() {}\n");
    assert_eq!(
        repo.code_change(&from, &code),
        Ok(Some("changes code at `a.rs:1`".to_string()))
    );
}

#[test]
fn an_inter_hunk_context_config_keeps_the_line_number() {
    let (repo, _) = Repo::with_pr("context");
    repo.git(&["config", "diff.interHunkContext", "10"]);
    let from = repo.commit("a.rs", "// a\nfn keep() {}\n// b\n");
    let end = repo.commit("a.rs", "// A\nfn keep() {}\n// b\nfn c() {}\n");
    assert_eq!(
        repo.code_change(&from, &end),
        Ok(Some("changes code at `a.rs:4`".to_string()))
    );
}

#[test]
fn a_merge_round_with_no_resolution_skips_the_breaker() {
    let (repo, end) = Repo::with_pr("merge-none");
    repo.advance_main("a.rs", "fn a() {}\n");
    repo.git(&["merge", "--quiet", "--no-edit", "origin/main"]);
    let docs = repo.commit("b.rs", "// B.\n");
    repo.advance_main("c.rs", "fn c() {}\n");
    repo.git(&["merge", "--quiet", "--no-edit", "origin/main"]);
    assert_eq!(repo.code_change(&end, &repo.head()), Ok(None));
    assert_eq!(repo.code_change(&docs, &repo.head()), Ok(None));
}

#[test]
fn a_merge_round_whose_resolution_changes_code_needs_the_breaker() {
    let (repo, end) = Repo::with_pr("merge-code");
    repo.advance_main("a.rs", "fn a() {}\n");
    repo.git(&["merge", "--quiet", "--no-commit", "origin/main"]);
    std::fs::write(repo.dir.join("a.rs"), "fn a() {}\nfn b() {}\n").unwrap();
    repo.git(&["commit", "--quiet", "--all", "--no-edit"]);
    assert_eq!(
        repo.code_change(&end, &repo.head()),
        Ok(Some("changes code at `a.rs:2`".to_string()))
    );
}

#[test]
fn a_merge_round_whose_resolution_resolves_a_conflict_needs_the_breaker() {
    let (repo, _) = Repo::with_pr("merge-conflict");
    let end = repo.commit("a.rs", "// PR.\n");
    repo.advance_main("a.rs", "// Main.\n");
    let merge = repo
        .command()
        .args(["merge", "--no-edit", "origin/main"])
        .output()
        .unwrap();
    assert_eq!(merge.status.code(), Some(1), "{merge:?}");
    std::fs::write(repo.dir.join("a.rs"), "// PR.\n// Main.\n").unwrap();
    repo.git(&["commit", "--quiet", "--all", "--no-edit"]);
    assert_eq!(
        repo.code_change(&end, &repo.head()),
        Ok(Some("changes code at `a.rs:1`".to_string()))
    );
}

#[test]
fn a_merge_round_finds_code_before_and_after_the_merge() {
    let (repo, end) = Repo::with_pr("merge-around");
    repo.commit("b.rs", "fn b() {}\n");
    repo.advance_main("a.rs", "fn a() {}\n");
    repo.git(&["merge", "--quiet", "--no-edit", "origin/main"]);
    let merge = repo.head();
    repo.commit("c.rs", "// C.\nfn c() {}\n");
    assert_eq!(
        repo.code_change(&end, &merge),
        Ok(Some("changes code at `b.rs:1`".to_string()))
    );
    assert_eq!(
        repo.code_change(&merge, &repo.head()),
        Ok(Some("changes code at `c.rs:2`".to_string()))
    );
}

#[test]
fn a_merge_of_a_branch_that_is_not_main_counts_whole() {
    let (repo, end) = Repo::with_pr("merge-other");
    repo.git(&["switch", "--quiet", "-c", "other", "main"]);
    let other = repo.commit("a.rs", "fn a() {}\n");
    repo.git(&["switch", "--quiet", "pr"]);
    repo.git(&["merge", "--quiet", "--no-edit", &other]);
    assert_eq!(
        repo.code_change(&end, &repo.head()),
        Ok(Some("changes code at `a.rs:1`".to_string()))
    );
}

#[test]
fn reads_only_the_file_that_a_directory_of_the_same_name_replaces() {
    let (repo, _) = Repo::with_pr("dir");
    let from = repo.commit("x.rs", "// a\n");
    repo.git(&["rm", "--quiet", "x.rs"]);
    std::fs::create_dir(repo.dir.join("x.rs")).unwrap();
    let docs = repo.commit("x.rs/b.rs", "// b\n");
    assert_eq!(repo.code_change(&from, &docs), Ok(None));
    let code = repo.commit("x.rs/b.rs", "fn b() {}\n");
    assert_eq!(
        repo.code_change(&from, &code),
        Ok(Some("changes code at `x.rs/b.rs:1`".to_string()))
    );
}

#[test]
fn names_the_file_when_a_file_and_a_directory_of_its_name_trade_places() {
    let (repo, _) = Repo::with_pr("swap");
    let comments = "// c\n".repeat(10);
    let from = repo.commit("x.rs", &comments);
    repo.git(&["rm", "--quiet", "x.rs"]);
    std::fs::create_dir(repo.dir.join("x.rs")).unwrap();
    let dir = repo.commit("x.rs/b.rs", &format!("{comments}fn b() {{}}\n"));
    let named = Ok(Some("changes code at `x.rs/b.rs:11`".to_string()));
    assert_eq!(repo.code_change(&from, &dir), named);
    repo.git(&["rm", "--quiet", "-r", "x.rs"]);
    let file = repo.commit("x.rs", &comments);
    assert_eq!(repo.code_change(&dir, &file), named);
}

#[cfg(unix)]
#[test]
fn reads_a_link_that_a_file_of_code_replaces() {
    let (repo, _) = Repo::with_pr("link");
    std::os::unix::fs::symlink("// a", repo.dir.join("x.rs")).unwrap();
    repo.git(&["add", "x.rs"]);
    repo.git(&["commit", "--quiet", "-m", "link"]);
    let from = repo.head();
    std::fs::remove_file(repo.dir.join("x.rs")).unwrap();
    let end = repo.commit("x.rs", "// a\nfn b() {}\n");
    assert_eq!(
        repo.code_change(&from, &end),
        Ok(Some("changes code at `x.rs:2`".to_string()))
    );
}

#[test]
fn an_order_file_config_does_not_hide_code() {
    let (repo, _) = Repo::with_pr("order");
    std::fs::write(repo.dir.join("order"), "x.rs/*\n*\n").unwrap();
    repo.git(&["config", "diff.orderFile", "order"]);
    let from = repo.commit("x.rs", "fn a() {}\n");
    repo.git(&["rm", "--quiet", "x.rs"]);
    std::fs::create_dir(repo.dir.join("x.rs")).unwrap();
    let end = repo.commit("x.rs/b.rs", "// b\n");
    assert_eq!(
        repo.code_change(&from, &end),
        Ok(Some("changes code at `x.rs:1`".to_string()))
    );
}

#[test]
fn a_decomposed_name_does_not_hide_code() {
    let (repo, _) = Repo::with_pr("nfd");
    repo.git(&["config", "core.precomposeUnicode", "false"]);
    let name = "cafe\u{301}.rs";
    let from = repo.commit(name, "fn a() {}\n");
    let end = repo.commit(name, "fn b() {}\n");
    repo.git(&["config", "core.precomposeUnicode", "true"]);
    assert_eq!(
        repo.code_change(&from, &end),
        Ok(Some(format!("changes code at `{name}:1`")))
    );
}

#[cfg(unix)]
#[test]
fn names_a_path_that_is_not_utf8() {
    use std::os::unix::ffi::OsStrExt;
    let (repo, from) = Repo::with_pr("bytes");
    let blob = repo.git(&["rev-parse", "HEAD:b.txt"]);
    let info = [b"100644,", blob.as_bytes(), b",a\xff.rs"].concat();
    let status = repo
        .command()
        .args(["update-index", "--add", "--cacheinfo"])
        .arg(std::ffi::OsStr::from_bytes(&info))
        .status()
        .unwrap();
    assert!(status.success());
    repo.git(&["commit", "--quiet", "-m", "bytes"]);
    assert_eq!(
        repo.code_change(&from, &repo.head()),
        Err(
            "git diff: the path `a\u{fffd}.rs` is not UTF-8: invalid utf-8 sequence \
             of 1 bytes from index 1"
                .to_string()
        )
    );
}

#[test]
fn a_ref_named_like_a_short_blob_does_not_hide_code() {
    let (repo, _) = Repo::with_pr("short");
    let from = repo.commit("a.rs", "// a\n");
    let code = repo.commit("a.rs", "fn a() {}\n");
    let raw = repo.git(&["diff", "--raw", &from, &code]);
    let short = raw.split(' ').nth(3).unwrap().to_string();
    repo.git(&["tag", &short, &format!("{from}:a.rs")]);
    assert_eq!(
        repo.code_change(&from, &code),
        Ok(Some("changes code at `a.rs:1`".to_string()))
    );
}

#[test]
fn a_tag_named_like_a_commit_prefix_does_not_hide_code() {
    let (repo, _) = Repo::with_pr("tagfrom");
    let from = repo.commit("a.rs", "// a\n");
    let end = repo.commit("a.rs", "fn a() {}\n");
    repo.git(&["tag", &from[..8], &end]);
    assert_eq!(
        repo.code_change(&from[..8], &end[..8]),
        Ok(Some("changes code at `a.rs:1`".to_string()))
    );
}

#[test]
fn a_tag_named_like_an_end_prefix_does_not_reach() {
    let (repo, end) = Repo::with_pr("tagend");
    let head = repo.commit("a.rs", "fn a() {}\n");
    repo.git(&["tag", &end[..8], &head]);
    assert_eq!(repo.reaches(&end[..8], &head), Ok(false));
}

#[test]
fn a_tag_named_like_the_base_does_not_put_a_commit_on_it() {
    let (repo, end) = Repo::with_pr("tagbase");
    repo.git(&["switch", "--quiet", "-c", "side", "origin/main"]);
    let side = repo.commit("a.rs", "fn unreviewed() {}\n");
    repo.git(&["switch", "--quiet", "pr"]);
    repo.git(&["tag", "origin/main", &side]);
    repo.git(&["merge", "--quiet", "--no-edit", &side]);
    assert_eq!(repo.reaches(&end, &repo.head()), Ok(false));
}

#[test]
fn hex_after_a_full_sha_names_no_commit() {
    let (repo, end) = Repo::with_pr("longhex");
    for zeros in [1, 25] {
        let long = format!("{end}{}", "0".repeat(zeros));
        assert_eq!(repo.reaches(&long, &end), Ok(false));
        assert_eq!(
            repo.code_change(&long, &end),
            Ok(Some(format!("has `{long}`, which names no commit")))
        );
    }
}

#[test]
fn a_prefix_of_two_commits_names_no_commit() {
    let (repo, _) = Repo::with_pr("twins");
    let tree = repo.git(&["hash-object", "-t", "tree", "/dev/null"]);
    let shas = [17902, 26499].map(|n| {
        let body = format!(
            "tree {tree}\nauthor t <t@t> 0 +0000\ncommitter t <t@t> 0 +0000\n\n{n}\n"
        );
        std::fs::write(repo.dir.join("body"), body).unwrap();
        repo.git(&["hash-object", "-t", "commit", "-w", "body"])
    });
    for sha in &shas {
        assert_eq!(&sha[..7], "303b29b");
        assert_eq!(repo.reaches(&sha[..7], sha), Ok(false));
        assert_eq!(repo.reaches(sha, sha), Ok(true));
    }
}

#[test]
fn a_local_main_does_not_put_a_commit_on_the_base() {
    let (repo, end) = Repo::with_pr("localmain");
    repo.git(&["switch", "--quiet", "-c", "side", "origin/main"]);
    let side = repo.commit("a.rs", "fn unreviewed() {}\n");
    repo.git(&["switch", "--quiet", "pr"]);
    repo.git(&["branch", "--force", "main", &side]);
    repo.git(&["merge", "--quiet", "--no-edit", &side]);
    assert_eq!(repo.reaches(&end, &repo.head()), Ok(false));
}

#[test]
fn a_tag_does_not_stand_in_for_a_missing_base() {
    let (repo, end) = Repo::with_pr("nobase");
    repo.git(&["switch", "--quiet", "-c", "side", "main"]);
    let side = repo.commit("a.rs", "fn unreviewed() {}\n");
    repo.git(&["switch", "--quiet", "pr"]);
    repo.git(&["merge", "--quiet", "--no-edit", &side]);
    repo.git(&["update-ref", "-d", "refs/remotes/origin/main"]);
    repo.git(&["tag", "refs/remotes/origin/main", &side]);
    assert_eq!(
        repo.reaches(&end, &repo.head()),
        Err(
            "git show-ref --verify --hash refs/remotes/origin/main: fatal: \
             'refs/remotes/origin/main' - not a valid ref"
                .to_string()
        )
    );
}

#[test]
fn a_directory_left_by_a_killed_run_does_not_break_a_test() {
    let stale = std::mem::ManuallyDrop::new(Repo::with_pr("stale").0);
    let _cleanup = Repo {
        dir: stale.dir.clone(),
    };
    let (repo, end) = Repo::with_pr("stale");
    assert_eq!(repo.reaches(&end, &end), Ok(true));
}

#[test]
fn an_inherited_git_environment_does_not_reach_another_repository() {
    let other = Repo::new("inherited");
    let output = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "review::history::tests::reaches_through_clean_merges_of_main",
        ])
        .env("GIT_DIR", other.dir.join(".git"))
        .env("GIT_CONFIG_PARAMETERS", "'commit.gpgsign'='true'")
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("1 passed"), "{output:?}");
    let refs = other.command().arg("show-ref").output().unwrap();
    assert_eq!(String::from_utf8_lossy(&refs.stdout), "");
}

#[test]
fn a_union_resolution_of_a_conflict_does_not_reach() {
    let (repo, _) = Repo::with_pr("union");
    let end = repo.commit("a.txt", "pr side\n");
    repo.advance_main("a.txt", "main side\n");
    repo.command()
        .args(["merge", "--no-edit", "origin/main"])
        .output()
        .unwrap();
    std::fs::write(repo.dir.join("a.txt"), "pr side\nmain side\n").unwrap();
    repo.git(&["add", "a.txt"]);
    repo.git(&["commit", "--quiet", "--no-edit"]);
    assert_eq!(repo.reaches(&end, &repo.head()), Ok(false));
}

#[test]
fn a_merge_driver_in_the_tree_does_not_hide_a_conflict() {
    let (repo, _) = Repo::with_pr("tree-driver");
    repo.commit(".gitattributes", "* merge=union\n");
    let end = repo.commit("a.txt", "pr side\n");
    repo.advance_main("a.txt", "main side\n");
    repo.git(&["merge", "--quiet", "--no-edit", "origin/main"]);
    let merged = std::fs::read_to_string(repo.dir.join("a.txt")).unwrap();
    assert_eq!(merged, "pr side\nmain side\n");
    assert_eq!(repo.reaches(&end, &repo.head()), Ok(false));
}

#[test]
fn reaches_through_a_clean_merge_of_a_rename() {
    let (repo, _) = Repo::with_pr("rename");
    let lines = (1..=10)
        .map(|n| format!("line {n}\n"))
        .collect::<Vec<_>>()
        .concat();
    let main = repo.advance_main("a.txt", &lines);
    repo.git(&["merge", "--quiet", "--no-edit", &main]);
    repo.git(&["switch", "--quiet", "main"]);
    repo.git(&["mv", "a.txt", "moved.txt"]);
    repo.git(&["commit", "--quiet", "-m", "move"]);
    let moved = repo.head();
    repo.git(&["update-ref", "refs/remotes/origin/main", &moved]);
    repo.git(&["switch", "--quiet", "pr"]);
    let end = repo.commit("a.txt", &lines.replace("line 10", "line ten"));
    repo.git(&["merge", "--quiet", "--no-edit", "origin/main"]);
    assert_eq!(repo.reaches(&end, &repo.head()), Ok(true));
}

#[test]
fn the_machine_git_settings_do_not_change_a_test() {
    let home =
        std::env::temp_dir().join(format!("xtask-history-home-{}", std::process::id()));
    std::fs::create_dir_all(home.join("git")).unwrap();
    std::fs::write(home.join(".gitconfig"), "[diff]\n\trenames = false\n").unwrap();
    std::fs::write(home.join("git/ignore"), "Cargo.lock\n").unwrap();
    std::fs::write(home.join("git/attributes"), "* merge=union\n").unwrap();
    let output = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "review::history::tests::a_union_resolution_of_a_conflict_does_not_reach",
            "review::history::tests::reaches_through_a_clean_merge_of_a_rename",
            "review::history::tests::a_moved_file_and_a_manifest_change_code",
        ])
        .env("HOME", &home)
        .env("XDG_CONFIG_HOME", &home)
        .output()
        .unwrap();
    std::fs::remove_dir_all(&home).unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("3 passed"), "{output:?}");
}

#[test]
fn reads_no_system_config_or_attributes_file() {
    let repo = Repo::new("system");
    for name in ["GIT_CONFIG_SYSTEM", "GIT_ATTR_SYSTEM"] {
        let output = command(&repo.dir).args(["var", name]).output().unwrap();
        // Exit 1 with no output is a known variable with no value; an unknown one
        // exits 129.
        assert_eq!(output.status.code(), Some(1), "{name}: {output:?}");
        assert_eq!(String::from_utf8_lossy(&output.stdout), "", "{name}");
    }
}
