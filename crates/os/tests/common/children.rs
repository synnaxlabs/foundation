//! Lists the sockets that a new child holds, for the test binaries of `os` that spawn
//! children.

use std::process::Command;

/// Each socket that a new child holds after its exec, by its path in `/dev/fd`.
pub(crate) fn held() -> Vec<String> {
    let list = r#"for f in /dev/fd/*; do if [ -S "$f" ]; then echo "$f"; fi; done"#;
    let listed = (Command::new("sh").args(["-c", list]).output()).expect("run sh");
    assert!(listed.status.success(), "sh lists the descriptors");
    let listed = String::from_utf8(listed.stdout).expect("sh writes UTF-8");
    listed.lines().map(String::from).collect()
}
