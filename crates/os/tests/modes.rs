//! The modes of the files and directories that `os::files` makes. The umask is one for
//! the process, so these tests run in a test binary of their own, and one test sets it.

// Lets Clippy treat the helpers as test code.
#![cfg(test)]

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use env::files::{Files, Mode};
use rustix::fs::Mode as Bits;

/// A new directory for one case, removed when it drops.
struct Scratch(PathBuf);

impl Scratch {
    fn new(case: &str) -> Self {
        let name = format!("foundation-os-modes-{}-{case}", std::process::id());
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

/// The permission bits of `path`, in octal.
fn mode(path: &Path) -> String {
    let mode = std::fs::metadata(path).unwrap().permissions().mode();
    format!("{:o}", mode & 0o7777)
}

/// Makes the files of `dir`, a file `a`, and a directory `b`.
fn create(dir: &Path) {
    let (disk, thread) = os::files(dir, &os::threads().unwrap(), "files").unwrap();
    let files = Files::new(disk);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    runtime.block_on(async {
        let file = files.open(Path::new("a"), Mode::Create { len: 4_096 });
        file.await.unwrap().close().await;
        files.create_dir(Path::new("b")).await.unwrap();
    });
    drop(files);
    thread.join().unwrap();
}

#[test]
fn a_new_file_is_0600_and_a_new_directory_0700_at_each_umask() {
    for umask in [0o022, 0] {
        rustix::process::umask(Bits::from_raw_mode(umask));
        let scratch = Scratch::new(&format!("{umask:o}"));
        create(&scratch.0);
        let data = scratch.0.join("data");
        assert_eq!(mode(&data), "700", "data at umask {umask:o}");
        assert_eq!(mode(&data.join("a")), "600", "file at umask {umask:o}");
        assert_eq!(mode(&data.join("b")), "700", "dir at umask {umask:o}");
    }
}

#[test]
fn a_data_directory_that_is_there_keeps_its_mode() {
    let scratch = Scratch::new("there");
    let data = scratch.0.join("data");
    std::fs::create_dir(&data).unwrap();
    std::fs::set_permissions(&data, std::fs::Permissions::from_mode(0o755)).unwrap();
    create(&scratch.0);
    assert_eq!(mode(&data), "755");
}
