use types::byte::Size;

use crate::Error;

/// The memory that the OS can give to new allocations of this process now, with no
/// swap. On Linux, the lesser of `MemAvailable` and the room left in the memory cgroup
/// of the process and in each cgroup above it: `memory.max` less `memory.current` on
/// cgroup v2, and `memory.limit_in_bytes` less `memory.usage_in_bytes` on cgroup v1.
/// On macOS, the free, inactive, and purgeable pages.
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

#[cfg(target_os = "linux")]
mod linux {
    use std::ffi::OsString;
    use std::fs;
    use std::io;
    use std::os::unix::ffi::OsStringExt;
    use std::path::{Path, PathBuf};

    /// The files of a cgroup that give its limit and its use.
    struct Files {
        limit: &'static str,
        usage: &'static str,
    }

    const V1: Files = Files {
        limit: "memory.limit_in_bytes",
        usage: "memory.usage_in_bytes",
    };

    const V2: Files = Files {
        limit: "memory.max",
        usage: "memory.current",
    };

    /// The available bytes of the process, whose file system root is `root`.
    pub(super) fn available(root: &Path) -> io::Result<u64> {
        let meminfo = read(&root.join("proc/meminfo"))?;
        let mut least = mem_available(&meminfo)?;
        let Some(cgroups) = optional(&root.join("proc/self/cgroup"))? else {
            // A kernel with no cgroups.
            return Ok(least);
        };
        for mount in read(&root.join("proc/self/mountinfo"))?.lines() {
            let Some((point, files, inside)) = hierarchy(mount, &cgroups)? else {
                continue;
            };
            let top = root.join(point.strip_prefix("/").unwrap_or(&point));
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

    /// For a line of `mountinfo` that mounts a memory cgroup hierarchy, its mount
    /// point, its files, and the cgroup of the process inside the mount, from the
    /// lines of `cgroups`. `None` for another mount, or a cgroup outside it.
    fn hierarchy(
        line: &str,
        cgroups: &str,
    ) -> io::Result<Option<(PathBuf, Files, PathBuf)>> {
        let bad = || invalid(format!("a line of mountinfo has too few fields: {line}"));
        let (mount, kind) = line.split_once(" - ").ok_or_else(bad)?;
        let mount: Vec<&str> = mount.split(' ').collect();
        let kind: Vec<&str> = kind.split(' ').collect();
        let (Some(&mount_root), Some(&point), Some(&fs_type), Some(&options)) =
            (mount.get(3), mount.get(4), kind.first(), kind.get(2))
        else {
            return Err(bad());
        };
        let (files, controller) = match fs_type {
            "cgroup2" => (V2, ""),
            "cgroup" if options.split(',').any(|o| o == "memory") => (V1, "memory"),
            _ => return Ok(None),
        };
        let path = cgroups.lines().find_map(|cgroup| {
            let mut fields = cgroup.splitn(3, ':');
            let (_, controllers, path) =
                (fields.next()?, fields.next()?, fields.next()?);
            let found = if controller.is_empty() {
                controllers.is_empty()
            } else {
                controllers.split(',').any(|c| c == controller)
            };
            found.then_some(path)
        });
        let mount_root = unescape(mount_root);
        let inside =
            path.and_then(|path| Path::new(path).strip_prefix(&mount_root).ok());
        Ok(inside.map(|inside| (unescape(point), files, inside.to_path_buf())))
    }

    /// A path field of `mountinfo`, where the kernel writes each space, tab, newline,
    /// and backslash as `\` and three octal digits.
    fn unescape(field: &str) -> PathBuf {
        let mut bytes = Vec::with_capacity(field.len());
        let mut rest = field.as_bytes();
        loop {
            let (byte, after) = match rest {
                [
                    b'\\',
                    a @ b'0'..=b'3',
                    b @ b'0'..=b'7',
                    c @ b'0'..=b'7',
                    after @ ..,
                ] => ((a - b'0') << 6 | (b - b'0') << 3 | (c - b'0'), after),
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
        let Some(limit) = optional(&dir.join(files.limit))? else {
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
        Ok(Some(limit.saturating_sub(usage)))
    }

    /// The bytes of `MemAvailable` in the text of `/proc/meminfo`.
    fn mem_available(meminfo: &str) -> io::Result<u64> {
        let line = meminfo
            .lines()
            .find_map(|line| line.strip_prefix("MemAvailable:"))
            .ok_or_else(|| invalid("/proc/meminfo has no MemAvailable".to_owned()))?;
        let kib = line.trim().strip_suffix(" kB").unwrap_or(line.trim());
        let kib = number(Path::new("/proc/meminfo"), kib)?;
        Ok(kib.saturating_mul(1024))
    }

    /// An error for a file of the OS that does not hold what it must.
    fn invalid(what: String) -> io::Error {
        io::Error::new(io::ErrorKind::InvalidData, what)
    }

    fn number(file: &Path, text: &str) -> io::Result<u64> {
        text.parse().map_err(|e| {
            invalid(format!("{} holds no number: {text:?}: {e}", file.display()))
        })
    }

    fn read(file: &Path) -> io::Result<String> {
        fs::read_to_string(file)
            .map_err(|e| io::Error::new(e.kind(), format!("{}: {e}", file.display())))
    }

    /// The text of `file`, or `None` when it does not exist.
    fn optional(file: &Path) -> io::Result<Option<String>> {
        match read(file) {
            Ok(text) => Ok(Some(text)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        /// A file system root in a new temporary directory.
        struct Root(PathBuf);

        impl Root {
            fn new(name: &str) -> Self {
                let dir = std::env::temp_dir().join(format!(
                    "foundation-os-available-{name}-{}",
                    std::process::id()
                ));
                drop(fs::remove_dir_all(&dir));
                fs::create_dir_all(&dir).unwrap();
                Self(dir)
            }

            fn write(&self, path: &str, text: &str) -> &Self {
                let path = self.0.join(path);
                fs::create_dir_all(path.parent().unwrap()).unwrap();
                fs::write(path, text).unwrap();
                self
            }

            fn available(&self) -> io::Result<u64> {
                available(&self.0)
            }
        }

        impl Drop for Root {
            fn drop(&mut self) {
                drop(fs::remove_dir_all(&self.0));
            }
        }

        const MIB: u64 = 1 << 20;
        const MEMINFO: &str = "MemTotal:       16777216 kB\n\
                               MemFree:         1048576 kB\n\
                               MemAvailable:    8388608 kB\n";
        const V2_MOUNT: &str = "37 31 0:31 / /sys/fs/cgroup rw,nosuid shared:8 - \
                                cgroup2 cgroup2 rw,nsdelegate\n";

        fn v2(name: &str) -> Root {
            let root = Root::new(name);
            root.write("proc/meminfo", MEMINFO)
                .write("proc/self/cgroup", "0::/a/b\n")
                .write("proc/self/mountinfo", V2_MOUNT);
            root
        }

        #[test]
        fn the_room_of_a_v2_cgroup_under_mem_available_is_the_available_memory() {
            let root = v2("v2");
            root.write("sys/fs/cgroup/a/memory.max", &format!("{}\n", 512 * MIB))
                .write(
                    "sys/fs/cgroup/a/memory.current",
                    &format!("{}\n", 100 * MIB),
                )
                .write("sys/fs/cgroup/a/b/memory.max", "max\n")
                .write("sys/fs/cgroup/a/b/memory.current", "4096\n");
            assert_eq!(root.available().unwrap(), 412 * MIB);
        }

        #[test]
        fn the_least_room_of_the_cgroup_and_each_above_it_counts() {
            let root = v2("v2-least");
            root.write("sys/fs/cgroup/a/memory.max", &format!("{}\n", 512 * MIB))
                .write(
                    "sys/fs/cgroup/a/memory.current",
                    &format!("{}\n", 500 * MIB),
                )
                .write("sys/fs/cgroup/a/b/memory.max", &format!("{}\n", 256 * MIB))
                .write("sys/fs/cgroup/a/b/memory.current", &format!("{MIB}\n"));
            assert_eq!(root.available().unwrap(), 12 * MIB);
        }

        #[test]
        fn a_cgroup_over_its_limit_has_no_room() {
            let root = v2("v2-over");
            root.write("sys/fs/cgroup/a/b/memory.max", &format!("{MIB}\n"))
                .write(
                    "sys/fs/cgroup/a/b/memory.current",
                    &format!("{}\n", 2 * MIB),
                );
            assert_eq!(root.available().unwrap(), 0);
        }

        #[test]
        fn with_no_limit_the_available_memory_is_mem_available() {
            let root = v2("v2-max");
            root.write("sys/fs/cgroup/a/memory.max", "max\n")
                .write("sys/fs/cgroup/a/memory.current", "4096\n")
                .write("sys/fs/cgroup/a/b/memory.max", "max\n")
                .write("sys/fs/cgroup/a/b/memory.current", "4096\n");
            assert_eq!(root.available().unwrap(), 8192 * MIB);
        }

        #[test]
        fn a_cgroup_room_over_mem_available_leaves_mem_available() {
            let root = v2("v2-more");
            root.write(
                "sys/fs/cgroup/a/memory.max",
                &format!("{}\n", 64 * 1024 * MIB),
            )
            .write("sys/fs/cgroup/a/memory.current", "0\n");
            assert_eq!(root.available().unwrap(), 8192 * MIB);
        }

        #[test]
        fn a_v1_memory_hierarchy_mounted_at_the_cgroup_of_the_process_counts() {
            let root = Root::new("v1");
            root.write("proc/meminfo", MEMINFO)
                .write(
                    "proc/self/cgroup",
                    "5:cpu,cpuacct:/docker/y\n4:memory:/docker/x\n0::/\n",
                )
                .write(
                    "proc/self/mountinfo",
                    "40 31 0:35 /docker/y /sys/fs/cgroup/cpu rw - cgroup cgroup \
                     rw,cpu,cpuacct\n\
                     41 31 0:36 /docker/x /sys/fs/cgroup/memory rw - cgroup cgroup \
                     rw,memory\n",
                )
                .write(
                    "sys/fs/cgroup/memory/memory.limit_in_bytes",
                    &format!("{}\n", 512 * MIB),
                )
                .write(
                    "sys/fs/cgroup/memory/memory.usage_in_bytes",
                    &format!("{}\n", 12 * MIB),
                );
            assert_eq!(root.available().unwrap(), 500 * MIB);
        }

        /// The memory files under the `cpu` mount stand in for a cgroup that the
        /// code must not read.
        #[test]
        fn a_v1_hierarchy_of_another_controller_does_not_count() {
            let root = Root::new("v1-cpu");
            root.write("proc/meminfo", MEMINFO)
                .write("proc/self/cgroup", "5:cpu:/docker/x\n4:memory:/docker/x\n")
                .write(
                    "proc/self/mountinfo",
                    "40 31 0:35 /docker/x /sys/fs/cgroup/cpu rw - cgroup cgroup \
                     rw,cpu\n",
                )
                .write(
                    "sys/fs/cgroup/cpu/memory.limit_in_bytes",
                    &format!("{MIB}\n"),
                )
                .write("sys/fs/cgroup/cpu/memory.usage_in_bytes", "0\n");
            assert_eq!(root.available().unwrap(), 8192 * MIB);
        }

        /// The process is in `/ab/c`, outside the mount whose root is `/a`. The
        /// cgroup `/a/b/c` of that mount is not above the process.
        #[test]
        fn a_cgroup_whose_name_only_starts_with_the_mount_root_is_outside_it() {
            let root = Root::new("v2-prefix");
            root.write("proc/meminfo", MEMINFO)
                .write("proc/self/cgroup", "0::/ab/c\n")
                .write(
                    "proc/self/mountinfo",
                    "37 31 0:31 /a /sys/fs/cgroup rw - cgroup2 cgroup2 rw\n",
                )
                .write("sys/fs/cgroup/b/c/memory.max", &format!("{MIB}\n"))
                .write("sys/fs/cgroup/b/c/memory.current", "0\n");
            assert_eq!(root.available().unwrap(), 8192 * MIB);
        }

        /// The kernel writes a space and a backslash in the root of a mount as
        /// `\040` and `\134`.
        #[test]
        fn a_mount_root_with_a_space_and_a_backslash_counts() {
            let root = Root::new("v2-root-space");
            root.write("proc/meminfo", MEMINFO)
                .write("proc/self/cgroup", "0::/a b\\c/d\n")
                .write(
                    "proc/self/mountinfo",
                    "37 31 0:31 /a\\040b\\134c /cg rw - cgroup2 cgroup2 rw\n",
                )
                .write("cg/d/memory.max", &format!("{MIB}\n"))
                .write("cg/d/memory.current", "0\n");
            assert_eq!(root.available().unwrap(), MIB);
        }

        /// The kernel writes a space in a mount point of mountinfo as `\040`.
        #[test]
        fn a_mount_point_with_a_space_counts() {
            let root = Root::new("v2-space");
            root.write("proc/meminfo", MEMINFO)
                .write("proc/self/cgroup", "0::/a\n")
                .write(
                    "proc/self/mountinfo",
                    "37 31 0:31 / /cg\\040root rw - cgroup2 cgroup2 rw\n",
                )
                .write("cg root/a/memory.max", &format!("{MIB}\n"))
                .write("cg root/a/memory.current", "0\n");
            assert_eq!(root.available().unwrap(), MIB);
        }

        #[test]
        fn a_kernel_with_no_cgroups_gives_mem_available() {
            let root = Root::new("none");
            root.write("proc/meminfo", MEMINFO);
            assert_eq!(root.available().unwrap(), 8192 * MIB);
        }

        #[test]
        fn meminfo_with_no_mem_available_is_an_error() {
            let root = Root::new("no-available");
            root.write("proc/meminfo", "MemTotal: 16777216 kB\n");
            let error = root.available().unwrap_err();
            assert_eq!(
                (error.kind(), error.to_string()),
                (
                    io::ErrorKind::InvalidData,
                    "/proc/meminfo has no MemAvailable".to_owned()
                )
            );
        }

        #[test]
        fn a_limit_that_is_no_number_is_an_error() {
            let root = v2("v2-bad");
            root.write("sys/fs/cgroup/a/b/memory.max", "lots\n")
                .write("sys/fs/cgroup/a/b/memory.current", "0\n");
            let error = root.available().unwrap_err();
            let file = root.0.join("sys/fs/cgroup/a/b/memory.max");
            assert_eq!(
                (error.kind(), error.to_string()),
                (
                    io::ErrorKind::InvalidData,
                    format!(
                        "{} holds no number: \"lots\": invalid digit found in string",
                        file.display()
                    )
                )
            );
        }

        #[test]
        fn a_cgroup_list_that_cannot_be_read_is_an_error() {
            let root = Root::new("cgroup-dir");
            root.write("proc/meminfo", MEMINFO);
            fs::create_dir_all(root.0.join("proc/self/cgroup")).unwrap();
            let error = root.available().unwrap_err();
            let file = root.0.join("proc/self/cgroup");
            assert_eq!(
                (error.kind(), error.to_string()),
                (
                    io::ErrorKind::IsADirectory,
                    format!("{}: Is a directory (os error 21)", file.display())
                )
            );
        }

        #[test]
        fn a_limit_that_cannot_be_read_is_an_error() {
            let root = v2("v2-dir");
            fs::create_dir_all(root.0.join("sys/fs/cgroup/a/b/memory.max")).unwrap();
            let error = root.available().unwrap_err();
            let file = root.0.join("sys/fs/cgroup/a/b/memory.max");
            assert_eq!(
                (error.kind(), error.to_string()),
                (
                    io::ErrorKind::IsADirectory,
                    format!("{}: Is a directory (os error 21)", file.display())
                )
            );
        }

        #[test]
        fn a_short_line_of_mountinfo_is_an_error() {
            let root = v2("v2-short");
            root.write("proc/self/mountinfo", "37 31 0:31 /\n");
            let error = root.available().unwrap_err();
            assert_eq!(
                (error.kind(), error.to_string()),
                (
                    io::ErrorKind::InvalidData,
                    "a line of mountinfo has too few fields: 37 31 0:31 /".to_owned()
                )
            );
        }

        #[test]
        fn this_process_has_memory_available() {
            let meminfo = fs::read_to_string("/proc/meminfo").unwrap();
            let available = crate::memory::available().unwrap().bytes();
            assert!(available > 0, "{available}");
            assert!(
                available <= mem_available(&meminfo).unwrap() * 2,
                "{available}"
            );
        }
    }
}

#[cfg(target_os = "macos")]
mod macos {
    use std::io;
    use std::mem::MaybeUninit;

    /// The bytes of the free, inactive, and purgeable pages.
    pub(super) fn available() -> io::Result<u64> {
        let mut stats = MaybeUninit::<libc::vm_statistics64>::zeroed();
        let mut count = libc::HOST_VM_INFO64_COUNT;
        // SAFETY: a call with no arguments.
        #[expect(deprecated, reason = "libc points to the mach2 crate, which we lack")]
        let host = unsafe { libc::mach_host_self() };
        // SAFETY: `stats` holds `count` integers, the most the call writes, and it
        // starts as zeros, a valid value.
        let code = unsafe {
            libc::host_statistics64(
                host,
                libc::HOST_VM_INFO64,
                stats.as_mut_ptr().cast(),
                &raw mut count,
            )
        };
        if code != libc::KERN_SUCCESS {
            return Err(io::Error::other(format!(
                "host_statistics64 failed with {code}"
            )));
        }
        // SAFETY: zeros are a valid value, and the call wrote the rest.
        let stats = unsafe { stats.assume_init() };
        Ok(bytes(&stats, page()))
    }

    /// The bytes of the free, inactive, and purgeable pages of `stats`, in pages of
    /// `page` bytes.
    fn bytes(stats: &libc::vm_statistics64, page: u64) -> u64 {
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
            // SAFETY: zeros are a valid value.
            let mut stats =
                unsafe { MaybeUninit::<libc::vm_statistics64>::zeroed().assume_init() };
            stats.free_count = 3;
            stats.inactive_count = 5;
            stats.purgeable_count = 7;
            stats.active_count = 11;
            stats.wire_count = 13;
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
