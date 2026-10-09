//! `os::files` on a small filesystem that the test mounts with `sudo`. Only one CI
//! step on a GitHub-hosted runner runs this target.
// Lets Clippy treat the helpers as test code.
#![cfg(test)]
#![cfg(target_os = "linux")]

use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use env::files::{Error, Files, Mode};

#[path = "../it/kept.rs"]
mod kept;

/// A 64 MiB ext4 filesystem on a loop device in a directory of its own, unmounted and
/// removed when it drops, except while the thread panics.
struct Small(PathBuf);

impl Small {
    fn new() -> Self {
        let thread = std::thread::current();
        let test = thread.name().expect("invariant: libtest names the thread");
        let name = format!("foundation-os-{}-{test}", std::process::id());
        let dir = std::env::temp_dir().join(name);
        let mount = dir.join("mount");
        std::fs::create_dir_all(&mount).unwrap();
        let image = dir.join("image");
        let user = std::fs::metadata(&dir).unwrap().uid();
        check(Command::new("mkfs.ext4").arg("-q").arg(&image).arg("64M"));
        check(sudo("mount").args(["-o", "loop"]).arg(&image).arg(&mount));
        check(sudo("chown").arg(user.to_string()).arg(&mount));
        Self(dir)
    }

    fn mount(&self) -> PathBuf {
        self.0.join("mount")
    }
}

impl Drop for Small {
    fn drop(&mut self) {
        // While the thread panics, a command can block and a second panic aborts the
        // test binary.
        if !std::thread::panicking() {
            check(sudo("umount").arg(self.mount()));
            std::fs::remove_dir_all(&self.0).unwrap();
        }
    }
}

fn sudo(program: &str) -> Command {
    let mut command = Command::new("sudo");
    command.args(["-n", program]);
    command
}

/// Runs `command` and panics with its error output when it fails.
fn check(command: &mut Command) {
    let output = command.output().unwrap();
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{command:?}: {error}");
}

/// Runs `body` with the files of a [`Small`] filesystem and the path of their data
/// directory.
fn run<F: Future<Output = ()>>(body: impl FnOnce(Files, PathBuf) -> F) {
    let disk = Small::new();
    let threads = os::threads().unwrap();
    let (files, thread) = os::files(&disk.mount(), &threads, "files").unwrap();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    runtime.block_on(body(Files::new(files), disk.mount().join("data")));
    thread.join().unwrap();
}

#[test]
fn a_create_past_the_free_bytes_gives_full_and_keeps_no_blocks() {
    run(|files, _| async move {
        let free = files.free().await.unwrap();
        let mode = Mode::Create { len: free * 2 };
        let error = files.open(Path::new("a"), mode).await.unwrap_err();
        assert_eq!(error, Error::Full { path: "a".into() });
        assert_eq!(files.free().await.unwrap(), free);
        let error = files.open(Path::new("a"), Mode::Write).await.unwrap_err();
        assert_eq!(error, Error::NotFound { path: "a".into() });
    });
}

#[test]
fn a_create_after_a_failed_create_gives_full_again() {
    run(|files, _| async move {
        let mode = Mode::Create {
            len: files.free().await.unwrap() * 2,
        };
        files.open(Path::new("a"), mode).await.unwrap_err();
        let error = files.open(Path::new("a"), mode).await.unwrap_err();
        assert_eq!(error, Error::Full { path: "a".into() });
    });
}

#[test]
fn a_create_on_fragmented_free_space_frees_the_blocks_past_the_end() {
    use rustix::fs::{self, FallocateFlags, OFlags};
    run(|files, data| async move {
        // Each free block is alone, but for the root reserve, which `kept::check`
        // gives to `a` before the create. So each block of the create is an extent
        // of its own, and the extent tree needs blocks.
        let stat = fs::statvfs(&data).unwrap();
        let block = stat.f_bsize;
        let flags = OFlags::WRONLY.union(OFlags::CREATE);
        let mode = fs::Mode::RUSR | fs::Mode::WUSR;
        let fill = fs::open(data.join("fill"), flags, mode).unwrap();
        let mut len = stat.f_bavail * stat.f_frsize;
        while let Err(error) = fs::fallocate(&fill, FallocateFlags::empty(), 0, len) {
            assert_eq!(error, rustix::io::Errno::NOSPC);
            len -= 64 * block;
        }
        let punch = FallocateFlags::PUNCH_HOLE.union(FallocateFlags::KEEP_SIZE);
        for at in (0..len).step_by(usize::try_from(2 * block).unwrap()) {
            fs::fallocate(&fill, punch, at, block).unwrap();
        }
        drop(fill);
        let allocated = kept::check(&files, &data).await;
        assert!(
            allocated >= kept::LEN + block,
            "{allocated} bytes allocated"
        );
    });
}
