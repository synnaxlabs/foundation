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
    let known = known(start, &threads)?;
    let (called, call) = mpsc::channel();
    let mut line = Vec::new();
    start.running(&known.name, &mut line);
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
            let memory = os::memory::available().map_err(failed)?.bytes();
            let (pool, pool_from) = quarter(memory, POOL_MOST);
            let free = files.free().await.map_err(failed)?;
            let (disk, disk_from) = quarter(free, DISK_MOST);
            Ok(Known {
                name,
                budget: Budget { pool, disk },
                from: Origins {
                    pool: pool_from,
                    disk: disk_from,
                },
            })
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

/// A quarter of `free` bytes up to `most`, and whether the quarter or the most won.
fn quarter(free: u64, most: Size) -> (Size, Origin) {
    let quarter = Size::from_bytes(free / 4);
    if quarter < most {
        (quarter, Origin::Quarter)
    } else {
        (most, Origin::Most)
    }
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
        node::Error::Buffer {
            core,
            error: error @ buffer::Error::Pool(pool),
        } => {
            let budget = known.budget.pool;
            let (message, fix) = match pool {
                block::Error::Refused { .. } => (
                    format!(
                        "the system refused memory that the pool budget {budget} of \
                         shard-{core} has room for: {error}"
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
            message: "the system refused memory that the pool budget 1GiB of shard-3 \
                      has room for: the pool has no block: the system refused memory \
                      for a block of 64 bytes"
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
}
