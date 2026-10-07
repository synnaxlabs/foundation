//! The allocation of a new file on macOS, where `rustix::fs::fallocate` can allocate
//! only a part of it.

use std::os::fd::{AsRawFd, OwnedFd};

use rustix::fs;
use rustix::io::{self, Errno};

/// Allocates the first `len` bytes of the empty file `fd` on disk, all or none, and
/// sets its length to `len`.
///
/// # Errors
///
/// `NOSPC` when the disk cannot hold all `len` bytes, and the errors of
/// `fcntl(F_PREALLOCATE)` and `ftruncate`.
pub(crate) fn all(fd: &OwnedFd, len: u64) -> io::Result<()> {
    let mut store = libc::fstore_t {
        fst_flags: libc::F_ALLOCATEALL,
        fst_posmode: libc::F_PEOFPOSMODE,
        fst_offset: 0,
        fst_length: libc::off_t::try_from(len).map_err(|_past| Errno::FBIG)?,
        fst_bytesalloc: 0,
    };
    // SAFETY: `fd` is open, and the call writes only into `store`, which lives past it.
    let done =
        unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_PREALLOCATE, &raw mut store) };
    if done == -1 {
        let code = std::io::Error::last_os_error().raw_os_error();
        return Err(Errno::from_raw_os_error(
            code.expect("invariant: an OS error"),
        ));
    }
    fs::ftruncate(fd, len)
}

#[cfg(test)]
mod tests {
    use rustix::fs::OFlags;

    use super::*;

    /// A read-only file of this test binary.
    fn read_only() -> OwnedFd {
        let path = std::env::current_exe().unwrap();
        fs::open(path, OFlags::RDONLY, fs::Mode::empty()).unwrap()
    }

    #[test]
    fn a_file_open_to_read_gives_the_error_of_the_allocation() {
        // `ftruncate` gives `INVAL` here, so this fails when the error is not checked.
        assert_eq!(all(&read_only(), 4096), Err(Errno::BADF));
    }

    #[test]
    fn a_length_past_the_largest_offset_gives_fbig() {
        assert_eq!(all(&read_only(), u64::MAX), Err(Errno::FBIG));
    }
}
