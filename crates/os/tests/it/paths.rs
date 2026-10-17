//! `os::paths` on the real files of the host.

use std::fs::{self, Permissions};
use std::io::Read;
use std::os::unix::fs::{FileTypeExt, OpenOptionsExt, PermissionsExt, chown, symlink};
use std::path::{Path, PathBuf};
use std::slice;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use env::paths::{Error, Found};
use rustix::fs::{CWD, RenameFlags, renameat_with};
use rustix::io::Errno;

use crate::cases::{self, Host};
use crate::disk::{Scratch, mode};
use crate::files::mkfifo;

fn run(case: fn(&Host<'_>)) {
    let scratch = Scratch::new();
    case(&Host {
        paths: &os::paths(),
        dir: &scratch.0,
        make_dir: &make_dir,
        make_link: &|path, target| symlink(target, path).unwrap(),
    });
}

fn make_dir(path: &Path) {
    fs::create_dir(path).unwrap();
}

/// A directory that takes no new file: a write that makes one in it gives `EACCES`.
fn sealed(scratch: &Scratch) -> PathBuf {
    let dir = scratch.0.join("sealed");
    fs::create_dir(&dir).unwrap();
    fs::set_permissions(&dir, Permissions::from_mode(0o500)).unwrap();
    dir
}

cases::tests!(run);

#[test]
fn a_write_with_no_directory_is_not_found_and_leaves_nothing() {
    let scratch = Scratch::new();
    let paths = os::paths();
    let path = scratch.0.join("none/plant.plan");
    assert_eq!(paths.write(&path, b"plan"), Err(Error::NotFound));
    assert_eq!(paths.read(&scratch.0, 0), Ok(Found::Directory(Vec::new())));
}

#[test]
fn a_path_with_a_nul_byte_is_an_invalid_argument() {
    let paths = os::paths();
    let invalid = Err(Error::Io { code: 22 });
    assert_eq!(paths.read(Path::new("a\0b"), 1), invalid);
    assert_eq!(
        paths.write(Path::new("a\0b"), b"x"),
        invalid.map(|_: Found| ())
    );
}

#[test]
fn a_new_file_gives_other_users_no_access() {
    let scratch = Scratch::new();
    let path = scratch.0.join("plant.plan");
    os::paths().write(&path, b"plan").unwrap();
    assert_eq!(mode(&path), 0o600);
}

#[test]
fn a_new_directory_gives_other_users_no_access() {
    let scratch = Scratch::new();
    let path = scratch.0.join("data");
    os::paths().make_dir(&path).unwrap();
    assert_eq!(mode(&path), 0o700);
}

#[test]
fn within_hides_a_folder_that_this_user_cannot_list() {
    let scratch = Scratch::new();
    let base = fs::canonicalize(&scratch.0).unwrap();
    let data = base.join("data");
    fs::create_dir(&data).unwrap();
    fs::set_permissions(&data, Permissions::from_mode(0o300)).unwrap();
    let within = os::paths().within(slice::from_ref(&base), slice::from_ref(&data));
    let error = within.as_ref().err().copied();
    fs::set_permissions(&data, Permissions::from_mode(0o700)).unwrap();
    assert_eq!(error, None);
    let key = data.join("admin.key");
    assert_eq!(within.unwrap().write(&key, b"x"), Err(Error::Hidden));
}

#[test]
fn within_hides_a_folder_that_this_user_cannot_search() {
    for bits in [0o600, 0o000] {
        let scratch = Scratch::new();
        let base = fs::canonicalize(&scratch.0).unwrap();
        let data = base.join("data");
        fs::create_dir(&data).unwrap();
        fs::set_permissions(&data, Permissions::from_mode(bits)).unwrap();
        let within = os::paths().within(slice::from_ref(&base), slice::from_ref(&data));
        let read = within.map(|within| within.read(&data, 1));
        fs::set_permissions(&data, Permissions::from_mode(0o700)).unwrap();
        assert_eq!(read, Ok(Err(Error::Hidden)), "{bits:o}");
    }
}

#[test]
fn within_hides_a_pipe_and_ends() {
    let scratch = Scratch::new();
    let base = fs::canonicalize(&scratch.0).unwrap();
    let pipe = base.join("pipe");
    mkfifo(&pipe);
    let (sent, got) = std::sync::mpsc::channel();
    let hidden = pipe.clone();
    #[expect(clippy::disallowed_methods, reason = "the test bounds the call")]
    let call = thread::spawn(move || {
        let within = os::paths().within(&[base], slice::from_ref(&hidden));
        sent.send(within.map(|within| within.read(&hidden, 1)))
            .unwrap();
    });
    #[expect(clippy::disallowed_methods, reason = "the test bounds the call")]
    let ended = got.recv_timeout(std::time::Duration::from_secs(5));
    if ended.is_err() {
        #[expect(clippy::disallowed_methods, reason = "the test frees the call")]
        let _writer = fs::OpenOptions::new().write(true).open(&pipe).unwrap();
    }
    call.join().unwrap();
    assert_eq!(ended, Ok(Ok(Err(Error::Hidden))));
}

/// APFS ignores the case and the form of a name.
#[cfg(target_os = "macos")]
#[test]
fn a_hidden_folder_holds_each_case_and_form_of_its_names() {
    run(|host| {
        let (_, base) = cases::within_work(host);
        let upper = base.join("work/DATA");
        let nfd = base.join("work/cafe\u{301}");
        (host.make_dir)(&base.join("work/caf\u{e9}"));
        let within = host
            .paths
            .within(&[base.join("work")], &[base.join("work/caf\u{e9}")])
            .unwrap();
        assert_eq!(within.make_dir(&nfd.join("new")), Err(Error::Hidden), "nfd");
        assert_eq!(
            within.write(&nfd.join("x"), b"y"),
            Err(Error::Hidden),
            "nfd"
        );
        let within = host
            .paths
            .within(&[base.join("work")], &[base.join("work/data")])
            .unwrap();
        assert_eq!(within.make_dir(&upper.join("new")), Err(Error::Hidden));
        symlink(base.join("work/data/loop"), base.join("work/data/loop")).unwrap();
        assert_eq!(within.make_dir(&upper.join("loop")), Err(Error::Hidden));
        assert_eq!(within.write(&upper.join("x"), b"y"), Err(Error::Hidden));
        assert_eq!(within.read(&upper.join("admin.key"), 1), Err(Error::Hidden));
    });
}

/// The resolve keeps the prefix of the data volume, so the text of the path is under
/// no folder.
#[cfg(target_os = "macos")]
#[test]
fn a_folder_holds_its_files_through_the_data_volume() {
    run(|host| {
        let (_, base) = cases::within_work(host);
        let volume =
            Path::new("/System/Volumes/Data").join(base.strip_prefix("/").unwrap());
        let within = host
            .paths
            .within(&[base.join("work")], &[base.join("work/data")])
            .unwrap();
        let key = volume.join("work/data/admin.key");
        assert_eq!(within.read(&key, 1), Err(Error::Hidden));
        assert_eq!(within.write(&key, b"y"), Err(Error::Hidden));
        let plan = volume.join("work/a.hcl");
        assert_eq!(within.read(&plan, 1), Ok(Found::File(b"x".to_vec())));
        let file = base.join("work/a.hcl");
        let within = host
            .paths
            .within(&[base.join("work")], slice::from_ref(&file))
            .unwrap();
        host.paths.write(&file, b"new").unwrap();
        assert_eq!(within.read(&plan, 3), Err(Error::Hidden));
    });
}

/// `/proc` and `/sys` are each inode 1, on two devices.
#[cfg(target_os = "linux")]
#[test]
fn a_folder_is_its_device_and_its_inode() {
    use std::os::unix::fs::MetadataExt;
    let ino = |path| fs::metadata(path).unwrap().ino();
    assert_eq!((ino("/proc"), ino("/sys")), (1, 1));
    let within = os::paths().within(&["/proc".into()], &[]).unwrap();
    let read = within.read(Path::new("/sys/kernel"), 1 << 16);
    assert_eq!(read, Err(Error::Outside));
}

#[test]
fn a_hard_link_to_a_hidden_file_is_hidden() {
    run(|host| {
        let (_, base) = cases::within_work(host);
        let key = base.join("work/data/admin.key");
        let twin = base.join("work/twin");
        fs::hard_link(&key, &twin).unwrap();
        let within = host
            .paths
            .within(&[base.join("work")], slice::from_ref(&key))
            .unwrap();
        assert_eq!(within.read(&twin, 1), Err(Error::Hidden));
        assert_eq!(within.write(&twin, b"y"), Err(Error::Hidden));
    });
}

/// Another program moves the hidden folder away and makes a new one at its name.
#[test]
fn a_hidden_folder_that_a_program_moves_and_makes_again_stays_hidden() {
    run(|host| {
        let (within, base) = cases::within_work(host);
        let data = base.join("work/data");
        fs::rename(&data, base.join("work/old")).unwrap();
        fs::create_dir(&data).unwrap();
        fs::write(data.join("admin.key"), b"k").unwrap();
        assert_eq!(within.read(&data.join("admin.key"), 1), Err(Error::Hidden));
        let old = base.join("work/old/admin.key");
        assert_eq!(within.read(&old, 1), Err(Error::Hidden));
        let listed = within.read(&base.join("work/old"), usize::MAX);
        assert!(matches!(listed, Ok(Found::Directory(_))), "{listed:?}");
    });
}

/// A relative link in `work` to a deep directory, so the given path is short and the
/// resolved path of a new name in it is one byte past the longest path of the OS.
#[test]
fn within_writes_a_new_name_whose_resolved_path_is_too_long_as_free() {
    run(|host| {
        let (within, base) = cases::within_work(host);
        let most: usize = if cfg!(target_os = "macos") {
            1023
        } else {
            4095
        };
        let mut found = Vec::new();
        for top in ["work", "work/data"] {
            let top = base.join(top);
            let (mut deep, mut relative) = (top.clone(), PathBuf::new());
            while deep.as_os_str().len() + 201 < most - 50 {
                deep.push("d".repeat(200));
                relative.push("d".repeat(200));
                fs::create_dir(&deep).unwrap();
            }
            symlink(&relative, top.join("s")).unwrap();
            let name = "f".repeat(most - deep.as_os_str().len());
            assert_eq!(deep.join(&name).as_os_str().len(), most + 1);
            let free = host.paths.write(&top.join("s").join(&name), b"x");
            let named = top.join("s").join(name.replace('f', "g"));
            let dir = top.join("s").join(name.replace('f', "h"));
            let made = within.make_dir(&dir);
            found.push((
                free,
                within.write(&named, b"x"),
                made,
                fs::symlink_metadata(&dir).is_ok(),
            ));
        }
        assert_eq!(
            found,
            [
                (Ok(()), Ok(()), Ok(()), true),
                (Ok(()), Err(Error::Hidden), Err(Error::Hidden), false),
            ]
        );
    });
}

/// A work folder that a program moves away and makes again holds nothing at either
/// name until a new `within`.
#[test]
fn a_folder_that_a_program_moves_and_makes_again_holds_nothing() {
    run(|host| {
        let (within, base) = cases::within_work(host);
        fs::rename(base.join("work"), base.join("moved")).unwrap();
        fs::create_dir(base.join("work")).unwrap();
        let moved = within.read(&base.join("moved/a.hcl"), 1);
        host.paths.write(&base.join("work/a.hcl"), b"y").unwrap();
        let made = within.read(&base.join("work/a.hcl"), 1);
        assert_eq!((moved, made), (Err(Error::Outside), Err(Error::Outside)));
    });
}

/// A hidden file that is not there, at a short path through a relative link, whose
/// resolved path is one byte past the longest path of the OS.
#[test]
fn within_hides_a_hidden_name_whose_resolved_path_is_too_long() {
    run(|host| {
        let (_, base) = cases::within_work(host);
        let most: usize = if cfg!(target_os = "macos") {
            1023
        } else {
            4095
        };
        let top = base.join("work");
        let (mut deep, mut relative) = (top.clone(), PathBuf::new());
        while deep.as_os_str().len() + 201 < most - 50 {
            deep.push("d".repeat(200));
            relative.push("d".repeat(200));
            fs::create_dir(&deep).unwrap();
        }
        symlink(&relative, top.join("s")).unwrap();
        let name = "f".repeat(most - deep.as_os_str().len());
        let key = top.join("s").join(&name);
        let within = host
            .paths
            .within(slice::from_ref(&top), slice::from_ref(&key))
            .unwrap();
        assert_eq!(within.write(&key, b"x"), Err(Error::Hidden));
        assert!(!deep.join(&name).exists());
        assert_eq!(within.write(&top.join("b.hcl"), b"x"), Ok(()));
    });
}

/// A hidden file whose resolved path is longer than the OS takes, made after
/// `within`, does not read at its path, and is hidden through a hard link.
#[test]
fn within_hides_a_hard_link_to_a_long_hidden_file_made_after_within() {
    run(|host| {
        let (_, base) = cases::within_work(host);
        let most: usize = if cfg!(target_os = "macos") {
            1023
        } else {
            4095
        };
        let top = base.join("work");
        let (mut deep, mut relative) = (top.clone(), PathBuf::new());
        while deep.as_os_str().len() + 201 < most - 50 {
            deep.push("d".repeat(200));
            relative.push("d".repeat(200));
            fs::create_dir(&deep).unwrap();
        }
        symlink(&relative, top.join("s")).unwrap();
        let name = "f".repeat(most - deep.as_os_str().len());
        let key = top.join("s").join(&name);
        let within = host
            .paths
            .within(slice::from_ref(&top), slice::from_ref(&key))
            .unwrap();
        fs::write(&key, b"secret").unwrap();
        let link = top.join("link.key");
        fs::hard_link(&key, &link).unwrap();
        let too_long = if cfg!(target_os = "macos") { 63 } else { 36 };
        assert_eq!(
            (within.read(&key, 64), within.read(&link, 64)),
            (Err(Error::Io { code: too_long }), Err(Error::Hidden))
        );
    });
}

/// A hidden folder that a link loop replaces after `within` holds nothing, so a new
/// name beside it passes.
#[test]
fn within_passes_a_new_name_beside_a_hidden_folder_that_a_loop_replaces() {
    run(|host| {
        let (_, base) = cases::within_work(host);
        let vault = base.join("work/vault");
        fs::create_dir(&vault).unwrap();
        let within = host
            .paths
            .within(&[base.join("work")], slice::from_ref(&vault))
            .unwrap();
        fs::remove_dir(&vault).unwrap();
        symlink(&vault, &vault).unwrap();
        assert_eq!(within.write(&base.join("work/b.hcl"), b"x"), Ok(()));
    });
}

/// A link in `work` that leads, through a chain of short links, to a directory whose
/// resolved path is longer than the OS takes, in the hidden folder or outside.
#[test]
fn within_gives_no_directory_through_a_link_that_is_too_long_to_resolve() {
    run(|host| {
        use rustix::fs::{Mode, OFlags, mkdirat, openat, symlinkat};
        let (within, base) = cases::within_work(host);
        let name = "d".repeat(200);
        let next = format!("{name}/n");
        let flags = OFlags::RDONLY | OFlags::DIRECTORY;
        let too_long = Err(Error::Io {
            code: Errno::NAMETOOLONG.raw_os_error(),
        });
        let mut found = Vec::new();
        for (top, target) in [("work/data", "data/n"), ("workshop", "../workshop/n")] {
            let mut at = openat(CWD, base.join(top), flags, Mode::empty()).unwrap();
            // 22 names of 201 bytes pass the 4 096 bytes of a path on Linux.
            for _ in 0..22 {
                symlinkat(next.as_str(), &at, "n").unwrap();
                mkdirat(&at, name.as_str(), Mode::from_raw_mode(0o700)).unwrap();
                at = openat(&at, name.as_str(), flags, Mode::empty()).unwrap();
            }
            symlinkat(".", &at, "n").unwrap();
            let link = base.join("work").join(top.replace('/', "-"));
            symlink(target, &link).unwrap();
            let free = host.paths.make_dir(&link);
            let read = within.read(&link, 1 << 12).map(|_| ());
            found.push((top, free, read, within.make_dir(&link)));
        }
        assert_eq!(
            found,
            [
                ("work/data", Ok(()), too_long, too_long),
                ("workshop", Ok(()), too_long, too_long),
            ]
        );
    });
}

#[test]
fn within_makes_a_directory_of_a_name_longer_than_the_host_takes_as_free() {
    run(|host| {
        let (within, base) = cases::within_work(host);
        let long = base.join("work").join("a".repeat(256));
        assert_eq!(within.make_dir(&long), host.paths.make_dir(&long));
    });
}

/// A hidden folder outside the folders that a program moves away tells no name that it
/// holds.
#[test]
fn within_tells_no_name_of_a_moved_hidden_folder_outside_the_folders() {
    let scratch = Scratch::new();
    let base = scratch.0.canonicalize().unwrap();
    for name in ["work", "out", "out/h", "out/h/s"] {
        fs::create_dir(base.join(name)).unwrap();
    }
    let within = os::paths()
        .within(&[base.join("work")], &[base.join("out/h")])
        .unwrap();
    fs::rename(base.join("out/h"), base.join("out/z")).unwrap();
    for name in ["out/z/s/x", "out/z/t/x"] {
        let path = base.join(name);
        assert_eq!(within.read(&path, 1), Err(Error::Outside), "{name}");
    }
}

/// A link that does not resolve, in a hidden folder that a program moves away, is under
/// that folder by its place for each call, as a new name there is.
#[test]
fn within_hides_a_link_that_does_not_resolve_in_a_moved_hidden_folder() {
    run(|host| {
        let (within, base) = cases::within_work(host);
        fs::rename(base.join("work/data"), base.join("work/moved")).unwrap();
        fs::create_dir(base.join("work/moved/sub")).unwrap();
        for name in ["work/moved/loop", "work/moved/sub/loop"] {
            symlink(base.join(name), base.join(name)).unwrap();
        }
        symlink(
            base.join("work/moved/none"),
            base.join("work/moved/to-none"),
        )
        .unwrap();
        for name in [
            "work/moved/new",
            "work/moved/none/x",
            "work/moved/loop",
            "work/moved/loop/x",
            "work/moved/to-none",
            "work/moved/to-none/x",
            "work/moved/sub/loop",
        ] {
            let path = base.join(name);
            let reached = [
                within.read(&path, 1).map(|_| ()),
                within.write(&path, b"x"),
                within.make_dir(&path),
            ];
            assert_eq!(reached, [Err(Error::Hidden); 3], "{name}");
        }
    });
}

/// A long path of names that each resolve stops at the longest path that the host
/// takes.
#[cfg(target_os = "linux")]
#[test]
fn within_stops_a_long_path_of_steps_that_resolve() {
    run(|host| {
        let (within, base) = cases::within_work(host);
        fs::create_dir(base.join("work/s")).unwrap();
        let path = base.join("work").join("s/../".repeat(50_000)).join("a.hcl");
        let name_too_long = Error::Io { code: 36 };
        assert_eq!(within.read(&path, 1), Err(name_too_long));
    });
}

/// A relative path that the host takes, but not once it is joined to the working
/// directory, still passes each step.
#[cfg(target_os = "linux")]
#[test]
fn within_checks_a_relative_path_longer_once_joined_by_its_own_prefixes() {
    run(|host| {
        let (_, base) = cases::within_work(host);
        fs::create_dir(base.join("work/s")).unwrap();
        let hidden = base.join("work/data");
        let within = host.paths.within(&["/".into()], slice::from_ref(&hidden));
        let within = within.unwrap();
        let working = host.paths.resolve(Path::new(".")).unwrap();
        let up = "../".repeat(working.components().count() - 1);
        let long = |name: &str| {
            let mut path = Path::new(&up).join(base.strip_prefix("/").unwrap());
            path.push("work");
            let pads = (4095 - path.join(name).as_os_str().len() - 1) / 5;
            path.push("s/../".repeat(pads));
            // The host takes the path, but not the join of its directory.
            let joined = working.join(&path).as_os_str().len();
            path.push(name);
            assert!(path.as_os_str().len() < 4096 && joined > 4096, "{joined}");
            path
        };
        let got = [
            within.read(&long("data/admin.key"), 1).map(|_| ()),
            within.write(&long("data/admin.key"), b"y"),
            within.make_dir(&long("data/made")),
        ];
        assert_eq!(got, [Err(Error::Hidden); 3]);
        let read = host.paths.read(&hidden.join("admin.key"), 1);
        assert_eq!(read, Ok(Found::File(b"x".to_vec())));
        assert!(!hidden.join("made").exists());
    });
}

#[test]
fn a_write_of_a_name_of_the_longest_length_succeeds() {
    let scratch = Scratch::new();
    let path = scratch.0.join("a".repeat(255));
    let paths = os::paths();
    paths.write(&path, b"plan").unwrap();
    assert_eq!(paths.read(&path, 4), Ok(Found::File(b"plan".to_vec())));
}

/// Writes over a file of each old mode in `dir`, a directory of this user.
fn write_over_each_mode(dir: &Path) {
    // On macOS a file takes the group of its directory, and the OS drops the setgid
    // bit of a file whose group this user is not in.
    let group = rustix::process::getegid().as_raw();
    chown(dir, None, Some(group)).unwrap();
    let path = dir.join("plant.hcl");
    for old in [0o600, 0o664, 0o4755, 0o2755, 0o1755] {
        fs::write(&path, b"old").unwrap();
        fs::set_permissions(&path, Permissions::from_mode(old)).unwrap();
        assert_eq!(mode(&path), old);
        os::paths().write(&path, b"new").unwrap();
        assert_eq!(mode(&path), 0o600, "{old:o}");
    }
}

#[test]
fn a_write_gives_the_group_and_other_users_no_access_whatever_the_old_mode() {
    write_over_each_mode(&Scratch::new().0);
}

/// A directory in `/private/tmp` takes its group, `wheel`, which a user is not in.
#[cfg(target_os = "macos")]
#[test]
fn a_write_gives_no_access_in_a_directory_that_took_the_group_of_another() {
    let scratch = Scratch::under(Path::new("/private/tmp"));
    write_over_each_mode(&scratch.0);
}

/// `os` sets no access control list.
#[cfg(target_os = "macos")]
#[test]
fn a_write_takes_the_access_control_entries_that_its_directory_gives_new_files() {
    use std::process::Command;
    let scratch = Scratch::new();
    let path = scratch.0.join("plant.hcl");
    fs::write(&path, b"old").unwrap();
    fs::set_permissions(&path, Permissions::from_mode(0o600)).unwrap();
    let entries = || {
        let listed = Command::new("/bin/ls")
            .arg("-le")
            .arg(&path)
            .output()
            .unwrap();
        assert!(listed.status.success(), "{listed:?}");
        let listed = String::from_utf8(listed.stdout).unwrap();
        listed
            .lines()
            .skip(1)
            .map(str::to_owned)
            .collect::<Vec<_>>()
    };
    assert_eq!(entries(), [""; 0]);
    let mut chmod = Command::new("/bin/chmod");
    chmod.args(["+a", "everyone allow read,file_inherit"]);
    assert!(chmod.arg(&scratch.0).status().unwrap().success());
    os::paths().write(&path, b"new").unwrap();
    assert_eq!(entries(), [" 0: group:everyone inherited allow read"]);
    assert_eq!(mode(&path), 0o600);
}

/// The `posix_acl_xattr` form of `entries`: a version, then a tag, the bits, and a user
/// for each.
#[cfg(target_os = "linux")]
fn access_list(entries: &[(u16, u16, u32)]) -> Vec<u8> {
    let mut bytes = 2u32.to_le_bytes().to_vec();
    for (tag, bits, user) in entries {
        bytes.extend(tag.to_le_bytes());
        bytes.extend(bits.to_le_bytes());
        bytes.extend(user.to_le_bytes());
    }
    bytes
}

/// `os` sets no access control list.
#[cfg(target_os = "linux")]
#[test]
fn a_write_takes_the_access_control_entries_that_its_directory_gives_new_files() {
    use rustix::fs::{XattrFlags, getxattr, setxattr};
    let (owner, named, group, mask, other) = (0x01, 0x02, 0x04, 0x10, 0x20);
    let (none, read, read_write, all, unnamed) = (0, 4, 6, 7, u32::MAX);
    let scratch = Scratch::new();
    let path = scratch.0.join("plant.hcl");
    fs::write(&path, b"old").unwrap();
    let access = "system.posix_acl_access";
    let old = access_list(&[
        (owner, read_write, unnamed),
        (named, read, 4343),
        (group, none, unnamed),
        (mask, read, unnamed),
        (other, none, unnamed),
    ]);
    setxattr(&path, access, &old, XattrFlags::empty()).unwrap();
    // A list of only the mode bits is stored as no list.
    let bare = scratch.0.join("bare.hcl");
    fs::write(&bare, b"old").unwrap();
    let mode_only = access_list(&[
        (owner, read_write, unnamed),
        (group, none, unnamed),
        (other, none, unnamed),
    ]);
    setxattr(&bare, access, &mode_only, XattrFlags::empty()).unwrap();
    let mut taken = [0; 64];
    assert_eq!(getxattr(&bare, access, &mut taken[..]), Err(Errno::NODATA));
    let given = access_list(&[
        (owner, all, unnamed),
        (named, read_write, 4242),
        (group, read, unnamed),
        (mask, all, unnamed),
        (other, none, unnamed),
    ]);
    let default = "system.posix_acl_default";
    setxattr(&scratch.0, default, &given, XattrFlags::empty()).unwrap();
    let expected = access_list(&[
        (owner, read_write, unnamed),
        (named, read_write, 4242),
        (group, read, unnamed),
        (mask, none, unnamed),
        (other, none, unnamed),
    ]);
    for path in [path, bare, scratch.0.join("site.hcl")] {
        os::paths().write(&path, b"new").unwrap();
        let len = getxattr(&path, access, &mut taken[..]).unwrap();
        assert_eq!(taken[..len], expected);
        assert_eq!(mode(&path), 0o600);
    }
    let tool = scratch.0.join("tool.hcl");
    let mut open = fs::OpenOptions::new();
    open.write(true).create_new(true).mode(0o666);
    #[expect(clippy::disallowed_methods, reason = "the test makes a file as a tool")]
    open.open(&tool).unwrap();
    let len = getxattr(&tool, access, &mut taken[..]).unwrap();
    let expected = access_list(&[
        (owner, read_write, unnamed),
        (named, read_write, 4242),
        (group, read, unnamed),
        (mask, read_write, unnamed),
        (other, none, unnamed),
    ]);
    assert_eq!(taken[..len], expected);
    assert_eq!(mode(&tool), 0o660);
}

#[test]
fn a_write_on_a_path_that_ends_in_a_slash_keeps_the_files_inside() {
    let scratch = Scratch::new();
    let config = scratch.0.join("config");
    fs::create_dir(&config).unwrap();
    fs::write(config.join(".new"), b"user").unwrap();
    let slashed = PathBuf::from(format!("{}/", config.display()));
    let paths = os::paths();
    assert_eq!(paths.write(&slashed, b"y"), Err(Error::Io { code: 21 }));
    let user = Ok(Found::File(b"user".to_vec()));
    assert_eq!(paths.read(&config.join(".new"), 4), user);
    let entries = Ok(Found::Directory(vec![config]));
    assert_eq!(paths.read(&scratch.0, 1 << 12), entries);
}

#[test]
fn a_write_on_a_path_with_no_name_writes_nothing() {
    let scratch = Scratch::new();
    let config = scratch.0.join("config");
    fs::create_dir_all(config.join("c")).unwrap();
    let paths = os::paths();
    let on_dir = Err(Error::Io { code: 21 });
    for path in ["/", ".", ".."].map(PathBuf::from) {
        assert_eq!(paths.write(&path, b"x"), on_dir, "{}", path.display());
    }
    for path in [config.join(".."), config.join("./"), config.join("c/..")] {
        assert_eq!(paths.write(&path, b"x"), on_dir, "{}", path.display());
    }
    let entries = Ok(Found::Directory(vec![config.join("c")]));
    assert_eq!(paths.read(&config, 1 << 12), entries);
}

#[test]
fn a_write_on_a_path_that_names_a_directory_makes_no_file_beside_it() {
    let scratch = Scratch::new();
    let dir = sealed(&scratch);
    for none in [dir.join("none/"), dir.join("none/.")] {
        let written = os::paths().write(&none, b"x");
        assert_eq!(written, Err(Error::NotFound), "{}", none.display());
    }
}

/// Another thread makes and removes `config`, so a stat that finds nothing does not
/// show that the open of a hidden file under `config/..` fails.
#[test]
fn a_write_on_a_path_that_ends_in_two_dots_makes_no_file_beside_the_directory() {
    let scratch = Scratch::new();
    let config = scratch.0.join("config");
    let dots = config.join("..");
    let stopped = AtomicBool::new(false);
    let named = [
        Err(Error::NotFound),
        Err(Error::Io {
            code: Errno::ISDIR.raw_os_error(),
        }),
    ];
    let other = thread::scope(|scope| {
        #[expect(clippy::disallowed_methods, reason = "the test races the write")]
        scope.spawn(|| {
            while !stopped.load(Ordering::Relaxed) {
                fs::create_dir(&config).unwrap();
                fs::remove_dir(&config).unwrap();
            }
        });
        let paths = os::paths();
        let mut written = (0..20_000).map(|_| paths.write(&dots, b"x"));
        let other = written.find(|written| !named.contains(written));
        stopped.store(true, Ordering::Relaxed);
        other
    });
    let left = fs::read_dir(&scratch.0).unwrap().count();
    assert_eq!((other, left), (None, 0));
}

#[test]
fn a_write_of_a_name_that_is_too_long_makes_no_file_beside_it() {
    let scratch = Scratch::new();
    let long = sealed(&scratch).join("a".repeat(256));
    let code = Errno::NAMETOOLONG.raw_os_error();
    assert_eq!(os::paths().write(&long, b"x"), Err(Error::Io { code }));
}

/// The name of the hidden file has 21 bytes, so its path passes the longest path of
/// the OS where a shorter name is at that path.
#[test]
fn a_write_whose_hidden_file_has_a_path_that_is_too_long_makes_no_file() {
    let most = if cfg!(target_os = "macos") {
        1023
    } else {
        4095
    };
    let scratch = Scratch::new();
    // The OS counts the path behind each symbolic link.
    let mut dir = fs::canonicalize(&scratch.0).unwrap();
    while dir.as_os_str().len() + 224 < most {
        dir.push("d".repeat(200));
        fs::create_dir(&dir).unwrap();
    }
    let room = most - dir.as_os_str().len() - 1;
    let [short, same] = [20, 21].map(|name| {
        let dir = dir.join("e".repeat(room - name - 1));
        fs::create_dir(&dir).unwrap();
        let file = dir.join("f".repeat(name));
        assert_eq!(file.as_os_str().len(), most);
        file
    });
    let paths = os::paths();
    let code = Errno::NAMETOOLONG.raw_os_error();
    assert_eq!(paths.write(&short, b"new"), Err(Error::Io { code }));
    assert_eq!(paths.read(&short, 3), Err(Error::NotFound));
    fs::write(&short, b"old").unwrap();
    assert_eq!(paths.write(&short, b"new"), Err(Error::Io { code }));
    assert_eq!(paths.read(&short, 3), Ok(Found::File(b"old".to_vec())));
    let dir = short.parent().unwrap();
    assert_eq!(paths.read(dir, most), Ok(Found::Directory(vec![short])));
    assert_eq!(paths.write(&same, b"new"), Ok(()));
    assert_eq!(paths.read(&same, 3), Ok(Found::File(b"new".to_vec())));
}

#[test]
fn a_write_leaves_the_old_bytes_at_another_hard_link_of_the_file() {
    let scratch = Scratch::new();
    let (path, twin) = (scratch.0.join("plant.hcl"), scratch.0.join("twin.hcl"));
    fs::write(&path, b"old").unwrap();
    fs::hard_link(&path, &twin).unwrap();
    let paths = os::paths();
    paths.write(&path, b"new").unwrap();
    let read = [&path, &twin].map(|path| paths.read(path, 3));
    let [new, old] = [b"new", b"old"].map(|bytes| Ok(Found::File(bytes.to_vec())));
    assert_eq!(read, [new, old]);
}

#[test]
fn a_write_leaves_the_old_file_whole_for_a_reader_that_has_it_open() {
    let scratch = Scratch::new();
    let path = scratch.0.join("plant.hcl");
    fs::write(&path, b"old").unwrap();
    #[expect(clippy::disallowed_methods, reason = "the test holds the old file")]
    let mut old = fs::File::open(&path).unwrap();
    os::paths().write(&path, b"new").unwrap();
    let mut bytes = Vec::new();
    old.read_to_end(&mut bytes).unwrap();
    assert_eq!(bytes, b"old");
}

#[test]
fn a_write_on_a_device_writes_in_place() {
    assert_eq!(os::paths().write(Path::new("/dev/null"), b"plan"), Ok(()));
    let null = fs::symlink_metadata("/dev/null").unwrap();
    assert!(null.file_type().is_char_device(), "{null:?}");
}

#[test]
fn a_write_on_a_pipe_gives_the_bytes_to_its_reader() {
    let scratch = Scratch::new();
    let pipe = scratch.0.join("pipe");
    mkfifo(&pipe);
    let nonblocking = rustix::fs::OFlags::NONBLOCK.bits().cast_signed();
    let mut reader = fs::OpenOptions::new();
    #[expect(clippy::disallowed_methods, reason = "the test reads the pipe")]
    let mut reader = reader
        .read(true)
        .custom_flags(nonblocking)
        .open(&pipe)
        .unwrap();
    let paths = os::paths();
    assert_eq!(paths.write(&pipe, b"plan"), Ok(()));
    let mut bytes = [0; 8];
    let read = reader.read(&mut bytes).unwrap();
    assert_eq!(&bytes[..read], b"plan");
    assert!(fs::symlink_metadata(&pipe).unwrap().file_type().is_fifo());
    assert_eq!(
        paths.read(&scratch.0, 1 << 12),
        Ok(Found::Directory(vec![pipe]))
    );
}

/// The read of a FIFO with no writer answers in place of a wait.
#[test]
fn a_read_of_a_pipe_is_an_invalid_argument() {
    let scratch = Scratch::new();
    let pipe = scratch.0.join("pipe");
    mkfifo(&pipe);
    let (send, receive) = mpsc::channel();
    #[expect(clippy::disallowed_methods, reason = "the test bounds a hang")]
    thread::spawn(move || send.send(os::paths().read(&pipe, 1)).unwrap());
    #[expect(clippy::disallowed_methods, reason = "the test bounds a hang")]
    let read = receive.recv_timeout(Duration::from_secs(10));
    assert_eq!(read, Ok(Err(Error::Io { code: 22 })));
}

#[test]
fn a_read_of_a_device_is_an_invalid_argument() {
    assert_eq!(
        os::paths().read(Path::new("/dev/null"), 1),
        Err(Error::Io { code: 22 })
    );
}

#[test]
fn a_read_of_a_socket_is_an_invalid_argument() {
    // A socket path has a bound of 104 bytes on macOS, which the scratch path can pass.
    let socket =
        PathBuf::from(format!("/tmp/foundation-os-r-{}.sock", std::process::id()));
    let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
    let read = os::paths().read(&socket, 1);
    drop(listener);
    fs::remove_file(&socket).unwrap();
    assert_eq!(read, Err(Error::Io { code: 22 }));
}

#[test]
fn a_write_on_a_link_writes_the_file_it_names() {
    let scratch = Scratch::new();
    let (file, link) = (scratch.0.join("plant.hcl"), scratch.0.join("link"));
    fs::write(&file, b"old file").unwrap();
    symlink(&file, &link).unwrap();
    let paths = os::paths();
    paths.write(&link, b"new").unwrap();
    assert_eq!(paths.read(&file, 3), Ok(Found::File(b"new".to_vec())));
    assert!(fs::symlink_metadata(&link).unwrap().is_symlink());
    let entries = Ok(Found::Directory(vec![link, file]));
    assert_eq!(paths.read(&scratch.0, 1 << 12), entries);
}

#[test]
fn a_write_on_a_link_leaves_the_mode_of_the_file_it_names() {
    let scratch = Scratch::new();
    let (file, link) = (scratch.0.join("plant.hcl"), scratch.0.join("link"));
    fs::write(&file, b"old").unwrap();
    fs::set_permissions(&file, Permissions::from_mode(0o644)).unwrap();
    symlink(&file, &link).unwrap();
    os::paths().write(&link, b"new").unwrap();
    assert_eq!(mode(&file), 0o644);
}

#[test]
fn a_write_on_a_link_to_nothing_is_not_found_and_makes_no_file() {
    let scratch = Scratch::new();
    let (target, link) = (scratch.0.join("plant.hcl"), scratch.0.join("link"));
    symlink(&target, &link).unwrap();
    let paths = os::paths();
    assert_eq!(paths.write(&link, b"new"), Err(Error::NotFound));
    assert!(fs::symlink_metadata(&link).unwrap().is_symlink());
    assert_eq!(paths.read(&target, 1), Err(Error::NotFound));
    let entries = Ok(Found::Directory(vec![link]));
    assert_eq!(paths.read(&scratch.0, 1 << 12), entries);
}

#[test]
fn two_writers_of_one_path_each_write_whole() {
    let scratch = Scratch::new();
    let path = scratch.0.join("plant.plan");
    let (a, b) = (vec![b'a'; 1 << 20], vec![b'b'; 1 << 20]);
    for _ in 0..8 {
        #[expect(clippy::disallowed_methods, reason = "os is the crate under test")]
        std::thread::scope(|scope| {
            let writers =
                [&a, &b].map(|bytes| scope.spawn(|| os::paths().write(&path, bytes)));
            for writer in writers {
                assert_eq!(writer.join().unwrap(), Ok(()));
            }
        });
        let read = os::paths().read(&path, 1 << 20);
        let whole = [&a, &b].map(|bytes| Ok(Found::File(bytes.clone())));
        assert!(whole.contains(&read), "a mixed or failed write");
    }
    let entries = Ok(Found::Directory(vec![path]));
    assert_eq!(os::paths().read(&scratch.0, 1 << 12), entries);
}

/// `link` names a directory or a file at each instant: another thread swaps it with one
/// rename. A read gives one of the two, and on macOS also `Io` 22 (#2964).
#[test]
fn a_read_of_a_path_that_swaps_a_directory_for_a_file_gives_one_of_them() {
    let scratch = Scratch::new();
    let (dir, file) = (scratch.0.join("dir"), scratch.0.join("file"));
    fs::create_dir(&dir).unwrap();
    fs::write(&file, b"x").unwrap();
    let (link, next) = (scratch.0.join("link"), scratch.0.join("next"));
    symlink(&dir, &link).unwrap();
    let swap = || {
        for target in [&file, &dir] {
            symlink(target, &next).unwrap();
            fs::rename(&next, &link).unwrap();
        }
    };
    let other = race(swap, 5_000, &link, |read| match read {
        Ok(Found::File(_) | Found::Directory(_)) => true,
        read => cfg!(target_os = "macos") && *read == Err(Error::Io { code: 22 }),
    });
    assert_eq!(other, None);
}

/// `link` names a file or a FIFO at each instant. A read gives the file or `Io` 22,
/// never the empty read of the FIFO, and never waits for its writer. On Linux it can
/// also give a folder on the way to the target.
#[test]
fn a_read_of_a_path_that_swaps_a_file_for_a_pipe_gives_the_file_or_an_invalid_argument()
{
    let scratch = Scratch::new();
    let (pipe, file) = (scratch.0.join("pipe"), scratch.0.join("file"));
    mkfifo(&pipe);
    fs::write(&file, b"x").unwrap();
    let (link, next) = (scratch.0.join("link"), scratch.0.join("next"));
    symlink(&file, &link).unwrap();
    let swap = || {
        for target in [&pipe, &file] {
            symlink(target, &next).unwrap();
            fs::rename(&next, &link).unwrap();
        }
    };
    let other = race(swap, 5_000, &link, |read| match read {
        Ok(Found::File(bytes)) => bytes == b"x",
        Ok(Found::Directory(entries)) => {
            cfg!(target_os = "linux") && on_the_way(entries, &link, &scratch.0)
        }
        read => *read == Err(Error::Io { code: 22 }),
    });
    assert_eq!(other, None);
}

/// Whether `entries`, as a read of `link` gives them, are the names of `scratch` or of
/// a folder above it, which holds the next folder on the way to `scratch`. Other tests
/// change the folders above, so only that one name is sure.
fn on_the_way(entries: &[PathBuf], link: &Path, scratch: &Path) -> bool {
    let names: Option<Vec<&Path>> = entries
        .iter()
        .map(|entry| entry.strip_prefix(link).ok())
        .collect();
    let Some(names) = names else {
        return false;
    };
    // `next` is there only during a swap.
    let here = [
        ["file", "link", "pipe"].as_slice(),
        &["file", "link", "next", "pipe"],
    ];
    if here
        .iter()
        .any(|here| names.iter().copied().eq(here.iter().map(Path::new)))
    {
        return true;
    }
    scratch.ancestors().skip(1).any(|folder| {
        let next = scratch.strip_prefix(folder).unwrap().iter().next();
        next.is_some_and(|next| names.contains(&Path::new(next)))
    })
}

/// `plant` and `file` swap a directory and a file in one step.
#[test]
fn a_read_of_a_directory_that_becomes_a_file_gives_no_error() {
    let scratch = Scratch::new();
    let (plant, file) = (scratch.0.join("plant"), scratch.0.join("file"));
    fs::create_dir(&plant).unwrap();
    fs::write(&file, b"x").unwrap();
    let swap = || {
        renameat_with(CWD, &plant, CWD, &file, RenameFlags::EXCHANGE).unwrap();
    };
    let other = race(swap, 100_000, &plant, |read| {
        matches!(read, Ok(Found::File(_) | Found::Directory(_)))
    });
    assert_eq!(other, None);
}

/// A path through `/dev/fd` names a directory that is removed before the read opens it.
#[test]
fn a_read_of_a_removed_directory_lists_no_entries() {
    use std::os::fd::AsRawFd;
    let scratch = Scratch::new();
    let plant = scratch.0.join("plant");
    fs::create_dir(&plant).unwrap();
    #[expect(clippy::disallowed_methods, reason = "the test holds the directory")]
    let held = fs::File::open(&plant).unwrap();
    fs::remove_dir(&plant).unwrap();
    let path = PathBuf::from(format!("/dev/fd/{}", held.as_raw_fd()));
    assert_eq!(
        os::paths().read(&path, 1 << 12),
        Ok(Found::Directory(Vec::new()))
    );
}

/// Reads `path` until another thread has called `swap` `cycles` times or ended, and
/// gives the first read that `expected` refuses. The bound is on the swaps, not the
/// reads, so a slow or busy host still races each read against many swaps. On macOS
/// it stops at 1 000 swaps: the race shows there in the first swaps, and each swap is
/// slow.
fn race(
    swap: impl Fn() + Sync,
    cycles: usize,
    path: &Path,
    expected: impl Fn(&Result<Found, Error>) -> bool,
) -> Option<Result<Found, Error>> {
    let cycles = if cfg!(target_os = "macos") {
        cycles.min(1_000)
    } else {
        cycles
    };
    let (done, stopped) = (AtomicUsize::new(0), AtomicBool::new(false));
    thread::scope(|scope| {
        #[expect(clippy::disallowed_methods, reason = "the test races the read")]
        let swapper = scope.spawn(|| {
            while !stopped.load(Ordering::Relaxed) {
                swap();
                done.fetch_add(1, Ordering::Relaxed);
            }
        });
        let paths = os::paths();
        let mut other = None;
        // A panic in `swap` ends the swapper, and the scope then fails the test.
        while other.is_none()
            && done.load(Ordering::Relaxed) < cycles
            && !swapper.is_finished()
        {
            other = Some(paths.read(path, 1 << 12)).filter(|read| !expected(read));
        }
        stopped.store(true, Ordering::Relaxed);
        other
    })
}

#[test]
fn a_resolve_of_a_relative_path_is_from_the_working_directory() {
    let working = fs::canonicalize(".").unwrap();
    let paths = os::paths();
    assert_eq!(paths.resolve(Path::new(".")), Ok(working.clone()));
    let none = paths.resolve(Path::new("none.plan"));
    assert_eq!(none, Ok(working.join("none.plan")));
}

#[test]
fn a_resolve_of_a_path_with_a_nul_byte_is_an_invalid_argument() {
    let resolved = os::paths().resolve(Path::new("a\0b"));
    assert_eq!(resolved, Err(Error::Io { code: 22 }));
}

#[test]
fn the_home_is_the_resolved_home_of_the_os() {
    let home = std::env::home_dir().unwrap();
    assert_eq!(os::paths().home(), Some(fs::canonicalize(home).unwrap()));
}

/// The resolved home that the child of
/// `a_home_through_a_link_that_is_not_there_is_as_a_resolve_gives_it` checks.
const HOME: &str = "FOUNDATION_TEST_HOME";

#[test]
fn a_home_through_a_link_that_is_not_there_is_as_a_resolve_gives_it() {
    let paths = os::paths();
    #[expect(
        clippy::disallowed_methods,
        reason = "the parent sets it for the child"
    )]
    if let Some(home) = std::env::var_os(HOME) {
        assert_eq!(paths.home(), Some(PathBuf::from(home)));
        return;
    }
    let scratch = Scratch::new();
    fs::create_dir(scratch.0.join("users")).unwrap();
    symlink(scratch.0.join("users"), scratch.0.join("home")).unwrap();
    let home = fs::canonicalize(&scratch.0).unwrap().join("users/user");
    let thread = thread::current();
    let test = thread.name().expect("invariant: libtest names the thread");
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", test])
        .env(HOME, &home)
        .env("HOME", scratch.0.join("home/user"))
        .output()
        .unwrap();
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(output.status.success(), "{stderr}");
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("1 passed"), "{stdout}");
}

/// A hidden file that is not there yet, written by a name in other case.
#[cfg(target_os = "macos")]
#[test]
fn within_makes_no_file_at_a_hidden_name_in_other_case() {
    run(|host| {
        let (_, base) = cases::within_work(host);
        let key = base.join("work/secret.key");
        let dir = base.join("work/vault");
        let within = host
            .paths
            .within(&[base.join("work")], &[key.clone(), dir.clone()])
            .unwrap();
        let wrote = within.write(&base.join("work/SECRET.KEY"), b"planted");
        let made = within.make_dir(&base.join("work/VAULT"));
        let there = (fs::metadata(&key).is_ok(), fs::metadata(&dir).is_ok());
        assert_eq!(
            (wrote, made, there),
            (Err(Error::Hidden), Err(Error::Hidden), (false, false))
        );
    });
}

/// A program replaces the folder with a link to a directory outside it.
#[test]
fn a_folder_that_a_link_replaces_holds_nothing_outside() {
    run(|host| {
        let (_, base) = cases::within_work(host);
        let work = base.join("work");
        let within = host.paths.within(slice::from_ref(&work), &[]).unwrap();
        fs::rename(&work, base.join("old")).unwrap();
        symlink(base.join("workshop"), &work).unwrap();
        let read = within.read(&work.join("a.hcl"), 1);
        let direct = within.read(&base.join("workshop/a.hcl"), 1);
        assert_eq!((read, direct), (Err(Error::Outside), Err(Error::Outside)));
    });
}

/// A program replaces the directory above a hidden file with a file, or
/// takes search from it. A path in the folder that is not hidden still reads.
#[test]
fn a_hidden_path_that_fails_its_key_fails_no_other_call() {
    run(|host| {
        let (_, base) = cases::within_work(host);
        let data = base.join("work/data");
        let key = data.join("admin.key");
        let within = host
            .paths
            .within(&[base.join("work")], slice::from_ref(&key))
            .unwrap();
        let plan = base.join("work/a.hcl");
        fs::set_permissions(&data, Permissions::from_mode(0o000)).unwrap();
        let unsearchable = within.read(&plan, 1);
        fs::set_permissions(&data, Permissions::from_mode(0o700)).unwrap();
        fs::remove_dir_all(&data).unwrap();
        fs::write(&data, b"file").unwrap();
        let through_file = within.read(&plan, 1);
        let ok = Ok(Found::File(b"x".to_vec()));
        assert_eq!((unsearchable, through_file), (ok.clone(), ok));
    });
}
