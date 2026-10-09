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
    let Read { name, budget, kept } = read(start, &threads)?;
    let (called, call) = mpsc::channel();
    let mut line = Vec::new();
    start.running(&name, &mut line);
    // Its own thread, which lives until the process ends, so a standard output that
    // nobody reads blocks neither shard 0 nor the stop.
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
            .map_err(|error| data(&start.data, error))?;
        disks.push(disk);
        handles.push(handle);
    }
    let mut disks = disks.into_iter();
    let node = node::Node::start(node::Config {
        shards,
        clock: os::clock(),
        wall,
        budget,
        memory: Box::new(os::memory::Memory::new),
        files: Box::new(move || {
            let disk = disks.next().expect("invariant: one disk for each shard");
            Box::new(move || env::files::Files::new(disk))
        }),
        entropy: os::entropy(),
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
    let joined = node
        .join()
        .map_err(|error| failure(&start.data, &error, budget, kept));
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

/// What a start reads before the node starts.
struct Read {
    name: Name,
    budget: Budget,
    /// The data directory keeps `budget` from its first start.
    kept: bool,
}

/// The name of the node of `start`, and its budgets: the ones that the data
/// directory keeps, else a quarter of the available memory and of the free disk,
/// each up to its most. Reads the data directory on a thread of `threads`.
#[expect(
    clippy::unwrap_in_result,
    reason = "a thread that ends with no panic has sent what it read"
)]
fn read(start: &Start, threads: &Threads) -> Result<Read, Failure> {
    let (disk, handle) = os::files(&start.data, threads, "files-read")
        .map_err(|error| data(&start.data, error))?;
    let (send, receive) = mpsc::channel();
    let dir = start.data.clone();
    let given = start.name.clone();
    let read = threads.start("read", move || async move {
        let files = env::files::Files::new(disk);
        let read = async {
            let name = node::name(&files, given)
                .await
                .map_err(|e| unread(&dir, &e))?;
            let kept = node::budget(&files).await.map_err(|e| unread(&dir, &e))?;
            let budget = match kept {
                Some(budget) => budget,
                None => Budget {
                    pool: quarter(
                        os::memory::available().map_err(failed)?.bytes(),
                        POOL_MOST,
                    ),
                    disk: quarter(files.free().await.map_err(failed)?, DISK_MOST),
                },
            };
            Ok(Read {
                name,
                budget,
                kept: kept.is_some(),
            })
        };
        send.send(read.await)
            .expect("invariant: main waits for what the thread reads");
    });
    read.map_err(failed)?.join().map_err(failed)?;
    handle.join().map_err(failed)?;
    receive
        .try_recv()
        .expect("invariant: the thread sent what it read")
}

/// A quarter of `free` bytes, up to `most`.
fn quarter(free: u64, most: Size) -> Size {
    Size::from_bytes(free / 4).min(most)
}

/// The failure of a read before the start, which gave `error`.
fn unread(dir: &Path, error: &node::Error) -> Failure {
    let none = Budget {
        pool: Size::ZERO,
        disk: Size::ZERO,
    };
    failure(dir, error, none, false)
}

/// The failure of a data directory that `os` could not open or make.
fn data(dir: &Path, error: os::Error) -> Failure {
    match error {
        os::Error::Dir(error) => Failure {
            code: DATA,
            message: format!(
                "cannot open or make the data directory {}: {error}",
                dir.display()
            ),
            fix: WRITABLE.to_owned(),
        },
        error => failed(error),
    }
}

/// The failure of a node that stopped with `error`, on `budget`, which the data
/// directory keeps when `kept`.
fn failure(dir: &Path, error: &node::Error, budget: Budget, kept: bool) -> Failure {
    let data = dir.display();
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
        node::Error::Disk { disk, cores, min } => {
            let free = (
                format!("a quarter of the free disk of {data}"),
                "Free space on that disk, or give a data directory on another disk \
                 with `--data`",
            );
            let (from, fix) = source(dir, kept, *disk == DISK_MOST, free);
            let message = format!(
                "the disk budget {disk}, {from}, holds no ring on each of {cores} \
                 shards; it needs at least {min}"
            );
            (DISK, message, fix)
        }
        node::Error::Buffer {
            core,
            error: error @ buffer::Error::Pool(pool),
        } => return memory(dir, *core, error, pool, budget, kept),
        error => return failed(error),
    };
    Failure { code, message, fix }
}

/// The failure of shard `core`, whose buffer gave `error`, a pool error `pool`, on
/// `budget`, which the data directory keeps when `kept`.
fn memory(
    dir: &Path,
    core: usize,
    error: &buffer::Error,
    pool: &block::Error,
    budget: Budget,
    kept: bool,
) -> Failure {
    let budget = budget.pool;
    let (message, fix) = match pool {
        block::Error::Refused { .. } => (
            format!(
                "the system refused memory that the pool budget {budget} of \
                 shard-{core} has room for: {error}"
            ),
            "Free memory on this host".to_owned(),
        ),
        block::Error::TooLarge { .. } | block::Error::Exhausted { .. } => {
            let free = (
                "a quarter of the available memory".to_owned(),
                "Free memory on this host",
            );
            let (from, fix) = source(dir, kept, budget == POOL_MOST, free);
            let message = format!(
                "the pool budget {budget}, {from}, gives shard-{core} too little: \
                 {error}"
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

/// Where a budget that gives the node too little came from, and its fix: the data
/// directory `dir` when it `kept` the budget, else the most that a first start
/// gives when the budget is at its `most`, else `free`, the source and fix of a
/// quarter of the free resource.
fn source(
    dir: &Path,
    kept: bool,
    most: bool,
    free: (String, &str),
) -> (String, String) {
    let data = dir.display();
    if kept {
        let fix = format!(
            "Remove the file `budget` in {data}, and the next start computes the \
             budgets again from the free memory and disk"
        );
        (format!("which {data} keeps from its first start"), fix)
    } else if most {
        let fix = "Start the node on fewer cores: on Linux, give it a smaller CPU \
                   affinity set, such as with `taskset`";
        (
            "the most that a first start gives".to_owned(),
            fix.to_owned(),
        )
    } else {
        (free.0, free.1.to_owned())
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The data directory of `foundation start` with no `--data`.
    fn dir() -> &'static Path {
        Path::new("foundation-data")
    }

    /// The failure of a disk budget `disk` that holds no ring on each of `cores`
    /// shards, which each need 4120KiB.
    fn disk(disk: Size, cores: usize, kept: bool) -> Failure {
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
        failure(dir(), &error, budget, kept)
    }

    /// What the pool of a test gives for a block that it cannot hold.
    const TOO_LARGE: &str = "the pool has no block: block of 52186 bytes is above the \
                             largest block of 28672 bytes";

    /// The fix of a budget at its most.
    const FEWER: &str = "Start the node on fewer cores: on Linux, give it a smaller \
                         CPU affinity set, such as with `taskset`";

    /// The failure of a pool budget `pool` that gives shard 3 `error`.
    fn pool_failure(pool: Size, kept: bool, error: block::Error) -> Failure {
        let error = node::Error::Buffer {
            core: 3,
            error: buffer::Error::Pool(error),
        };
        let budget = Budget {
            pool,
            disk: DISK_MOST,
        };
        failure(dir(), &error, budget, kept)
    }

    /// The failure of a pool budget `pool` too small for a block of shard 3.
    fn pool(pool: Size, kept: bool) -> Failure {
        let error = block::Error::TooLarge {
            requested: 52_186,
            largest: 28_672,
        };
        pool_failure(pool, kept, error)
    }

    #[test]
    fn a_first_start_gives_a_quarter_of_what_is_free_up_to_the_most() {
        assert_eq!(quarter(4 << 20, POOL_MOST), Size::MEBIBYTE);
        assert_eq!(quarter(7, POOL_MOST), Size::from_bytes(1));
        assert_eq!(quarter(4 << 30, POOL_MOST), Size::GIBIBYTE);
        assert_eq!(quarter((4 << 30) + 4, POOL_MOST), Size::GIBIBYTE);
        assert_eq!(quarter(64 << 30, DISK_MOST), DISK_MOST);
        assert_eq!(DISK_MOST, Size::from_bytes(8_589_934_592));
    }

    #[test]
    fn a_kept_disk_budget_that_holds_no_ring_tells_the_user_to_remove_it() {
        assert_eq!(
            disk(Size::from_bytes(1 << 20), 16, true),
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
            disk(DISK_MOST, 2048, false),
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
            disk(Size::from_bytes(1 << 20), 16, false),
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
            pool(Size::MEBIBYTE, true),
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
            pool_failure(POOL_MOST, false, full),
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
            pool(Size::MEBIBYTE, false),
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
    fn a_budget_file_that_no_node_wrote_tells_the_user_to_remove_it() {
        let budget = Budget {
            pool: Size::ZERO,
            disk: Size::ZERO,
        };
        assert_eq!(
            failure(dir(), &node::Error::Budget, budget, false),
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
            pool_failure(Size::MEBIBYTE, false, full),
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

    /// The failure of memory that the system refused shard 3, on a pool budget
    /// that the data directory keeps when `kept`.
    fn refused(kept: bool) -> Failure {
        pool_failure(POOL_MOST, kept, block::Error::Refused { requested: 64 })
    }

    #[test]
    fn memory_that_the_system_refused_tells_the_user_to_free_memory() {
        let refused_memory = Failure {
            code: MEMORY,
            message: "the system refused memory that the pool budget 1GiB of shard-3 \
                      has room for: the pool has no block: the system refused memory \
                      for a block of 64 bytes"
                .to_owned(),
            fix: "Free memory on this host".to_owned(),
        };
        assert_eq!(refused(false), refused_memory);
        assert_eq!(refused(true), refused_memory, "also for a kept budget");
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
        assert_eq!(failure(dir(), &error, budget, false), failed(&error));
    }
}
