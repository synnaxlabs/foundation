//! A get of the secret store of `node`, on the real disk, leaves the value in no
//! freed block: a get that reads it, a get whose read fails after it filled part of
//! the block, and a get that drops while its read waits. The scan covers every thread,
//! so this binary has no test harness.

#![expect(clippy::disallowed_macros, reason = "COUNTING ALLOCATOR")]

use std::fs::Permissions;
use std::future::poll_fn;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::process::Command;
use std::sync::Arc;
use std::task::Poll;

use env::files::{Files, Mode};
use secret::{Error, Resolver, Source, Value};

/// A get of a resolver, boxed so that a test can poll it and then drop it.
type Get<'a> = Pin<Box<dyn Future<Output = Result<Value, Error>> + 'a>>;

#[global_allocator]
static ALLOCATOR: counting::Allocator = counting::Allocator::new();

/// The value of the file, which the scan looks for.
type Needle = [u8; 32];

/// The needle of case `case`. No other case and no fill of one byte gives it, so a
/// case finds no copy that an earlier case left and no fill of the allocator.
const fn needle(case: u8) -> Needle {
    let mut needle = *b"?a needle that no fill can give.";
    needle[0] = case;
    needle
}

/// The needle of each case, in static memory, so that a case holds only a reference
/// to it in the futures and threads that it frees.
static NEEDLES: [Needle; 3] = [needle(0), needle(1), needle(2)];

fn main() {
    let found = [
        ("reads", freed("reads", &NEEDLES[0], reads)),
        ("cut", freed("cut", &NEEDLES[1], cut)),
        ("dropped", freed("dropped", &NEEDLES[2], dropped)),
    ];
    assert_eq!(
        found,
        [("reads", 0), ("cut", 0), ("dropped", 0)],
        "a get of the file store leaves the value in no freed block"
    );
}

/// Runs `case` on a shard with the real disk of a new directory, whose file
/// `data/secrets/site.token` holds `plain` and a line end. Gives the count of freed
/// blocks that hold `plain`, from the start of `case` to the end of the I/O thread.
fn freed<F>(
    name: &str,
    plain: &'static Needle,
    case: fn(Files, PathBuf, &'static Needle) -> F,
) -> u64
where
    F: Future<Output = ()> + 'static,
{
    let dir = std::env::temp_dir()
        .join(format!("foundation-erase-{}-{name}", std::process::id()));
    let threads = os::threads().expect("the OS gives the cores");
    let (disk, io) = os::files(&dir, &threads, "files").expect("the disk opens");
    let data = dir.join("data");
    std::fs::create_dir(data.join("secrets")).expect("makes the directory");
    let token = [&plain[..], b"\n"].concat();
    let file = data.join("secrets/site.token");
    std::fs::write(&file, token).expect("writes it");
    std::fs::set_permissions(&file, Permissions::from_mode(0o600))
        .expect("makes it private");
    let ((), found) = ALLOCATOR.freed_holding(plain, || {
        let get = threads.start("get", move || case(Files::new(disk), data, plain));
        get.expect("the thread starts")
            .join()
            .expect("the get ends");
        io.join().expect("the I/O thread ends");
    });
    std::fs::remove_dir_all(&dir).expect("removes the directory");
    found
}

async fn reads(files: Files, _: PathBuf, plain: &'static Needle) {
    let resolver = resolver(files);
    let value = resolver.get(&source()).await;
    let value = value.expect("the file holds the value");
    assert_eq!(value.expose(), plain, "the file store reads the value");
}

/// Cuts the file to `plain` after the get opened it, so the read fills 32 of its 33
/// bytes and fails.
async fn cut(files: Files, data: PathBuf, plain: &'static Needle) {
    let resolver = resolver(files.clone());
    let source = source();
    let mut get: Get<'_> = Box::pin(resolver.get(&source));
    hold_read(&files, &data, &mut get).await;
    std::fs::write(data.join("secrets/site.token"), plain).expect("cuts the file");
    release(&data, "b");
    let failed = Err(Error::Unavailable {
        detail: format!(
            "the read_at of the file {} failed with OS error 5. Check the path and the \
             file",
            data.join("secrets/site.token").display()
        ),
    });
    let got = get.await.map(|value| value.expose().to_vec());
    assert_eq!(got, failed, "a read that ends short fails");
}

/// Drops the get while its read waits on the I/O thread.
async fn dropped(files: Files, data: PathBuf, _: &'static Needle) {
    let resolver = resolver(files.clone());
    let source = source();
    let mut get: Get<'_> = Box::pin(resolver.get(&source));
    hold_read(&files, &data, &mut get).await;
    drop(get);
    release(&data, "b");
}

/// Polls `get` until its read waits on the I/O thread behind an open of the FIFO
/// `b`, which [`release`] ends. An open of the FIFO `a` holds the I/O thread at the
/// first poll, so the open of the file ends only after it, and a list after that open
/// tells when it ended.
async fn hold_read(files: &Files, data: &Path, get: &mut Get<'_>) {
    for fifo in ["a", "b"] {
        let made = Command::new("mkfifo").arg(data.join(fifo)).status();
        assert!(
            made.expect("mkfifo runs").success(),
            "makes the FIFO {fifo}"
        );
    }
    let mut a = Box::pin(files.open(Path::new("a"), Mode::Read));
    assert!(
        once(a.as_mut()).await.is_pending(),
        "`a` waits for a writer"
    );
    assert!(
        once(get.as_mut()).await.is_pending(),
        "the open waits behind `a`"
    );
    let mut listed = Box::pin(files.list(Path::new("secrets")));
    assert!(
        once(listed.as_mut()).await.is_pending(),
        "the list waits behind `a`"
    );
    let mut b = Box::pin(files.open(Path::new("b"), Mode::Read));
    assert!(once(b.as_mut()).await.is_pending(), "`b` waits behind `a`");
    // The I/O thread runs a call that it took also when its future drops.
    drop(b);
    release(data, "a");
    drop(a.await.expect("opens `a`"));
    listed.await.expect("lists the directory");
    assert!(
        once(get.as_mut()).await.is_pending(),
        "the read waits behind `b`"
    );
}

/// Polls `future` once.
async fn once<F: Future + ?Sized>(mut future: Pin<&mut F>) -> Poll<F::Output> {
    poll_fn(|cx| Poll::Ready(future.as_mut().poll(cx))).await
}

/// Opens the FIFO `fifo` of `data` to write, which ends the open that waits on it.
#[expect(
    clippy::disallowed_methods,
    reason = "the other end of a FIFO that the disk under test waits on"
)]
fn release(data: &Path, fifo: &str) {
    let writer = std::fs::OpenOptions::new()
        .write(true)
        .open(data.join(fifo));
    drop(writer.expect("opens the FIFO"));
}

/// A resolver whose file store is the directory `secrets` of `files`.
fn resolver(files: Files) -> Resolver {
    Resolver::new(Arc::default(), files, Path::new("secrets"))
}

fn source() -> Source {
    Source::Secret("site.token".parse().expect("a valid name"))
}
