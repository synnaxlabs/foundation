//! The drop of `free` on a disk that no other process writes.

use std::path::Path;

use env::files::{Files, Mode};

/// Creates the file `a` of 8 MiB through `files`, and asserts that `free` drops by
/// exactly its length. No other process may write the disk.
pub(crate) async fn check(files: &Files) {
    const LEN: u64 = 8 << 20;
    let before = files.free().await.unwrap();
    let mode = Mode::Create { len: LEN };
    let file = files.open(Path::new("a"), mode).await.unwrap();
    let after = files.free().await.unwrap();
    file.close().await;
    assert_eq!(
        before.checked_sub(after),
        Some(LEN),
        "{before} bytes free before, {after} after"
    );
}
