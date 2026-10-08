//! A scratch directory for a test, and the modes of what a disk makes in it.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use env::files::{Files, Mode};

/// The length of the file that [`opened`] opens.
pub(crate) const LEN: u64 = 1 << 10;

/// A directory of its own for the test on this thread, removed when it drops.
pub(crate) struct Scratch(pub(crate) PathBuf);

impl Scratch {
    pub(crate) fn new() -> Self {
        let thread = std::thread::current();
        let test = thread.name().expect("invariant: libtest names the thread");
        let name = format!("foundation-os-{}-{test}", std::process::id());
        let dir = std::env::temp_dir().join(name.replace("::", "-"));
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

/// The mode of `path`, with the setuid, setgid, and sticky bits.
pub(crate) fn mode(path: &Path) -> u32 {
    std::fs::metadata(path).unwrap().permissions().mode() & 0o7777
}

/// Opens a disk on `dir`, opens the file `f` with `Create` and makes the directory
/// `d` in it, and gives the modes of `data`, `data/f`, and `data/d` after the disk
/// ends.
pub(crate) fn opened(dir: &Path) -> [u32; 3] {
    let (disk, thread) = os::files(dir, &os::threads().unwrap(), "files").unwrap();
    let files = Files::new(disk);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    runtime.block_on(async {
        let mode = Mode::Create { len: LEN };
        let file = files.open(Path::new("f"), mode).await.unwrap();
        file.close().await;
        files.create_dir(Path::new("d")).await.unwrap();
    });
    drop(files);
    thread.join().unwrap();
    let data = dir.join("data");
    [mode(&data), mode(&data.join("f")), mode(&data.join("d"))]
}
