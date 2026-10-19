//! The `foundation` binary. It runs the command line, and `foundation start` runs a
//! node until a SIGINT or SIGTERM that reaches it.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::io::{self, Write};
use std::panic;
use std::path::Path;
use std::process::ExitCode;
use std::sync::mpsc;
use std::time::Duration;

use document::diagnostic::Code;
use env::shards::Shards;
use env::tasks::Tasks;
use env::threads::Threads;
use node::Budget;
use ops::{Failure, Run, Start};
use types::byte::Size;
use types::name::Name;

/// The most pool budget that a first start gives.
const POOL_MOST: Size = Size::GIBIBYTE;
/// The most disk budget that a first start gives.
const DISK_MOST: Size = Size::from_bytes(8 << 30);
/// The longest the end of the process waits for the log.
const FLUSH: Duration = Duration::from_secs(1);
/// The longest a panic waits for the log.
const PANIC: Duration = Duration::from_secs(1);

const BUSY: Code = Code::new("node.busy");
const DATA: Code = Code::new("node.data");
const UNNAMED: Code = Code::new("node.unnamed");
const RENAMED: Code = Code::new("node.renamed");
const NAME: Code = Code::new("node.name");
const RESERVED: Code = Code::new("node.reserved");
const LONG: Code = Code::new("node.long");
const DISK: Code = Code::new("node.disk");
const MEMORY: Code = Code::new("node.memory");
const BUDGET: Code = Code::new("node.budget");
const PORT: Code = Code::new("node.port");
const FAILED: Code = Code::new("node.failed");
const NOT_RUNNING: Code = Code::new("node.not-running");
const UNREADABLE: Code = Code::new("node.unreadable");
const ADMIN_KEY: Code = Code::new("node.admin-key");
const VERSION: Code = Code::new("node.version");
const SOCKET: Code = Code::new("node.socket");
const UNANSWERED: Code = Code::new("node.unanswered");
const SESSION: Code = Code::new("node.session");
const REFUSED: Code = Code::new("node.refused");
const NO_REPLY: Code = Code::new("node.no-reply");

/// Runs the command line on a shard that ends before a node starts, then the node of
/// `foundation start`. A failure to start the shard, or a failure of the node, goes to
/// standard error through [`ops::fail`].
fn main() -> ExitCode {
    #[expect(clippy::disallowed_methods, reason = "node reads its arguments")]
    let args: Vec<OsString> = std::env::args_os().collect();
    #[expect(
        clippy::disallowed_methods,
        reason = "env() reads the node's environment"
    )]
    let vars = std::env::vars_os().collect();
    let status = match run(&args, vars) {
        Ok(status) => status,
        Err(failure) => ops::fail(&args, &failure, io::stderr().lock()),
    };
    ExitCode::from(status)
}

/// The exit status of the command line `args`, and of the node that it starts.
fn run(args: &[OsString], vars: BTreeMap<OsString, OsString>) -> Result<u8, Failure> {
    // No thread runs when `node` calls `os::interrupt`: `shards` starts none, and
    // `command_line` joins each thread that it starts.
    let shards = os::shards().map_err(failed)?;
    match command_line(&shards, args.to_vec())? {
        Run::Exit(status) => Ok(status),
        Run::Start(start) => node(&start, shards, vars).map(|()| 0),
    }
}

/// Runs the command line `args` on a shard of its own. The shard and each I/O thread
/// of a reach or of standard input end before this returns, so a node that
/// `foundation start` runs gets every shard, and its first thread takes the signals.
///
/// # Panics
///
/// When the shard panics.
#[expect(
    clippy::unwrap_in_result,
    reason = "a panic of the shard goes on, and a shard that ends has sent its run"
)]
fn command_line(shards: &Shards, args: Vec<OsString>) -> Result<Run, Failure> {
    let (ran, run) = mpsc::channel();
    let (started, threads) = mpsc::channel();
    let (opened, reaches) = mpsc::channel();
    let config = env::shards::Config {
        name: "cli".into(),
        core: None,
    };
    let shard = shards.start(config, move |tasks| async move {
        // Only `foundation mcp` reads, so the thread starts at the first read.
        let mut stdin = None;
        let input = async || {
            let input = match &stdin {
                Some(input) => input,
                None => stdin.insert(input(&started)?),
            };
            input.read(ops::line).await
        };
        let run = ops::cli(
            args,
            input,
            io::stdout().lock(),
            io::stderr().lock(),
            &os::paths(),
            &node::front_ends(),
            &node::kinds(),
            async |dir: &Path| reach(dir, &tasks, &opened).await.map(send),
        )
        .await;
        ran.send(run).expect("invariant: `main` keeps the receiver");
    });
    let shard = shard.map_err(failed)?;
    // Each reach joins here as it comes, so a server that reaches many times holds no
    // ended thread. The stdin thread goes to `threads`, since it ends only with the
    // shard.
    let joined = reaches
        .iter()
        .map(env::thread::Handle::join)
        .fold(Ok(()), Result::and);
    shard
        .join()
        .expect("invariant: the shard of the command line does not panic");
    joined.map_err(failed)?;
    threads
        .try_iter()
        .try_for_each(env::thread::Handle::join)
        .map_err(failed)?;
    Ok(run
        .try_recv()
        .expect("invariant: the shard sends its run before it ends"))
}

/// The standard input of the process, read on an I/O thread whose handle goes to
/// `started`. `cli` reads only after each read before it ends, so the thread ends
/// once the input drops.
#[expect(
    clippy::unwrap_in_result,
    reason = "`main` keeps the receiver until the shard ends"
)]
fn input(started: &mpsc::Sender<env::thread::Handle>) -> io::Result<os::Input> {
    let threads = os::threads().map_err(io::Error::other)?;
    let (input, handle) = os::input(&threads, "stdin").map_err(io::Error::other)?;
    started
        .send(handle)
        .expect("invariant: `main` keeps the receiver");
    Ok(input)
}

/// The client of the node that runs on the data directory `dir`. It sends the handle
/// of the I/O thread that reads `dir` to `opened`, for the main thread to join.
async fn reach(
    dir: &Path,
    tasks: &Tasks,
    opened: &mpsc::Sender<env::thread::Handle>,
) -> Result<program::Session, Failure> {
    let threads = os::threads().map_err(failed)?;
    let (disk, handle) =
        os::stored(dir, &threads, "files-reach").map_err(|error| stored(dir, error))?;
    opened
        .send(handle)
        .expect("invariant: `main` keeps the receiver");
    let reached = node::client::reach(node::client::Config {
        files: env::files::Files::new(disk),
        net: net(),
        clock: os::clock(),
        entropy: os::entropy(),
        tasks: tasks.clone(),
    })
    .await;
    reached.map_err(|error| unreached(dir, error))
}

/// What sends a request through `client` and gives the reply.
fn send(client: program::Session) -> impl AsyncFn(&[u8]) -> Result<Vec<u8>, Failure> {
    async move |body: &[u8]| {
        client.request(body).await.map_err(|error| no_reply(&error))
    }
}

/// The failure of a reach whose `os::stored` of the data directory `dir` failed.
fn stored(dir: &Path, error: os::Error) -> Failure {
    match error {
        os::Error::Dir(error) if error.kind() == io::ErrorKind::NotFound => {
            not_running(dir)
        }
        os::Error::Dir(error) if error.kind() == io::ErrorKind::NotADirectory => {
            Failure {
                code: NOT_RUNNING,
                message: format!(
                    "no node runs in {}, which is not a data directory",
                    dir.display()
                ),
                fix: "Give `--data` the data directory of a node that runs".to_owned(),
            }
        }
        os::Error::Dir(error) => unreadable(dir, error),
        error @ (os::Error::Thread(_)
        | os::Error::Cores(_)
        | os::Error::Memory(_)
        | os::Error::Wall(_)) => failed(error),
    }
}

/// The failure of a reach of the node of the data directory `dir` that gave `error`.
fn unreached(dir: &Path, error: node::client::Error) -> Failure {
    let data = dir.display();
    let (code, message, fix): (_, _, String) = match error {
        node::client::Error::Local(sector::local::Error::NotRunning) => {
            return not_running(dir);
        }
        node::client::Error::Local(sector::local::Error::Files(error)) => {
            return unreadable(dir, error);
        }
        node::client::Error::Key => (
            ADMIN_KEY,
            format!("{data} holds no admin.key that a node wrote"),
            "Put back the `admin.key` that the first start wrote".into(),
        ),
        node::client::Error::Local(sector::local::Error::Version {
            file,
            found,
            expected,
        }) => (
            VERSION,
            format!(
                "{} holds format version {}, and this foundation reads version {}",
                data_file(dir, file),
                char::from(found),
                char::from(expected)
            ),
            "Use a `foundation` program of the same version as the node".into(),
        ),
        node::client::Error::Transport(error) => (
            SOCKET,
            format!("the socket of this program failed: {error}"),
            "Fix the cause in the message, then run the command again".into(),
        ),
        node::client::Error::Unanswered { at, error } => (
            UNANSWERED,
            format!("no node answers at {at}, the address in {data}: {error}"),
            match quoted(dir) {
                Some(dir) => {
                    format!("Start the node again with `foundation start --data={dir}`")
                }
                None => {
                    "Start the node again with `foundation start` and `--data` set \
                         to that directory"
                        .into()
                }
            },
        ),
        node::client::Error::Session { at, error } => (
            SESSION,
            format!("the session with the node at {at} failed: {error}"),
            "Fix the cause in the message, then run the command again".into(),
        ),
        node::client::Error::Refused(refusal) => (
            REFUSED,
            format!(
                "the node refused the key in {}: {refusal}",
                data_file(dir, sector::admin::FILE)
            ),
            "Use the key of a subject in the node's spec".into(),
        ),
    };
    Failure { code, message, fix }
}

/// The path of the file `file` of the node in the data directory `dir`.
fn data_file(dir: &Path, file: &str) -> String {
    dir.join("data").join(file).display().to_string()
}

/// The failure of a data directory `dir` where no node runs.
fn not_running(dir: &Path) -> Failure {
    Failure {
        code: NOT_RUNNING,
        message: format!("no node runs in {}", dir.display()),
        fix: match quoted(dir) {
            Some(dir) => format!(
                "Run `foundation start --data={dir}`, with `--name <name>` on its first \
                 start, or give `--data` the data directory of a node that runs"
            ),
            None => "Run `foundation start` with `--data` set to that directory, with \
                     `--name <name>` on its first start, or give `--data` the data \
                     directory of a node that runs"
                .to_owned(),
        },
    }
}

/// `dir` as the end of the shell word `--data={dir}` that gives back `dir`, so that a
/// command in a fix runs as printed: in single quotes unless it holds only safe
/// characters. `None` when `dir` is not UTF-8 or holds
/// a character that `char::escape_debug` escapes other than a quote, such as `\` or a
/// newline, since the text of a fix escapes it.
fn quoted(dir: &Path) -> Option<String> {
    let dir = dir.to_str().filter(|dir| {
        dir.chars()
            .all(|c| matches!(c, '"' | '\'') || c.escape_debug().len() == 1)
    })?;
    // No `=` or `~`: zsh with `MAGIC_EQUAL_SUBST` expands either after `--data=`.
    let safe = |c: char| c.is_ascii_alphanumeric() || "_./:@%+,-".contains(c);
    Some(if !dir.is_empty() && dir.chars().all(safe) {
        dir.to_owned()
    } else {
        format!("'{}'", dir.replace('\'', r#"'"'"'"#))
    })
}

/// The failure of a data directory `dir` that this user cannot read.
fn unreadable(dir: &Path, error: impl std::fmt::Display) -> Failure {
    Failure {
        code: UNREADABLE,
        message: format!("cannot read the data directory {}: {error}", dir.display()),
        fix: "Run the command as the user that started the node".to_owned(),
    }
}

/// The failure of a request that the node gave no reply to.
fn no_reply(error: &program::Error) -> Failure {
    Failure {
        code: NO_REPLY,
        message: format!("the node gave no reply: {error}"),
        fix: "See that the node runs, then run the command again. After `apply`, plan \
              again to see whether the change applied"
            .to_owned(),
    }
}

/// Runs a node on the data directory of `start` until a SIGINT or SIGTERM that reaches
/// the process stops it.
fn node(
    start: &Start,
    shards: Shards,
    vars: BTreeMap<OsString, OsString>,
) -> Result<(), Failure> {
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
        listen: start.listen,
        region: node::Region::Stored,
        name: known.name.clone(),
        settings: node::Settings::default(),
        vars,
        roots: os::roots::read(),
        log: node::Log {
            writer: Box::new(io::stderr()),
            format: start.log,
        },
        threads: threads.clone(),
    });
    let flush = node.flush();
    panic::set_hook(flush.hook(panic::take_hook(), PANIC));
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
    // A writer that is stuck past the bound loses the lines that wait.
    flush.wait(FLUSH);
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
            undersized(dir, known, Resource::Disk, *disk, *cores, *min)
        }
        node::Error::Small { pool, cores, min } => {
            undersized(dir, known, Resource::Memory, *pool, *cores, *min)
        }
        node::Error::Pool { pool, cores } => match known.from.pool {
            Origin::Kept => kept_pool(dir, *pool, *cores),
            Origin::Most | Origin::Quarter => failed(error),
        },
        node::Error::Memory {
            core,
            error: error @ os::memory::Error::Reserve { .. },
        } if known.from.pool == Origin::Kept => {
            let budget = known.budget.pool;
            let (from, fix) = kept(dir);
            Failure {
                code: MEMORY,
                message: format!(
                    "the pool budget {budget}, {from}, needs more address space for \
                     shard-{core} than the system gives: {error}"
                ),
                fix,
            }
        }
        node::Error::Buffer {
            core,
            error: error @ buffer::Error::Pool(block::Error::Refused { .. }),
        } => {
            let budget = known.budget.pool;
            Failure {
                code: MEMORY,
                message: format!(
                    "the system refused memory for shard-{core} that its part of the \
                     pool budget {budget} has room for: {error}"
                ),
                fix: "Free memory on this host".to_owned(),
            }
        }
        node::Error::Buffer {
            core,
            error:
                error @ buffer::Error::Pool(
                    block::Error::TooLarge { .. } | block::Error::Exhausted { .. },
                ),
        } => {
            let budget = known.budget.pool;
            let (from, fix) = source(dir, known.from.pool, Resource::Memory);
            Failure {
                code: MEMORY,
                message: format!(
                    "the pool budget {budget}, {from}, gives shard-{core} too little: \
                     {error}"
                ),
                fix,
            }
        }
        error => failure(dir, error),
    }
}

/// The failure of a `budget` of `resource`, which comes from where `known` says, that
/// gives each of `cores` shards too little, which need a budget of at least `min`.
fn undersized(
    dir: &Path,
    known: &Known,
    resource: Resource,
    budget: Size,
    cores: usize,
    min: Size,
) -> Failure {
    let (name, code, from, lacks) = match resource {
        Resource::Memory => ("pool", MEMORY, known.from.pool, "opens no buffer"),
        Resource::Disk => ("disk", DISK, known.from.disk, "holds no ring"),
    };
    let (from, fix) = source(dir, from, resource);
    Failure {
        code,
        message: format!(
            "the {name} budget {budget}, {from}, {lacks} on each of {cores} shards; it \
             needs at least {min}"
        ),
        fix,
    }
}

/// The failure of a pool budget `pool`, kept in the data directory `dir`, that gives
/// one of `cores` shards a pool past the address space of the host.
fn kept_pool(dir: &Path, pool: types::byte::Size, cores: usize) -> Failure {
    let (from, fix) = kept(dir);
    Failure {
        code: MEMORY,
        message: format!(
            "the pool budget {pool}, {from}, gives one of {cores} shards a pool that \
             needs more address space than this host has"
        ),
        fix,
    }
}

/// The failure of `error`, an error that no budget causes, in the data directory
/// `dir`.
fn failure(dir: &Path, error: &node::Error) -> Failure {
    if let Some(error @ env::files::Error::Io { code, .. }) = error.files()
        && refused(*code)
    {
        return unwritable(dir, error);
    }
    let data = dir.display();
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
        node::Error::Reserved { name } => (
            RESERVED,
            format!(
                "the name {name} has a segment that starts with `@`, which is reserved \
                 for Foundation"
            ),
            "Give a name with no segment that starts with `@`".to_owned(),
        ),
        error @ node::Error::Long { .. } => (
            LONG,
            error.to_string(),
            "Give a shorter name with `--name`".to_owned(),
        ),
        node::Error::Name => (
            NAME,
            format!("the file `name` in the data directory {data} is not a node name"),
            "Remove it, and start the node with its name".to_owned(),
        ),
        error @ (node::Error::Port { .. } | node::Error::Listen { .. }) => (
            PORT,
            error.to_string(),
            "Give `--listen` an address of this host with a port that is free and that \
             this user can bind"
                .to_owned(),
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

/// That a budget comes from the file `budget` of the data directory `dir`, as text,
/// and the fix of a kept budget that does not fit: remove the file.
fn kept(dir: &Path) -> (String, String) {
    let data = dir.display();
    (
        format!("which {data} keeps from its first start"),
        format!(
            "Remove the file `budget` in {data}, and the next start computes the \
             budgets again from the free memory and disk"
        ),
    )
}

/// Where a budget of `resource` that gives the node too little comes from, `from`,
/// as text, and its fix. `dir` is the data directory.
fn source(dir: &Path, from: Origin, resource: Resource) -> (String, String) {
    let data = dir.display();
    match (from, resource) {
        (Origin::Kept, _) => kept(dir),
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

/// The failure of a data directory `dir` that this user cannot use as it is.
fn unwritable(dir: &Path, error: impl std::fmt::Display) -> Failure {
    let dir = dir.display();
    Failure {
        code: DATA,
        message: format!("cannot write the data directory {dir}: {error}"),
        fix: format!(
            "Make {dir} a directory that this user can write, and let it write each \
             file in it, or give another directory with `--data`"
        ),
    }
}

/// The failure for an error that the user cannot fix by a change to the command.
fn failed(error: impl std::fmt::Display) -> Failure {
    Failure {
        code: FAILED,
        message: error.to_string(),
        fix: "Fix the cause that the message states, then run the command again"
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

    /// The failure of a disk budget `disk`, which comes `from` there while the pool
    /// budget comes from elsewhere, that holds no ring on each of `cores` shards, which
    /// each need 4212KiB.
    fn disk(disk: Size, cores: usize, from: Origin) -> Failure {
        let count = u64::try_from(cores).expect("a core count fits a u64");
        let error = node::Error::Disk {
            disk,
            cores,
            min: Size::from_bytes(4_313_088 * count),
        };
        let budget = Budget {
            pool: Size::GIBIBYTE,
            disk,
        };
        let pool = if from == Origin::Kept {
            Origin::Most
        } else {
            Origin::Kept
        };
        let known = Known {
            from: Origins { pool, disk: from },
            ..known(budget, from)
        };
        stopped(dir(), &error, &known)
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
                          least 67392KiB"
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
                          8424MiB"
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
                          needs at least 67392KiB"
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

    /// A first start gives at most 1 GiB, so no process makes `Error::Pool` from it,
    /// and only this test sees the arm of `stopped` for a first start.
    #[test]
    fn a_pool_budget_past_the_address_space_from_a_first_start_fails_with_its_text() {
        let error = node::Error::Pool {
            pool: POOL_MOST,
            cores: 16,
        };
        let budget = Budget {
            pool: POOL_MOST,
            disk: Size::GIBIBYTE,
        };
        for from in [Origin::Most, Origin::Quarter] {
            assert_eq!(
                stopped(dir(), &error, &known(budget, from)),
                Failure {
                    code: FAILED,
                    message: "the pool budget 1GiB gives one of 16 shards a pool that \
                              needs more address space than this host has"
                        .to_owned(),
                    fix: "Fix the cause that the message states, then run the command \
                          again"
                        .to_owned(),
                }
            );
        }
    }

    #[test]
    fn a_kept_pool_budget_that_the_system_cannot_reserve_tells_the_user_to_remove_it() {
        let error = node::Error::Memory {
            core: 3,
            error: os::memory::Error::Reserve {
                len: 1 << 46,
                code: 12,
            },
        };
        let budget = Budget {
            pool: Size::from_bytes(1 << 50),
            disk: Size::GIBIBYTE,
        };
        assert_eq!(
            stopped(dir(), &error, &known(budget, Origin::Kept)),
            Failure {
                code: MEMORY,
                message:
                    "the pool budget 1024TiB, which foundation-data keeps from its \
                          first start, needs more address space for shard-3 than the \
                          system gives: a reserve of 70368744177664 bytes of address \
                          space failed: Cannot allocate memory (os error 12); lower \
                          the memory budget"
                        .to_owned(),
                fix: "Remove the file `budget` in foundation-data, and the next start \
                      computes the budgets again from the free memory and disk"
                    .to_owned(),
            }
        );
    }

    #[test]
    fn a_memory_error_but_a_reserve_of_a_kept_pool_budget_fails_with_its_text() {
        let reserve = os::memory::Error::Reserve {
            len: 1 << 46,
            code: 12,
        };
        let budget = Budget {
            pool: Size::from_bytes(1 << 50),
            disk: Size::GIBIBYTE,
        };
        for (error, from) in [
            (reserve, Origin::Most),
            (reserve, Origin::Quarter),
            (os::memory::Error::Refused, Origin::Kept),
        ] {
            let error = node::Error::Memory { core: 3, error };
            assert_eq!(
                stopped(dir(), &error, &known(budget, from)),
                Failure {
                    code: FAILED,
                    message: error.to_string(),
                    fix: "Fix the cause that the message states, then run the command \
                          again"
                        .to_owned(),
                }
            );
        }
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

    #[test]
    fn a_pool_budget_that_opens_no_buffer_names_the_least_budget_of_the_node() {
        let pool = Size::from_bytes(512 << 10);
        let error = node::Error::Small {
            pool,
            cores: 16,
            min: Size::from_bytes(996_352),
        };
        let budget = Budget {
            pool,
            disk: DISK_MOST,
        };
        let remove = "Remove the file `budget` in foundation-data, and the next start \
                      computes the budgets again from the free memory and disk";
        for (from, disk, text, fix) in [
            (
                Origin::Kept,
                Origin::Kept,
                "which foundation-data keeps from its first start",
                remove,
            ),
            (
                Origin::Most,
                Origin::Quarter,
                "the most that a first start gives",
                FEWER,
            ),
            (
                Origin::Quarter,
                Origin::Most,
                "a quarter of the available memory",
                "Free memory on this host",
            ),
        ] {
            let known = Known {
                from: Origins { pool: from, disk },
                ..known(budget, disk)
            };
            assert_eq!(
                stopped(dir(), &error, &known),
                Failure {
                    code: MEMORY,
                    message: format!(
                        "the pool budget 512KiB, {text}, opens no buffer on each of 16 \
                         shards; it needs at least 973KiB"
                    ),
                    fix: fix.to_owned(),
                },
                "{from:?}"
            );
        }
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
            let log = mesh::log::Error::Files(io("mesh/log/log-0", code));
            let reason = io::Error::from_raw_os_error(code);
            for (error, file) in [
                (node::Error::Directory(io("node.key", code)), "node.key"),
                (ring, "shard-2/ring"),
                (node::Error::Mesh(mesh::Error::Log(log)), "mesh/log/log-0"),
            ] {
                assert_eq!(
                    of(&error),
                    Failure {
                        code: DATA,
                        message: format!(
                            "cannot write the data directory foundation-data: open of \
                             {file} failed with OS error {code}"
                        ),
                        fix: "Make foundation-data a directory that this user can \
                              write, and let it write each file in it, or give another \
                              directory with `--data`"
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
            fix: "Fix the cause that the message states, then run the command again"
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
        let founding = mesh::Error::Files(io("mesh/founding", ENOSPC));
        assert_eq!(
            of(&node::Error::Mesh(founding)),
            failed(
                "the node's mesh did not open: open of mesh/founding failed with OS \
                 error 28"
            )
        );
        let chunk = blob::Error::Files(io("blob/chunk", ENOSPC));
        assert_eq!(
            of(&node::Error::Blob(chunk)),
            failed(
                "cannot open the node's chunk store: open of blob/chunk failed with OS \
                 error 28"
            )
        );
        let log = mesh::log::Error::Files(io("mesh/log/log-0", ENOSPC));
        let stopped = node::Error::Group(mesh::Stopped::Write(log));
        assert_eq!(
            of(&stopped),
            failed(
                "the group of the node's mesh stopped: open of mesh/log/log-0 failed \
                 with OS error 28"
            )
        );
    }

    /// The address in the `address` file of a test.
    fn at() -> std::net::SocketAddr {
        "127.0.0.1:54321".parse().expect("an address")
    }

    /// The failure of `code`, `message`, and `fix`.
    fn of_reach(code: Code, message: &str, fix: &str) -> Failure {
        Failure {
            code,
            message: message.to_owned(),
            fix: fix.to_owned(),
        }
    }

    const NOT_RUNNING_FIX: &str = "Run `foundation start --data=plant`, with `--name \
                                   <name>` on its first start, or give `--data` the \
                                   data directory of a node that runs";
    const UNREADABLE_FIX: &str = "Run the command as the user that started the node";

    #[test]
    fn a_directory_with_no_node_is_not_running() {
        let plant = Path::new("plant");
        let not_running =
            of_reach(NOT_RUNNING, "no node runs in plant", NOT_RUNNING_FIX);
        assert_eq!(
            unreached(
                plant,
                node::client::Error::Local(sector::local::Error::NotRunning)
            ),
            not_running
        );
        let missing = io::Error::from(io::ErrorKind::NotFound);
        assert_eq!(stored(plant, os::Error::Dir(missing)), not_running);
    }

    #[test]
    fn a_file_as_the_data_directory_is_not_running() {
        let file = io::Error::from(io::ErrorKind::NotADirectory);
        assert_eq!(
            stored(Path::new("plant.hcl"), os::Error::Dir(file)),
            of_reach(
                NOT_RUNNING,
                "no node runs in plant.hcl, which is not a data directory",
                "Give `--data` the data directory of a node that runs",
            )
        );
    }

    #[test]
    fn the_fix_of_a_data_directory_where_no_node_runs_quotes_it_for_the_shell() {
        let fix = |dir| not_running(Path::new(dir)).fix;
        assert_eq!(
            ["plant", "my plant", "it's", "-x", "=plant"].map(fix),
            [
                "Run `foundation start --data=plant`, with `--name <name>` on its first \
                 start, or give `--data` the data directory of a node that runs",
                "Run `foundation start --data='my plant'`, with `--name <name>` on its \
                 first start, or give `--data` the data directory of a node that runs",
                "Run `foundation start --data='it'\"'\"'s'`, with `--name <name>` on its \
                 first start, or give `--data` the data directory of a node that runs",
                "Run `foundation start --data=-x`, with `--name <name>` on its first \
                 start, or give `--data` the data directory of a node that runs",
                "Run `foundation start --data='=plant'`, with `--name <name>` on its \
                 first start, or give `--data` the data directory of a node that runs",
            ]
        );
    }

    proptest::proptest! {
        #[test]
        fn the_shell_gives_back_the_path_of_its_word(dir in "[\t\n -~é\u{301}\u{200b}]{0,24}") {
            let Some(word) = quoted(Path::new(&dir)) else {
                let escaped = ['\\', '\t', '\n', '\u{301}', '\u{200b}'];
                proptest::prop_assert!(dir.contains(escaped), "{dir:?} has a word");
                return Ok(());
            };
            let shells: [&[&str]; 3] = [&["sh"], &["zsh"], &["zsh", "-o", "magic_equal_subst"]];
            for shell in shells {
                let Ok(output) = std::process::Command::new(shell[0])
                    .args(&shell[1..])
                    .arg("-c")
                    .arg(format!("printf %s --data={word}"))
                    .output()
                else {
                    proptest::prop_assert_eq!(shell[0], "zsh", "{} is not there", shell[0]);
                    continue;
                };
                let given = String::from_utf8_lossy(&output.stdout);
                proptest::prop_assert_eq!(given.strip_prefix("--data="), Some(&*dir), "{:?}", shell);
            }
        }
    }

    #[test]
    fn a_path_in_a_command_is_one_word_of_the_shell() {
        let words = [
            "plant",
            "/var/lib/foundation-2.d",
            "my plant",
            "it's",
            "",
            "~/a",
            "-x",
            "-my x",
            "=plant",
            "=my plant",
        ]
        .map(|dir| quoted(Path::new(dir)).expect("a UTF-8 path has a word"));
        assert_eq!(
            words,
            [
                "plant",
                "/var/lib/foundation-2.d",
                "'my plant'",
                "'it'\"'\"'s'",
                "''",
                "'~/a'",
                "-x",
                "'-my x'",
                "'=plant'",
                "'=my plant'",
            ]
        );
    }

    #[test]
    fn a_fix_names_no_data_directory_that_it_cannot_print() {
        use std::os::unix::ffi::OsStrExt;
        let dir = Path::new(std::ffi::OsStr::from_bytes(b"plant\xff"));
        let words = [
            dir,
            Path::new("a\\b"),
            Path::new("a\nb"),
            Path::new("cafe\u{301}"),
            Path::new("a\u{200b}b"),
        ];
        assert_eq!(words.map(quoted), [None, None, None, None, None]);
        assert_eq!(
            not_running(dir).fix,
            "Run `foundation start` with `--data` set to that directory, with `--name \
             <name>` on its first start, or give `--data` the data directory of a node \
             that runs"
        );
        let error = node::client::Error::Unanswered {
            at: at(),
            error: transport::Error::TimedOut,
        };
        assert_eq!(
            unreached(dir, error).fix,
            "Start the node again with `foundation start` and `--data` set to that \
             directory"
        );
    }

    #[test]
    fn a_directory_that_does_not_read_is_unreadable() {
        let error = env::files::Error::Io {
            path: PathBuf::from("admin.key"),
            operation: env::files::Operation::ReadAt,
            code: 13,
        };
        assert_eq!(
            unreached(
                dir(),
                node::client::Error::Local(sector::local::Error::Files(error))
            ),
            of_reach(
                UNREADABLE,
                "cannot read the data directory foundation-data: read_at of admin.key \
                 failed with OS error 13",
                UNREADABLE_FIX,
            )
        );
        let denied = io::Error::from_raw_os_error(13);
        assert_eq!(
            stored(dir(), os::Error::Dir(denied)),
            of_reach(
                UNREADABLE,
                "cannot read the data directory foundation-data: Permission denied (os \
                 error 13)",
                UNREADABLE_FIX,
            )
        );
    }

    #[test]
    fn a_stored_thread_that_does_not_start_is_a_failure() {
        let error = os::Error::Thread(env::thread::Error::Start {
            name: "files-reach".to_owned(),
            reason: "no memory".to_owned(),
        });
        assert_eq!(
            stored(dir(), error),
            failed("cannot start thread files-reach: no memory")
        );
    }

    #[test]
    fn a_directory_with_no_admin_key_names_the_file() {
        assert_eq!(
            unreached(dir(), node::client::Error::Key),
            of_reach(
                ADMIN_KEY,
                "foundation-data holds no admin.key that a node wrote",
                "Put back the `admin.key` that the first start wrote",
            )
        );
    }

    #[test]
    fn a_file_of_another_version_names_both_versions() {
        let error = node::client::Error::Local(sector::local::Error::Version {
            file: "address",
            found: b'2',
            expected: b'1',
        });
        assert_eq!(
            unreached(dir(), error),
            of_reach(
                VERSION,
                "foundation-data/data/address holds format version 2, and this \
                 foundation reads version 1",
                "Use a `foundation` program of the same version as the node",
            )
        );
    }

    #[test]
    fn a_broken_socket_is_the_socket_of_this_program() {
        let error = transport::Error::Network {
            error: env::net::Error::Io { code: 5 },
        };
        assert_eq!(
            unreached(dir(), node::client::Error::Transport(error)),
            of_reach(
                SOCKET,
                "the socket of this program failed: the socket broke: network call \
                 failed with OS error 5",
                "Fix the cause in the message, then run the command again",
            )
        );
    }

    #[test]
    fn a_dial_with_no_session_is_unanswered() {
        let error = node::client::Error::Unanswered {
            at: at(),
            error: transport::Error::TimedOut,
        };
        assert_eq!(
            unreached(Path::new("plant"), error),
            of_reach(
                UNANSWERED,
                "no node answers at 127.0.0.1:54321, the address in plant: the peer \
                 stopped answering",
                "Start the node again with `foundation start --data=plant`",
            )
        );
        let error = node::client::Error::Unanswered {
            at: at(),
            error: transport::Error::TimedOut,
        };
        assert_eq!(
            unreached(Path::new("my plant"), error).fix,
            "Start the node again with `foundation start --data='my plant'`"
        );
    }

    #[test]
    fn a_session_that_fails_names_the_address() {
        let error = node::client::Error::Session {
            at: at(),
            error: program::Error::Unanswered,
        };
        assert_eq!(
            unreached(dir(), error),
            of_reach(
                SESSION,
                "the session with the node at 127.0.0.1:54321 failed: the node \
                 finished a stream with no answer",
                "Fix the cause in the message, then run the command again",
            )
        );
    }

    #[test]
    fn a_refused_hello_names_the_key_file() {
        let refused = node::client::Error::Refused(wire::hub::client::Refusal::Refused);
        assert_eq!(
            unreached(dir(), refused),
            of_reach(
                REFUSED,
                "the node refused the key in foundation-data/data/admin.key: the spec \
                 has no such subject, does not list the key for it, or the signature \
                 is not valid",
                "Use the key of a subject in the node's spec",
            )
        );
    }

    #[test]
    fn a_request_with_no_reply_says_to_plan_again() {
        assert_eq!(
            no_reply(&program::Error::Unanswered),
            of_reach(
                NO_REPLY,
                "the node gave no reply: the node finished a stream with no answer",
                "See that the node runs, then run the command again. After `apply`, \
                 plan again to see whether the change applied",
            )
        );
    }
}
