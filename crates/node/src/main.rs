//! The `foundation` binary. It runs the command line, and `foundation start` runs a
//! node until SIGINT or SIGTERM.

use std::io;
use std::net::{Ipv4Addr, SocketAddr};
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
    let Read { name, budget, kept } = read(start, &threads)?;
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
        let start = start.clone();
        node.spawn(move |_| {
            start.running(&name, io::stdout().lock());
            async {}
        });
    } else {
        node.stop();
    }
    let joined = node.join();
    for handle in handles {
        handle.join().map_err(failed)?;
    }
    // The thread lives until the process ends: a node that fails gets no signal.
    drop(stop.map_err(failed)?);
    joined.map_err(|error| failure(start, &error, budget, kept))
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
        .map_err(|error| data(start, error))?;
    let (send, receive) = mpsc::channel();
    let own = start.clone();
    let read = threads.start("read", move || async move {
        let files = env::files::Files::new(disk);
        let read = async {
            let given = own.name.clone();
            let name = node::name(&files, given)
                .await
                .map_err(|e| unread(&own, &e))?;
            let kept = node::budget(&files).await.map_err(|e| unread(&own, &e))?;
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
fn unread(start: &Start, error: &node::Error) -> Failure {
    let none = Budget {
        pool: Size::ZERO,
        disk: Size::ZERO,
    };
    failure(start, error, none, false)
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
            fix: "Give with `--data` a directory that this user can make and write"
                .to_owned(),
        },
        error => failed(error),
    }
}

/// The failure of a node that stopped with `error`, on `budget`, which the data
/// directory keeps when `kept`.
fn failure(start: &Start, error: &node::Error, budget: Budget, kept: bool) -> Failure {
    let data = start.data.display();
    let (code, message, fix) = match error {
        node::Error::Directory(env::files::Error::Busy { .. }) => (
            BUSY,
            format!("another node runs in {data}"),
            "Stop that node, or give another data directory with `--data`".to_owned(),
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
                "the file `budget` in the data directory {data} is not a node's budgets"
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
            let (from, fix) = source(start, kept, *disk == DISK_MOST, free);
            let message = format!(
                "the disk budget {disk}, {from}, holds no ring on each of {cores} \
                 shards; it needs at least {min}"
            );
            (DISK, message, fix)
        }
        node::Error::Buffer {
            core,
            error: error @ buffer::Error::Pool(block::Error::TooLarge { .. }),
        } => {
            let pool = budget.pool;
            let free = (
                "a quarter of the available memory".to_owned(),
                "Free memory on this host",
            );
            let (from, fix) = source(start, kept, pool == POOL_MOST, free);
            let message = format!(
                "the pool budget {pool}, {from}, gives shard-{core} too little: {error}"
            );
            (MEMORY, message, fix)
        }
        error => return failed(error),
    };
    Failure { code, message, fix }
}

/// Where a budget that gives the node too little came from, and its fix: the data
/// directory of `start` when it `kept` the budget, else the most that a first start
/// gives when the budget is at its `most`, else `free`, the source and fix of a
/// quarter of the free resource.
fn source(
    start: &Start,
    kept: bool,
    most: bool,
    free: (String, &str),
) -> (String, String) {
    let data = start.data.display();
    if kept {
        let fix = format!(
            "Remove the file `budget` in {data}, and the next start computes the \
             budgets again from the free memory and disk"
        );
        (format!("which {data} keeps from its first start"), fix)
    } else if most {
        let fix = "Start the node on a host with fewer cores";
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

    /// The start of `foundation start`, as the command line gives it.
    fn start() -> Start {
        let args = ["foundation", "start"].map(std::ffi::OsString::from);
        match ops::cli(args, io::empty(), io::sink(), io::sink()) {
            Run::Start(start) => start,
            Run::Exit(status) => panic!("exit {status}"),
        }
    }

    /// The failure of a disk budget `disk` that holds no ring on each of 16 shards.
    fn disk(disk: Size, kept: bool) -> Failure {
        let error = node::Error::Disk {
            disk,
            cores: 16,
            min: Size::from_bytes(67_502_080),
        };
        let budget = Budget {
            pool: Size::GIBIBYTE,
            disk,
        };
        failure(&start(), &error, budget, kept)
    }

    /// What the pool of a test gives for a block that it cannot hold.
    const TOO_LARGE: &str = "the pool has no block: block of 52186 bytes is above the \
                             largest block of 28672 bytes";

    /// The failure of a pool budget `pool` too small for shard 3.
    fn pool(pool: Size, kept: bool) -> Failure {
        let error = node::Error::Buffer {
            core: 3,
            error: buffer::Error::Pool(block::Error::TooLarge {
                requested: 52_186,
                largest: 28_672,
            }),
        };
        let budget = Budget {
            pool,
            disk: DISK_MOST,
        };
        failure(&start(), &error, budget, kept)
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
            disk(Size::from_bytes(1 << 20), true),
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
            disk(DISK_MOST, false),
            Failure {
                code: DISK,
                message: "the disk budget 8GiB, the most that a first start gives, \
                          holds no ring on each of 16 shards; it needs at least \
                          65920KiB"
                    .to_owned(),
                fix: "Start the node on a host with fewer cores".to_owned(),
            }
        );
    }

    #[test]
    fn a_disk_budget_from_the_free_disk_tells_the_user_to_free_it() {
        assert_eq!(
            disk(Size::from_bytes(1 << 20), false),
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
        assert_eq!(
            pool(POOL_MOST, false),
            Failure {
                code: MEMORY,
                message: format!(
                    "the pool budget 1GiB, the most that a first start gives, \
                     gives shard-3 too little: {TOO_LARGE}"
                ),
                fix: "Start the node on a host with fewer cores".to_owned(),
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
            failure(&start(), &node::Error::Budget, budget, false),
            Failure {
                code: BUDGET,
                message: "the file `budget` in the data directory foundation-data is \
                          not a node's budgets"
                    .to_owned(),
                fix: "Remove it, and the next start computes the budgets again from \
                      the free memory and disk"
                    .to_owned(),
            }
        );
    }

    #[test]
    fn memory_that_the_system_refused_is_not_a_budget_that_gives_too_little() {
        let error = node::Error::Buffer {
            core: 3,
            error: buffer::Error::Pool(block::Error::Refused { requested: 64 }),
        };
        let budget = Budget {
            pool: Size::MEBIBYTE,
            disk: DISK_MOST,
        };
        assert_eq!(failure(&start(), &error, budget, false), failed(&error));
    }
}
