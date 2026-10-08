//! Helpers that the test binaries of `os` share: a scratch directory, the files of a
//! disk on it, and the modes of what a disk makes.

use std::fs::Permissions;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use env::files::{Files, Mode};

pub(crate) const KIB: u64 = 1 << 10;

/// A directory of its own for the test on this thread, removed when it drops.
pub(crate) struct Scratch(pub(crate) PathBuf);

impl Scratch {
    pub(crate) fn new() -> Self {
        let thread = std::thread::current();
        let test = thread.name().expect("invariant: libtest names the thread");
        let name = format!("foundation-os-{}-{test}", std::process::id());
        let dir = std::env::temp_dir().join(name.replace("::", "-"));
        std::fs::create_dir(&dir).unwrap();
        // Clears a setgid bit that the directory takes from a setgid `TMPDIR`.
        std::fs::set_permissions(&dir, Permissions::from_mode(0o700)).unwrap();
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

/// The files of `dir`, on an I/O thread named `name`, and the handle of the thread.
pub(crate) fn files(dir: &Path, name: &str) -> (Files, env::thread::Handle) {
    let (disk, thread) = os::files(dir, &os::threads().unwrap(), name).unwrap();
    (Files::new(disk), thread)
}

/// The mode of `path`, with the setuid, setgid, and sticky bits.
pub(crate) fn mode(path: &Path) -> u32 {
    std::fs::metadata(path).unwrap().permissions().mode() & 0o7777
}

/// Opens a disk on `dir`, opens the file `f` with `Create` and makes the directory
/// `d` in it, and gives the modes of `data`, `data/f`, and `data/d` after the disk
/// ends.
pub(crate) fn opened(dir: &Path) -> [u32; 3] {
    let (files, thread) = files(dir, "files");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    runtime.block_on(async {
        let mode = Mode::Create { len: KIB };
        let file = files.open(Path::new("f"), mode).await.unwrap();
        file.close().await;
        files.create_dir(Path::new("d")).await.unwrap();
    });
    drop(files);
    thread.join().unwrap();
    let data = dir.join("data");
    [mode(&data), mode(&data.join("f")), mode(&data.join("d"))]
}
