//! The modes of the files and directories that `os::files` makes under each umask.
//! The umask is a value of the process, so this test runs in a test binary of its
//! own, with this one test only: the harness runs the tests of a binary on threads of
//! one process.

// Lets Clippy treat the helpers as test code.
#![cfg(test)]
#![cfg(any(target_os = "linux", target_os = "macos"))]

mod common;

use common::{Scratch, opened};

#[test]
fn each_new_file_is_0600_and_each_new_directory_0700_under_the_umasks_022_and_0() {
    let scratch = Scratch::new();
    for mask in [0o022, 0o000] {
        rustix::process::umask(rustix::fs::Mode::from_raw_mode(mask));
        let dir = scratch.0.join(format!("{mask:o}"));
        std::fs::create_dir(&dir).unwrap();
        assert_eq!(opened(&dir), [0o700, 0o600, 0o700], "umask {mask:o}");
    }
}
