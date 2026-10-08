//! The modes of the files and directories that `os::files` makes. The umask is a
//! value of the process, so these tests run in a test binary of their own: the
//! harness runs the tests of a binary on threads of one process.

// Lets Clippy treat the helpers as test code.
#![cfg(test)]
#![cfg(any(target_os = "linux", target_os = "macos"))]

use std::fs::Permissions;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use env::files::{Files, Mode};

const LEN: u64 = 1 << 10;

/// A directory of its own for the test, removed when it drops.
struct Scratch(PathBuf);

impl Scratch {
    fn new(name: &str) -> Self {
        let name = format!("foundation-os-modes-{}-{name}", std::process::id());
        let dir = std::env::temp_dir().join(name);
        std::fs::create_dir(&dir).unwrap();
        Self(dir)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        if !std::thread::panicking() {
            std::fs::remove_dir_all(&self.0).unwrap();
        }
    }
}

fn mode(path: &Path) -> u32 {
    std::fs::metadata(path).unwrap().permissions().mode() & 0o7777
}

/// Opens a disk on `dir`, opens the file `f` with `Create` and makes the directory
/// `d` in it, and gives the modes of `data`, `data/f`, and `data/d` after the disk
/// ends.
fn opened(dir: &Path) -> [u32; 3] {
    let (disk, thread) = os::files(dir, &os::threads().unwrap(), "files").unwrap();
    let files = Files::new(disk);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    runtime.block_on(async {
        let mode = Mode::Create { len: LEN };
        files
            .open(Path::new("f"), mode)
            .await
            .unwrap()
            .close()
            .await;
        files.create_dir(Path::new("d")).await.unwrap();
    });
    drop(files);
    thread.join().unwrap();
    let data = dir.join("data");
    [mode(&data), mode(&data.join("f")), mode(&data.join("d"))]
}

#[test]
fn each_new_file_is_0600_and_each_new_directory_0700_under_any_umask() {
    for mask in [0o022, 0o000] {
        rustix::process::umask(rustix::fs::Mode::from_raw_mode(mask));
        let scratch = Scratch::new(&format!("{mask:o}"));
        assert_eq!(opened(&scratch.0), [0o700, 0o600, 0o700], "umask {mask:o}");
    }
}

#[test]
fn a_file_or_directory_that_is_there_keeps_its_mode() {
    let scratch = Scratch::new("there");
    let data = scratch.0.join("data");
    std::fs::create_dir_all(data.join("d")).unwrap();
    std::fs::write(data.join("f"), vec![0; usize::try_from(LEN).unwrap()]).unwrap();
    for (path, mode) in [("", 0o755), ("f", 0o644), ("d", 0o755)] {
        std::fs::set_permissions(data.join(path), Permissions::from_mode(mode))
            .unwrap();
    }
    assert_eq!(opened(&scratch.0), [0o755, 0o644, 0o755]);
}
