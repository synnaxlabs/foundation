//! The available memory on Linux, read from the files of `/proc` and of the cgroups
//! under a temporary root.

use std::ffi::OsStr;
use std::fs;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

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

    /// The available bytes under this root.
    fn bytes(&self) -> u64 {
        os::memory::available_under(&self.0).unwrap().bytes()
    }

    /// The kind and message of the error in [`os::Error::Memory`] under this root.
    fn error(&self) -> (io::ErrorKind, String) {
        match os::memory::available_under(&self.0) {
            Err(os::Error::Memory(e)) => (e.kind(), e.to_string()),
            other => panic!("not Error::Memory: {other:?}"),
        }
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
    assert_eq!(root.bytes(), 412 * MIB);
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
    assert_eq!(root.bytes(), 12 * MIB);
}

#[test]
fn a_cgroup_over_its_limit_has_no_room() {
    let root = v2("v2-over");
    root.write("sys/fs/cgroup/a/b/memory.max", format!("{MIB}\n"))
        .write("sys/fs/cgroup/a/b/memory.current", format!("{}\n", 2 * MIB))
        .write("sys/fs/cgroup/a/b/memory.stat", "inactive_file 0\n");
    assert_eq!(root.bytes(), 0);
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
    assert_eq!(root.bytes(), 8192 * MIB);
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
    assert_eq!(root.bytes(), 8192 * MIB);
}

#[test]
fn a_named_hierarchy_whose_name_ends_in_memory_counts_for_nothing() {
    let root = Root::new("v1-named");
    root.write("proc/meminfo", MEMINFO)
        .write("proc/self/cgroup", "4:name=memory:/x\n")
        .write(
            "proc/self/mountinfo",
            "41 31 0:36 / /cg rw - cgroup cgroup rw,name=memory\n",
        )
        .write("cg/x/memory.limit_in_bytes", format!("{MIB}\n"))
        .write("cg/x/memory.usage_in_bytes", "0\n")
        .write("cg/x/memory.stat", "total_inactive_file 0\n");
    assert_eq!(root.bytes(), 8192 * MIB);
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
    assert_eq!(root.bytes(), 500 * MIB);
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
    assert_eq!(root.bytes(), 8192 * MIB);
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
    assert_eq!(root.bytes(), MIB);
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
    assert_eq!(root.bytes(), 8192 * MIB);
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
    assert_eq!(root.bytes(), 8192 * MIB);
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
    assert_eq!(root.bytes(), MIB);
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
    assert_eq!(root.bytes(), MIB);
}

/// The kernel writes each byte of a path raw but a space, tab, newline, and
/// backslash, so a path need not be UTF-8.
#[test]
fn a_mount_point_that_is_not_utf8_of_another_mount_counts_for_nothing() {
    let root = Root::new("v2-raw-other");
    root.write("proc/meminfo", MEMINFO)
        .write("proc/self/cgroup", "0::/a\n")
        .write(
            "proc/self/mountinfo",
            b"38 31 8:17 / /media/caf\xe9 rw - vfat /dev/sdb1 rw\n\
              37 31 0:31 / /cg rw - cgroup2 cgroup2 rw\n",
        )
        .write("cg/a/memory.max", format!("{MIB}\n"))
        .write("cg/a/memory.current", "0\n")
        .write("cg/a/memory.stat", "inactive_file 0\n");
    assert_eq!(root.bytes(), MIB);
}

#[test]
fn a_cgroup_mount_point_that_is_not_utf8_counts() {
    let root = Root::new("v2-raw-mount");
    let dir = OsStr::from_bytes(b"cg\xe9/a");
    root.write("proc/meminfo", MEMINFO)
        .write("proc/self/cgroup", "0::/a\n")
        .write(
            "proc/self/mountinfo",
            b"37 31 0:31 / /cg\xe9 rw - cgroup2 cgroup2 rw\n",
        )
        .write(Path::new(dir).join("memory.max"), format!("{MIB}\n"))
        .write(Path::new(dir).join("memory.current"), "0\n")
        .write(Path::new(dir).join("memory.stat"), "inactive_file 0\n");
    assert_eq!(root.bytes(), MIB);
}

#[test]
fn a_cgroup_path_that_is_not_utf8_counts() {
    let root = Root::new("v2-raw-cgroup");
    let dir = OsStr::from_bytes(b"cg/caf\xe9");
    root.write("proc/meminfo", MEMINFO)
        .write("proc/self/cgroup", b"0::/caf\xe9\n")
        .write(
            "proc/self/mountinfo",
            "37 31 0:31 / /cg rw - cgroup2 cgroup2 rw\n",
        )
        .write(Path::new(dir).join("memory.max"), format!("{MIB}\n"))
        .write(Path::new(dir).join("memory.current"), "0\n")
        .write(Path::new(dir).join("memory.stat"), "inactive_file 0\n");
    assert_eq!(root.bytes(), MIB);
}

/// The path of a cgroup is the rest of its line, and can hold a `:`.
#[test]
fn a_cgroup_path_with_a_colon_counts() {
    let root = Root::new("v2-colon");
    root.write("proc/meminfo", MEMINFO)
        .write("proc/self/cgroup", "0::/a:b\n")
        .write(
            "proc/self/mountinfo",
            "37 31 0:31 / /cg rw - cgroup2 cgroup2 rw\n",
        )
        .write("cg/a:b/memory.max", format!("{MIB}\n"))
        .write("cg/a:b/memory.current", "0\n")
        .write("cg/a:b/memory.stat", "inactive_file 0\n");
    assert_eq!(root.bytes(), MIB);
}

#[test]
fn a_mount_with_two_optional_fields_counts() {
    let root = Root::new("v1-optional");
    root.write("proc/meminfo", MEMINFO)
        .write("proc/self/cgroup", "4:memory:/x\n")
        .write(
            "proc/self/mountinfo",
            "40 31 0:35 / /sys/fs/cgroup/memory rw shared:9 master:2 - cgroup \
             cgroup rw,memory\n",
        )
        .write(
            "sys/fs/cgroup/memory/x/memory.limit_in_bytes",
            format!("{MIB}\n"),
        )
        .write("sys/fs/cgroup/memory/x/memory.usage_in_bytes", "0\n")
        .write(
            "sys/fs/cgroup/memory/x/memory.stat",
            "total_inactive_file 0\n",
        );
    assert_eq!(root.bytes(), MIB);
}

/// The message shows a line that is not UTF-8 lossily.
#[test]
fn a_line_of_the_cgroup_list_with_no_colon_is_an_error() {
    let root = Root::new("cgroup-short");
    root.write("proc/meminfo", MEMINFO)
        .write("proc/self/cgroup", b"0::/a\n0\xe9\n");
    let file = root.0.join("proc/self/cgroup");
    assert_eq!(
        root.error(),
        (
            io::ErrorKind::InvalidData,
            format!("{} has a line that is no cgroup: 0\u{fffd}", file.display())
        )
    );
}

#[test]
fn a_line_of_mountinfo_with_no_separator_is_an_error() {
    let root = v2("v2-no-separator");
    root.write(
        "proc/self/mountinfo",
        "37 31 0:31 / /cg rw cgroup2 cgroup2 rw\n",
    );
    let file = root.0.join("proc/self/mountinfo");
    assert_eq!(
        root.error(),
        (
            io::ErrorKind::InvalidData,
            format!(
                "{} has a line that is no mount: 37 31 0:31 / /cg rw cgroup2 \
                 cgroup2 rw",
                file.display()
            )
        )
    );
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
    assert_eq!(root.bytes(), 246 * MIB);
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
    assert_eq!(root.bytes(), 64 * MIB);
}

#[test]
fn a_cgroup_whose_stat_has_no_inactive_file_pages_is_an_error() {
    let root = v2("v2-no-stat");
    root.write("sys/fs/cgroup/a/b/memory.max", format!("{MIB}\n"))
        .write("sys/fs/cgroup/a/b/memory.current", "0\n")
        .write("sys/fs/cgroup/a/b/memory.stat", "active_file 0\n");
    let file = root.0.join("sys/fs/cgroup/a/b/memory.stat");
    assert_eq!(
        root.error(),
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
    assert_eq!(root.bytes(), 8192 * MIB);
}

#[test]
fn meminfo_with_no_mem_available_is_an_error() {
    let root = Root::new("no-available");
    root.write("proc/meminfo", MEMINFO.replace("MemAvailable", "Other"));
    let file = root.0.join("proc/meminfo");
    assert_eq!(
        root.error(),
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
    let file = root.0.join("proc/meminfo");
    assert_eq!(
        root.error(),
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
    assert_eq!(root.bytes(), 1024 * MIB);
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
            "6 1 0:6 / /sys/fs/cgroup/memory rw,nosuid - cgroup none \
             rw,memory\n",
        )
        .write("sys/fs/cgroup/memory/memory.limit_in_bytes", "1073741824\n")
        .write("sys/fs/cgroup/memory/memory.usage_in_bytes", "104857600\n");
    assert_eq!(root.bytes(), 924 * MIB);
}

#[test]
fn a_mem_available_over_u64_bytes_saturates() {
    let root = Root::new("huge");
    root.write("proc/meminfo", "MemAvailable: 18014398509481984 kB\n");
    assert_eq!(root.bytes(), u64::MAX);
}

#[test]
fn a_limit_that_is_no_number_is_an_error() {
    let root = v2("v2-bad");
    root.write("sys/fs/cgroup/a/b/memory.max", "lots\n")
        .write("sys/fs/cgroup/a/b/memory.current", "0\n")
        .write("sys/fs/cgroup/a/b/memory.stat", "inactive_file 0\n");
    let file = root.0.join("sys/fs/cgroup/a/b/memory.max");
    assert_eq!(
        root.error(),
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
    let file = root.0.join("proc/self/cgroup");
    assert_eq!(
        root.error(),
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
    let file = root.0.join("sys/fs/cgroup/a/b/memory.max");
    assert_eq!(
        root.error(),
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
    let file = root.0.join("proc/self/mountinfo");
    assert_eq!(
        root.error(),
        (
            io::ErrorKind::InvalidData,
            format!(
                "{} has a line that is no mount: 37 31 0:31 /",
                file.display()
            )
        )
    );
}

/// Each backslash and three digits in a mount point decodes as an octal byte, or
/// stays as it is. A NUL and a `/` cannot be a byte of a directory name.
#[test]
fn each_escape_of_a_mount_point_decodes_as_an_octal_byte_or_stays() {
    let root = Root::new("v2-escapes");
    root.write("proc/meminfo", MEMINFO)
        .write("proc/self/cgroup", "0::/\n");
    for a in b'0'..=b'9' {
        for b in b'0'..=b'9' {
            for c in b'0'..=b'9' {
                let escape = [b'\\', a, b, c];
                let digits = std::str::from_utf8(&escape[1..]).unwrap();
                let point = match u8::from_str_radix(digits, 8) {
                    Ok(0 | b'/') => continue,
                    Ok(byte) => vec![b'c', byte, b'g'],
                    Err(_) => [b"c".as_slice(), &escape, b"g"].concat(),
                };
                let line = [
                    b"37 31 0:31 / /c".as_slice(),
                    &escape,
                    b"g rw - cgroup2 cgroup2 rw\n",
                ]
                .concat();
                let dir = Path::new(OsStr::from_bytes(&point));
                root.write("proc/self/mountinfo", line)
                    .write(dir.join("memory.max"), format!("{MIB}\n"))
                    .write(dir.join("memory.current"), "0\n")
                    .write(dir.join("memory.stat"), "inactive_file 0\n");
                assert_eq!(root.bytes(), MIB, "{}", String::from_utf8_lossy(&escape));
                fs::remove_dir_all(root.0.join(dir)).unwrap();
            }
        }
    }
}

#[test]
fn this_process_has_memory_available() {
    let meminfo = fs::read_to_string("/proc/meminfo").unwrap();
    let kib = meminfo
        .lines()
        .find_map(|line| line.strip_prefix("MemAvailable:"))
        .and_then(|line| line.trim().strip_suffix(" kB"))
        .unwrap();
    let mem_available = kib.parse::<u64>().unwrap() * 1024;
    let available = os::memory::available().unwrap().bytes();
    assert!(available > 0, "{available}");
    assert!(available <= mem_available * 2, "{available}");
}
