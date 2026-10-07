//! A create over an empty file that holds blocks past its end.

use std::os::unix::fs::MetadataExt;
use std::path::Path;

use env::files::{Files, Mode};
use rustix::fs::{self, FallocateFlags, OFlags};

/// The length that [`check`] creates.
pub(crate) const LEN: u64 = 4 << 20;
// ext4 counts the extent tree of the file in `st_blocks`.
const SLACK: u64 = 64 << 10;

/// Makes the empty file `a` in `data` with blocks past its end, creates it with
/// [`LEN`] through `files`, and asserts that it holds [`LEN`] and its extent tree,
/// and no block past its end. Returns the bytes that `a` holds.
pub(crate) async fn check(files: &Files, data: &Path) -> u64 {
    // What a crash after an allocation that kept the length leaves.
    let flags = OFlags::WRONLY.union(OFlags::CREATE);
    let fd = fs::open(data.join("a"), flags, fs::Mode::RUSR | fs::Mode::WUSR).unwrap();
    fs::fallocate(&fd, FallocateFlags::KEEP_SIZE, 0, 4 * LEN).unwrap();
    drop(fd);
    let mode = Mode::Create { len: LEN };
    files
        .open(Path::new("a"), mode)
        .await
        .unwrap()
        .close()
        .await;
    let allocated = std::fs::metadata(data.join("a")).unwrap().blocks() * 512;
    assert!(
        (LEN..=LEN + SLACK).contains(&allocated),
        "{allocated} bytes allocated for {LEN}"
    );
    // A truncate to the length frees each block past the end and no other.
    let fd = fs::open(data.join("a"), OFlags::WRONLY, fs::Mode::empty()).unwrap();
    fs::ftruncate(&fd, LEN).unwrap();
    drop(fd);
    let truncated = std::fs::metadata(data.join("a")).unwrap().blocks() * 512;
    assert_eq!(truncated, allocated);
    allocated
}
