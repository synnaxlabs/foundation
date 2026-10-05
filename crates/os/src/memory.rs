//! Address space from the OS for block pools.

use std::ffi::c_void;
use std::ops::Range;
use std::ptr::{self, NonNull};
use std::{fmt, process};

use rustix::io::Errno;
use rustix::mm::{self, MapFlags, MprotectFlags, ProtFlags};

/// The protection of reserved pages. Miri takes no `PROT_NONE`, so under it each page
/// is usable from the start.
#[cfg(not(miri))]
const RESERVED: ProtFlags = ProtFlags::empty();
#[cfg(miri)]
const RESERVED: ProtFlags = ProtFlags::READ.union(ProtFlags::WRITE);

/// Address space from the OS for one block pool. A reserved page takes no memory and
/// no commit charge. A commit makes pages readable and writable, and the OS charges
/// them. A purge gives pages and their charge back at once.
pub struct Memory {
    base: NonNull<u8>,
    len: usize,
    page: usize,
}

impl Memory {
    /// Reserves `len` bytes of address space, aligned to the page size, and commits
    /// its first page.
    ///
    /// # Errors
    ///
    /// [`Error`] when the OS fails the reserve or the commit of the first page.
    ///
    /// # Panics
    ///
    /// If `len` is 0.
    pub fn new(len: usize) -> Result<Self, Error> {
        assert!(len > 0, "memory must be more than 0 bytes");
        let reserve = |errno: Errno| Error::Reserve {
            len,
            code: errno.raw_os_error(),
        };
        // SAFETY: the OS picks the address, so the mapping replaces nothing.
        let base = unsafe {
            mm::mmap_anonymous(ptr::null_mut(), len, RESERVED, MapFlags::PRIVATE)
        }
        .map_err(reserve)?;
        let base = NonNull::new(base.cast()).expect("invariant: a mapping is not null");
        let memory = Self {
            base,
            len,
            page: rustix::param::page_size(),
        };
        #[cfg(all(target_os = "linux", not(miri)))]
        no_huge_pages(memory.at(0), len).map_err(reserve)?;
        memory.protect(0..1).map_err(reserve)?;
        Ok(memory)
    }

    /// The end of `len` bytes at `offset`.
    ///
    /// # Panics
    ///
    /// If the range ends past the reserve.
    fn end(&self, call: &str, offset: usize, len: usize) -> usize {
        let end = offset.checked_add(len).filter(|&end| end <= self.len);
        let Some(end) = end else {
            panic!(
                "{call} of {len} bytes at {offset} ends past {} bytes",
                self.len
            )
        };
        end
    }

    /// The address of page `index`.
    fn at(&self, index: usize) -> *mut c_void {
        self.base.as_ptr().wrapping_add(index * self.page).cast()
    }

    /// Makes `pages` readable and writable.
    fn protect(&self, pages: Range<usize>) -> rustix::io::Result<()> {
        let flags = MprotectFlags::READ.union(MprotectFlags::WRITE);
        // SAFETY: the pages are in this mapping, and the change of protection changes
        // no byte.
        unsafe { mm::mprotect(self.at(pages.start), pages.len() * self.page, flags) }
    }
}

/// Turns off huge pages in the range: the first touch of one byte of a huge page
/// takes all of it, and a purge of part of it gives memory back only later. A kernel
/// with no huge pages gives `EINVAL`, and has nothing to turn off.
#[cfg(all(target_os = "linux", not(miri)))]
fn no_huge_pages(at: *mut c_void, len: usize) -> rustix::io::Result<()> {
    let advice = mm::Advice::LinuxNoHugepage;
    // SAFETY: the advice changes no byte of the mapping.
    match unsafe { mm::madvise(at, len, advice) } {
        Ok(()) | Err(Errno::INVAL) => Ok(()),
        Err(errno) => Err(errno),
    }
}

// SAFETY: `Memory` owns its mapping, and any thread may unmap it.
unsafe impl Send for Memory {}

impl fmt::Debug for Memory {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Self { len, page, .. } = self;
        f.debug_struct("Memory")
            .field("len", len)
            .field("page", page)
            .finish()
    }
}

// SAFETY: the mapping stays at `base` for `len` bytes until the drop. `new` commits
// the first page. A commit makes each page it touches readable and writable, and a
// page reads zero until its first write. Only a purge, of whole pages inside its
// range, makes pages unusable again.
unsafe impl block::Memory for Memory {
    fn base(&self) -> NonNull<u8> {
        self.base
    }

    fn len(&self) -> usize {
        self.len
    }

    /// # Panics
    ///
    /// If the range ends past the reserve, or the OS fails the commit for a cause
    /// other than memory.
    fn commit(&self, offset: usize, len: usize) -> Result<(), block::Refused> {
        let end = self.end("commit", offset, len);
        let pages = offset / self.page..end.div_ceil(self.page);
        match self.protect(pages) {
            Ok(()) => Ok(()),
            Err(Errno::NOMEM) => Err(block::Refused),
            Err(errno) => panic!("commit of {len} bytes at {offset} failed: {errno}"),
        }
    }

    /// # Panics
    ///
    /// If the range ends past the reserve. Aborts if the OS fails the purge: on Linux
    /// a failed `MAP_FIXED` can leave a hole that another mapping fills, and the drop
    /// would unmap that mapping.
    fn purge(&self, offset: usize, len: usize) {
        let end = self.end("purge", offset, len);
        let pages = offset.div_ceil(self.page)..end / self.page;
        // Miri takes no `MAP_FIXED`.
        if pages.is_empty() || cfg!(miri) {
            return;
        }
        let at = self.at(pages.start);
        let len = pages.len() * self.page;
        let flags = MapFlags::PRIVATE.union(MapFlags::FIXED);
        // SAFETY: the pages are in this mapping, and the `block::Memory` contract lets
        // a purge change the bytes in its range.
        let remapped = unsafe { mm::mmap_anonymous(at, len, RESERVED, flags) };
        if remapped.is_err() {
            process::abort();
        }
        #[cfg(all(target_os = "linux", not(miri)))]
        no_huge_pages(at, len)
            .unwrap_or_else(|errno| panic!("purge of {len} bytes failed: {errno}"));
    }
}

impl Drop for Memory {
    fn drop(&mut self) {
        // SAFETY: `new` mapped `len` bytes at `base`, and the pool that used them is
        // gone.
        let unmapped = unsafe { mm::munmap(self.base.as_ptr().cast(), self.len) };
        // It fails only on arguments that `new` rules out, and `Drop` never panics.
        if unmapped.is_err() {
            process::abort();
        }
    }
}

/// A failure to get memory from the OS.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// The OS failed the reserve of `len` bytes of address space, or the commit of
    /// its first page.
    Reserve {
        /// The bytes asked for.
        len: usize,
        /// The OS error code.
        code: i32,
    },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Self::Reserve { len, code } = self;
        write!(
            f,
            "a reserve of {len} bytes of address space failed with OS error {code}; \
             lower the memory budget"
        )
    }
}

impl std::error::Error for Error {}

#[cfg(test)]
mod tests {
    use std::io;
    use std::os::fd::AsRawFd;
    use std::slice;

    use block::{Config, Memory as _, Pool};
    use rustix::param::page_size;

    use super::*;

    /// `msync` starts the writes and returns at once.
    const MS_ASYNC: i32 = 1;

    unsafe extern "C" {
        /// Sets bit 0 of one byte per page of the range when the page is resident.
        fn mincore(addr: *mut c_void, len: usize, vec: *mut u8) -> i32;
        /// Fails with `ENOMEM` when a page of the range is not mapped.
        fn msync(addr: *mut c_void, len: usize, flags: i32) -> i32;
        /// Fails with `EFAULT` when the kernel cannot read the buffer.
        fn write(fd: i32, buf: *const c_void, len: usize) -> isize;
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

    /// Whether the first byte of each page of `pages` of `memory` is readable.
    fn readable(memory: &Memory, pages: Range<usize>) -> Vec<bool> {
        let (_reader, writer) = io::pipe().unwrap();
        let fault = Some(Errno::FAULT.raw_os_error());
        pages
            .map(|index| {
                // SAFETY: the kernel reads one byte of the mapping, and no Rust
                // reference covers it.
                match unsafe { write(writer.as_raw_fd(), memory.at(index), 1) } {
                    1 => true,
                    -1 if io::Error::last_os_error().raw_os_error() == fault => false,
                    written => panic!("a write of 1 byte gave {written}"),
                }
            })
            .collect()
    }

    /// The flags of the mapping that holds page `index` of `memory`, from
    /// `/proc/self/smaps`.
    #[cfg(target_os = "linux")]
    fn flags(memory: &Memory, index: usize) -> Vec<String> {
        let at = memory.at(index).addr();
        let smaps = std::fs::read_to_string("/proc/self/smaps").unwrap();
        let mut holds = false;
        for line in smaps.lines() {
            if let Some(flags) = line.strip_prefix("VmFlags:")
                && holds
            {
                return flags.split_whitespace().map(str::to_owned).collect();
            }
            let range = line
                .split_once(' ')
                .and_then(|(range, _)| range.split_once('-'));
            if let Some((start, end)) = range
                && let (Ok(start), Ok(end)) = (
                    usize::from_str_radix(start, 16),
                    usize::from_str_radix(end, 16),
                )
            {
                holds = (start..end).contains(&at);
            }
        }
        panic!("no mapping holds the address");
    }

    /// The bytes of `range` of `memory`, which a commit made usable.
    fn bytes(memory: &mut Memory, range: Range<usize>) -> &mut [u8] {
        assert!(range.end <= memory.len());
        // SAFETY: the range is inside the mapping.
        let start = unsafe { memory.base().add(range.start) };
        // SAFETY: the caller committed the range, and the borrow of `memory` keeps
        // its bytes for this slice alone.
        unsafe { slice::from_raw_parts_mut(start.as_ptr(), range.len()) }
    }

    #[test]
    fn a_committed_reserve_is_zeroed_aligned_and_writable() {
        let page = page_size();
        let len = 3 * page + 100;
        let mut memory = Memory::new(len).unwrap();
        memory.commit(0, len).unwrap();
        assert_eq!(memory.len(), len);
        assert_eq!(memory.base().as_ptr().addr() % page, 0);
        assert!(bytes(&mut memory, 0..len).iter().all(|&byte| byte == 0));
        bytes(&mut memory, 0..len).fill(7);
        assert!(bytes(&mut memory, 0..len).iter().all(|&byte| byte == 7));
    }

    #[test]
    #[cfg_attr(miri, ignore = "Miri takes no PROT_NONE")]
    fn a_commit_makes_each_page_it_touches_usable() {
        let page = page_size();
        let memory = Memory::new(4 * page).unwrap();
        assert_eq!(readable(&memory, 0..4), [true, false, false, false]);
        memory.commit(page + 1, page).unwrap();
        assert_eq!(readable(&memory, 0..4), [true, true, true, false]);
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
    #[cfg_attr(miri, ignore = "Miri has no MAP_FIXED or mincore")]
    fn a_purge_gives_back_each_page_fully_inside_its_range() {
        let page = page_size();
        let mut memory = Memory::new(4 * page).unwrap();
        memory.commit(0, 4 * page).unwrap();
        bytes(&mut memory, 0..4 * page).fill(1);
        assert_eq!(resident(memory.base(), 0..4), [true; 4]);
        memory.purge(page / 2, 3 * page);
        assert_eq!(resident(memory.base(), 0..4), [true, false, false, true]);
        assert_eq!(readable(&memory, 0..4), [true, false, false, true]);
        assert!(bytes(&mut memory, 0..page).iter().all(|&byte| byte == 1));
        assert!(
            bytes(&mut memory, 3 * page..4 * page)
                .iter()
                .all(|&byte| byte == 1)
        );
        memory.commit(page, 2 * page).unwrap();
        assert!(
            bytes(&mut memory, page..3 * page)
                .iter()
                .all(|&byte| byte == 0)
        );
    }

    #[test]
    #[cfg_attr(miri, ignore = "Miri has no MAP_FIXED or mincore")]
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
        assert_eq!(pool.purge(), block::footprint(1 << 19));
        assert_eq!(resident(base, pages.clone()), vec![false; pages.len()]);
    }

    #[test]
    #[should_panic(expected = "purge of 1 bytes at 100 ends past 100 bytes")]
    fn a_purge_past_the_end_panics() {
        Memory::new(100).unwrap().purge(100, 1);
    }

    #[test]
    #[should_panic(expected = "commit of 2 bytes at 99 ends past 100 bytes")]
    fn a_commit_past_the_end_panics() {
        Memory::new(100).unwrap().commit(99, 2).unwrap();
    }

    #[test]
    #[cfg(target_os = "linux")]
    #[cfg_attr(miri, ignore = "Miri has no /proc")]
    fn only_a_committed_page_takes_a_commit_charge() {
        let page = page_size();
        let memory = Memory::new(4 * page).unwrap();
        let huge = std::path::Path::new("/sys/kernel/mm/transparent_hugepage").exists();
        let has = |index: usize, flag: &str| {
            let flags = flags(&memory, index);
            flags.iter().any(|on| on == flag)
        };
        assert!(has(0, "ac"));
        assert!(!has(1, "ac"));
        memory.commit(page, 2 * page).unwrap();
        assert!(has(1, "ac") && has(2, "ac"));
        assert!(!has(3, "ac"));
        memory.purge(page, page);
        assert!(!has(1, "ac"));
        assert!(has(2, "ac"));
        for index in 0..4 {
            assert_eq!(has(index, "nh"), huge, "huge pages on page {index}");
        }
    }

    #[test]
    #[cfg_attr(miri, ignore = "Miri has no msync")]
    fn a_drop_gives_back_the_range() {
        // No other mapping of the test binary is large enough to fill the range.
        let len = 1 << 30;
        let base = Memory::new(len).unwrap().base();
        // SAFETY: an async sync of anonymous pages changes nothing.
        let code = unsafe { msync(base.as_ptr().cast(), len, MS_ASYNC) };
        let error = io::Error::last_os_error().raw_os_error();
        assert_eq!((code, error), (-1, Some(12)), "the range is still mapped");
    }

    #[test]
    #[cfg(target_pointer_width = "64")]
    #[cfg_attr(miri, ignore = "Miri has no address space failure")]
    fn a_reserve_larger_than_the_address_space_fails() {
        let error = Memory::new(1 << 62).unwrap_err();
        assert_eq!(
            error,
            Error::Reserve {
                len: 1 << 62,
                code: 12
            }
        );
        assert_eq!(
            error.to_string(),
            "a reserve of 4611686018427387904 bytes of address space failed with OS \
             error 12; lower the memory budget"
        );
    }

    #[test]
    fn debug_prints_no_address() {
        let page = page_size();
        let memory = Memory::new(3 * page).unwrap();
        assert_eq!(
            format!("{memory:?}"),
            format!("Memory {{ len: {}, page: {page} }}", 3 * page)
        );
    }

    #[test]
    #[should_panic(expected = "memory must be more than 0 bytes")]
    fn a_reserve_of_no_bytes_panics() {
        drop(Memory::new(0));
    }
}
