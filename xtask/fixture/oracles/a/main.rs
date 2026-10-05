//! An oracle in two files, with one more test that fails under loom.

mod common;

#[test]
fn passes() {
    assert!(common::ready());
}

#[cfg(loom)]
#[test]
fn fails_under_loom() {
    panic!("expected: the xtask tests check that this loom test fails");
}
