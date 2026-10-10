use types::byte::Size;

use crate::Error;

/// The memory that the OS can give to new allocations of this process now, with no
/// swap. On Linux, the lesser of `MemAvailable` and the room left in the memory cgroup
/// of the process and in each cgroup above it: the limit less the working set, which
/// is the usage less the inactive file pages. On cgroup v2 these are `memory.max`,
/// `memory.current`, and `inactive_file` in `memory.stat`; on cgroup v1,
/// `memory.limit_in_bytes`, `memory.usage_in_bytes`, and `total_inactive_file` in
/// `memory.stat`. A cgroup with no `memory.stat`, as under gVisor, counts no inactive
/// file pages. On macOS, the free, inactive, and purgeable pages.
///
/// # Errors
///
/// [`Error::Memory`] when the OS cannot tell.
pub fn available() -> Result<Size, Error> {
    #[cfg(target_os = "linux")]
    let bytes = linux::available(std::path::Path::new("/"));
    #[cfg(target_os = "macos")]
    let bytes = macos::available();
    bytes.map(Size::from_bytes).map_err(Error::Memory)
}

/// The memory available to a process whose file system root is `root`: what
/// [`available`] gives, with each file of `/proc` and of the cgroups read under
/// `root`. For tests.
///
/// # Errors
///
/// [`Error::Memory`] when the files under `root` cannot tell.
#[cfg(all(target_os = "linux", feature = "sim"))]
pub fn available_under(root: &std::path::Path) -> Result<Size, Error> {
    linux::available(root)
        .map(Size::from_bytes)
        .map_err(Error::Memory)
}

#[cfg(target_os = "linux")]
mod linux {
    use std::ffi::{OsStr, OsString};
    use std::fs;
    use std::io;
    use std::os::unix::ffi::{OsStrExt, OsStringExt};
    use std::path::{Component, Path, PathBuf};

    /// The files of a cgroup that give its limit and its use, and the key in
    /// `memory.stat` of its inactive file pages.
    struct Files {
        limit: &'static str,
        usage: &'static str,
        inactive: &'static str,
    }

    const V1: Files = Files {
        limit: "memory.limit_in_bytes",
        usage: "memory.usage_in_bytes",
        inactive: "total_inactive_file",
    };

    const V2: Files = Files {
        limit: "memory.max",
        usage: "memory.current",
        inactive: "inactive_file",
    };

    /// A line of `mountinfo`.
    struct Mount<'a> {
        root: PathBuf,
        point: PathBuf,
        kind: &'a [u8],
        super_options: &'a [u8],
    }

    impl<'a> Mount<'a> {
        /// The mount of `line`, or `None` when it is no mount.
        fn parse(line: &'a [u8]) -> Option<Self> {
            let mut fields = line.split(|&byte| byte == b' ');
            let root = fields.nth(3)?;
            let point = fields.next()?;
            // The mount options, then 0 or more optional fields up to `-`.
            let mut fields = fields.skip(1).skip_while(|field| *field != b"-").skip(1);
            let kind = fields.next()?;
            let super_options = fields.nth(1)?;
            Some(Self {
                root: unescape(root),
                point: unescape(point),
                kind,
                super_options,
            })
        }
    }

    /// A line of `/proc/self/cgroup`.
    struct Cgroup<'a> {
        /// Empty on cgroup v2.
        controllers: &'a [u8],
        path: &'a [u8],
    }

    impl<'a> Cgroup<'a> {
        /// The cgroup of `line`, or `None` when it is no cgroup.
        fn parse(line: &'a [u8]) -> Option<Self> {
            // The path is the rest of the line, and can hold a `:`.
            let mut fields = line.splitn(3, |&byte| byte == b':');
            fields.next()?;
            Some(Self {
                controllers: fields.next()?,
                path: fields.next()?,
            })
        }
    }

    /// The available bytes of the process, whose file system root is `root`.
    pub(super) fn available(root: &Path) -> io::Result<u64> {
        let file = root.join("proc/meminfo");
        let mut least = mem_available(&file, &read(&file)?)?;
        let file = root.join("proc/self/cgroup");
        let Some(cgroups) = optional(bytes(&file))? else {
            // A kernel with no cgroups.
            return Ok(least);
        };
        let cgroups = lines(&cgroups)
            .map(|line| {
                Cgroup::parse(line).ok_or_else(|| malformed(&file, "cgroup", line))
            })
            .collect::<io::Result<Vec<_>>>()?;
        let file = root.join("proc/self/mountinfo");
        for line in lines(&bytes(&file)?) {
            let mount =
                Mount::parse(line).ok_or_else(|| malformed(&file, "mount", line))?;
            let Some((files, inside)) = hierarchy(&mount, &cgroups) else {
                continue;
            };
            let point = &mount.point;
            let top = root.join(point.strip_prefix("/").unwrap_or(point));
            let mut dir = top.join(inside);
            loop {
                if let Some(room) = room(&dir, &files)? {
                    least = least.min(room);
                }
                if dir == top || !dir.pop() {
                    break;
                }
            }
        }
        Ok(least)
    }

    /// For a mount of a memory cgroup hierarchy, its files and the cgroup of the
    /// process inside the mount. `None` for another mount, or a cgroup outside it.
    fn hierarchy(
        mount: &Mount<'_>,
        cgroups: &[Cgroup<'_>],
    ) -> Option<(Files, PathBuf)> {
        let (files, controller) = match mount.kind {
            b"cgroup2" => (V2, None),
            b"cgroup" if listed(mount.super_options, b"memory") => {
                (V1, Some(b"memory"))
            }
            _ => return None,
        };
        let cgroup = cgroups.iter().find(|cgroup| match controller {
            None => cgroup.controllers.is_empty(),
            Some(controller) => listed(cgroup.controllers, controller),
        })?;
        let inside = Path::new(OsStr::from_bytes(cgroup.path))
            .strip_prefix(&mount.root)
            .ok()?;
        // A cgroup outside the root of the cgroup namespace starts with `..`.
        let outside = inside.components().any(|c| c == Component::ParentDir);
        (!outside).then(|| (files, inside.to_path_buf()))
    }

    /// Whether the list `list`, split at commas, holds `item`.
    fn listed(list: &[u8], item: &[u8]) -> bool {
        list.split(|&byte| byte == b',').any(|entry| entry == item)
    }

    /// The lines of `bytes`, each with no `\n`.
    fn lines(bytes: &[u8]) -> impl Iterator<Item = &[u8]> {
        bytes
            .split_inclusive(|&byte| byte == b'\n')
            .map(|line| line.strip_suffix(b"\n").unwrap_or(line))
    }

    /// A path field of `mountinfo`, where the kernel writes each space, tab, newline,
    /// and backslash as `\` and three octal digits.
    fn unescape(field: &[u8]) -> PathBuf {
        let mut bytes = Vec::with_capacity(field.len());
        let mut rest = field;
        loop {
            let (byte, after) = match rest {
                [
                    b'\\',
                    a @ b'0'..=b'3',
                    b @ b'0'..=b'7',
                    c @ b'0'..=b'7',
                    after @ ..,
                ] => ((a - b'0') * 64 + (b - b'0') * 8 + (c - b'0'), after),
                [byte, after @ ..] => (*byte, after),
                [] => break,
            };
            bytes.push(byte);
            rest = after;
        }
        PathBuf::from(OsString::from_vec(bytes))
    }

    /// The room left in the cgroup `dir`, or `None` when it has no limit.
    fn room(dir: &Path, files: &Files) -> io::Result<Option<u64>> {
        let Some(limit) = optional(read(&dir.join(files.limit)))? else {
            // The root cgroup, or one whose parent gives it no memory controller.
            return Ok(None);
        };
        let limit = limit.trim();
        if limit == "max" {
            return Ok(None);
        }
        let limit = number(&dir.join(files.limit), limit)?;
        let usage = dir.join(files.usage);
        let usage = number(&usage, read(&usage)?.trim())?;
        let stat = dir.join("memory.stat");
        let Some(text) = optional(read(&stat))? else {
            // gVisor writes no `memory.stat`, so the working set is the usage.
            return Ok(Some(limit.saturating_sub(usage)));
        };
        let inactive = text
            .lines()
            .find_map(|line| line.strip_prefix(files.inactive)?.strip_prefix(' '))
            .ok_or_else(|| {
                invalid(format!("{} has no {}", stat.display(), files.inactive))
            })?;
        let inactive = number(&stat, inactive)?;
        // Each file is read after the one before, so the inactive pages can exceed
        // the usage.
        Ok(Some(limit.saturating_sub(usage.saturating_sub(inactive))))
    }

    /// The bytes of `MemAvailable` in `text`, the text of `file`.
    fn mem_available(file: &Path, text: &str) -> io::Result<u64> {
        let line = text
            .lines()
            .find_map(|line| line.strip_prefix("MemAvailable:"))
            .ok_or_else(|| invalid(format!("{} has no MemAvailable", file.display())))?
            .trim();
        let kib = line.strip_suffix(" kB").ok_or_else(|| {
            invalid(format!(
                "{} gives MemAvailable with no unit: {line}",
                file.display()
            ))
        })?;
        Ok(number(file, kib)?.saturating_mul(1024))
    }

    /// An error for a file of the OS that does not hold what it must.
    fn invalid(what: String) -> io::Error {
        io::Error::new(io::ErrorKind::InvalidData, what)
    }

    /// An error for a line of `file` that is no `what`.
    fn malformed(file: &Path, what: &str, line: &[u8]) -> io::Error {
        let line = String::from_utf8_lossy(line);
        invalid(format!(
            "{} has a line that is no {what}: {line}",
            file.display()
        ))
    }

    fn number(file: &Path, text: &str) -> io::Result<u64> {
        text.parse().map_err(|e| {
            invalid(format!("{} holds no number: {text:?}: {e}", file.display()))
        })
    }

    fn read(file: &Path) -> io::Result<String> {
        fs::read_to_string(file).map_err(|e| named(file, &e))
    }

    #[expect(clippy::disallowed_methods, reason = "os reads the files of /proc")]
    fn bytes(file: &Path) -> io::Result<Vec<u8>> {
        fs::read(file).map_err(|e| named(file, &e))
    }

    fn named(file: &Path, error: &io::Error) -> io::Error {
        io::Error::new(error.kind(), format!("{}: {error}", file.display()))
    }

    /// `result`, with `NotFound` as `None`.
    fn optional<T>(result: io::Result<T>) -> io::Result<Option<T>> {
        match result {
            Ok(content) => Ok(Some(content)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }
}

#[cfg(target_os = "macos")]
mod macos {
    use std::io;

    use mach2::host_info::HOST_VM_INFO64_COUNT;
    use mach2::kern_return::KERN_SUCCESS;
    use mach2::vm_statistics::vm_statistics64;

    /// The bytes of the free, inactive, and purgeable pages.
    pub(super) fn available() -> io::Result<u64> {
        let mut stats = vm_statistics64::default();
        let mut count = HOST_VM_INFO64_COUNT;
        // SAFETY: a call with no arguments.
        let host = unsafe { mach2::mach_init::mach_host_self() };
        // SAFETY: `stats` holds `count` integers, the most the call writes.
        let code = unsafe {
            libc::host_statistics64(
                host,
                libc::HOST_VM_INFO64,
                (&raw mut stats).cast(),
                &raw mut count,
            )
        };
        if code != KERN_SUCCESS {
            return Err(io::Error::other(format!(
                "host_statistics64 failed with {code}"
            )));
        }
        Ok(bytes(&stats, page()))
    }

    /// The bytes of the free, inactive, and purgeable pages of `stats`, in pages of
    /// `page` bytes.
    fn bytes(stats: &vm_statistics64, page: u64) -> u64 {
        let pages = u64::from(stats.free_count)
            + u64::from(stats.inactive_count)
            + u64::from(stats.purgeable_count);
        pages.saturating_mul(page)
    }

    /// The bytes of a page of this host.
    fn page() -> u64 {
        u64::try_from(rustix::param::page_size())
            .expect("invariant: a page size fits 64 bits")
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn the_free_inactive_and_purgeable_pages_are_available() {
            let stats = vm_statistics64 {
                free_count: 3,
                inactive_count: 5,
                purgeable_count: 7,
                active_count: 11,
                wire_count: 13,
                ..vm_statistics64::default()
            };
            assert_eq!(bytes(&stats, 16384), 15 * 16384);
        }

        #[test]
        fn this_host_has_whole_pages_available() {
            let available = available().unwrap();
            assert!(available > 0, "{available}");
            assert_eq!(available % page(), 0, "{available}");
        }
    }
}
