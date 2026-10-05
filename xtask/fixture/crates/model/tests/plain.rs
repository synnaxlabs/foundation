//! A test target that names no cfg, so the loom task must not run it.

#[test]
fn fails_when_run() {
    panic!("expected: the loom task runs only the targets that name loom");
}
