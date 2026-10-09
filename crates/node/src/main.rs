//! The `foundation` binary. It runs the command line, and `foundation start` runs a
//! node until SIGINT or SIGTERM.

use std::io::{self, Write};
use std::net::{Ipv4Addr, SocketAddr};
use std::process::ExitCode;
use std::sync::mpsc;

use document::diagnostic::Code;
use env::threads::Threads;
use ops::{Failure, Run, Start};
use types::byte::Size;
use types::name::Name;

/// The pool budget of the node, a patch until the start sizes it from the host
/// (#1732).
const BUDGET: Size = Size::from_bytes(1 << 30);
/// The disk budget of the node, a patch as [`BUDGET`] is.
const DISK: Size = Size::from_bytes(256 << 20);

const BUSY: Code = Code::new("node.busy");
const DATA: Code = Code::new("node.data");
const UNNAMED: Code = Code::new("node.unnamed");
const RENAMED: Code = Code::new("node.renamed");
const NAME: Code = Code::new("node.name");
const FAILED: Code = Code::new("node.failed");
/// The fix of [`DATA`].
const WRITABLE: &str =
    "Give with `--data` a directory that this user can make and write";

fn main() -> ExitCode {
    let run = ops::cli(
        std::env::args_os(),
        io::stdin().lock(),
        io::stdout().lock(),
        io::stderr().lock(),
    );
    ExitCode::from(match run {
        Run::Exit(status) => status,
        Run::Start(start) => match node(&start) {
            Ok(()) => 0,
            Err(failure) => start.fail(&failure, io::stderr().lock()),
        },
    })
}

/// Runs a node on the data directory of `start` until SIGINT or SIGTERM stops it.
fn node(start: &Start) -> Result<(), Failure> {
    // Before any thread starts, else that thread takes the signals.
    let interrupt = os::interrupt().map_err(failed)?;
    let threads = os::threads().map_err(failed)?;
    let name = name(start, &threads)?;
    let (called, call) = mpsc::channel();
    let mut line = Vec::new();
    start.running(&name, &mut line);
    // Its own thread, so a standard output that nobody reads blocks neither shard 0
    // nor the stop.
    let show = threads.start("show", move || async move {
        #[expect(
            clippy::disallowed_methods,
            reason = "the thread has no other work than this wait"
        )]
        let called = call.recv();
        if called.is_ok() {
            io::stdout().lock().write_all(&line).unwrap_or(());
        }
    });
    let show = show.map_err(failed)?;
    let shards = os::shards().map_err(failed)?;
    let wall = os::wall().map_err(failed)?;
    let mut disks = Vec::new();
    let mut handles = Vec::new();
    for core in 0..shards.cores().get() {
        let (disk, handle) = os::files(&start.data, &threads, &format!("files-{core}"))
            .map_err(|error| data(start, error))?;
        disks.push(disk);
        handles.push(handle);
    }
    let mut disks = disks.into_iter();
    let node = node::Node::start(node::Config {
        shards,
        clock: os::clock(),
        wall,
        budget: BUDGET,
        memory: Box::new(os::memory::Memory::new),
        files: Box::new(move || {
            let disk = disks.next().expect("invariant: one disk for each shard");
            Box::new(move || env::files::Files::new(disk))
        }),
        entropy: os::entropy(),
        disk: DISK,
        net: net(),
        listen: SocketAddr::from((Ipv4Addr::LOCALHOST, 0)),
        region: None,
        name: name.clone(),
    });
    let stopper = node.stopper();
    let stop = threads.start("stop", move || async move {
        interrupt.await;
        stopper.stop();
    });
    if stop.is_ok() {
        node.spawn(move |_| {
            called
                .send(())
                .expect("invariant: the thread `show` waits for the call");
            async {}
        });
    } else {
        node.stop();
    }
    let joined = node.join().map_err(|error| failure(start, &error));
    let closed = handles.into_iter().try_for_each(env::thread::Handle::join);
    // Each waits only on the process: for a signal, or for a reader of standard output.
    let ended = stop.map(drop).map_err(failed);
    drop(show);
    joined.and(closed.map_err(failed)).and(ended)
}

/// The network of the node.
#[cfg(not(loom))]
fn net() -> env::net::Net {
    os::net()
}

/// A `--cfg loom` build has no network, and its tests run no node.
#[cfg(loom)]
fn net() -> env::net::Net {
    unreachable!("a `--cfg loom` build runs no node")
}

/// The name of the node of `start`, read on a thread of `threads`.
#[expect(
    clippy::unwrap_in_result,
    reason = "a thread that ends with no panic has sent the name"
)]
fn name(start: &Start, threads: &Threads) -> Result<Name, Failure> {
    let (disk, handle) = os::files(&start.data, threads, "files-name")
        .map_err(|error| data(start, error))?;
    let (send, named) = mpsc::channel();
    let given = start.name.clone();
    let read = threads.start("name", move || async move {
        let files = env::files::Files::new(disk);
        let name = node::name(&files, given).await;
        send.send(name).expect("invariant: main waits for the name");
    });
    read.map_err(failed)?.join().map_err(failed)?;
    handle.join().map_err(failed)?;
    let name = named
        .try_recv()
        .expect("invariant: the thread sent the name");
    name.map_err(|error| failure(start, &error))
}

/// The failure of a data directory that `os` could not open or make.
fn data(start: &Start, error: os::Error) -> Failure {
    match error {
        os::Error::Dir(error) => Failure {
            code: DATA,
            message: format!(
                "cannot open or make the data directory {}: {error}",
                start.data.display()
            ),
            fix: WRITABLE.to_owned(),
        },
        error => failed(error),
    }
}

/// The failure of a node that stopped with `error`.
fn failure(start: &Start, error: &node::Error) -> Failure {
    let data = start.data.display();
    let (code, message, fix) = match error {
        node::Error::Directory(env::files::Error::Busy { .. }) => (
            BUSY,
            format!("another node runs in {data}"),
            "Stop that node, or give another data directory with `--data`".to_owned(),
        ),
        node::Error::Directory(error) => (
            DATA,
            format!("cannot use the data directory {data}: {error}"),
            WRITABLE.to_owned(),
        ),
        node::Error::Unnamed => (
            UNNAMED,
            format!("the data directory {data} holds no node"),
            "Give the new node a name with `--name`".to_owned(),
        ),
        node::Error::Renamed { stored, given } => (
            RENAMED,
            format!("the data directory {data} holds the node {stored}, not {given}"),
            format!("Give `--name {stored}`, or another data directory with `--data`"),
        ),
        node::Error::Name => (
            NAME,
            format!("the file `name` in the data directory {data} is not a node name"),
            "Remove it, and start the node with its name".to_owned(),
        ),
        error => return failed(error),
    };
    Failure { code, message, fix }
}

/// The failure for an error that the user cannot fix by a change to the command.
fn failed(error: impl std::fmt::Display) -> Failure {
    Failure {
        code: FAILED,
        message: error.to_string(),
        fix: "Fix the cause that the message states, then start the node again"
            .to_owned(),
    }
}
