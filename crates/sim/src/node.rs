//! One simulated node and its settings.

use std::fmt;
use std::net::{IpAddr, Ipv6Addr, SocketAddr};
use std::num::NonZeroUsize;
use std::path::{Component, Path, PathBuf};

use types::time::{Monotonic, Span, Stamp};

use crate::log::Waker;
use crate::state::lock;
use crate::{PATH_MAX, drivers, net, shard};

/// One simulated node: the `env` handles that its code gets. Clones refer to the same
/// node.
///
/// ```
/// let mut sim = sim::Sim::new(sim::Config::default());
/// let node = sim.node(sim::node::Config::default());
/// assert_eq!(node.clock().now(), sim::node::Config::default().monotonic);
/// ```
#[derive(Clone)]
pub struct Node(pub(crate) drivers::Node);

impl Node {
    /// The node's monotonic clock. A sleep on it panics outside the node's threads,
    /// and stops when its thread ends, a leaked one too.
    #[must_use]
    pub fn clock(&self) -> env::clock::Clock {
        env::clock::Clock::new(self.0.clone())
    }

    /// The node's wall clock. Each reading has the node's current wall error.
    #[must_use]
    pub fn wall(&self) -> env::wall::Wall {
        env::wall::Wall::new(self.0.clone())
    }

    /// The node's random bytes: its own stream from the run's seed.
    #[must_use]
    pub fn entropy(&self) -> env::entropy::Entropy {
        env::entropy::Entropy::new(self.0.clone())
    }

    /// Starts shards on the node, which has [`Config::cores`] cores.
    #[must_use]
    pub fn shards(&self) -> env::shards::Shards {
        env::shards::Shards::new(self.0.clone())
    }

    /// Makes the next shard start on `core` of the node get `fault`. Faults on one
    /// core fire in turn, one per start. A shard with no core gets none.
    ///
    /// # Panics
    ///
    /// When `core` is not below [`Config::cores`], or the node is
    /// [`Config::unpinnable`]: no start on it has a core.
    pub fn fail_shard(&self, core: usize, fault: shard::Fault) {
        let cores = lock(&self.0.shared).cores(self.0.node);
        assert!(
            core < cores.get(),
            "a shard fault aims at core {core} of {cores}"
        );
        let pinnable = lock(&self.0.shared).pinnable(self.0.node);
        assert!(
            pinnable,
            "a shard fault aims at core {core} of a node that cannot pin"
        );
        lock(&self.0.shared).shards(self.0.node).fail(core, fault);
    }

    /// The config of each shard start on the node, in order, with the ones that a
    /// fault failed.
    #[must_use]
    pub fn shard_starts(&self) -> Vec<env::shards::Config> {
        lock(&self.0.shared).shards(self.0.node).configs()
    }

    /// Starts dedicated threads on the node.
    #[must_use]
    pub fn threads(&self) -> env::threads::Threads {
        env::threads::Threads::new(self.0.clone())
    }

    /// Makes the next start of a thread named `name` on the node, through
    /// [`Node::threads`], give `thread::Error::Start` with reason `injected`. The start
    /// drops its body. Faults on one name fire in turn, one per start.
    pub fn fail_thread(&self, name: &str) {
        lock(&self.0.shared).threads(self.0.node).fail(name);
    }

    /// The node's network, on its [`Node::addresses`].
    ///
    /// - A bind or a listen on a link-local address gives `Error::Io` with code 22
    ///   (`EINVAL`) with scope 0, and with code 19 (`ENODEV`) with a scope other than
    ///   that of [`Node::link_local`]. Else one on an address that is not the node's
    ///   gives `Error::Io` with code 99 (`EADDRNOTAVAIL`). A send from a source that
    ///   the socket cannot send from gives `Error::Io` with code 22 (`EINVAL`) when the
    ///   source or the destination is IPv6 or the source is not unicast, and with code
    ///   99 for a unicast IPv4 source to an IPv4 destination. Port 0 binds the lowest
    ///   free port from 49152.
    /// - A socket on IPv6 takes `::ffff:a.b.c.d` as `a.b.c.d`, in the destination and
    ///   the source of a send. A send to the other family than the socket's gives
    ///   `Error::Unreachable` with the destination as given.
    /// - Each socket draws its send and receive batch maxes from 1, 8, and 64.
    /// - A datagram is lost when it is over the link's
    ///   [`mtu`](crate::link::Config::mtu), when nothing is bound at its
    ///   destination, when its socket failed ([`Node::fail_udp`]), or when its
    ///   receive queue takes more than `recv_buffer_bytes`, in which each datagram
    ///   takes its length plus 768 bytes.
    /// - A datagram takes its length plus 768 bytes of its socket's send buffer until
    ///   it leaves its link: at once when the link has no
    ///   [`rate`](crate::link::Config::rate) and no packet waits on it. A send is
    ///   pending while the send buffer is not empty and takes `send_buffer_bytes` or
    ///   more, so the datagrams of one send may go past it.
    /// - A TCP segment is never duplicated, and each direction of a stream keeps its
    ///   order. A lost one goes again after a timeout, with the RTO and the limits of
    ///   Linux, and an end that waits on it too long gives `Error::TimedOut`. On a
    ///   link with no loss, a connect is ready after one round trip, and its accept
    ///   after one and a half. A connect takes the next free port after the node's
    ///   last connect, from 49152. A listen conflicts only with other listens.
    /// - A peer sends at most `recv_buffer_bytes` past the bytes read, and a stream
    ///   holds at most `send_buffer_bytes` that its peer has not received. A write
    ///   of bytes after `poll_close` gives `Error::Io` with code 32 (`EPIPE`).
    /// - TCP panics on `delayed` sends, on a connect to an address that no node has,
    ///   and on a connect to a full backlog.
    /// - A socket half, a stream, or a listener panics when it polls outside the
    ///   node's threads or after a crash of the node.
    #[must_use]
    pub fn net(&self) -> env::net::Net {
        env::net::Net::new(self.0.clone())
    }

    /// Makes the UDP socket of the node at `local` fail, as when the OS breaks it:
    /// each receive of it first gives the datagrams already in its receive queue,
    /// then gives `Error::Io` with code 5 (`EIO`), also one that waits. The
    /// datagrams that arrive at it after the fault are lost. A send of it still
    /// works. A socket bound at `local` after it drops works. A fault on a socket
    /// that already failed does nothing.
    ///
    /// # Panics
    ///
    /// When no UDP socket of the node is bound at `local`.
    pub fn fail_udp(&self, local: SocketAddr) {
        let node = self.0.node;
        let waker = lock(&self.0.shared).net().udp().fail(node, local);
        let Some(waker) = waker else {
            panic!("no UDP socket of node {node} is bound at {local}");
        };
        waker.wake();
    }

    /// Makes the TCP listener of the node at `local` fail, as when the OS breaks it:
    /// each accept of it first gives the streams already in its backlog, then gives
    /// `Error::Io` with code 5 (`EIO`), also one that waits. A connect to it is refused
    /// when its SYN arrives after the fault. A connect whose SYN it took, but not its
    /// ACK, before the fault ends `Ok`, and its stream is reset when the RST of the
    /// fault arrives. The streams it accepted still work. A listener bound at `local`
    /// after it drops works. A fault on a listener that already failed does nothing.
    ///
    /// # Panics
    ///
    /// When no TCP listener of the node is bound at `local`.
    pub fn fail_listener(&self, local: SocketAddr) {
        let node = self.0.node;
        let mut state = lock(&self.0.shared);
        let now = state.now();
        let waker = state.net().tcp().fail(now, node, local);
        drop(state);
        let Some(waker) = waker else {
            panic!("no TCP listener of node {node} is bound at {local}");
        };
        waker.wake();
    }

    /// The count of open TCP streams of the node: each connect that has not given its
    /// stream, and each stream that a connect or an accept gave and that has not
    /// dropped. A stream that a listener holds before its accept does not count, and a
    /// crash drops each stream of the node.
    #[must_use]
    pub fn streams(&self) -> usize {
        lock(&self.0.shared).net().tcp().streams(self.0.node)
    }

    /// The node's serial ports: one at each end of a line that
    /// [`Sim::line`](crate::Sim::line) joins to the node.
    ///
    /// - An open ends at once.
    /// - Each port holds at most 4 KiB of bytes written that have not arrived: a
    ///   write queues up to that and then waits for room. Each port also holds at
    ///   most 4 KiB of bytes not read, and loses the bytes past that.
    /// - A byte that arrives at an end that is not open is lost, and so are the
    ///   bytes in flight from a port that drops.
    /// - A port panics when it polls outside the node's threads or after a crash of
    ///   the node.
    #[must_use]
    pub fn serial(&self) -> env::serial::Serial {
        env::serial::Serial::new(self.0.clone())
    }

    /// Makes the port at `path` of the node fail, as when its USB adapter is pulled
    /// out: the open port, or else the next one to open. Each read and write of it
    /// then gives `Error::Io` with code 5 (`EIO`), also one that waits. The bytes it
    /// has not read, the bytes it sent that have not arrived, and the bytes that
    /// arrive at it are lost. The next port to open after it drops works. A fault
    /// on a port that already failed does nothing.
    ///
    /// # Panics
    ///
    /// When no line joins `path` of the node.
    pub fn fail_serial(&self, path: &Path) {
        let node = self.0.node;
        let wakers = lock(&self.0.shared).serial().fail(node, path);
        let Some(wakers) = wakers else {
            let path = path.display();
            panic!("no line joins port {path} of node {node}");
        };
        wakers.into_iter().flatten().for_each(Waker::wake);
    }

    /// The node's disk: [`Config::disk_bytes`] bytes, with an empty data directory.
    ///
    /// - Each call takes up to 100 us of true time and takes effect when it ends.
    ///   A file call panics outside the node's threads.
    /// - A directory takes 4 KiB. A file takes its length until it is removed, a
    ///   `sync_dir` makes the removal durable, and no descriptor or call in flight
    ///   uses it.
    /// - Where calls in flight at the same time overlap, a read gives, in each
    ///   512-byte sector, the old bytes, the bytes of one of the writes, or the bytes
    ///   of one of these over a part of the sector and of another over the rest.
    ///   Writes go on each sector in an order that their times allow, and a write
    ///   that overlaps another can go in up to three parts, each at its own place.
    ///   A write whose future dropped still ends, with any subset of its sectors.
    /// - A failure gives the code that Linux gives: 20 (`ENOTDIR`) for a path
    ///   through a file, 21 (`EISDIR`) for a file call on a directory, and 17
    ///   (`EEXIST`) for `create_dir` on a file.
    /// - Each `Files` that it gives acts as a clone of one: a write open, a remove, or
    ///   a rename from any of them waits for each call that a drop from any of them
    ///   left to run.
    /// - `open_absolute` reads the file at its path in [`Node::paths`], the host's
    ///   tree, when the open ends. The open and each read take the time of a file
    ///   call, and [`Node::fail_file`] fails them. It gives `NotFound` when no file
    ///   is there, and the error of a read of the tree, such as one of
    ///   [`Node::fail_path`], as `Error::Io` with operation `Open`. A later write of
    ///   the tree does not change the open file. Its file is never exposed.
    #[must_use]
    pub fn files(&self) -> env::files::Files {
        env::files::Files::new(self.0.clone())
    }

    /// The path of each file descriptor that the node closed or dropped, in order, from
    /// the start of the run: the path of its open, or of the last rename that it made,
    /// with only its names. That path is relative to the data directory, or absolute
    /// for a file of `open_absolute`. A crash closes each descriptor of the node, a
    /// leaked one too. Those that the drops of its futures close come first, in the
    /// order of the drops. The leaked ones come last, in the order that their opens
    /// started.
    #[must_use]
    pub fn file_closes(&self) -> Vec<PathBuf> {
        lock(&self.0.shared).files().closes(self.0.node)
    }

    /// Makes the next call of `operation` on `path` on the node fail with
    /// `Error::Io` and code 5 (`EIO`). Faults on one path and operation fire in
    /// turn, one per call. The call does not touch the disk, except a sync: each
    /// sector keeps its durable bytes, or its bytes after one write that the sync
    /// covers or a part of one. These bytes are then durable. As on Linux, the writes
    /// that the sync covers stay in the cache, clean: a read sees them, and a later
    /// write goes over them. A power cut drops them, and at each read or write of
    /// their sector the cache may drop them, by a coin.
    ///
    /// # Panics
    ///
    /// When `operation` is `Free` and `path` is not empty: `free` has no path.
    pub fn fail_file(&self, path: &Path, operation: env::files::Operation) {
        let free = operation == env::files::Operation::Free;
        assert!(
            !free || path.as_os_str().is_empty(),
            "free has no path; aim a fault at it with an empty path"
        );
        lock(&self.0.shared)
            .files()
            .fail(self.0.node, path, operation);
    }

    /// Makes the mode of the file at `path`, relative to the data directory, give read
    /// to other users at once, as a `chmod o+r` from outside the node does. A file
    /// that an open makes is not exposed, as on `os`. A descriptor keeps what its open
    /// found, and a crash keeps the setting.
    ///
    /// # Panics
    ///
    /// When `path` is absolute, has a `..` segment, or no file is at `path`.
    pub fn expose_file(&self, path: &Path) {
        self.set_file_exposed(path, true);
    }

    /// Makes the file at `path`, relative to the data directory, unreadable by other
    /// users at once, as a `chmod o-r` from outside the node does.
    ///
    /// # Panics
    ///
    /// When `path` is absolute, has a `..` segment, or no file is at `path`.
    pub fn hide_file(&self, path: &Path) {
        self.set_file_exposed(path, false);
    }

    fn set_file_exposed(&self, path: &Path, exposed: bool) {
        assert!(path.is_relative(), "{} is absolute", path.display());
        let parent = path.components().any(|part| part == Component::ParentDir);
        assert!(!parent, "{} has a `..` segment", path.display());
        let found = lock(&self.0.shared)
            .files()
            .expose(self.0.node, path, exposed);
        assert!(found, "no file is at {}", path.display());
    }

    /// The files of the node's host by path: a tree in memory, apart from the node's
    /// disk, with no file at first. A directory is there once [`Node::make_dir`] or
    /// `make_dir` of the paths makes it, as [`Node::link_path`] does above its link. A
    /// relative path is under the working directory, `/` until
    /// [`Node::set_working_dir`] sets it. A call follows each symbolic link of
    /// [`Node::link_path`], and a `..` after a link goes above what the link names. A
    /// call through a directory that is not there, or a read or a write of a link to
    /// nothing, fails with `NotFound`. The home directory is the one of
    /// [`Node::set_home`], else `None`. A call takes no time, may come from any
    /// thread, and acts at once. A crash of the node keeps the tree, the working
    /// directory, and the home. A failure gives the code that Linux gives: 17
    /// (`EEXIST`) for a directory made on a file or a link, 20 (`ENOTDIR`) for a path
    /// through a file, 21 (`EISDIR`) for a write on a directory, and 40 (`ELOOP`) for a
    /// path through too many links.
    #[must_use]
    pub fn paths(&self) -> env::paths::Paths {
        env::paths::Paths::new(self.0.clone())
    }

    /// Makes the next read, write, or `make_dir` of `path` on the node's host fail
    /// with `env::paths::Error::Io` and `code`. A call is of `path` when the path of
    /// its directory and its name give `path` by text, and a listing when the path of
    /// its directory does. That text starts at the working directory at that call,
    /// with `.` and `..` taken as text, so a relative `path` follows a later
    /// [`Node::set_working_dir`]. The path of a directory is its path from `/` with
    /// each link resolved, so a `path` through a link is of no call. A call that
    /// `env::paths::Paths` does not make is of no path, such as one that its walk
    /// stops before, a `make_dir` where a directory or a link is, or a write where a
    /// directory is. An open of [`Node::files`] is of `path` when its path gives `path`
    /// by text, before its links are followed. Faults that a call is of fire in the
    /// order set, one per call, and a failed call changes no file.
    pub fn fail_path(&self, path: &Path, code: i32) {
        lock(&self.0.shared).paths().fail(self.0.node, path, code);
    }

    /// Runs `change` once, at the next read, write, or `make_dir` of `path` on the
    /// node's host, after the walk of `env::paths::Paths` and before the call reaches
    /// its file, as a process that changes the files between the check and the call
    /// does. A call is of `path` as for [`Node::fail_path`].
    pub fn race_path(&self, path: &Path, change: impl FnOnce() + Send + 'static) {
        let change = Box::new(change);
        lock(&self.0.shared)
            .paths()
            .add_race(self.0.node, path, change);
    }

    /// Makes a symbolic link at `path` on the node's host that names `target`, and so
    /// each directory above it. A relative `target` is from the directory of the link,
    /// and a link on the way to `path` is followed.
    ///
    /// # Panics
    ///
    /// When `path` is relative or `/`, has a `..` segment, goes through a file, a link
    /// to nothing, or more than 40 links, or a file, a link, or a directory is at it,
    /// or when `target` has 4096 bytes or more, which Linux refuses.
    pub fn link_path(&self, path: &Path, target: &Path) {
        assert!(path.is_absolute(), "{} is relative", path.display());
        let long = target.as_os_str().len() >= PATH_MAX;
        assert!(!long, "the target of {} is too long", path.display());
        let parent = path.components().any(|part| part == Component::ParentDir);
        assert!(!parent, "{} has a `..` segment", path.display());
        lock(&self.0.shared).paths().link(self.0.node, path, target);
    }

    /// Makes a directory at `path` on the node's host, and so each directory above it.
    /// A link on the way to `path` is followed, and a directory at it stays.
    ///
    /// # Panics
    ///
    /// When `path` is relative or `/`, has a `..` segment, goes through a file, a link
    /// to nothing, or more than 40 links, or a file or a link is at it.
    pub fn make_dir(&self, path: &Path) {
        assert!(path.is_absolute(), "{} is relative", path.display());
        let parent = path.components().any(|part| part == Component::ParentDir);
        assert!(!parent, "{} has a `..` segment", path.display());
        lock(&self.0.shared).paths().place_dir(self.0.node, path);
    }

    /// Removes the empty directory at `path` on the node's host, as a removal from
    /// outside the node does. A link on the way to `path` is followed. A directory
    /// that a call holds open stays open, with no entries and no path, and each name
    /// in it is `NotFound`.
    ///
    /// # Panics
    ///
    /// When `path` is relative or `/`, has a `..` segment, its directory is not there,
    /// or no empty directory is at it.
    pub fn remove_dir(&self, path: &Path) {
        assert!(path.is_absolute(), "{} is relative", path.display());
        let parent = path.components().any(|part| part == Component::ParentDir);
        assert!(!parent, "{} has a `..` segment", path.display());
        lock(&self.0.shared).paths().remove_dir(self.0.node, path);
    }

    /// Moves the file, link, or directory at `from` on the node's host to `to`, as a
    /// rename from outside the node does: it keeps its key, and a directory keeps
    /// what it holds. A link on the way to `from` or `to` is followed, and a link at
    /// `from` moves itself. When the node's working directory resolves, it stays on
    /// the directory that it resolves to, as on a host: it moves with it when that
    /// directory is `from` or under it, and a move of a link on its way leaves it
    /// there. It keeps its path while that path resolves to that directory, and else
    /// takes the path of the directory.
    ///
    /// # Panics
    ///
    /// When `from` or `to` is relative or `/`, or has a `..` segment, when nothing is
    /// at `from`, when a file, a link, or a directory is at `to`, when the directory
    /// of `to` is not there, or when `to` is under `from`.
    pub fn move_path(&self, from: &Path, to: &Path) {
        for path in [from, to] {
            assert!(path.is_absolute(), "{} is relative", path.display());
            let parent = path.components().any(|part| part == Component::ParentDir);
            assert!(!parent, "{} has a `..` segment", path.display());
        }
        lock(&self.0.shared)
            .paths()
            .move_entry(self.0.node, from, to);
    }

    /// Sets the working directory of the node's host, from which each relative path
    /// starts. Nothing need be there.
    ///
    /// # Panics
    ///
    /// When `dir` is relative or has a `..` segment.
    pub fn set_working_dir(&self, dir: &Path) {
        assert!(dir.is_absolute(), "{} is relative", dir.display());
        let parent = dir.components().any(|part| part == Component::ParentDir);
        assert!(!parent, "{} has a `..` segment", dir.display());
        lock(&self.0.shared).paths().set_working(self.0.node, dir);
    }

    /// Sets the home directory of the user of the node's host, which
    /// `env::paths::Paths::home` gives resolved.
    ///
    /// # Panics
    ///
    /// When `home` is relative.
    pub fn set_home(&self, home: &Path) {
        assert!(home.is_absolute(), "{} is relative", home.display());
        lock(&self.0.shared).paths().set_home(self.0.node, home);
    }

    /// The node's IPv4 and IPv6 addresses, in that order: node `k`, from 0 in the
    /// order of [`Sim::node`](crate::Sim::node), has `10.0.0.0` and `fd00::`, each
    /// plus `k + 1`.
    ///
    /// # Panics
    ///
    /// For the 16,777,215th node and after: `10.0.0.0/8` has no host for them.
    #[must_use]
    pub fn addresses(&self) -> [IpAddr; 2] {
        net::addresses(self.0.node)
    }

    /// The node's link-local IPv6 address, `fe80::` plus `k + 1`, and the index of the
    /// one interface that holds it. A bind of it takes that index as its scope. No
    /// packet reaches it.
    ///
    /// # Panics
    ///
    /// As [`Node::addresses`].
    #[must_use]
    pub fn link_local(&self) -> (Ipv6Addr, u32) {
        (net::link_local(self.0.node), net::INTERFACE)
    }

    /// Steps the wall clock by `span`, forward or back, as when NTP or an operator
    /// sets it. The monotonic clock does not move. A step can move the end of true
    /// time (see [`Sim::run_for`](crate::Sim::run_for)).
    ///
    /// # Panics
    ///
    /// When the wall leaves the range of a [`Stamp`].
    pub fn step_wall(&self, span: Span) {
        let stepped = lock(&self.0.shared).step_wall(self.0.node, span);
        let node = self.0.node;
        assert!(
            stepped,
            "step_wall({span}) moves the wall of node {node} out of range"
        );
    }

    /// Sets the error bound of the node's next wall readings, as when the time daemon
    /// updates it. The wall does not move. A negative bound makes the driver broken,
    /// as in [`Config::wall_error`].
    pub fn set_wall_error(&self, error: Option<Span>) {
        lock(&self.0.shared).set_wall_error(self.0.node, error);
    }

    /// Runs nothing on the node for `span` of true time while its clocks move, as in
    /// a VM pause or a machine suspend; a negative span is zero. Each wake in the
    /// pause, from a timer or from another node, polls its task when the pause ends.
    /// A pause that overlaps another ends at the later end, and one past the end of
    /// true time never ends.
    pub fn pause(&self, span: Span) {
        lock(&self.0.shared).pause(self.0.node, span);
    }
}

impl fmt::Debug for Node {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("Node").field(&self.0.node).finish()
    }
}

/// Settings for one node. Build it with `..Config::default()`: fields get added.
///
/// ```
/// let config = sim::node::Config {
///     wall: types::time::Stamp::EPOCH,
///     ..sim::node::Config::default()
/// };
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Config {
    /// The core count that [`env::shards::Shards::cores`] reports.
    pub cores: NonZeroUsize,
    /// The node cannot pin a shard to a core: [`env::shards::Shards::pinnable`] is
    /// `false`.
    pub unpinnable: bool,
    /// The monotonic reading when the node is added.
    pub monotonic: Monotonic,
    /// The wall time when the node is added.
    pub wall: Stamp,
    /// The error bound that the OS gives with each wall reading, or `None` when it
    /// gives none. A negative bound makes the driver broken, as a test of what meets
    /// one.
    pub wall_error: Option<Span>,
    /// The bytes of the node's disk.
    pub disk_bytes: u64,
    /// The longest wait that a timer arms for, as `os` arms each Tokio sleep for at
    /// most a second. A timer with a later deadline wakes its task early, and arms
    /// again only at its next poll. A deadline past the end of true time at the poll
    /// gets no early wake. `None` arms each timer for its deadline. A span must be
    /// positive.
    pub arm_max: Option<Span>,
}

impl Default for Config {
    /// Four cores that can pin, one hour after boot, at 2026-01-01T00:00:00Z, with a
    /// wall error of 10 ms, a disk of 64 GiB, and each timer armed for at most 1 s.
    fn default() -> Self {
        Self {
            cores: NonZeroUsize::new(4).expect("four is not zero"),
            unpinnable: false,
            monotonic: Monotonic::default() + Span::HOUR,
            wall: Stamp::from_nanos(1_767_225_600 * Span::SECOND.nanos()),
            wall_error: Some(Span::from_nanos(10 * Span::MILLISECOND.nanos())),
            disk_bytes: 64 << 30,
            arm_max: Some(Span::SECOND),
        }
    }
}
