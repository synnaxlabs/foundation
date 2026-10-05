//! Address space from the OS for block pools.

use std::fmt;
use std::ptr::{self, NonNull};

use rustix::mm::{self, MapFlags, ProtFlags};

/// `MAP_NORESERVE` keeps the reserve out of the OS commit count, since a pool reserves
/// far more than its budget. Miri takes no flag but `MAP_PRIVATE`.
#[cfg(not(miri))]
const FLAGS: MapFlags = MapFlags::PRIVATE.union(MapFlags::NORESERVE);
#[cfg(miri)]
const FLAGS: MapFlags = MapFlags::PRIVATE;

const PROT: ProtFlags = ProtFlags::READ.union(ProtFlags::WRITE);

/// Address space from the OS for one block pool. Pages take memory when first
/// touched, so a commit is free, and a purge gives pages back at once.
///
/// On Linux with strict overcommit (`vm.overcommit_memory=2`), the OS counts the full
/// reserve as committed, so a large reserve can fail.
#[derive(Debug)]
pub struct Memory {
    base: NonNull<u8>,
    len: usize,
    page: usize,
}

impl Memory {
    /// Reserves `len` bytes of zeroed address space, aligned to the page size.
    ///
    /// # Errors
    ///
    /// [`Error`] when the OS gives no range of `len` bytes.
    ///
    /// # Panics
    ///
    /// If `len` is 0.
    pub fn new(len: usize) -> Result<Self, Error> {
        assert!(len > 0, "memory must be more than 0 bytes");
        // SAFETY: the OS picks the address, so the mapping replaces nothing.
        let base = unsafe { mm::mmap_anonymous(ptr::null_mut(), len, PROT, FLAGS) }
            .map_err(|errno| Error {
                len,
                code: errno.raw_os_error(),
            })?;
        let base = NonNull::new(base.cast()).expect("invariant: a mapping is not null");
        Ok(Self {
            base,
            len,
            page: rustix::param::page_size(),
        })
    }
}

// SAFETY: `Memory` owns its mapping, and any thread may unmap it.
unsafe impl Send for Memory {}

// SAFETY: the mapping stays at `base` for `len` bytes until the drop. The OS zeroes
// each page when first touched, so each byte is readable, writable, and initialized
// from the start. `purge` replaces only whole pages inside its range, with zeroed
// pages.
unsafe impl block::Memory for Memory {
    fn base(&self) -> NonNull<u8> {
        self.base
    }

    fn len(&self) -> usize {
        self.len
    }

    fn commit(&self, _offset: usize, _len: usize) {}

    /// # Panics
    ///
    /// If the range ends past the reserve, or the OS fails the purge.
    fn purge(&self, offset: usize, len: usize) {
        let end = offset.checked_add(len).filter(|&end| end <= self.len);
        let Some(end) = end else {
            panic!(
                "purge of {len} bytes at {offset} ends past {} bytes",
                self.len
            )
        };
        let (start, end) = (
            offset.next_multiple_of(self.page),
            end / self.page * self.page,
        );
        if start < end {
            // SAFETY: `start` is inside the mapping.
            let at = unsafe { self.base.add(start) };
            discard(at, end - start);
        }
    }
}

/// Gives back `len` bytes of whole pages at `at`, which read zero after it. Not
/// `MAP_FIXED`, as on macOS: a failed `MAP_FIXED` on Linux can leave a hole.
#[cfg(target_os = "linux")]
fn discard(at: NonNull<u8>, len: usize) {
    // SAFETY: the pages are in a private anonymous mapping, and no Rust reference
    // covers them.
    unsafe { mm::madvise(at.as_ptr().cast(), len, mm::Advice::LinuxDontNeed) }
        .unwrap_or_else(|errno| panic!("purge of {len} bytes failed: {errno}"));
}

/// Gives back `len` bytes of whole pages at `at`, which read zero after it. Not
/// `MADV_FREE`, which keeps the pages resident until the system needs memory.
#[cfg(not(target_os = "linux"))]
fn discard(at: NonNull<u8>, len: usize) {
    // SAFETY: the pages are in this mapping, and no Rust reference covers them.
    // `MAP_FIXED` puts zeroed pages at the same address.
    unsafe {
        mm::mmap_anonymous(at.as_ptr().cast(), len, PROT, FLAGS | MapFlags::FIXED)
    }
    .unwrap_or_else(|errno| panic!("purge of {len} bytes failed: {errno}"));
}

impl Drop for Memory {
    fn drop(&mut self) {
        // It fails only on arguments that `new` rules out. `Drop` never panics, so a
        // failure leaks the range.
        // SAFETY: `new` mapped `len` bytes at `base`, and the pool that used them is
        // gone.
        let _unmapped = unsafe { mm::munmap(self.base.as_ptr().cast(), self.len) };
    }
}

/// The OS gave no address space for a reserve.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Error {
    /// The bytes asked for.
    pub len: usize,
    /// The OS error code.
    pub code: i32,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Self { len, code } = self;
        write!(
            f,
            "a reserve of {len} bytes of address space failed with OS error {code}"
        )
    }
}

impl std::error::Error for Error {}

#[cfg(test)]
mod tests {
    use std::ffi::c_void;
    use std::ops::Range;
    use std::slice;

    use block::{Config, Memory as _, Pool};
    use rustix::param::page_size;

    use super::*;

    unsafe extern "C" {
        /// Sets bit 0 of one byte per page of the range when the page is resident.
        fn mincore(addr: *mut c_void, len: usize, vec: *mut u8) -> i32;
    }

    /// Whether each page of `pages` in the mapping at `base` is resident.
    fn resident(base: NonNull<u8>, pages: Range<usize>) -> Vec<bool> {
        let page = page_size();
        let mut vec = vec![0; pages.len()];
        // SAFETY: the caller's mapping holds the pages.
        let start = unsafe { base.add(pages.start * page) };
        // SAFETY: `start` is page-aligned in a live mapping that holds the pages, and
        // `vec` has one byte per page.
        let code = unsafe {
            mincore(start.as_ptr().cast(), pages.len() * page, vec.as_mut_ptr())
        };
        assert_eq!(code, 0, "mincore failed");
        vec.iter().map(|byte| byte & 1 == 1).collect()
    }

    /// The bytes of `memory`.
    fn bytes(memory: &mut Memory) -> &mut [u8] {
        // SAFETY: the reserve is zeroed and writable, and the borrow of `memory`
        // keeps its bytes for this slice alone.
        unsafe { slice::from_raw_parts_mut(memory.base().as_ptr(), memory.len()) }
    }

    #[test]
    fn a_reserve_is_zeroed_aligned_and_writable() {
        let page = page_size();
        let mut memory = Memory::new(3 * page + 100).unwrap();
        assert_eq!(memory.len(), 3 * page + 100);
        assert_eq!(memory.base().as_ptr().addr() % page, 0);
        assert!(bytes(&mut memory).iter().all(|&byte| byte == 0));
        bytes(&mut memory).fill(7);
        assert!(bytes(&mut memory).iter().all(|&byte| byte == 7));
    }

    #[test]
    fn a_pool_over_it_allocates_returns_and_keeps_its_budget() {
        let config = Config { budget: 1 << 16 };
        let pool =
            Pool::new(config.clone(), Memory::new(config.reservation()).unwrap());
        let largest = pool.largest();
        let blocks: Vec<_> = [1, 64, 65, 1000, 4096]
            .into_iter()
            .map(|len| {
                let mut unique = pool.alloc(len).unwrap();
                unique.fill(u8::try_from(len % 251).unwrap());
                unique.freeze()
            })
            .collect();
        for block in &blocks {
            let fill = u8::try_from(block.len() % 251).unwrap();
            assert!(block.iter().all(|&byte| byte == fill));
        }
        drop(blocks);
        pool.reclaim();
        let first = pool.alloc(largest).unwrap();
        let available = (1 << 16) - pool.committed();
        assert_eq!(
            pool.alloc(largest).unwrap_err(),
            block::Error::Exhausted {
                requested: largest,
                available,
            }
        );
        drop(first);
        assert_eq!(pool.alloc(largest).unwrap().len(), largest);
    }

    #[test]
    #[cfg_attr(miri, ignore = "Miri has no madvise, MAP_FIXED, or mincore")]
    fn a_purge_gives_back_each_page_fully_inside_its_range() {
        let page = page_size();
        let mut memory = Memory::new(4 * page).unwrap();
        bytes(&mut memory).fill(1);
        assert_eq!(resident(memory.base(), 0..4), [true; 4]);
        memory.purge(page / 2, 3 * page);
        assert_eq!(resident(memory.base(), 0..4), [true, false, false, true]);
        let bytes = bytes(&mut memory);
        assert!(bytes[..page].iter().all(|&byte| byte == 1));
        assert!(bytes[page..3 * page].iter().all(|&byte| byte == 0));
        assert!(bytes[3 * page..].iter().all(|&byte| byte == 1));
    }

    #[test]
    #[cfg_attr(miri, ignore = "Miri has no madvise, MAP_FIXED, or mincore")]
    fn a_pool_purge_gives_back_the_pages_of_an_idle_block() {
        let page = page_size();
        let config = Config { budget: 1 << 20 };
        let memory = Memory::new(config.reservation()).unwrap();
        let base = memory.base();
        let pool = Pool::new(config, memory);
        let mut unique = pool.alloc(1 << 19).unwrap();
        unique.fill(1);
        let start = unique.as_ptr().addr() - base.as_ptr().addr();
        let pages = start.div_ceil(page)..(start + unique.len()) / page;
        assert_eq!(resident(base, pages.clone()), vec![true; pages.len()]);
        drop(unique);
        pool.purge();
        assert!(pool.purge() >= 1 << 19, "the idle block was not purged");
        assert_eq!(resident(base, pages.clone()), vec![false; pages.len()]);
    }

    #[test]
    #[cfg(target_pointer_width = "64")]
    #[cfg_attr(miri, ignore = "Miri has no address space failure")]
    fn a_reserve_larger_than_the_address_space_fails() {
        let error = Memory::new(1 << 62).unwrap_err();
        assert_eq!(
            error,
            Error {
                len: 1 << 62,
                code: 12
            }
        );
        assert_eq!(
            error.to_string(),
            "a reserve of 4611686018427387904 bytes of address space failed with OS \
             error 12"
        );
    }

    #[test]
    #[should_panic(expected = "memory must be more than 0 bytes")]
    fn a_reserve_of_no_bytes_panics() {
        drop(Memory::new(0));
    }
}
