//! The `foundation` binary. It runs the command line, and `foundation start` runs a
//! node until SIGINT or SIGTERM.

use std::io::{self, Write};
use std::net::{Ipv4Addr, SocketAddr};
use std::path::Path;
use std::process::ExitCode;
use std::sync::mpsc;

use document::diagnostic::Code;
use env::threads::Threads;
use node::Budget;
use ops::{Failure, Run, Start};
use types::byte::Size;
use types::name::Name;

/// The most pool budget that a first start gives.
const POOL_MOST: Size = Size::GIBIBYTE;
/// The most disk budget that a first start gives.
const DISK_MOST: Size = Size::from_bytes(8 << 30);

const BUSY: Code = Code::new("node.busy");
const DATA: Code = Code::new("node.data");
const UNNAMED: Code = Code::new("node.unnamed");
const RENAMED: Code = Code::new("node.renamed");
const NAME: Code = Code::new("node.name");
const DISK: Code = Code::new("node.disk");
const MEMORY: Code = Code::new("node.memory");
const BUDGET: Code = Code::new("node.budget");
const FAILED: Code = Code::new("node.failed");

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
    let known = known(start, &threads)?;
    let (called, call) = mpsc::channel();
    let line = start.line(&known.name);
    // Its own thread, so a standard output that nobody reads blocks neither shard 0
    // nor the stop.
    let show = threads.start("show", move || async move {
        #[expect(
            clippy::disallowed_methods,
            reason = "the thread has no other work than this wait"
        )]
        let called = call.recv();
        if called.is_ok() {
            // A write that fails changes nothing: the node runs either way. The
            // line ends in a newline, so standard output flushes it.
            io::stdout().write_all(line.as_bytes()).unwrap_or(());
        }
    });
    let show = show.map_err(failed)?;
    let shards = os::shards().map_err(failed)?;
    let wall = os::wall().map_err(failed)?;
    let (disks, handles) = disks(&start.data, &threads, shards.cores().get())?;
    let mut disks = disks.into_iter();
    let node = node::Node::start(node::Config {
        shards,
        clock: os::clock(),
        wall,
        budget: known.budget,
        memory: Box::new(os::memory::Memory::new),
        files: Box::new(move || {
            let disk = disks.next().expect("invariant: one disk for each shard");
            Box::new(move || env::files::Files::new(disk))
        }),
        entropy: os::entropy(),
        net: net(),
        listen: SocketAddr::from((Ipv4Addr::LOCALHOST, 0)),
        region: None,
        name: known.name.clone(),
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
    let joined = node
        .join()
        .map_err(|error| stopped(&start.data, &error, &known));
    let closed = handles.into_iter().try_for_each(env::thread::Handle::join);
    // Each waits only on the process: for a signal, or for a reader of standard output.
    let ended = stop.map(drop).map_err(failed);
    drop(show);
    joined.and(closed.map_err(failed)).and(ended)
}

/// The disk of each of `cores` shards on the data directory `dir`, and the handle of
/// its I/O thread.
fn disks(
    dir: &Path,
    threads: &Threads,
    cores: usize,
) -> Result<(Vec<os::Disk>, Vec<env::thread::Handle>), Failure> {
    let files =
        (0..cores).map(|core| os::files(dir, threads, &format!("files-{core}")));
    let opened: Result<Vec<_>, _> = files.collect();
    Ok(opened
        .map_err(|error| data(dir, error))?
        .into_iter()
        .unzip())
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

/// What a start knows before the node starts.
struct Known {
    name: Name,
    budget: Budget,
    /// Where each budget of `budget` comes from.
    from: Origins,
}

/// Where the pool budget and the disk budget come from.
#[derive(Debug, PartialEq, Eq)]
struct Origins {
    pool: Origin,
    disk: Origin,
}

/// Where a budget comes from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Origin {
    /// The data directory keeps it from its first start.
    Kept,
    /// A first start gives its most.
    Most,
    /// A first start gives a quarter of what is free.
    Quarter,
}

/// A resource that a first start gives a quarter of.
#[derive(Clone, Copy)]
enum Resource {
    Memory,
    Disk,
}

/// The name of the node of `start`, and its budgets: the ones that the data
/// directory keeps, else a quarter of the available memory and of the free disk,
/// each up to its most. Reads the data directory on a thread of `threads`.
#[expect(
    clippy::unwrap_in_result,
    reason = "a thread that ends with no panic has sent what it read"
)]
fn known(start: &Start, threads: &Threads) -> Result<Known, Failure> {
    let (disk, handle) = os::files(&start.data, threads, "files-read")
        .map_err(|error| data(&start.data, error))?;
    let (send, receive) = mpsc::channel();
    let dir = start.data.clone();
    let given = start.name.clone();
    let thread = threads.start("read", move || async move {
        let files = env::files::Files::new(disk);
        let known = async {
            let name = node::name(&files, given)
                .await
                .map_err(|e| failure(&dir, &e))?;
            let kept = node::budget(&files).await.map_err(|e| failure(&dir, &e))?;
            if let Some(budget) = kept {
                let from = Origins {
                    pool: Origin::Kept,
                    disk: Origin::Kept,
                };
                return Ok(Known { name, budget, from });
            }
            let memory = os::memory::available().map_err(failed)?;
            let free = files.free().await.map_err(failed)?;
            let (budget, from) = first(memory, free);
            Ok(Known { name, budget, from })
        };
        send.send(known.await)
            .expect("invariant: main waits for what the thread reads");
    });
    thread.map_err(failed)?.join().map_err(failed)?;
    handle.join().map_err(failed)?;
    receive
        .try_recv()
        .expect("invariant: the thread sent what it read")
}

/// The budgets of a first start on a host with `memory` available and `free` bytes
/// free on the disk of the data directory, and where each comes from.
fn first(memory: Size, free: u64) -> (Budget, Origins) {
    let (pool, pool_from) = quarter(memory.bytes(), POOL_MOST);
    let (disk, disk_from) = quarter(free, DISK_MOST);
    let from = Origins {
        pool: pool_from,
        disk: disk_from,
    };
    (Budget { pool, disk }, from)
}

/// A quarter of `free` bytes up to `most`, and whether the quarter or the most won.
fn quarter(free: u64, most: Size) -> (Size, Origin) {
    let quarter = Size::from_bytes(free / 4);
    if quarter < most {
        (quarter, Origin::Quarter)
    } else {
        (most, Origin::Most)
    }
}

/// The failure of a data directory `dir` that `os` could not open or make.
fn data(dir: &Path, error: os::Error) -> Failure {
    match error {
        os::Error::Dir(error) => unwritable(dir, error),
        error => failed(error),
    }
}

/// The failure of a node that stopped with `error`, after a start that knew `known`.
fn stopped(dir: &Path, error: &node::Error, known: &Known) -> Failure {
    match error {
        node::Error::Disk { disk, cores, min } => {
            let (from, fix) = source(dir, known.from.disk, Resource::Disk);
            Failure {
                code: DISK,
                message: format!(
                    "the disk budget {disk}, {from}, holds no ring on each of {cores} \
                     shards; it needs at least {min}"
                ),
                fix,
            }
        }
        node::Error::Pool { pool, cores } => {
            let (from, fix) = source(dir, known.from.pool, Resource::Memory);
            Failure {
                code: MEMORY,
                message: format!(
                    "the pool budget {pool}, {from}, gives one of {cores} shards a pool \
                     that needs more address space than this host has"
                ),
                fix,
            }
        }
        node::Error::Buffer {
            core,
            error: error @ buffer::Error::Pool(pool),
        } => {
            let budget = known.budget.pool;
            let (message, fix) = match pool {
                block::Error::Refused { .. } => (
                    format!(
                        "the system refused memory for shard-{core} that its part of \
                         the pool budget {budget} has room for: {error}"
                    ),
                    "Free memory on this host".to_owned(),
                ),
                block::Error::TooLarge { .. } | block::Error::Exhausted { .. } => {
                    let (from, fix) = source(dir, known.from.pool, Resource::Memory);
                    let message = format!(
                        "the pool budget {budget}, {from}, gives shard-{core} too \
                         little: {error}"
                    );
                    (message, fix)
                }
            };
            Failure {
                code: MEMORY,
                message,
                fix,
            }
        }
        error => failure(dir, error),
    }
}

/// The failure of `error`, an error that no budget causes, in the data directory
/// `dir`.
fn failure(dir: &Path, error: &node::Error) -> Failure {
    let data = dir.display();
    let (code, message, fix) = match error {
        node::Error::Directory(env::files::Error::Busy { .. }) => (
            BUSY,
            format!("another node runs in {data}"),
            "Stop that node, or give another data directory with `--data`".to_owned(),
        ),
        node::Error::Directory(error @ env::files::Error::Io { code, .. })
        | node::Error::Buffer {
            error: buffer::Error::Files(error @ env::files::Error::Io { code, .. }),
            ..
        } if refused(*code) => return unwritable(dir, error),
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
        node::Error::Budget => (
            BUDGET,
            format!(
                "the file `budget` in the data directory {data} does not hold budgets \
                 that a node wrote"
            ),
            "Remove it, and the next start computes the budgets again from the free \
             memory and disk"
                .to_owned(),
        ),
        error => return failed(error),
    };
    Failure { code, message, fix }
}

/// Where a budget of `resource` that gives the node too little comes from, `from`,
/// as text, and its fix. `dir` is the data directory.
fn source(dir: &Path, from: Origin, resource: Resource) -> (String, String) {
    let data = dir.display();
    match (from, resource) {
        (Origin::Kept, _) => (
            format!("which {data} keeps from its first start"),
            format!(
                "Remove the file `budget` in {data}, and the next start computes the \
                 budgets again from the free memory and disk"
            ),
        ),
        (Origin::Most, _) => (
            "the most that a first start gives".to_owned(),
            "Start the node on fewer cores: on Linux, give it a smaller CPU affinity \
             set, such as with `taskset`"
                .to_owned(),
        ),
        (Origin::Quarter, Resource::Memory) => (
            "a quarter of the available memory".to_owned(),
            "Free memory on this host".to_owned(),
        ),
        (Origin::Quarter, Resource::Disk) => (
            format!("a quarter of the free disk of {data}"),
            "Free space on that disk, or give a data directory on another disk with \
             `--data`"
                .to_owned(),
        ),
    }
}

/// Whether the OS error `code` says that this user cannot write the file.
fn refused(code: i32) -> bool {
    matches!(
        io::Error::from_raw_os_error(code).kind(),
        io::ErrorKind::PermissionDenied | io::ErrorKind::ReadOnlyFilesystem
    )
}

/// The failure of a data directory `dir` that this user cannot write.
fn unwritable(dir: &Path, error: impl std::fmt::Display) -> Failure {
    let dir = dir.display();
    Failure {
        code: DATA,
        message: format!("cannot write the data directory {dir}: {error}"),
        fix: format!(
            "Let this user make and write {dir} and each file in it, or give another \
             directory with `--data`"
        ),
    }
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

/// These tests call the functions of `main`, since a process test cannot set what
/// they need: a host with a known free memory and free disk (`quarter`, and the texts
/// of a default budget), or a buffer error that only a failing host makes (`Refused`,
/// `Exhausted`, another buffer error), or a file system that refuses a write with
/// `EPERM` or `EROFS` (`failure`). So they are the only kill of the mutants of
/// `quarter` and of the arms of `stopped` and `source` for a default budget. The
/// process tests in `tests/it/start.rs` cover each kept budget.
#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    /// The data directory of `foundation start` with no `--data`.
    fn dir() -> &'static Path {
        Path::new("foundation-data")
    }

    /// What a start knows of the budgets `budget`, which each come `from` there.
    fn known(budget: Budget, from: Origin) -> Known {
        Known {
            name: "edge".parse().expect("a name"),
            budget,
            from: Origins {
                pool: from,
                disk: from,
            },
        }
    }

    /// The failure of a disk budget `disk`, which comes `from` there, that holds no
    /// ring on each of `cores` shards, which each need 4120KiB.
    fn disk(disk: Size, cores: usize, from: Origin) -> Failure {
        let count = u64::try_from(cores).expect("a core count fits a u64");
        let error = node::Error::Disk {
            disk,
            cores,
            min: Size::from_bytes(4_218_880 * count),
        };
        let budget = Budget {
            pool: Size::GIBIBYTE,
            disk,
        };
        stopped(dir(), &error, &known(budget, from))
    }

    /// What the pool of a test gives for a block that it cannot hold.
    const TOO_LARGE: &str = "the pool has no block: block of 52186 bytes is above the \
                             largest block of 28672 bytes";

    /// The fix of a budget at its most.
    const FEWER: &str = "Start the node on fewer cores: on Linux, give it a smaller \
                         CPU affinity set, such as with `taskset`";

    /// The failure of a pool budget `pool`, which comes `from` there, that gives
    /// shard 3 `error`.
    fn pool_failure(pool: Size, from: Origin, error: block::Error) -> Failure {
        let error = node::Error::Buffer {
            core: 3,
            error: buffer::Error::Pool(error),
        };
        let budget = Budget {
            pool,
            disk: DISK_MOST,
        };
        stopped(dir(), &error, &known(budget, from))
    }

    /// The failure of a pool budget `pool`, which comes `from` there, too small for a
    /// block of shard 3.
    fn pool(pool: Size, from: Origin) -> Failure {
        let error = block::Error::TooLarge {
            requested: 52_186,
            largest: 28_672,
        };
        pool_failure(pool, from, error)
    }

    #[test]
    fn a_first_start_gives_a_quarter_of_what_is_free_up_to_the_most() {
        let from_quarter = |bytes| (Size::from_bytes(bytes), Origin::Quarter);
        assert_eq!(quarter(4 << 20, POOL_MOST), from_quarter(1 << 20));
        assert_eq!(quarter(7, POOL_MOST), from_quarter(1));
        assert_eq!(
            quarter((4 << 30) - 4, POOL_MOST),
            from_quarter((1 << 30) - 1)
        );
        assert_eq!(quarter(4 << 30, POOL_MOST), (Size::GIBIBYTE, Origin::Most));
        assert_eq!(
            quarter((4 << 30) + 4, POOL_MOST),
            (Size::GIBIBYTE, Origin::Most)
        );
        assert_eq!(quarter(64 << 30, DISK_MOST), (DISK_MOST, Origin::Most));
        assert_eq!(DISK_MOST, Size::from_bytes(8_589_934_592));
    }

    /// Through `first`, as a process test cannot set the memory and disk of the host.
    #[test]
    fn a_first_start_sizes_the_pool_from_memory_and_the_disk_budget_from_disk() {
        let budget = Budget {
            pool: Size::from_bytes(512 << 20),
            disk: Size::from_bytes(4 << 30),
        };
        let from = Origins {
            pool: Origin::Quarter,
            disk: Origin::Quarter,
        };
        assert_eq!(first(Size::from_bytes(2 << 30), 16 << 30), (budget, from));
    }

    #[test]
    fn a_kept_disk_budget_that_holds_no_ring_tells_the_user_to_remove_it() {
        assert_eq!(
            disk(Size::from_bytes(1 << 20), 16, Origin::Kept),
            Failure {
                code: DISK,
                message: "the disk budget 1MiB, which foundation-data keeps from its \
                          first start, holds no ring on each of 16 shards; it needs at \
                          least 65920KiB"
                    .to_owned(),
                fix: "Remove the file `budget` in foundation-data, and the next start \
                      computes the budgets again from the free memory and disk"
                    .to_owned(),
            }
        );
    }

    #[test]
    fn a_disk_budget_at_its_most_tells_the_user_to_use_fewer_cores() {
        assert_eq!(
            disk(DISK_MOST, 2048, Origin::Most),
            Failure {
                code: DISK,
                message: "the disk budget 8GiB, the most that a first start gives, \
                          holds no ring on each of 2048 shards; it needs at least \
                          8240MiB"
                    .to_owned(),
                fix: FEWER.to_owned(),
            }
        );
    }

    #[test]
    fn a_disk_budget_from_the_free_disk_tells_the_user_to_free_it() {
        assert_eq!(
            disk(Size::from_bytes(1 << 20), 16, Origin::Quarter),
            Failure {
                code: DISK,
                message: "the disk budget 1MiB, a quarter of the free disk of \
                          foundation-data, holds no ring on each of 16 shards; it \
                          needs at least 65920KiB"
                    .to_owned(),
                fix: "Free space on that disk, or give a data directory on another \
                      disk with `--data`"
                    .to_owned(),
            }
        );
    }

    #[test]
    fn a_kept_pool_budget_that_gives_a_shard_too_little_tells_the_user_to_remove_it() {
        assert_eq!(
            pool(Size::MEBIBYTE, Origin::Kept),
            Failure {
                code: MEMORY,
                message: format!(
                    "the pool budget 1MiB, which foundation-data keeps from its \
                     first start, gives shard-3 too little: {TOO_LARGE}"
                ),
                fix: "Remove the file `budget` in foundation-data, and the next start \
                      computes the budgets again from the free memory and disk"
                    .to_owned(),
            }
        );
    }

    #[test]
    fn a_pool_budget_at_its_most_tells_the_user_to_use_fewer_cores() {
        let full = block::Error::Exhausted {
            requested: 65_536,
            available: 4096,
        };
        assert_eq!(
            pool_failure(POOL_MOST, Origin::Most, full),
            Failure {
                code: MEMORY,
                message: "the pool budget 1GiB, the most that a first start gives, \
                          gives shard-3 too little: the pool has no block: pool is \
                          full: asked for 65536 bytes, 4096 bytes free"
                    .to_owned(),
                fix: FEWER.to_owned(),
            }
        );
    }

    #[test]
    fn a_pool_budget_from_the_available_memory_tells_the_user_to_free_it() {
        assert_eq!(
            pool(Size::MEBIBYTE, Origin::Quarter),
            Failure {
                code: MEMORY,
                message: format!(
                    "the pool budget 1MiB, a quarter of the available memory, \
                     gives shard-3 too little: {TOO_LARGE}"
                ),
                fix: "Free memory on this host".to_owned(),
            }
        );
    }

    #[test]
    fn a_kept_pool_budget_past_the_address_space_tells_the_user_to_remove_it() {
        let pool = Size::from_bytes(u64::MAX);
        let error = node::Error::Pool { pool, cores: 16 };
        let budget = Budget {
            pool,
            disk: Size::GIBIBYTE,
        };
        assert_eq!(
            stopped(dir(), &error, &known(budget, Origin::Kept)),
            Failure {
                code: MEMORY,
                message:
                    "the pool budget 18446744073709551615B, which foundation-data \
                          keeps from its first start, gives one of 16 shards a pool \
                          that needs more address space than this host has"
                        .to_owned(),
                fix: "Remove the file `budget` in foundation-data, and the next start \
                      computes the budgets again from the free memory and disk"
                    .to_owned(),
            }
        );
    }

    #[test]
    fn a_budget_file_that_no_node_wrote_tells_the_user_to_remove_it() {
        assert_eq!(
            failure(dir(), &node::Error::Budget),
            Failure {
                code: BUDGET,
                message:
                    "the file `budget` in the data directory foundation-data does \
                          not hold budgets that a node wrote"
                        .to_owned(),
                fix: "Remove it, and the next start computes the budgets again from \
                      the free memory and disk"
                    .to_owned(),
            }
        );
    }

    #[test]
    fn a_full_pool_is_a_pool_budget_that_gives_too_little() {
        let full = block::Error::Exhausted {
            requested: 64,
            available: 0,
        };
        assert_eq!(
            pool_failure(Size::MEBIBYTE, Origin::Quarter, full),
            Failure {
                code: MEMORY,
                message: "the pool budget 1MiB, a quarter of the available memory, \
                          gives shard-3 too little: the pool has no block: pool is \
                          full: asked for 64 bytes, 0 bytes free"
                    .to_owned(),
                fix: "Free memory on this host".to_owned(),
            }
        );
    }

    /// The failure of memory that the system refused shard 3, on a pool budget that
    /// comes `from` there.
    fn refused(from: Origin) -> Failure {
        pool_failure(POOL_MOST, from, block::Error::Refused { requested: 64 })
    }

    #[test]
    fn memory_that_the_system_refused_tells_the_user_to_free_memory() {
        let refused_memory = Failure {
            code: MEMORY,
            message: "the system refused memory for shard-3 that its part of the pool \
                      budget 1GiB has room for: the pool has no block: the system \
                      refused memory for a block of 64 bytes"
                .to_owned(),
            fix: "Free memory on this host".to_owned(),
        };
        for from in [Origin::Kept, Origin::Most, Origin::Quarter] {
            assert_eq!(refused(from), refused_memory, "{from:?}");
        }
    }

    #[test]
    fn another_buffer_error_is_a_failure() {
        let error = node::Error::Buffer {
            core: 3,
            error: buffer::Error::Version(9),
        };
        let budget = Budget {
            pool: POOL_MOST,
            disk: DISK_MOST,
        };
        let known = known(budget, Origin::Quarter);
        assert_eq!(stopped(dir(), &error, &known), failed(&error));
    }

    /// A failed open of `path` with the OS error `code`.
    fn io(path: &str, code: i32) -> env::files::Error {
        env::files::Error::Io {
            path: PathBuf::from(path),
            operation: env::files::Operation::Open,
            code,
        }
    }

    /// The failure of the data directory `foundation-data` for `error`.
    fn of(error: &node::Error) -> Failure {
        failure(dir(), error)
    }

    #[test]
    fn each_refusal_of_a_write_is_node_data() {
        const EACCES: i32 = 13;
        const EPERM: i32 = 1;
        const EROFS: i32 = 30;
        for code in [EACCES, EPERM, EROFS] {
            let ring = node::Error::Buffer {
                core: 2,
                error: buffer::Error::Files(io("shard-2/ring", code)),
            };
            let reason = io::Error::from_raw_os_error(code);
            for (error, file) in [
                (node::Error::Directory(io("node.key", code)), "node.key"),
                (ring, "shard-2/ring"),
            ] {
                assert_eq!(
                    of(&error),
                    Failure {
                        code: DATA,
                        message: format!(
                            "cannot write the data directory foundation-data: open of \
                             {file} failed with OS error {code}"
                        ),
                        fix: "Let this user make and write foundation-data and each \
                              file in it, or give another directory with `--data`"
                            .to_owned(),
                    },
                    "{reason}"
                );
            }
        }
    }

    #[test]
    fn a_file_error_that_is_no_refusal_is_node_failed() {
        const ENOSPC: i32 = 28;
        let failed = |message: &str| Failure {
            code: FAILED,
            message: message.to_owned(),
            fix: "Fix the cause that the message states, then start the node again"
                .to_owned(),
        };
        let lost = env::files::Error::NotFound {
            path: PathBuf::from("lock"),
        };
        assert_eq!(
            of(&node::Error::Directory(lost)),
            failed("cannot use the data directory: path lock is not there")
        );
        assert_eq!(
            of(&node::Error::Directory(io("node.key", ENOSPC))),
            failed(
                "cannot use the data directory: open of node.key failed with OS error 28"
            )
        );
        let ring = node::Error::Buffer {
            core: 2,
            error: buffer::Error::Files(io("shard-2/ring", ENOSPC)),
        };
        assert_eq!(
            of(&ring),
            failed(
                "cannot open the buffer of shard-2: a file call failed: open of \
                 shard-2/ring failed with OS error 28"
            )
        );
    }
}
