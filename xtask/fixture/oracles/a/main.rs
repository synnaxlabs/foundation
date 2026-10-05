//! An oracle in two files, with one more test that runs only under loom.

mod common;

#[test]
fn passes() {
    assert!(common::ready());
}

#[cfg(loom)]
#[test]
fn passes_under_loom() {}
