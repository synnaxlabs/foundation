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

#[cfg(target_os = "linux")]
mod linux {
    use std::ffi::OsString;
    use std::fs;
    use std::io;
    use std::os::unix::ffi::{OsStrExt, OsStringExt};
    use std::path::{Component, Path, PathBuf};

    use procfs_core::process::MountInfo;
    use procfs_core::{FromBufRead, ProcError, ProcessCGroups};

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

    /// The available bytes of the process, whose file system root is `root`.
    pub(super) fn available(root: &Path) -> io::Result<u64> {
        let file = root.join("proc/meminfo");
        let mut least = mem_available(&file, &read(&file)?)?;
        let file = root.join("proc/self/cgroup");
        let Some(cgroups) = optional(&file)? else {
            // A kernel with no cgroups.
            return Ok(least);
        };
        let cgroups = cgroups_of(&file, &cgroups)?;
        let file = root.join("proc/self/mountinfo");
        for line in read(&file)?.lines() {
            // Its error names a file of `procfs_core` as a bug, and no field.
            let mount = MountInfo::from_line(line).map_err(|_bug| {
                invalid(format!(
                    "{} has a line that is no mount: {line}",
                    file.display()
                ))
            })?;
            let Some((files, inside)) = hierarchy(&mount, &cgroups) else {
                continue;
            };
            let point = unescape(mount.mount_point.as_os_str().as_bytes());
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

    /// For a mount of a memory cgroup hierarchy, its files and the cgroup of the
    /// process inside the mount. `None` for another mount, or a cgroup outside it.
    fn hierarchy(
        mount: &MountInfo,
        cgroups: &ProcessCGroups,
    ) -> Option<(Files, PathBuf)> {
        let (files, controller) = match mount.fs_type.as_str() {
            "cgroup2" => (V2, None),
            "cgroup" if mount.super_options.contains_key("memory") => {
                (V1, Some("memory"))
            }
            _ => return None,
        };
        let cgroup = cgroups.0.iter().find(|cgroup| match controller {
            None => cgroup.controllers.is_empty(),
            Some(controller) => cgroup.controllers.iter().any(|c| c == controller),
        })?;
        let inside = Path::new(&cgroup.pathname)
            .strip_prefix(unescape(mount.root.as_bytes()))
            .ok()?;
        // A cgroup outside the root of the cgroup namespace starts with `..`.
        let outside = inside.components().any(|c| c == Component::ParentDir);
        (!outside).then(|| (files, inside.to_path_buf()))
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
        let stat = dir.join("memory.stat");
        let Some(text) = optional(&stat)? else {
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

    /// The bytes of `MemAvailable` in `text`, the text of `file`. Not
    /// `procfs_core::Meminfo`, which fails when a field that this does not use is not
    /// there, as under gVisor.
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

    /// The cgroups that `text`, the text of `file`, lists.
    fn cgroups_of(file: &Path, text: &str) -> io::Result<ProcessCGroups> {
        ProcessCGroups::from_buf_read(text.as_bytes()).map_err(|error| {
            let error = match error {
                // Its text calls a line of `procfs_core` a bug.
                ProcError::InternalError(error) => error.msg,
                error => error.to_string(),
            };
            invalid(format!("{}: {error}", file.display()))
        })
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

            fn write(&self, path: impl AsRef<Path>, text: impl AsRef<[u8]>) -> &Self {
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
            root.write("sys/fs/cgroup/a/memory.max", format!("{}\n", 512 * MIB))
                .write("sys/fs/cgroup/a/memory.current", format!("{}\n", 100 * MIB))
                .write("sys/fs/cgroup/a/memory.stat", "inactive_file 0\n")
                .write("sys/fs/cgroup/a/b/memory.max", "max\n")
                .write("sys/fs/cgroup/a/b/memory.current", "4096\n")
                .write("sys/fs/cgroup/a/b/memory.stat", "inactive_file 0\n");
            assert_eq!(root.available().unwrap(), 412 * MIB);
        }

        #[test]
        fn the_least_room_of_the_cgroup_and_each_above_it_counts() {
            let root = v2("v2-least");
            root.write("sys/fs/cgroup/a/memory.max", format!("{}\n", 512 * MIB))
                .write("sys/fs/cgroup/a/memory.current", format!("{}\n", 500 * MIB))
                .write("sys/fs/cgroup/a/memory.stat", "inactive_file 0\n")
                .write("sys/fs/cgroup/a/b/memory.max", format!("{}\n", 256 * MIB))
                .write("sys/fs/cgroup/a/b/memory.current", format!("{MIB}\n"))
                .write("sys/fs/cgroup/a/b/memory.stat", "inactive_file 0\n");
            assert_eq!(root.available().unwrap(), 12 * MIB);
        }

        #[test]
        fn a_cgroup_over_its_limit_has_no_room() {
            let root = v2("v2-over");
            root.write("sys/fs/cgroup/a/b/memory.max", format!("{MIB}\n"))
                .write("sys/fs/cgroup/a/b/memory.current", format!("{}\n", 2 * MIB))
                .write("sys/fs/cgroup/a/b/memory.stat", "inactive_file 0\n");
            assert_eq!(root.available().unwrap(), 0);
        }

        #[test]
        fn with_no_limit_the_available_memory_is_mem_available() {
            let root = v2("v2-max");
            root.write("sys/fs/cgroup/a/memory.max", "max\n")
                .write("sys/fs/cgroup/a/memory.current", "4096\n")
                .write("sys/fs/cgroup/a/memory.stat", "inactive_file 0\n")
                .write("sys/fs/cgroup/a/b/memory.max", "max\n")
                .write("sys/fs/cgroup/a/b/memory.current", "4096\n")
                .write("sys/fs/cgroup/a/b/memory.stat", "inactive_file 0\n");
            assert_eq!(root.available().unwrap(), 8192 * MIB);
        }

        #[test]
        fn a_cgroup_room_over_mem_available_leaves_mem_available() {
            let root = v2("v2-more");
            root.write(
                "sys/fs/cgroup/a/memory.max",
                format!("{}\n", 64 * 1024 * MIB),
            )
            .write("sys/fs/cgroup/a/memory.current", "0\n")
            .write("sys/fs/cgroup/a/memory.stat", "inactive_file 0\n");
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
                    format!("{}\n", 512 * MIB),
                )
                .write(
                    "sys/fs/cgroup/memory/memory.usage_in_bytes",
                    format!("{}\n", 12 * MIB),
                )
                .write(
                    "sys/fs/cgroup/memory/memory.stat",
                    "total_inactive_file 0\n",
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
                    format!("{MIB}\n"),
                )
                .write("sys/fs/cgroup/cpu/memory.usage_in_bytes", "0\n")
                .write("sys/fs/cgroup/cpu/memory.stat", "total_inactive_file 0\n");
            assert_eq!(root.available().unwrap(), 8192 * MIB);
        }

        /// A hybrid host: the v2 mount takes the line of the unified hierarchy, not
        /// the v1 line of `memory`, whose files stand in for a cgroup that it must not
        /// read.
        #[test]
        fn a_v2_mount_takes_the_cgroup_of_the_unified_hierarchy() {
            let root = Root::new("hybrid");
            root.write("proc/meminfo", MEMINFO)
                .write("proc/self/cgroup", "4:memory:/x\n0::/y\n")
                .write(
                    "proc/self/mountinfo",
                    "37 31 0:31 / /cg rw - cgroup2 cgroup2 rw\n",
                )
                .write("cg/x/memory.max", format!("{}\n", MIB / 2))
                .write("cg/x/memory.current", "0\n")
                .write("cg/x/memory.stat", "inactive_file 0\n")
                .write("cg/y/memory.max", format!("{MIB}\n"))
                .write("cg/y/memory.current", "0\n")
                .write("cg/y/memory.stat", "inactive_file 0\n");
            assert_eq!(root.available().unwrap(), MIB);
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
                .write("sys/fs/cgroup/b/c/memory.max", format!("{MIB}\n"))
                .write("sys/fs/cgroup/b/c/memory.current", "0\n")
                .write("sys/fs/cgroup/b/c/memory.stat", "inactive_file 0\n");
            assert_eq!(root.available().unwrap(), 8192 * MIB);
        }

        /// A process outside the root of its cgroup namespace sees its cgroup as
        /// `/../x`: outside each mount, whose root is `/`.
        #[test]
        fn a_cgroup_outside_the_namespace_root_is_outside_the_mount() {
            let root = Root::new("v2-outside-ns");
            root.write("proc/meminfo", MEMINFO)
                .write("proc/self/cgroup", "0::/../x\n")
                .write("proc/self/mountinfo", V2_MOUNT)
                .write("sys/fs/cgroup/memory.max", format!("{MIB}\n"))
                .write("sys/fs/cgroup/memory.current", "0\n")
                .write("sys/fs/cgroup/memory.stat", "inactive_file 0\n")
                .write("sys/fs/x/memory.max", format!("{}\n", MIB / 2))
                .write("sys/fs/x/memory.current", "0\n")
                .write("sys/fs/x/memory.stat", "inactive_file 0\n");
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
                .write("cg/d/memory.max", format!("{MIB}\n"))
                .write("cg/d/memory.current", "0\n")
                .write("cg/d/memory.stat", "inactive_file 0\n");
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
                .write("cg root/a/memory.max", format!("{MIB}\n"))
                .write("cg root/a/memory.current", "0\n")
                .write("cg root/a/memory.stat", "inactive_file 0\n");
            assert_eq!(root.available().unwrap(), MIB);
        }

        /// Each backslash and three digits decodes as an octal byte, or stays as it is.
        /// The kernel writes only `\040`, `\011`, `\012`, and `\134`, and the public
        /// call reads the live `/proc`, so only this call can show the rest.
        #[test]
        fn unescape_decodes_each_octal_byte_and_keeps_each_other_escape() {
            for a in b'0'..=b'9' {
                for b in b'0'..=b'9' {
                    for c in b'0'..=b'9' {
                        let escape = [b'\\', a, b, c];
                        let digits = std::str::from_utf8(&escape[1..]).unwrap();
                        let want = match u8::from_str_radix(digits, 8) {
                            Ok(byte) => vec![b'x', byte, b'y'],
                            Err(_) => [b"x".as_slice(), &escape, b"y"].concat(),
                        };
                        let field = [b"x".as_slice(), &escape, b"y"].concat();
                        assert_eq!(unescape(&field).into_os_string().into_vec(), want);
                    }
                }
            }
        }

        /// Page cache that the kernel can drop is not used memory.
        #[test]
        fn the_inactive_file_pages_of_a_cgroup_are_room() {
            let root = v2("v2-cache");
            root.write("sys/fs/cgroup/a/b/memory.max", format!("{}\n", 256 * MIB))
                .write(
                    "sys/fs/cgroup/a/b/memory.current",
                    format!("{}\n", 250 * MIB),
                )
                .write(
                    "sys/fs/cgroup/a/b/memory.stat",
                    format!("active_file 0\ninactive_file {}\n", 240 * MIB),
                );
            assert_eq!(root.available().unwrap(), 246 * MIB);
        }

        /// `memory.stat` is read after `memory.current`, so it can give more
        /// inactive pages than the usage.
        #[test]
        fn inactive_file_pages_over_the_usage_leave_the_whole_limit() {
            let root = Root::new("v1-cache");
            root.write("proc/meminfo", MEMINFO)
                .write("proc/self/cgroup", "4:memory:/x\n")
                .write(
                    "proc/self/mountinfo",
                    "41 31 0:36 / /cg rw - cgroup cgroup rw,memory\n",
                )
                .write("cg/x/memory.limit_in_bytes", format!("{}\n", 64 * MIB))
                .write("cg/x/memory.usage_in_bytes", format!("{}\n", 10 * MIB))
                .write(
                    "cg/x/memory.stat",
                    format!("inactive_file 1\ntotal_inactive_file {}\n", 11 * MIB),
                );
            assert_eq!(root.available().unwrap(), 64 * MIB);
        }

        #[test]
        fn a_cgroup_whose_stat_has_no_inactive_file_pages_is_an_error() {
            let root = v2("v2-no-stat");
            root.write("sys/fs/cgroup/a/b/memory.max", format!("{MIB}\n"))
                .write("sys/fs/cgroup/a/b/memory.current", "0\n")
                .write("sys/fs/cgroup/a/b/memory.stat", "active_file 0\n");
            let error = root.available().unwrap_err();
            let file = root.0.join("sys/fs/cgroup/a/b/memory.stat");
            assert_eq!(
                (error.kind(), error.to_string()),
                (
                    io::ErrorKind::InvalidData,
                    format!("{} has no inactive_file", file.display())
                )
            );
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
            root.write("proc/meminfo", MEMINFO.replace("MemAvailable", "Other"));
            let error = root.available().unwrap_err();
            let file = root.0.join("proc/meminfo");
            assert_eq!(
                (error.kind(), error.to_string()),
                (
                    io::ErrorKind::InvalidData,
                    format!("{} has no MemAvailable", file.display())
                )
            );
        }

        #[test]
        fn mem_available_with_no_unit_is_an_error() {
            let root = Root::new("no-unit");
            root.write("proc/meminfo", "MemAvailable:    8388608\n");
            let error = root.available().unwrap_err();
            let file = root.0.join("proc/meminfo");
            assert_eq!(
                (error.kind(), error.to_string()),
                (
                    io::ErrorKind::InvalidData,
                    format!(
                        "{} gives MemAvailable with no unit: 8388608",
                        file.display()
                    )
                )
            );
        }

        /// gVisor writes no `Slab`, `Committed_AS`, or `Vmalloc*`.
        #[test]
        fn a_meminfo_with_only_mem_available_gives_it() {
            let root = Root::new("gvisor");
            root.write("proc/meminfo", "MemAvailable:    1048576 kB\nShmem: 0 kB\n");
            assert_eq!(root.available().unwrap(), 1024 * MIB);
        }

        /// gVisor's memory cgroup has `memory.limit_in_bytes` and
        /// `memory.usage_in_bytes`, and no `memory.stat`.
        #[test]
        fn a_v1_cgroup_with_no_stat_takes_the_usage_as_the_working_set() {
            let root = Root::new("gvisor-cgroup");
            root.write("proc/meminfo", MEMINFO)
                .write("proc/self/cgroup", "2:memory:/\n1:cpu:/\n")
                .write(
                    "proc/self/mountinfo",
                    "6 1 0:6 / /sys/fs/cgroup/memory rw,nosuid - cgroup none rw,memory\n",
                )
                .write("sys/fs/cgroup/memory/memory.limit_in_bytes", "1073741824\n")
                .write("sys/fs/cgroup/memory/memory.usage_in_bytes", "104857600\n");
            assert_eq!(root.available().unwrap(), 924 * MIB);
        }

        #[test]
        fn a_mem_available_over_u64_bytes_saturates() {
            let root = Root::new("huge");
            root.write("proc/meminfo", "MemAvailable: 18014398509481984 kB\n");
            assert_eq!(root.available().unwrap(), u64::MAX);
        }

        #[test]
        fn a_limit_that_is_no_number_is_an_error() {
            let root = v2("v2-bad");
            root.write("sys/fs/cgroup/a/b/memory.max", "lots\n")
                .write("sys/fs/cgroup/a/b/memory.current", "0\n")
                .write("sys/fs/cgroup/a/b/memory.stat", "inactive_file 0\n");
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
            let file = root.0.join("proc/self/mountinfo");
            assert_eq!(
                (error.kind(), error.to_string()),
                (
                    io::ErrorKind::InvalidData,
                    format!(
                        "{} has a line that is no mount: 37 31 0:31 /",
                        file.display()
                    )
                )
            );
        }

        #[test]
        fn this_process_has_memory_available() {
            let file = Path::new("/proc/meminfo");
            let mem_available = mem_available(file, &read(file).unwrap()).unwrap();
            let available = crate::memory::available().unwrap().bytes();
            assert!(available > 0, "{available}");
            assert!(available <= mem_available * 2, "{available}");
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
