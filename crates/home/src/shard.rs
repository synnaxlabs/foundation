//! The indexes one shard carries: their writers, each frame from split to one buffer
//! append, and their readers.

use std::rc::Rc;
use std::sync::Arc;
use std::{fmt, mem};

use buffer::{Buffer, Entry};
use types::channel::Slot;
use types::frame::key_set::{self, KeySet};
use types::frame::{self, Draft, Frame, Label, Path};
use types::hash;
use types::time::{Interval, Monotonic, Stamp};

use crate::Refusal;
use crate::index::{Accepted, Index};
use crate::reader::{self, Mode};
use crate::split::Split;
use crate::writer::{self, Writer};
use crate::{handoff, order, split, stored};

/// The indexes of one shard, with their writers and readers. Each call is on the
/// shard's thread.
#[derive(Debug)]
pub(crate) struct Shard {
    buffer: Buffer,
    pool: Rc<block::Pool>,
    limits: order::Config,
    indexes: Vec<Index>,
    /// The place in `indexes` of each carried index.
    places: hash::Map<Slot, usize>,
    writers: hash::Map<writer::Key, Session>,
    next: u64,
    scratch: Scratch,
    wake: Wake,
}

/// The readers to wake, and the indexes whose live frames wait for a commit.
#[derive(Debug, Default)]
struct Wake {
    /// The slot and place of each index whose readers may be
    /// [pending](delivery::Readers::pending), each once.
    pending: Vec<(Slot, usize)>,
    /// Whether `pending` holds the index, by place. A place past the end is not.
    listed: Vec<bool>,
    /// Each reader to wake, with a frame to take. A key can repeat.
    keys: Vec<reader::Key>,
    /// The commits the buffer had ended at the last settle.
    commits: u64,
}

/// The shard's state for an open writer.
#[derive(Debug)]
struct Session {
    set: Arc<KeySet>,
    /// The claim on the index of each group, by group number.
    claims: Vec<Claim>,
}

/// A writer's place on one index.
#[derive(Clone, Copy, Debug)]
struct Claim {
    /// The index's place in [`Shard::indexes`].
    place: usize,
    key: control::Key,
}

/// The buffers of a write, kept from one write to the next.
#[derive(Debug, Default)]
struct Scratch {
    split: split::Scratch,
    /// The check of each present group, in group order.
    checks: Vec<(u32, Result<Accepted, Refusal>)>,
    /// The stored entry of each accepted group, in group order. Empty between
    /// appends.
    entries: Vec<Entry>,
    outcomes: Vec<Outcome>,
}

/// What became of one group of a frame.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Outcome {
    /// Queued for the next group commit.
    Applied {
        /// The slot of the group's index.
        slot: Slot,
        /// The seq of the group's samples.
        range: frame::Range,
    },
    /// A live group found no room in the ring or the pool. Its seq is a gap in the
    /// log.
    Lost {
        /// The slot of the group's index.
        slot: Slot,
        /// The seq of the group's samples.
        range: frame::Range,
    },
    /// Refused. The index spent no seq.
    Refused {
        /// The slot of the group's index.
        slot: Slot,
        /// Why the index refused it.
        refusal: Refusal,
    },
}

/// Why a write failed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Error {
    /// The frame is labeled resend, which the home does not take yet.
    Resend,
    /// A backfill frame found no room in the ring or the pool. No seq moves. The pool
    /// has room again when commits end and readers take their frames. The ring has
    /// room again only when records leave it, which no write does.
    Full,
    /// The frame is too large for one write. Nothing is spent: the writer splits the
    /// frame by samples or by indexes and writes each part.
    Large,
    /// A commit failed. The shard takes no more frames.
    Disk(env::files::Error),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Resend => write!(f, "the home does not take a resend frame yet"),
            Self::Full => write!(f, "the ring or the pool has no room for the frame"),
            Self::Large => write!(f, "the frame is too large for one write: split it"),
            Self::Disk(error) => write!(f, "a commit failed: {error}"),
        }
    }
}

impl std::error::Error for Error {}

impl Shard {
    /// A shard over `buffer` that carries no index yet. Index frames, stored headers,
    /// and handoff bodies come from `pool`, the pool of `buffer`.
    pub(crate) fn new(
        buffer: Buffer,
        pool: Rc<block::Pool>,
        limits: order::Config,
    ) -> Self {
        Self {
            buffer,
            pool,
            limits,
            indexes: Vec::new(),
            places: hash::Map::default(),
            writers: hash::Map::default(),
            next: 0,
            scratch: Scratch::default(),
            wake: Wake::default(),
        }
    }

    /// Carries the index at `slot`, with an empty gate. Each path continues from its
    /// tail in the buffer.
    ///
    /// # Panics
    ///
    /// If the shard carries `slot` already.
    pub(crate) fn carry(&mut self, slot: Slot) {
        let tail = |path| {
            let tail = self.buffer.tail(slot, path);
            order::Tail {
                stamp: tail.stamp,
                seq: tail.seq,
            }
        };
        let index = Index::new(self.limits, tail(Path::Live), tail(Path::Backfill));
        let carried = self.places.insert(slot, self.indexes.len());
        assert!(carried.is_none(), "the shard carries {slot:?} already");
        self.indexes.push(index);
    }

    /// Opens `writer` on each index of its key set at monotonic time `now`, and
    /// appends a handoff for each index where it takes control. A handoff that finds
    /// no room waits for the next append on its index. A failed commit fails the next
    /// write.
    ///
    /// # Panics
    ///
    /// If an index of the key set is not carried.
    pub(crate) fn open_writer(
        &mut self,
        writer: Writer,
        now: Monotonic,
        mesh: Interval,
    ) -> writer::Key {
        let Writer {
            control,
            lease,
            set,
        } = writer;
        let entries = set.entries();
        let mut claims = Vec::with_capacity(set.groups().len());
        for &entry in set.groups() {
            let place = self.place(entries[entry].slot);
            let key = self.indexes[place].gate.open(control.clone(), lease, now);
            claims.push(Claim { place, key });
        }
        let session = Session { set, claims };
        self.record_all(&session, mesh);
        let key = writer::Key(self.next);
        self.next += 1;
        self.writers.insert(key, session);
        key
    }

    /// Closes the writer at monotonic time `now`, and appends a handoff for each index
    /// it held, as [`open_writer`](Self::open_writer) does.
    ///
    /// # Panics
    ///
    /// If the writer is not open.
    pub(crate) fn close_writer(
        &mut self,
        key: writer::Key,
        now: Monotonic,
        mesh: Interval,
    ) {
        let Some(session) = self.writers.remove(&key) else {
            panic!("writer {} is not open", key.0);
        };
        for claim in &session.claims {
            self.indexes[claim.place].gate.close(claim.key, now);
        }
        self.record_all(&session, mesh);
    }

    /// Applies `frame` to each index it holds, whole or not at all per index, at
    /// monotonic time `now` and mesh time `mesh`. The frame's bodies go in one append,
    /// after the unrecorded handoff of each of its indexes. Returns the outcome of
    /// each present group, in group order.
    ///
    /// # Errors
    ///
    /// [`Error::Resend`] for a frame labeled resend. [`Error::Full`] for a backfill
    /// frame when the ring or the pool has no room, and [`Error::Large`] for a frame
    /// whose bodies no record or no block of the pool holds; no seq moves for either.
    /// A handoff with no room decides first: the frame is lost or gets
    /// [`Error::Full`] before its size is checked. [`Error::Disk`] after a failed
    /// commit.
    ///
    /// # Panics
    ///
    /// If the writer is not open, or the frame is not of the writer's key set.
    pub(crate) fn write(
        &mut self,
        key: writer::Key,
        label: Label,
        frame: Draft,
        now: Monotonic,
        mesh: Interval,
    ) -> Result<&[Outcome], Error> {
        let Label::Path(path) = label else {
            return Err(Error::Resend);
        };
        let Some(session) = self.writers.get(&key) else {
            panic!("writer {} is not open", key.0);
        };
        let scratch = &mut self.scratch;
        let mut split = scratch.split.split(&session.set, frame);
        while let Some((group, stamps)) = split.next() {
            let (claim, _) = session.claim(group);
            let index = &mut self.indexes[claim.place];
            let checked = index.check(claim.key, path, stamps, now, mesh);
            scratch.checks.push((group, checked));
        }
        let groups = scratch.checks.iter().map(|&(group, _)| group);
        let recorded = record(
            &self.buffer,
            &self.pool,
            &mut self.indexes,
            session,
            groups,
            mesh,
        );
        let entries = &mut scratch.entries;
        let made = freeze(
            entries,
            &self.pool,
            &mut split,
            &session.set,
            &mut scratch.checks,
            mesh.latest,
        );
        drop(split);
        // Made also when a handoff found no room: freezing gives a lost live frame to
        // latest readers.
        let ready = made.is_ok() && recorded == Ok(true);
        if !ready {
            entries.clear();
        }
        let appended = match made {
            Err(block::Error::TooLarge { .. }) if recorded == Ok(true) => {
                Err(Error::Large)
            }
            // An empty append still reports a failed commit.
            _ => room(self.buffer.append(entries.drain(..))),
        };
        let room =
            recorded
                .and(appended)
                .and_then(|room| match (room && ready, path) {
                    (false, Path::Backfill) => Err(Error::Full),
                    (room, _) => Ok(room),
                });
        match room {
            Ok(room) => Ok(spend(
                &mut scratch.checks,
                &mut self.indexes,
                session,
                room,
                &mut self.wake,
                &mut scratch.outcomes,
            )),
            Err(error) => {
                scratch.checks.clear();
                Err(error)
            }
        }
    }

    /// Resolves when every frame written before the call is on disk: at once when
    /// none waits, else at the end of the group commit that holds the last of them.
    /// Gives the error that ended the buffer when it ended before they were on disk.
    /// Commits run without this future, so a caller may drop it. Call
    /// [`woken`](Self::woken) after it resolves.
    pub(crate) fn committed(&self) -> buffer::Commit<'_> {
        self.buffer.committed()
    }

    /// The first seq on `path` of the index at `slot` that is not on disk.
    pub(crate) fn stored(&self, slot: Slot, path: Path) -> u64 {
        self.buffer.durable(slot, path).seq
    }

    /// Opens an unnamed reader on the index at `slot`, at mesh time `mesh`. A complete
    /// reader starts at the index's live tail.
    ///
    /// # Panics
    ///
    /// If the shard does not carry `slot`.
    pub(crate) fn open_reader(
        &mut self,
        slot: Slot,
        mode: Mode,
        mesh: Interval,
    ) -> reader::Key {
        let place = self.place(slot);
        let index = &mut self.indexes[place];
        let (session, woken) = match mode {
            Mode::Complete { limit_bytes } => (index.open_complete(limit_bytes), false),
            Mode::Latest => {
                let latest = index.readers.open_latest(None, mesh.latest);
                (latest.key, latest.woken)
            }
        };
        let key = reader::Key { slot, session };
        if woken {
            self.wake.keys.push(key);
        }
        key
    }

    /// Raises the credit of a complete reader to `limit_bytes` since it opened. A
    /// limit that is not higher changes nothing, and so does a grant to a reader that
    /// is not open: a grant can arrive after its reader closes.
    ///
    /// # Panics
    ///
    /// If the shard does not carry the reader's index, or its readers never gave the
    /// reader's session.
    pub(crate) fn grant(&mut self, key: reader::Key, limit_bytes: u64) {
        self.index(key).readers.grant(key.session, limit_bytes);
    }

    /// Takes the reader's next frame, or `None` when none waits.
    ///
    /// # Panics
    ///
    /// If the reader is not open.
    pub(crate) fn take(&mut self, key: reader::Key) -> Option<Frame> {
        self.index(key).readers.take(key.session)
    }

    /// Closes the reader at mesh time `mesh`. Its waiting frames do not go out, and
    /// [`woken`](Self::woken) does not name it.
    ///
    /// # Panics
    ///
    /// If the reader is not open.
    pub(crate) fn close_reader(&mut self, key: reader::Key, mesh: Interval) {
        self.index(key).readers.close(key.session, mesh.latest);
        self.wake.keys.retain(|&woken| woken != key);
    }

    /// Replaces `keys` with the readers to wake since the last call, sorted, each
    /// once. Complete readers first get the live frames now on disk. A key is a hint:
    /// take from each until [`take`](Self::take) gives `None`. Call it after each
    /// write and each commit. When a commit ended since the last call, it reads each
    /// index with live frames queued for complete readers; else it reads none.
    pub(crate) fn woken(&mut self, keys: &mut Vec<reader::Key>) {
        self.wake.settle(&self.buffer, &mut self.indexes);
        keys.clear();
        mem::swap(keys, &mut self.wake.keys);
        keys.sort_unstable();
        keys.dedup();
    }

    fn index(&mut self, key: reader::Key) -> &mut Index {
        let place = self.place(key.slot);
        &mut self.indexes[place]
    }

    /// The place in `indexes` of the index at `slot`.
    ///
    /// # Panics
    ///
    /// If the shard does not carry `slot`.
    fn place(&self, slot: Slot) -> usize {
        let Some(&place) = self.places.get(&slot) else {
            panic!("the shard does not carry the index at {slot:?}");
        };
        place
    }

    /// Appends the unrecorded handoff of each index of `session`.
    fn record_all(&mut self, session: &Session, mesh: Interval) {
        let groups = (0..session.claims.len()).map(|group| {
            u32::try_from(group).expect("invariant: a key set has u32 groups")
        });
        // A handoff with no room waits, and a failed commit fails the next write.
        drop(record(
            &self.buffer,
            &self.pool,
            &mut self.indexes,
            session,
            groups,
            mesh,
        ));
    }
}

impl Wake {
    /// Adds the `sessions` of the index at `slot` to wake.
    fn add(&mut self, slot: Slot, sessions: &[delivery::Key]) {
        if sessions.is_empty() {
            return;
        }
        let keys = sessions
            .iter()
            .map(|&session| reader::Key { slot, session });
        self.keys.extend(keys);
    }

    /// Lists the index at `slot` and `place`, whose readers are pending, once.
    fn list(&mut self, slot: Slot, place: usize) {
        if place >= self.listed.len() {
            self.listed.resize(place + 1, false);
        }
        if !mem::replace(&mut self.listed[place], true) {
            self.pending.push((slot, place));
        }
    }

    /// Gives complete readers the live frames on disk. Reads no index when no commit
    /// ended since the last settle, as only a commit moves `durable`.
    fn settle(&mut self, buffer: &Buffer, indexes: &mut [Index]) {
        let commits = buffer.commits();
        if mem::replace(&mut self.commits, commits) == commits {
            return;
        }
        let mut pending = mem::take(&mut self.pending);
        pending.retain(|&(slot, place)| {
            let readers = &mut indexes[place].readers;
            self.add(slot, readers.release(buffer.durable(slot, Path::Live).seq));
            self.listed[place] = readers.pending();
            self.listed[place]
        });
        self.pending = pending;
    }
}

impl Session {
    /// The writer's claim on the index of `group`, and the index's entry in the key
    /// set.
    fn claim(&self, group: u32) -> (Claim, &key_set::Entry) {
        let group = usize::try_from(group).expect("invariant: a usize holds a u32");
        let entry = &self.set.entries()[self.set.groups()[group]];
        (self.claims[group], entry)
    }
}

/// Freezes the index frame from `split` of each accepted group of `checks`, and pushes
/// its stored entry onto `entries`, in group order, at mesh time `stored_at`.
///
/// # Errors
///
/// [`block::Error`] when `pool` has no block for an index frame or a header.
fn freeze(
    entries: &mut Vec<Entry>,
    pool: &block::Pool,
    split: &mut Split<'_>,
    set: &KeySet,
    checks: &mut [(u32, Result<Accepted, Refusal>)],
    stored_at: Stamp,
) -> Result<(), block::Error> {
    for (group, checked) in checks {
        if let Ok(accepted) = checked {
            let draft = split.frame(pool, *group)?;
            let last = accepted.last();
            let frame = accepted.freeze(draft, *group);
            entries.push(stored::entry(pool, frame, set, last, stored_at)?);
        }
    }
    Ok(())
}

/// Appends the unrecorded handoff of the index of each of `groups` of `session`, each
/// alone, on the live path at mesh time `mesh`, and marks it recorded. A handoff that
/// finds no room in the ring or the pool waits for the next append on its index.
/// Returns whether every handoff was recorded.
///
/// # Errors
///
/// [`Error::Disk`] after a failed commit.
fn record(
    buffer: &Buffer,
    pool: &block::Pool,
    indexes: &mut [Index],
    session: &Session,
    groups: impl Iterator<Item = u32>,
    mesh: Interval,
) -> Result<bool, Error> {
    let mut all = true;
    for group in groups {
        let (claim, entry) = session.claim(group);
        let index = &mut indexes[claim.place];
        let Some((handoff, first)) = index.handoff() else {
            continue;
        };
        let appended = match handoff::entry(pool, handoff, entry, first, mesh.latest) {
            Ok(handoff) => buffer.append([handoff]),
            Err(block::Error::Exhausted { .. } | block::Error::Refused { .. }) => {
                all = false;
                continue;
            }
            Err(error @ block::Error::TooLarge { .. }) => {
                panic!("invariant: the pool of the ring holds a handoff: {error}")
            }
        };
        if let Err(buffer::Rejected::Large(limit)) = appended {
            panic!("invariant: a record holds one handoff: {limit}");
        }
        if room(appended)? {
            index.gate.recorded();
        } else {
            all = false;
        }
    }
    Ok(all)
}

/// Spends the seq of each accepted group of `checks`, and makes the outcome of each
/// present group into `out`: applied when the append found `room`, else lost. The
/// readers to wake and each index that queued a live frame go into `wake`.
fn spend<'a>(
    checks: &mut Vec<(u32, Result<Accepted, Refusal>)>,
    indexes: &mut [Index],
    session: &Session,
    room: bool,
    wake: &mut Wake,
    out: &'a mut Vec<Outcome>,
) -> &'a [Outcome] {
    out.clear();
    for (group, checked) in checks.drain(..) {
        let (claim, entry) = session.claim(group);
        let slot = entry.slot;
        let index = &mut indexes[claim.place];
        out.push(match checked {
            Ok(accepted) if room => {
                let range = range(&accepted);
                wake.add(slot, index.advance(accepted));
                if index.readers.pending() {
                    wake.list(slot, claim.place);
                }
                Outcome::Applied { slot, range }
            }
            Ok(accepted) => {
                let range = range(&accepted);
                wake.add(slot, index.lose(accepted));
                Outcome::Lost { slot, range }
            }
            Err(refusal) => Outcome::Refused { slot, refusal },
        });
    }
    out
}

/// The seq range of an accepted group.
fn range(accepted: &Accepted) -> frame::Range {
    let seq = accepted.seq();
    let count = u32::try_from(seq.end - seq.start)
        .expect("invariant: a group holds at most u32::MAX samples");
    frame::Range {
        seq: seq.start,
        count,
    }
}

/// Whether an append found room in the ring and the pool.
///
/// # Errors
///
/// [`Error::Large`] when no record holds the batch, and [`Error::Disk`] after a
/// failed commit.
fn room(appended: Result<(), buffer::Rejected>) -> Result<bool, Error> {
    match appended {
        Ok(()) => Ok(true),
        Err(buffer::Rejected::Full { .. } | buffer::Rejected::Pool(_)) => Ok(false),
        Err(buffer::Rejected::Large(_)) => Err(Error::Large),
        Err(buffer::Rejected::Files(error)) => Err(Error::Disk(error)),
    }
}

#[cfg(test)]
mod tests {
    use std::iter;
    use std::path::{Path as FilePath, PathBuf};

    use block::{Heap, Pool, Unique};
    use buffer::Layout;
    use env::clock::Clock;
    use env::entropy::Entropy;
    use env::files::{Mode, Operation};
    use env::tasks::Tasks;
    use types::authority::Authority;
    use types::channel::{self, Slots};
    use types::frame::Form;
    use types::frame::Range;
    use types::frame::key_set::Group;
    use types::name::Name;
    use types::sample::{Scalar, Type};
    use types::time::Span;

    use super::*;
    use crate::common::{interner, key, pool};

    const DIR: &str = "shard-0";
    const RING: &str = "shard-0/ring";
    const BLOCK: usize = 4096;
    /// A ring with room for each test but the full ring.
    const AREA: u64 = 1 << 18;
    /// A body that keeps a record in one block.
    const BODY_MAX: usize = 4087;
    const POOL: usize = 1 << 21;
    const COMMIT: Span = Span::from_nanos(10_000_000);
    /// Past the first commit interval, while its sync runs.
    const SYNC: Span = Span::from_nanos(10_001_000);
    const LIMITS: order::Config = order::Config {
        earliest: Stamp::from_nanos(1),
        ahead: Span::from_nanos(1_000_000_000),
    };
    const NOW: Monotonic = Monotonic(0);
    const MESH: Interval = Interval {
        earliest: Stamp::from_nanos(1_000),
        latest: Stamp::from_nanos(2_000),
    };
    const LIVE: Label = Label::Path(Path::Live);
    const BACKFILL: Label = Label::Path(Path::Backfill);
    /// Indexes whose handoffs to a 255-byte subject no record of `BODY_MAX` holds
    /// together.
    const WIDE: u32 = 14;

    /// What one test gets on its shard.
    struct Test {
        node: sim::node::Node,
        pool: Rc<Pool>,
        clock: Clock,
        tasks: Tasks,
        entropy: Entropy,
    }

    impl Test {
        /// A shard over the ring of the node, made with `area` bytes when it is new,
        /// with slots 0 to `slots` assigned and no index carried.
        async fn open(&self, area: u64, slots: u32) -> Shard {
            let buffer = self.buffer(area, BODY_MAX, slots).await;
            Shard::new(buffer, Rc::clone(&self.pool), LIMITS)
        }

        /// The ring of the node, made with `area` bytes and bodies of `body_max` when
        /// it is new, with slots 0 to `slots` assigned.
        async fn buffer(&self, area: u64, body_max: usize, slots: u32) -> Buffer {
            let config = buffer::Config {
                files: self.node.files(),
                dir: PathBuf::from(DIR),
                pool: Rc::clone(&self.pool),
                clock: self.clock.clone(),
                tasks: self.tasks.clone(),
                entropy: self.entropy.clone(),
                layout: Layout::new(area, body_max).expect("a ring"),
                commit: COMMIT,
            };
            let mut assigned = Slots::new();
            for n in 0..slots {
                assigned.assign(key(Slot::new(n)));
            }
            Buffer::open(config, &mut assigned).await.expect("opens")
        }

        /// A shard as [`open`](Self::open) makes, that carries the indexes of
        /// [`two_indexes`].
        async fn shard(&self, area: u64) -> Shard {
            let mut shard = self.open(area, 4).await;
            shard.carry(Slot::new(0));
            shard.carry(Slot::new(2));
            shard
        }

        /// A shard that carries `count` indexes with no data channels, at slots 0 to
        /// `count`, and their key set.
        async fn wide(&self, count: u32) -> (Shard, Arc<KeySet>) {
            let mut shard = self.open(AREA, count).await;
            let groups: Vec<_> = (0..count)
                .map(|n| {
                    shard.carry(Slot::new(n));
                    Group {
                        index: key(Slot::new(n)),
                        data: &[],
                    }
                })
                .collect();
            (shard, interner().intern(&groups))
        }

        /// The bytes of the ring file.
        async fn ring(&self) -> Vec<u8> {
            let files = self.node.files();
            let file = files
                .open(FilePath::new(RING), Mode::Read)
                .await
                .expect("opens");
            let mut bytes = Vec::new();
            for offset in (0..file.len()).step_by(BLOCK) {
                let block = self.pool.alloc(BLOCK).expect("the pool has a block");
                let block = file.read_at(offset, block).await.expect("reads");
                bytes.extend_from_slice(&block);
            }
            bytes
        }

        /// Takes every block of the pool, of each size class.
        fn fill(&self) -> Vec<Unique> {
            let mut blocks = Vec::new();
            let mut len = self.pool.largest();
            while len > 0 {
                while let Ok(block) = self.pool.alloc(len) {
                    blocks.push(block);
                }
                len -= len.div_ceil(16);
            }
            blocks
        }
    }

    /// Starts one shard of one node to run `main`. A panic in `main` comes back from
    /// the run as [`sim::Error::Panicked`].
    fn start<F>(
        seed: u64,
        main: impl FnOnce(Test) -> F + Send + 'static,
    ) -> (sim::Sim, env::thread::Handle)
    where
        F: Future<Output = ()> + 'static,
    {
        let mut sim = sim::Sim::new(sim::Config {
            seed,
            ..sim::Config::default()
        });
        let node = sim.node(sim::node::Config::default());
        let config = env::shards::Config {
            name: DIR.into(),
            core: None,
        };
        let handle = node
            .shards()
            .start(config, move |tasks| {
                let config = block::Config { budget: POOL };
                let pool = Pool::new(config.clone(), Heap::new(config.reservation()));
                main(Test {
                    clock: node.clock(),
                    entropy: node.entropy(),
                    node,
                    pool: Rc::new(pool),
                    tasks,
                })
            })
            .expect("the shard starts");
        (sim, handle)
    }

    fn run<F>(seed: u64, main: impl FnOnce(Test) -> F + Send + 'static)
    where
        F: Future<Output = ()> + 'static,
    {
        let (mut sim, handle) = start(seed, main);
        sim.run().expect("the run ends");
        handle.join().expect("the shard ended");
    }

    /// Two indexes: slot 0 with an `i64` channel at slot 1, then slot 2 alone.
    fn two_indexes() -> Arc<KeySet> {
        interner().intern(&[
            Group {
                index: key(Slot::new(0)),
                data: &[(key(Slot::new(1)), Type::Scalar(Scalar::I64))],
            },
            Group {
                index: key(Slot::new(2)),
                data: &[],
            },
        ])
    }

    fn writer(subject: &str, authority: u8, set: &Arc<KeySet>) -> Writer {
        Writer {
            control: control::Writer {
                subject: subject.parse().expect("a valid name"),
                authority: Authority(authority),
            },
            lease: None,
            set: Arc::clone(set),
        }
    }

    /// A raw frame of `set` with each series of `series`, an entry and its values.
    /// The count of each group is the length of its index series.
    fn frame(pool: &Pool, set: &KeySet, series: &[(usize, &[i64])]) -> Draft {
        let lens: Vec<_> = series
            .iter()
            .map(|&(entry, values)| (entry, values.len() * 8))
            .collect();
        let mut draft = Draft::new(pool, set, Form::Raw, &lens).expect("a frame");
        for &(entry, values) in series {
            let bytes = draft.series_mut(entry).expect("the series is present");
            for (bytes, value) in bytes.as_chunks_mut::<8>().0.iter_mut().zip(values) {
                *bytes = value.to_le_bytes();
            }
            if set.index(entry) == entry {
                let count = u32::try_from(values.len()).expect("a short frame");
                draft.set_count(set.entries()[entry].group, count);
            }
        }
        draft
    }

    /// `stamps` encoded as an index series.
    fn encoded(stamps: &[i64]) -> Vec<u8> {
        let values: Vec<u8> = stamps.iter().flat_map(|s| s.to_le_bytes()).collect();
        let mut out =
            vec![0; codec::max_len(Type::Scalar(Scalar::Stamp), values.len())];
        let len = codec::Encoder::new(Type::Scalar(Scalar::Stamp))
            .encode(stamps.len(), &values, &mut out)
            .expect("stamps");
        out.truncate(len);
        out
    }

    fn applied(slot: u32, seq: u64, count: u32) -> Outcome {
        Outcome::Applied {
            slot: Slot::new(slot),
            range: Range { seq, count },
        }
    }

    fn lost(slot: u32, seq: u64, count: u32) -> Outcome {
        Outcome::Lost {
            slot: Slot::new(slot),
            range: Range { seq, count },
        }
    }

    fn refused(slot: u32, refusal: Refusal) -> Outcome {
        Outcome::Refused {
            slot: Slot::new(slot),
            refusal,
        }
    }

    /// Where each copy of `needle` starts in `bytes`.
    fn find(bytes: &[u8], needle: &[u8]) -> Vec<usize> {
        let windows = bytes.windows(needle.len()).enumerate();
        windows
            .filter(|(_, window)| *window == needle)
            .map(|(at, _)| at)
            .collect()
    }

    /// `count` values that no codec makes smaller.
    fn scattered(count: usize) -> Vec<i64> {
        let next = |x: &i64| {
            x.wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407)
        };
        iter::successors(Some(1), |x| Some(next(x)))
            .take(count)
            .collect()
    }

    /// The bytes of a handoff to `subject` at authority 1.
    fn handoff_to(subject: &str) -> Vec<u8> {
        [&[1], subject.as_bytes()].concat()
    }

    /// The length of an entry header in a record's table.
    const HEADER_LEN: usize = 51;
    /// Where the `stored_at` stamp starts in an entry header.
    const STAMP_AT: usize = 29;
    /// A mesh time whose stamp bytes appear nowhere else in a ring.
    const MARKED: Interval = Interval {
        earliest: Stamp::from_nanos(1),
        latest: Stamp::from_nanos(0x0FED_CBA9_8765_4321),
    };

    /// The entry header in `ring` whose `stored_at` stamp starts at `stamp`.
    fn header(ring: &[u8], stamp: usize) -> &[u8; HEADER_LEN] {
        let start = stamp
            .checked_sub(STAMP_AT)
            .expect("a header holds the stamp");
        ring[start..].first_chunk().expect("a whole header")
    }

    /// The index, path byte, first, len, and tag of each entry header in `ring`
    /// stamped `stored_at`, in ring order.
    fn headers(ring: &[u8], stored_at: Stamp) -> Vec<(u128, u8, u64, u32, u8)> {
        find(ring, &stored_at.nanos().to_le_bytes())
            .into_iter()
            .map(|at| {
                let header = header(ring, at);
                (
                    u128::from_le_bytes(header[..16].try_into().expect("16 bytes")),
                    header[16],
                    u64::from_le_bytes(header[17..25].try_into().expect("8 bytes")),
                    u32::from_le_bytes(header[25..29].try_into().expect("4 bytes")),
                    header[46],
                )
            })
            .collect()
    }

    /// The tag and body of each entry of the one record in `ring` whose entries are
    /// all stamped `stored_at`, in order.
    fn bodies(ring: &[u8], stored_at: Stamp) -> Vec<(u8, Vec<u8>)> {
        let stamps = find(ring, &stored_at.nanos().to_le_bytes());
        let table = stamps[0] - STAMP_AT;
        let count = u32::from_le_bytes(ring[table - 4..table].try_into().expect("4"));
        assert_eq!(usize::try_from(count), Ok(stamps.len()), "one record");
        let mut at = table + stamps.len() * HEADER_LEN;
        stamps
            .iter()
            .map(|&stamp| {
                let header = header(ring, stamp);
                let len = u32::from_le_bytes(header[47..].try_into().expect("4 bytes"));
                let body = &ring[at..][..usize::try_from(len).expect("a short body")];
                at += body.len();
                (header[46], body.to_vec())
            })
            .collect()
    }

    #[test]
    fn gives_each_index_gapless_seq_stored_after_the_commit() {
        run(1, |test| async move {
            let set = two_indexes();
            let mut shard = test.shard(AREA).await;
            let a = shard.open_writer(writer("a", 1, &set), NOW, MESH);
            let both = frame(
                &test.pool,
                &set,
                &[(0, &[10, 20]), (1, &[1, 2]), (2, &[15])],
            );
            assert_eq!(
                shard.write(a, LIVE, both, NOW, MESH),
                Ok(&[applied(0, 0, 2), applied(2, 0, 1)][..])
            );
            let one = frame(&test.pool, &set, &[(0, &[30]), (1, &[3])]);
            assert_eq!(
                shard.write(a, LIVE, one, NOW, MESH),
                Ok(&[applied(0, 2, 1)][..])
            );
            assert_eq!(shard.stored(Slot::new(0), Path::Live), 0);
            shard.committed().await.expect("the commit ends");
            assert_eq!(shard.stored(Slot::new(0), Path::Live), 3);
            assert_eq!(shard.stored(Slot::new(2), Path::Live), 1);
            assert_eq!(shard.stored(Slot::new(0), Path::Backfill), 0);
        });
    }

    #[test]
    fn continues_each_path_from_its_tail_in_the_buffer() {
        run(2, |test| async move {
            let set = two_indexes();
            let mut shard = test.shard(AREA).await;
            let a = shard.open_writer(writer("a", 1, &set), NOW, MESH);
            let write = frame(&test.pool, &set, &[(0, &[10, 20]), (1, &[1, 2])]);
            shard.write(a, LIVE, write, NOW, MESH).expect("written");
            shard.committed().await.expect("the commit ends");
            drop(shard);
            let mut shard = test.shard(AREA).await;
            let a = shard.open_writer(writer("a", 1, &set), NOW, MESH);
            let late = frame(&test.pool, &set, &[(0, &[15]), (1, &[3])]);
            let backwards = order::Error::Backwards {
                path: Path::Live,
                before: Stamp::from_nanos(20),
                stamp: Stamp::from_nanos(15),
            };
            assert_eq!(
                shard.write(a, LIVE, late, NOW, MESH),
                Ok(&[refused(0, Refusal::Order(backwards))][..])
            );
            let next = frame(&test.pool, &set, &[(0, &[30]), (1, &[3])]);
            assert_eq!(
                shard.write(a, LIVE, next, NOW, MESH),
                Ok(&[applied(0, 2, 1)][..])
            );
        });
    }

    #[test]
    fn refuses_a_writer_without_control_before_its_series_and_spends_no_seq() {
        run(3, |test| async move {
            let set = two_indexes();
            let mut shard = test.shard(AREA).await;
            let a = shard.open_writer(writer("a", 2, &set), NOW, MESH);
            let b = shard.open_writer(writer("b", 1, &set), NOW, MESH);
            let short =
                frame(&test.pool, &set, &[(0, &[10, 20]), (1, &[1]), (2, &[10])]);
            let waiting = Refusal::Control(control::Error::Waiting);
            assert_eq!(
                waiting.to_string(),
                "not in control: another writer holds the gate"
            );
            assert_eq!(
                shard.write(b, LIVE, short, NOW, MESH),
                Ok(&[refused(0, waiting.clone()), refused(2, waiting)][..])
            );
            let write = frame(&test.pool, &set, &[(2, &[10])]);
            assert_eq!(
                shard.write(a, LIVE, write, NOW, MESH),
                Ok(&[applied(2, 0, 1)][..])
            );
        });
    }

    #[test]
    fn refuses_a_backwards_index_and_applies_the_other() {
        run(4, |test| async move {
            let set = two_indexes();
            let mut shard = test.shard(AREA).await;
            let a = shard.open_writer(writer("a", 1, &set), NOW, MESH);
            let write = frame(&test.pool, &set, &[(0, &[20]), (1, &[1]), (2, &[20])]);
            shard.write(a, LIVE, write, NOW, MESH).expect("written");
            let write = frame(&test.pool, &set, &[(0, &[10]), (1, &[2]), (2, &[30])]);
            let backwards = order::Error::Backwards {
                path: Path::Live,
                before: Stamp::from_nanos(20),
                stamp: Stamp::from_nanos(10),
            };
            assert_eq!(
                shard.write(a, LIVE, write, NOW, MESH),
                Ok(&[refused(0, Refusal::Order(backwards)), applied(2, 1, 1)][..])
            );
        });
    }

    #[test]
    fn refuses_a_backwards_stamp_in_a_later_vector_of_its_index() {
        run(46, |test| async move {
            let set = two_indexes();
            let mut shard = test.shard(AREA).await;
            let a = shard.open_writer(writer("a", 1, &set), NOW, MESH);
            let mut stamps: Vec<i64> = (10..2510).collect();
            stamps[2100] = 5;
            let data = vec![0; stamps.len()];
            let series = [(0, &stamps[..]), (1, &data[..]), (2, &[10][..])];
            let write = frame(&test.pool, &set, &series);
            let backwards = order::Error::Backwards {
                path: Path::Live,
                before: Stamp::from_nanos(2109),
                stamp: Stamp::from_nanos(5),
            };
            assert_eq!(
                shard.write(a, LIVE, write, NOW, MESH),
                Ok(&[refused(0, Refusal::Order(backwards)), applied(2, 0, 1)][..])
            );
        });
    }

    #[test]
    fn refuses_an_encoded_index_whose_later_vector_is_not_valid() {
        run(47, |test| async move {
            let set = two_indexes();
            let mut shard = test.shard(AREA).await;
            let a = shard.open_writer(writer("a", 1, &set), NOW, MESH);
            let stamps: Vec<i64> = (10..2510).collect();
            let mut index = encoded(&stamps);
            index[encoded(&stamps[..1024]).len()] = 9;
            let other = encoded(&[10]);
            let lens = [(0, index.len()), (2, other.len())];
            let mut write =
                Draft::new(&test.pool, &set, Form::Encoded, &lens).expect("a frame");
            write
                .series_mut(0)
                .expect("index 0")
                .copy_from_slice(&index);
            write
                .series_mut(2)
                .expect("index 2")
                .copy_from_slice(&other);
            write.set_count(0, 2500);
            write.set_count(1, 1);
            let refusal = Refusal::Codec(split::Error {
                channel: key(Slot::new(0)),
                error: codec::Error::Tag { vector: 1, tag: 9 },
            });
            assert_eq!(
                shard.write(a, LIVE, write, NOW, MESH),
                Ok(&[refused(0, refusal), applied(2, 0, 1)][..])
            );
            let write = frame(&test.pool, &set, &[(0, &[11])]);
            assert_eq!(
                shard.write(a, LIVE, write, NOW, MESH),
                Ok(&[applied(0, 0, 1)][..])
            );
        });
    }

    #[test]
    fn refuses_an_encoded_index_that_is_not_valid_before_its_order() {
        run(48, |test| async move {
            let set = two_indexes();
            let mut shard = test.shard(AREA).await;
            let a = shard.open_writer(writer("a", 1, &set), NOW, MESH);
            let mut stamps: Vec<i64> = (10..2510).collect();
            stamps[5] = 1;
            let mut index = encoded(&stamps);
            index[encoded(&stamps[..1024]).len()] = 9;
            let other = encoded(&[10]);
            let lens = [(0, index.len()), (2, other.len())];
            let mut write =
                Draft::new(&test.pool, &set, Form::Encoded, &lens).expect("a frame");
            write
                .series_mut(0)
                .expect("index 0")
                .copy_from_slice(&index);
            write
                .series_mut(2)
                .expect("index 2")
                .copy_from_slice(&other);
            write.set_count(0, 2500);
            write.set_count(1, 1);
            let refusal = Refusal::Codec(split::Error {
                channel: key(Slot::new(0)),
                error: codec::Error::Tag { vector: 1, tag: 9 },
            });
            assert_eq!(
                shard.write(a, LIVE, write, NOW, MESH),
                Ok(&[refused(0, refusal), applied(2, 0, 1)][..])
            );
        });
    }

    #[test]
    fn refuses_a_series_that_does_not_fit_its_count_and_names_its_channel() {
        run(5, |test| async move {
            let set = two_indexes();
            let mut shard = test.shard(AREA).await;
            let a = shard.open_writer(writer("a", 1, &set), NOW, MESH);
            let short =
                frame(&test.pool, &set, &[(0, &[10, 20]), (1, &[1]), (2, &[10])]);
            let refusal = Refusal::Codec(split::Error {
                channel: key(Slot::new(1)),
                error: codec::Error::Length {
                    expected: 16,
                    actual: 8,
                },
            });
            assert_eq!(
                refusal.to_string(),
                "channel 01000000-0000-0000-0000-000000000001: the values hold 8 bytes, \
                 but the samples take 16"
            );
            assert_eq!(
                shard.write(a, LIVE, short, NOW, MESH),
                Ok(&[refused(0, refusal), applied(2, 0, 1)][..])
            );
            let write = frame(&test.pool, &set, &[(0, &[10]), (1, &[1])]);
            assert_eq!(
                shard.write(a, LIVE, write, NOW, MESH),
                Ok(&[applied(0, 0, 1)][..])
            );
        });
    }

    #[test]
    fn loses_live_frames_and_refuses_a_backfill_frame_when_the_ring_is_full() {
        run(6, |test| async move {
            let set = two_indexes();
            let mut shard = test.shard(1 << 16).await;
            let a = shard.open_writer(writer("a", 1, &set), NOW, MESH);
            let mut outcomes = Vec::new();
            for n in 0..24_i64 {
                let stamps: Vec<i64> = (0..400).map(|k| 10 + n * 400 + k).collect();
                let values = scattered(400);
                let write = frame(&test.pool, &set, &[(0, &stamps), (1, &values)]);
                let written = shard.write(a, LIVE, write, NOW, MESH).expect("written");
                outcomes.extend_from_slice(written);
                if matches!(written, [Outcome::Applied { .. }]) {
                    shard.committed().await.expect("the commit ends");
                }
            }
            let first = outcomes
                .iter()
                .position(|outcome| matches!(outcome, Outcome::Lost { .. }))
                .expect("the ring fills");
            assert!(first > 0, "the first frame fits");
            for (n, outcome) in (0_u64..).zip(&outcomes) {
                let expected = if n < u64::try_from(first).expect("small") {
                    applied(0, n * 400, 400)
                } else {
                    lost(0, n * 400, 400)
                };
                assert_eq!(*outcome, expected, "frame {n}");
            }
            let write = frame(&test.pool, &set, &[(0, &[1]), (1, &[1])]);
            assert_eq!(shard.write(a, BACKFILL, write, NOW, MESH), Err(Error::Full));
            assert_eq!(
                Error::Full.to_string(),
                "the ring or the pool has no room for the frame"
            );
        });
    }

    #[test]
    fn loses_a_live_frame_and_refuses_a_backfill_frame_when_the_pool_is_full() {
        run(7, |test| async move {
            let set = two_indexes();
            let mut shard = test.shard(AREA).await;
            let a = shard.open_writer(writer("a", 1, &set), NOW, MESH);
            let live = frame(&test.pool, &set, &[(0, &[10, 20]), (1, &[1, 2])]);
            let backfill = frame(&test.pool, &set, &[(0, &[1, 2]), (1, &[1, 2])]);
            let blocks = test.fill();
            assert_eq!(
                shard.write(a, LIVE, live, NOW, MESH),
                Ok(&[lost(0, 0, 2)][..])
            );
            assert_eq!(
                shard.write(a, BACKFILL, backfill, NOW, MESH),
                Err(Error::Full)
            );
            drop(blocks);
            let backfill = frame(&test.pool, &set, &[(0, &[1, 2]), (1, &[1, 2])]);
            assert_eq!(
                shard.write(a, BACKFILL, backfill, NOW, MESH),
                Ok(&[applied(0, 0, 2)][..])
            );
            let live = frame(&test.pool, &set, &[(0, &[30]), (1, &[3])]);
            assert_eq!(
                shard.write(a, LIVE, live, NOW, MESH),
                Ok(&[applied(0, 2, 1)][..])
            );
        });
    }

    #[test]
    fn loses_a_live_frame_when_the_pool_has_no_block_for_its_record() {
        run(15, |test| async move {
            let set = two_indexes();
            let mut shard = test.shard(AREA).await;
            let live = frame(&test.pool, &set, &[(0, &[10]), (1, &[1])]);
            let backfill = frame(&test.pool, &set, &[(0, &[1]), (1, &[1])]);
            let room: Vec<_> = (0..4)
                .map(|_| test.pool.alloc(BLOCK).expect("a block"))
                .collect();
            let blocks = test.fill();
            drop(room);
            let a = shard.open_writer(writer("a", 1, &set), NOW, MESH);
            assert_eq!(
                shard.write(a, LIVE, live, NOW, MESH),
                Ok(&[lost(0, 0, 1)][..])
            );
            assert_eq!(
                shard.write(a, BACKFILL, backfill, NOW, MESH),
                Err(Error::Full)
            );
            drop(blocks);
        });
    }

    #[test]
    fn stores_no_group_of_a_frame_whose_blocks_ran_out_partway() {
        run(16, |test| async move {
            let set = two_indexes();
            let mut shard = test.shard(AREA).await;
            let stamps: Vec<_> = (1..=100).collect();
            let series: [(usize, &[i64]); 3] = [(0, &[10]), (1, &[1]), (2, &stamps)];
            let live = frame(&test.pool, &set, &series);
            // The split frees the raw frame. Its twin keeps that size in use, so no
            // other size takes the room.
            let twin = frame(&test.pool, &set, &series);
            // The open record keeps its header block, so the append needs no block.
            let a = shard.open_writer(writer("a", 1, &set), NOW, MESH);
            let room = [80, 56].map(|len| test.pool.alloc(len).expect("a block"));
            let blocks = test.fill();
            drop(room);
            assert_eq!(
                shard.write(a, LIVE, live, NOW, MESH),
                Ok(&[lost(0, 0, 1), lost(2, 0, 100)][..])
            );
            drop((twin, blocks));
            shard.committed().await.expect("the commit ends");
            assert_eq!(shard.stored(Slot::new(0), Path::Live), 0);
        });
    }

    #[test]
    fn refuses_a_frame_too_large_for_one_write_and_spends_nothing() {
        run(38, |test| async move {
            let set = two_indexes();
            let mut shard = test.shard(AREA).await;
            let a = shard.open_writer(writer("a", 1, &set), NOW, MESH);
            let stamps: Vec<i64> = (10..610).collect();
            let values = scattered(600);
            for (label, stamp) in [(LIVE, 700), (BACKFILL, 5)] {
                let large = frame(&test.pool, &set, &[(0, &stamps), (1, &values)]);
                assert_eq!(shard.write(a, label, large, NOW, MESH), Err(Error::Large));
                let small = frame(&test.pool, &set, &[(0, &[stamp]), (1, &[1])]);
                assert_eq!(
                    shard.write(a, label, small, NOW, MESH),
                    Ok(&[applied(0, 0, 1)][..])
                );
            }
            assert_eq!(
                Error::Large.to_string(),
                "the frame is too large for one write: split it"
            );
        });
    }

    #[test]
    fn refuses_a_frame_whose_group_no_block_of_the_shard_holds() {
        run(45, |test| async move {
            let set = two_indexes();
            let mut shard = test.shard(AREA).await;
            let a = shard.open_writer(writer("a", 1, &set), NOW, MESH);
            // The shard's largest block is 1835008 bytes. The writer's pool has larger
            // blocks, and scattered values do not compress.
            let writers = pool(4 * POOL);
            let len = 240_000;
            let stamps: Vec<i64> = (10..).take(len).collect();
            let values = scattered(len);
            for (label, stamp) in [(LIVE, 300_000), (BACKFILL, 5)] {
                let series: [(usize, &[i64]); 3] =
                    [(0, &stamps), (1, &values), (2, &[stamp])];
                let large = frame(&writers, &set, &series);
                assert_eq!(shard.write(a, label, large, NOW, MESH), Err(Error::Large));
                let small = frame(&test.pool, &set, &[(0, &[stamp]), (1, &[1])]);
                assert_eq!(
                    shard.write(a, label, small, NOW, MESH),
                    Ok(&[applied(0, 0, 1)][..])
                );
            }
        });
    }

    #[test]
    fn records_a_waiting_handoff_before_a_frame_too_large_for_one_write() {
        run(39, |test| async move {
            let set = two_indexes();
            let mut shard = test.shard(AREA).await;
            let stamps: Vec<i64> = (10..610).collect();
            let large = frame(&test.pool, &set, &[(0, &stamps), (1, &scattered(600))]);
            let blocks = test.fill();
            let a = shard.open_writer(writer("subject-a", 1, &set), NOW, MESH);
            drop(blocks);
            assert_eq!(shard.write(a, LIVE, large, NOW, MESH), Err(Error::Large));
            shard.committed().await.expect("the commit ends");
            let handoff = handoff_to("subject-a");
            assert_eq!(find(&test.ring().await, &handoff).len(), 1);
            let small = frame(&test.pool, &set, &[(0, &[700]), (1, &[1])]);
            assert_eq!(
                shard.write(a, LIVE, small, NOW, MESH),
                Ok(&[applied(0, 0, 1)][..])
            );
            shard.committed().await.expect("the commit ends");
            assert_eq!(find(&test.ring().await, &handoff).len(), 1);
        });
    }

    #[test]
    fn loses_a_large_live_frame_whose_handoff_finds_no_room() {
        run(40, |test| async move {
            let set = two_indexes();
            let mut shard = test.shard(1 << 16).await;
            let a = shard.open_writer(writer("a", 1, &set), NOW, MESH);
            let mut stamp = 10;
            loop {
                let write = frame(&test.pool, &set, &[(2, &[stamp])]);
                let written = shard.write(a, LIVE, write, NOW, MESH).expect("written");
                stamp += 1;
                if matches!(written, [Outcome::Lost { .. }]) {
                    break;
                }
                shard.committed().await.expect("the commit ends");
            }
            let stamps: Vec<i64> = (10..610).collect();
            let large =
                || frame(&test.pool, &set, &[(0, &stamps), (1, &scattered(600))]);
            assert_eq!(shard.write(a, LIVE, large(), NOW, MESH), Err(Error::Large));
            let b = shard.open_writer(writer("b", 2, &set), NOW, MESH);
            assert!(shard.indexes[0].handoff().is_some(), "no room at the open");
            // The handoff is appended before the bodies, so the size is never checked.
            assert_eq!(
                shard.write(b, LIVE, large(), NOW, MESH),
                Ok(&[lost(0, 0, 600)][..])
            );
            let len = 240_000;
            let stamps: Vec<i64> = (1000..).take(len).collect();
            let values = scattered(len);
            let series: [(usize, &[i64]); 3] =
                [(0, &stamps), (1, &values), (2, &[stamp])];
            let huge = frame(&pool(4 * POOL), &set, &series);
            assert_eq!(
                shard.write(b, LIVE, huge, NOW, MESH),
                Ok(&[lost(0, 600, 240_000), lost(2, 16, 1)][..])
            );
        });
    }

    #[test]
    fn records_a_handoff_at_open_and_close() {
        run(8, |test| async move {
            let set = two_indexes();
            let mut shard = test.shard(AREA).await;
            let a = shard.open_writer(writer("subject-a", 1, &set), NOW, MESH);
            shard.committed().await.expect("the commit ends");
            let handoffs = find(&test.ring().await, &handoff_to("subject-a"));
            assert_eq!(handoffs.len(), 2, "a handoff on each index");
            let b = shard.open_writer(writer("subject-b", 1, &set), NOW, MESH);
            shard.close_writer(a, NOW, MESH);
            shard.committed().await.expect("the commit ends");
            let handoffs = find(&test.ring().await, &handoff_to("subject-b"));
            assert_eq!(handoffs.len(), 2, "b takes each index at the close of a");
            shard.close_writer(b, NOW, MESH);
        });
    }

    #[test]
    fn records_each_handoff_at_open_when_no_record_holds_them_together() {
        run(17, |test| async move {
            let (mut shard, set) = test.wide(WIDE).await;
            let long = "b".repeat(Name::MAX_BYTES);
            shard.open_writer(writer(&long, 1, &set), NOW, MESH);
            shard.committed().await.expect("the commit ends");
            let waiting = shard
                .indexes
                .iter()
                .filter(|index| index.handoff().is_some());
            let handoffs = find(&test.ring().await, &handoff_to(&long));
            assert_eq!((waiting.count(), handoffs.len()), (0, 14));
        });
    }

    #[test]
    fn records_a_handoff_with_room_at_close_when_an_earlier_one_has_none() {
        run(25, |test| async move {
            let set = two_indexes();
            let zero = interner().intern(&[Group {
                index: key(Slot::new(0)),
                data: &[],
            }]);
            let two = interner().intern(&[Group {
                index: key(Slot::new(2)),
                data: &[],
            }]);
            let mut shard = test.shard(AREA).await;
            // Writer a holds both indexes and leaves an open record, so an append
            // needs a block only for its handoff body.
            let a = shard.open_writer(writer("subject-a", 4, &set), NOW, MESH);
            let long = "c".repeat(200);
            shard.open_writer(writer(&long, 2, &zero), NOW, MESH);
            shard.open_writer(writer("subject-x", 1, &two), NOW, MESH);
            // A block for the handoff to x, and none for the handoff to c.
            let to_x = handoff_to("subject-x");
            let room = test.pool.alloc(to_x.len()).expect("a block");
            let blocks = test.fill();
            drop(room);
            shard.close_writer(a, NOW, MESH);
            drop(blocks);
            // The next input on index 2: y outranks x and takes control.
            shard.open_writer(writer("subject-y", 3, &two), NOW, MESH);
            shard.committed().await.expect("the commit ends");
            let ring = test.ring().await;
            let to_y = [&[3], "subject-y".as_bytes()].concat();
            assert_eq!(find(&ring, &to_y).len(), 1, "y takes index 2");
            assert_eq!(
                find(&ring, &to_x).len(),
                1,
                "x held index 2 between the close of a and the open of y"
            );
        });
    }

    #[test]
    fn records_the_handoff_of_a_write_refused_for_an_expired_lease() {
        run(26, |test| async move {
            let set = two_indexes();
            let mut shard = test.shard(AREA).await;
            let lease = control::Lease::new(Span::from_nanos(10)).expect("a lease");
            let a = Writer {
                lease: Some(lease),
                ..writer("subject-a", 2, &set)
            };
            let a = shard.open_writer(a, NOW, MESH);
            shard.open_writer(writer("subject-b", 1, &set), NOW, MESH);
            let write = frame(&test.pool, &set, &[(2, &[10])]);
            let expired = refused(2, Refusal::Control(control::Error::Expired));
            assert_eq!(
                shard.write(a, LIVE, write, Monotonic(20), MESH),
                Ok(&[expired][..])
            );
            shard.committed().await.expect("the commit ends");
            let handoffs = find(&test.ring().await, &handoff_to("subject-b"));
            assert_eq!(
                handoffs.len(),
                1,
                "b takes index 2 when the lease of a ends"
            );
        });
    }

    #[test]
    fn records_each_handoff_at_close_when_no_record_holds_them_together() {
        run(18, |test| async move {
            let (mut shard, set) = test.wide(WIDE).await;
            let a = shard.open_writer(writer("a", 2, &set), NOW, MESH);
            let long = "b".repeat(Name::MAX_BYTES);
            shard.open_writer(writer(&long, 1, &set), NOW, MESH);
            shard.close_writer(a, NOW, MESH);
            shard.committed().await.expect("the commit ends");
            let handoffs = find(&test.ring().await, &handoff_to(&long));
            assert_eq!(handoffs.len(), 14, "the next writer takes each index");
        });
    }

    #[test]
    fn applies_a_frame_after_waiting_handoffs_that_no_record_holds_together() {
        run(19, |test| async move {
            let (mut shard, set) = test.wide(WIDE).await;
            let long = "b".repeat(Name::MAX_BYTES);
            let blocks = test.fill();
            let a = shard.open_writer(writer(&long, 1, &set), NOW, MESH);
            drop(blocks);
            let series: Vec<(usize, &[i64])> = set
                .groups()
                .iter()
                .map(|&entry| (entry, &[10][..]))
                .collect();
            let write = frame(&test.pool, &set, &series);
            let each: Vec<_> = (0..WIDE).map(|slot| applied(slot, 0, 1)).collect();
            assert_eq!(shard.write(a, LIVE, write, NOW, MESH), Ok(&each[..]));
            shard.committed().await.expect("the commit ends");
            let handoffs = find(&test.ring().await, &handoff_to(&long));
            assert_eq!(handoffs.len(), 14);
        });
    }

    #[test]
    fn keeps_a_handoff_waiting_while_the_ring_has_no_room() {
        run(13, |test| async move {
            let set = two_indexes();
            let mut shard = test.shard(1 << 16).await;
            let a = shard.open_writer(writer("a", 1, &set), NOW, MESH);
            let mut seq = 0;
            loop {
                let stamp = 10 + i64::try_from(seq).expect("a short test");
                let write = frame(&test.pool, &set, &[(2, &[stamp])]);
                let written = shard.write(a, LIVE, write, NOW, MESH).expect("written");
                seq += 1;
                if matches!(written, [Outcome::Lost { .. }]) {
                    break;
                }
                shard.committed().await.expect("the commit ends");
            }
            let subject = "b".repeat(64);
            let b = shard.open_writer(writer(&subject, 2, &set), NOW, MESH);
            let waiting = |shard: &Shard| {
                shard
                    .indexes
                    .iter()
                    .all(|index| index.gate.handoff().is_some())
            };
            assert!(waiting(&shard), "no room at the open");
            let write = frame(&test.pool, &set, &[(2, &[1_000])]);
            assert_eq!(
                shard.write(b, LIVE, write, NOW, MESH),
                Ok(&[lost(2, seq, 1)][..])
            );
            assert!(waiting(&shard), "no room for the frame");
        });
    }

    #[test]
    fn stamps_each_entry_with_the_latest_mesh_time() {
        run(14, |test| async move {
            let set = two_indexes();
            let mut shard = test.shard(AREA).await;
            let mesh = Interval {
                earliest: Stamp::from_nanos(0x0123_4567_89AB_CDEF),
                latest: Stamp::from_nanos(0x0FED_CBA9_8765_4321),
            };
            let a = shard.open_writer(writer("a", 1, &set), NOW, mesh);
            let write = frame(&test.pool, &set, &[(0, &[10]), (1, &[1]), (2, &[10])]);
            shard.write(a, LIVE, write, NOW, mesh).expect("written");
            shard.committed().await.expect("the commit ends");
            let ring = test.ring().await;
            let count = |stamp: Stamp| find(&ring, &stamp.nanos().to_le_bytes()).len();
            assert_eq!(count(mesh.latest), 4, "two handoffs and two bodies");
            assert_eq!(count(mesh.earliest), 0);
        });
    }

    #[test]
    fn loses_a_frame_whose_index_has_a_handoff_with_no_block() {
        run(20, |test| async move {
            let set = two_indexes();
            let mut shard = test.shard(AREA).await;
            // Writer a leaves an open record, so the frame needs a block only for its
            // body.
            let _a = shard.open_writer(writer("a", 1, &set), NOW, MESH);
            let live = frame(&test.pool, &set, &[(0, &[10]), (1, &[1])]);
            let again = frame(&test.pool, &set, &[(0, &[20]), (1, &[2])]);
            let room = test.pool.alloc(80).expect("a block");
            let blocks = test.fill();
            let subject = "b".repeat(200);
            let b = shard.open_writer(writer(&subject, 2, &set), NOW, MESH);
            drop(room);
            assert_eq!(
                shard.write(b, LIVE, live, NOW, MESH),
                Ok(&[lost(0, 0, 1)][..])
            );
            drop(blocks);
            assert_eq!(
                shard.write(b, LIVE, again, NOW, MESH),
                Ok(&[applied(0, 1, 1)][..])
            );
            shard.committed().await.expect("the commit ends");
            let ring = test.ring().await;
            let handoffs = find(&ring, &[&[2], subject.as_bytes()].concat());
            assert_eq!(handoffs.len(), 1, "only the index of the frame records it");
            let series = find(&ring, &key(Slot::new(1)).as_u128().to_le_bytes());
            assert_eq!(series.len(), 1, "the lost frame stores nothing");
            assert!(
                handoffs[0] < series[0],
                "the handoff goes in before the frame"
            );
        });
    }

    #[test]
    fn records_a_waiting_handoff_before_its_frame_takes_the_blocks() {
        run(24, |test| async move {
            let set = two_indexes();
            let mut shard = test.shard(AREA).await;
            let _a = shard.open_writer(writer("a", 1, &set), NOW, MESH);
            let live = frame(&test.pool, &set, &[(0, &[10]), (1, &[1])]);
            // The handoff and the frame's body each need the one free block.
            let room = test.pool.alloc(80).expect("a block");
            let blocks = test.fill();
            let subject = "b".repeat(70);
            let b = shard.open_writer(writer(&subject, 2, &set), NOW, MESH);
            drop(room);
            assert_eq!(
                shard.write(b, LIVE, live, NOW, MESH),
                Ok(&[lost(0, 0, 1)][..])
            );
            drop(blocks);
            shard.committed().await.expect("the commit ends");
            let handoff = [&[2], subject.as_bytes()].concat();
            assert_eq!(find(&test.ring().await, &handoff).len(), 1);
        });
    }

    #[test]
    fn records_a_handoff_that_found_no_block_before_the_next_frame_of_its_index() {
        run(9, |test| async move {
            let set = two_indexes();
            let mut shard = test.shard(AREA).await;
            let first = frame(&test.pool, &set, &[(0, &[10]), (1, &[1])]);
            let blocks = test.fill();
            let a = shard.open_writer(writer("subject-a", 1, &set), NOW, MESH);
            drop(blocks);
            assert_eq!(
                shard.write(a, LIVE, first, NOW, MESH),
                Ok(&[applied(0, 0, 1)][..])
            );
            shard.committed().await.expect("the commit ends");
            let ring = test.ring().await;
            let handoffs = find(&ring, &handoff_to("subject-a"));
            assert_eq!(handoffs.len(), 1, "only the index of the frame records it");
            let series = find(&ring, &key(Slot::new(1)).as_u128().to_le_bytes());
            assert!(
                handoffs[0] < series[0],
                "the handoff goes in before the frame"
            );
            let again = frame(&test.pool, &set, &[(0, &[20]), (1, &[2])]);
            assert_eq!(
                shard.write(a, LIVE, again, NOW, MESH),
                Ok(&[applied(0, 1, 1)][..])
            );
            let second = frame(&test.pool, &set, &[(2, &[10])]);
            assert_eq!(
                shard.write(a, LIVE, second, NOW, MESH),
                Ok(&[applied(2, 0, 1)][..])
            );
            shard.committed().await.expect("the commit ends");
            let handoffs = find(&test.ring().await, &handoff_to("subject-a"));
            assert_eq!(handoffs.len(), 2);
        });
    }

    #[test]
    fn fails_each_write_after_a_failed_sync() {
        run(10, |test| async move {
            let set = two_indexes();
            let mut shard = test.shard(AREA).await;
            let a = shard.open_writer(writer("a", 2, &set), NOW, MESH);
            let b = shard.open_writer(writer("b", 1, &set), NOW, MESH);
            test.node.fail_file(FilePath::new(RING), Operation::Sync);
            let write = frame(&test.pool, &set, &[(0, &[10]), (1, &[1])]);
            assert_eq!(
                shard.write(a, LIVE, write, NOW, MESH),
                Ok(&[applied(0, 0, 1)][..])
            );
            let failed = env::files::Error::Io {
                path: PathBuf::from(RING),
                operation: Operation::Sync,
                code: 5,
            };
            assert_eq!(shard.committed().await, Err(failed.clone()));
            let disk = Error::Disk(failed);
            assert_eq!(
                disk.to_string(),
                "a commit failed: sync of shard-0/ring failed with OS error 5"
            );
            let write = frame(&test.pool, &set, &[(0, &[20]), (1, &[2])]);
            assert_eq!(shard.write(a, LIVE, write, NOW, MESH), Err(disk.clone()));
            let refused = frame(&test.pool, &set, &[(2, &[20])]);
            assert_eq!(shard.write(b, LIVE, refused, NOW, MESH), Err(disk.clone()));
            let live = frame(&test.pool, &set, &[(0, &[30]), (1, &[3])]);
            let backfill = frame(&test.pool, &set, &[(0, &[1]), (1, &[1])]);
            let blocks = test.fill();
            assert_eq!(shard.write(a, LIVE, live, NOW, MESH), Err(disk.clone()));
            assert_eq!(shard.write(a, BACKFILL, backfill, NOW, MESH), Err(disk));
            drop(blocks);
        });
    }

    #[test]
    fn refuses_a_resend_frame() {
        run(11, |test| async move {
            let set = two_indexes();
            let mut shard = test.shard(AREA).await;
            let a = shard.open_writer(writer("a", 1, &set), NOW, MESH);
            let write = frame(&test.pool, &set, &[(2, &[10])]);
            assert_eq!(
                shard.write(a, Label::Resend, write, NOW, MESH),
                Err(Error::Resend)
            );
            assert_eq!(
                Error::Resend.to_string(),
                "the home does not take a resend frame yet"
            );
        });
    }

    /// The smallest ring holds the largest handoff, so `new` needs no check for it.
    #[test]
    fn records_the_largest_handoff_in_the_smallest_ring() {
        run(42, |test| async move {
            let buffer = test.buffer(AREA, 4087, 1).await;
            let mut shard = Shard::new(buffer, Rc::clone(&test.pool), LIMITS);
            shard.carry(Slot::new(0));
            let set = interner().intern(&[Group {
                index: key(Slot::new(0)),
                data: &[],
            }]);
            let long = "b".repeat(Name::MAX_BYTES);
            shard.open_writer(writer(&long, 1, &set), NOW, MESH);
            shard.committed().await.expect("the commit ends");
            let waiting = shard.indexes[0].handoff().is_some();
            let handoffs = find(&test.ring().await, &handoff_to(&long));
            assert_eq!((waiting, handoffs.len()), (false, 1));
        });
    }

    #[test]
    fn panics_at_the_open_of_a_writer_of_an_index_it_does_not_carry() {
        let (mut sim, _handle) = start(12, |test| async move {
            let set = interner().intern(&[Group {
                index: key(Slot::new(3)),
                data: &[],
            }]);
            let mut shard = test.shard(AREA).await;
            shard.open_writer(writer("a", 1, &set), NOW, MESH);
        });
        assert_eq!(
            sim.run(),
            Err(sim::Error::Panicked {
                thread: DIR.into(),
                message: "the shard does not carry the index at Slot(3)".into(),
                seed: 12,
            })
        );
    }

    #[test]
    fn tags_each_handoff_and_each_body() {
        run(21, |test| async move {
            let set = two_indexes();
            let mut shard = test.shard(AREA).await;
            let a = shard.open_writer(writer("a", 1, &set), NOW, MARKED);
            let write = frame(&test.pool, &set, &[(0, &[10]), (1, &[1]), (2, &[10])]);
            shard.write(a, LIVE, write, NOW, MARKED).expect("written");
            shard.committed().await.expect("the commit ends");
            let zero = key(Slot::new(0)).as_u128();
            let two = key(Slot::new(2)).as_u128();
            assert_eq!(
                headers(&test.ring().await, MARKED.latest),
                [
                    (zero, 0, 0, 0, 1),
                    (two, 0, 0, 0, 1),
                    (zero, 0, 0, 1, 0),
                    (two, 0, 0, 1, 0),
                ]
            );
        });
    }

    #[test]
    fn stores_a_body_under_its_index_when_a_data_channel_has_a_lower_slot() {
        run(46, |test| async move {
            let set = interner().intern(&[Group {
                index: key(Slot::new(2)),
                data: &[(key(Slot::new(1)), Type::Scalar(Scalar::I64))],
            }]);
            let mut shard = test.open(AREA, 4).await;
            shard.carry(Slot::new(2));
            let a = shard.open_writer(writer("a", 1, &set), NOW, MARKED);
            let write = frame(&test.pool, &set, &[(0, &[1, 2]), (1, &[10, 20])]);
            assert_eq!(
                shard.write(a, LIVE, write, NOW, MARKED),
                Ok(&[applied(2, 0, 2)][..])
            );
            shard.committed().await.expect("the commit ends");
            let two = key(Slot::new(2)).as_u128();
            assert_eq!(
                headers(&test.ring().await, MARKED.latest),
                [(two, 0, 0, 0, 1), (two, 0, 0, 2, 0)]
            );
        });
    }

    #[test]
    fn records_a_waiting_handoff_on_the_live_path_before_a_backfill_frame() {
        run(22, |test| async move {
            let set = two_indexes();
            let mut shard = test.shard(AREA).await;
            let a = shard.open_writer(writer("a", 1, &set), NOW, MESH);
            let live = frame(&test.pool, &set, &[(0, &[10, 20]), (1, &[1, 2])]);
            shard.write(a, LIVE, live, NOW, MESH).expect("written");
            let backfill = frame(&test.pool, &set, &[(0, &[1]), (1, &[1])]);
            let blocks = test.fill();
            let b = shard.open_writer(writer("b", 2, &set), NOW, MESH);
            drop(blocks);
            assert_eq!(
                shard.write(b, BACKFILL, backfill, NOW, MARKED),
                Ok(&[applied(0, 0, 1)][..])
            );
            shard.committed().await.expect("the commit ends");
            let zero = key(Slot::new(0)).as_u128();
            assert_eq!(
                headers(&test.ring().await, MARKED.latest),
                [(zero, 0, 2, 0, 1), (zero, 1, 0, 1, 0)]
            );
        });
    }

    #[test]
    fn stores_the_series_of_each_frame() {
        run(23, |test| async move {
            let set = two_indexes();
            let mut shard = test.shard(AREA).await;
            let a = shard.open_writer(writer("a", 1, &set), NOW, MARKED);
            let values: [i64; 2] = [0x0123_4567_89AB_CDEF, -5];
            let write = frame(&test.pool, &set, &[(0, &[10, 20]), (1, &values)]);
            shard.write(a, LIVE, write, NOW, MARKED).expect("written");
            shard.committed().await.expect("the commit ends");
            let ring = test.ring().await;
            let data: Vec<_> = bodies(&ring, MARKED.latest)
                .into_iter()
                .filter(|(tag, _)| *tag == 0)
                .collect();
            assert_eq!(data.len(), 1, "one data entry");
            let decoded: Vec<(channel::Key, Vec<u8>)> = stored::read(&data[0].1)
                .map(|series| {
                    let mut out = vec![0; 16];
                    codec::decode(series.data_type, 2, series.bytes, &mut out)
                        .expect("decodes");
                    (series.channel, out)
                })
                .collect();
            let le = |values: &[i64]| -> Vec<u8> {
                values
                    .iter()
                    .flat_map(|value| value.to_le_bytes())
                    .collect()
            };
            assert_eq!(
                decoded,
                [
                    (key(Slot::new(0)), le(&[10, 20])),
                    (key(Slot::new(1)), le(&values)),
                ]
            );
        });
    }

    mod read {
        use super::*;
        use crate::reader::Mode;

        const CREDIT: u64 = 1 << 20;
        const COMPLETE: Mode = Mode::Complete {
            limit_bytes: CREDIT,
        };

        fn woken(shard: &mut Shard) -> Vec<reader::Key> {
            let mut keys = Vec::new();
            shard.woken(&mut keys);
            keys
        }

        /// The seq of the index group `group` of each frame `reader` takes now.
        fn taken(shard: &mut Shard, reader: reader::Key, group: u32) -> Vec<Range> {
            iter::from_fn(|| shard.take(reader))
                .map(|frame| frame.range(group).expect("the index is present"))
                .collect()
        }

        /// Writes a live frame of the index at slot 0 with `stamps`.
        fn write(test: &Test, shard: &mut Shard, a: writer::Key, stamps: &[i64]) {
            let set = two_indexes();
            let values = vec![1; stamps.len()];
            let write = frame(&test.pool, &set, &[(0, stamps), (1, &values)]);
            let written = shard.write(a, LIVE, write, NOW, MESH).expect("written");
            assert!(
                matches!(written, [Outcome::Applied { .. } | Outcome::Lost { .. }]),
                "{written:?}"
            );
        }

        fn seq(seq: u64, count: u32) -> Range {
            Range { seq, count }
        }

        #[test]
        fn gives_a_latest_reader_the_newest_frame_before_its_commit() {
            run(33, |test| async move {
                let set = two_indexes();
                let mut shard = test.shard(AREA).await;
                let reader = shard.open_reader(Slot::new(0), Mode::Latest, MESH);
                assert_eq!(woken(&mut shard), []);
                let a = shard.open_writer(writer("a", 1, &set), NOW, MESH);
                write(&test, &mut shard, a, &[10, 20]);
                write(&test, &mut shard, a, &[30]);
                assert_eq!(shard.stored(Slot::new(0), Path::Live), 0);
                assert_eq!(woken(&mut shard), [reader]);
                assert_eq!(taken(&mut shard, reader, 0), [seq(2, 1)]);
                let late = shard.open_reader(Slot::new(0), Mode::Latest, MESH);
                assert_eq!(woken(&mut shard), [late]);
                assert_eq!(taken(&mut shard, late, 0), [seq(2, 1)]);
            });
        }

        #[test]
        fn gives_a_complete_reader_each_frame_after_the_commit_that_holds_it() {
            run(34, |test| async move {
                let set = two_indexes();
                let mut shard = test.shard(AREA).await;
                let reader = shard.open_reader(Slot::new(0), COMPLETE, MESH);
                let a = shard.open_writer(writer("a", 1, &set), NOW, MESH);
                write(&test, &mut shard, a, &[10, 20]);
                write(&test, &mut shard, a, &[30]);
                assert_eq!(woken(&mut shard), []);
                assert_eq!(taken(&mut shard, reader, 0), []);
                shard.committed().await.expect("the commit ends");
                assert_eq!(woken(&mut shard), [reader]);
                assert_eq!(taken(&mut shard, reader, 0), [seq(0, 2), seq(2, 1)]);
                assert_eq!(woken(&mut shard), []);
                write(&test, &mut shard, a, &[40]);
                shard.committed().await.expect("the commit ends");
                assert_eq!(woken(&mut shard), [reader]);
                assert_eq!(taken(&mut shard, reader, 0), [seq(3, 1)]);
            });
        }

        #[test]
        fn writes_and_serves_a_latest_reader_while_a_commit_runs() {
            run(32, |test| async move {
                let set = two_indexes();
                let mut shard = test.shard(AREA).await;
                let latest = shard.open_reader(Slot::new(0), Mode::Latest, MESH);
                let complete = shard.open_reader(Slot::new(0), COMPLETE, MESH);
                let a = shard.open_writer(writer("a", 1, &set), NOW, MESH);
                write(&test, &mut shard, a, &[10]);
                test.clock.sleep(SYNC).await;
                assert_eq!(shard.stored(Slot::new(0), Path::Live), 0, "a sync runs");
                write(&test, &mut shard, a, &[20]);
                assert_eq!(woken(&mut shard), [latest]);
                assert_eq!(taken(&mut shard, latest, 0), [seq(1, 1)]);
                assert_eq!(taken(&mut shard, complete, 0), []);
                shard.committed().await.expect("the commit ends");
                assert_eq!(woken(&mut shard), [complete]);
                assert_eq!(taken(&mut shard, complete, 0), [seq(0, 1), seq(1, 1)]);
            });
        }

        #[test]
        fn gives_a_complete_reader_a_lost_group_as_a_gap_in_seq() {
            run(35, |test| async move {
                let set = two_indexes();
                let mut shard = test.shard(AREA).await;
                let reader = shard.open_reader(Slot::new(0), COMPLETE, MESH);
                let a = shard.open_writer(writer("a", 1, &set), NOW, MESH);
                write(&test, &mut shard, a, &[10]);
                let lost = frame(&test.pool, &set, &[(0, &[20, 30]), (1, &[2, 3])]);
                let blocks = test.fill();
                assert_eq!(
                    shard.write(a, LIVE, lost, NOW, MESH),
                    Ok(&[super::lost(0, 1, 2)][..])
                );
                drop(blocks);
                write(&test, &mut shard, a, &[40]);
                shard.committed().await.expect("the commit ends");
                assert_eq!(woken(&mut shard), [reader]);
                assert_eq!(taken(&mut shard, reader, 0), [seq(0, 1), seq(3, 1)]);
            });
        }

        #[test]
        fn gives_a_latest_reader_a_live_frame_the_ring_had_no_room_for() {
            run(36, |test| async move {
                let set = two_indexes();
                let mut shard = test.shard(1 << 16).await;
                let reader = shard.open_reader(Slot::new(2), Mode::Latest, MESH);
                let a = shard.open_writer(writer("a", 1, &set), NOW, MESH);
                let mut next = 0;
                loop {
                    let stamp = 10 + i64::try_from(next).expect("a short test");
                    let write = frame(&test.pool, &set, &[(2, &[stamp])]);
                    let written =
                        shard.write(a, LIVE, write, NOW, MESH).expect("written");
                    if matches!(written, [Outcome::Lost { .. }]) {
                        break;
                    }
                    next += 1;
                    shard.committed().await.expect("the commit ends");
                }
                assert_eq!(taken(&mut shard, reader, 1), [seq(next, 1)]);
            });
        }

        #[test]
        fn gives_a_complete_reader_out_of_credit_no_later_frame() {
            run(37, |test| async move {
                let set = two_indexes();
                let mut shard = test.shard(AREA).await;
                let one = Mode::Complete { limit_bytes: 1 };
                let reader = shard.open_reader(Slot::new(0), one, MESH);
                let a = shard.open_writer(writer("a", 1, &set), NOW, MESH);
                write(&test, &mut shard, a, &[10]);
                write(&test, &mut shard, a, &[20]);
                shard.committed().await.expect("the commit ends");
                assert_eq!(woken(&mut shard), [reader]);
                assert_eq!(taken(&mut shard, reader, 0), [seq(0, 1)]);
                shard.grant(reader, CREDIT);
                write(&test, &mut shard, a, &[30]);
                shard.committed().await.expect("the commit ends");
                assert_eq!(woken(&mut shard), []);
                assert_eq!(taken(&mut shard, reader, 0), []);
            });
        }

        #[test]
        fn raises_the_credit_of_a_complete_reader_with_a_grant() {
            run(25, |test| async move {
                let set = two_indexes();
                let mut shard = test.shard(AREA).await;
                let one = Mode::Complete { limit_bytes: 1 };
                let reader = shard.open_reader(Slot::new(0), one, MESH);
                let a = shard.open_writer(writer("a", 1, &set), NOW, MESH);
                write(&test, &mut shard, a, &[10]);
                shard.committed().await.expect("the commit ends");
                assert_eq!(woken(&mut shard), [reader]);
                assert_eq!(taken(&mut shard, reader, 0), [seq(0, 1)]);
                shard.grant(reader, CREDIT);
                write(&test, &mut shard, a, &[20]);
                write(&test, &mut shard, a, &[30]);
                shard.committed().await.expect("the commit ends");
                assert_eq!(woken(&mut shard), [reader]);
                assert_eq!(taken(&mut shard, reader, 0), [seq(1, 1), seq(2, 1)]);
            });
        }

        #[test]
        fn ignores_a_grant_to_a_closed_complete_reader() {
            run(46, |test| async move {
                let mut shard = test.shard(AREA).await;
                let one = Mode::Complete { limit_bytes: 1 };
                let reader = shard.open_reader(Slot::new(0), one, MESH);
                shard.close_reader(reader, MESH);
                shard.grant(reader, CREDIT);
                let after = shard.open_reader(Slot::new(0), one, MESH);
                assert_ne!(after, reader);
            });
        }

        #[test]
        fn gives_a_complete_reader_that_opens_after_writes_only_later_frames() {
            run(26, |test| async move {
                let set = two_indexes();
                let mut shard = test.shard(AREA).await;
                let a = shard.open_writer(writer("a", 1, &set), NOW, MESH);
                write(&test, &mut shard, a, &[10]);
                shard.committed().await.expect("the commit ends");
                write(&test, &mut shard, a, &[20, 30]);
                let reader = shard.open_reader(Slot::new(0), COMPLETE, MESH);
                write(&test, &mut shard, a, &[40]);
                shard.committed().await.expect("the commit ends");
                assert_eq!(woken(&mut shard), [reader]);
                assert_eq!(taken(&mut shard, reader, 0), [seq(3, 1)]);
            });
        }

        #[test]
        fn gives_complete_readers_of_an_index_the_same_frames_in_order() {
            run(27, |test| async move {
                let set = two_indexes();
                let mut shard = test.shard(AREA).await;
                let first = shard.open_reader(Slot::new(0), COMPLETE, MESH);
                let second = shard.open_reader(Slot::new(0), COMPLETE, MESH);
                let other = shard.open_reader(Slot::new(2), COMPLETE, MESH);
                let a = shard.open_writer(writer("a", 1, &set), NOW, MESH);
                write(&test, &mut shard, a, &[10, 20]);
                let both = frame(&test.pool, &set, &[(0, &[30]), (1, &[3]), (2, &[5])]);
                shard.write(a, LIVE, both, NOW, MESH).expect("written");
                write(&test, &mut shard, a, &[40]);
                shard.committed().await.expect("the commit ends");
                assert_eq!(woken(&mut shard), [first, second, other]);
                let frames = [seq(0, 2), seq(2, 1), seq(3, 1)];
                assert_eq!(taken(&mut shard, first, 0), frames);
                assert_eq!(taken(&mut shard, second, 0), frames);
                assert_eq!(taken(&mut shard, other, 1), [seq(0, 1)]);
            });
        }

        #[test]
        fn wakes_no_reader_after_its_close() {
            run(28, |test| async move {
                let set = two_indexes();
                let mut shard = test.shard(AREA).await;
                let latest = shard.open_reader(Slot::new(0), Mode::Latest, MESH);
                let complete = shard.open_reader(Slot::new(0), COMPLETE, MESH);
                let a = shard.open_writer(writer("a", 1, &set), NOW, MESH);
                write(&test, &mut shard, a, &[10]);
                shard.close_reader(latest, MESH);
                assert_eq!(woken(&mut shard), []);
                write(&test, &mut shard, a, &[20]);
                shard.committed().await.expect("the commit ends");
                shard.close_reader(complete, MESH);
                assert_eq!(woken(&mut shard), []);
            });
        }

        #[test]
        fn gives_no_reader_a_backfill_frame() {
            run(30, |test| async move {
                let set = two_indexes();
                let mut shard = test.shard(AREA).await;
                let latest = shard.open_reader(Slot::new(0), Mode::Latest, MESH);
                let complete = shard.open_reader(Slot::new(0), COMPLETE, MESH);
                let a = shard.open_writer(writer("a", 1, &set), NOW, MESH);
                let backfill = frame(&test.pool, &set, &[(0, &[1, 2]), (1, &[1, 2])]);
                assert_eq!(
                    shard.write(a, BACKFILL, backfill, NOW, MESH),
                    Ok(&[applied(0, 0, 2)][..])
                );
                shard.committed().await.expect("the commit ends");
                assert_eq!(woken(&mut shard), []);
                assert_eq!(taken(&mut shard, latest, 0), []);
                assert_eq!(taken(&mut shard, complete, 0), []);
            });
        }

        #[test]
        fn holds_a_frame_written_while_a_sync_runs_until_its_own_commit() {
            run(38, |test| async move {
                let set = two_indexes();
                let mut shard = test.shard(AREA).await;
                let reader = shard.open_reader(Slot::new(0), COMPLETE, MESH);
                let a = shard.open_writer(writer("a", 1, &set), NOW, MESH);
                write(&test, &mut shard, a, &[10]);
                test.clock.sleep(SYNC).await;
                assert_eq!(shard.stored(Slot::new(0), Path::Live), 0, "a sync runs");
                write(&test, &mut shard, a, &[20]);
                while shard.stored(Slot::new(0), Path::Live) == 0 {
                    test.clock.sleep(Span::from_nanos(1_000)).await;
                }
                assert_eq!(shard.stored(Slot::new(0), Path::Live), 1);
                assert_eq!(woken(&mut shard), [reader]);
                assert_eq!(taken(&mut shard, reader, 0), [seq(0, 1)]);
                shard.committed().await.expect("the commit ends");
                assert_eq!(woken(&mut shard), [reader]);
                assert_eq!(taken(&mut shard, reader, 0), [seq(1, 1)]);
            });
        }

        #[test]
        fn lists_an_index_only_while_a_live_frame_is_not_on_disk() {
            run(42, |test| async move {
                let set = two_indexes();
                let mut shard = test.shard(AREA).await;
                let reader = shard.open_reader(Slot::new(0), COMPLETE, MESH);
                let a = shard.open_writer(writer("a", 1, &set), NOW, MESH);
                write(&test, &mut shard, a, &[10]);
                test.clock.sleep(SYNC).await;
                write(&test, &mut shard, a, &[20]);
                while shard.stored(Slot::new(0), Path::Live) == 0 {
                    test.clock.sleep(Span::from_nanos(1_000)).await;
                }
                let place = shard.place(Slot::new(0));
                assert_eq!(woken(&mut shard), [reader]);
                assert_eq!(taken(&mut shard, reader, 0), [seq(0, 1)]);
                assert_eq!(shard.wake.pending, [(Slot::new(0), place)]);
                shard.committed().await.expect("the commit ends");
                assert_eq!(woken(&mut shard), [reader]);
                assert_eq!(shard.wake.pending, Vec::new());
            });
        }

        #[test]
        fn releases_a_frame_at_the_first_woken_after_its_commit() {
            run(50, |test| async move {
                let set = two_indexes();
                let mut shard = test.shard(AREA).await;
                let reader = shard.open_reader(Slot::new(0), COMPLETE, MESH);
                let a = shard.open_writer(writer("a", 1, &set), NOW, MESH);
                write(&test, &mut shard, a, &[10]);
                assert_eq!(woken(&mut shard), []);
                assert_eq!(woken(&mut shard), []);
                shard.committed().await.expect("the commit ends");
                write(&test, &mut shard, a, &[20]);
                assert_eq!(woken(&mut shard), [reader]);
                assert_eq!(taken(&mut shard, reader, 0), [seq(0, 1)]);
                assert_eq!(woken(&mut shard), []);
                shard.committed().await.expect("the commit ends");
                assert_eq!(woken(&mut shard), [reader]);
                assert_eq!(taken(&mut shard, reader, 0), [seq(1, 1)]);
            });
        }

        #[test]
        fn releases_a_frame_listed_after_a_settle_at_the_next_commit() {
            run(51, |test| async move {
                let set = two_indexes();
                let mut shard = test.shard(AREA).await;
                let reader = shard.open_reader(Slot::new(0), COMPLETE, MESH);
                let a = shard.open_writer(writer("a", 1, &set), NOW, MESH);
                write(&test, &mut shard, a, &[10]);
                shard.committed().await.expect("the commit ends");
                assert_eq!(woken(&mut shard), [reader]);
                assert_eq!(taken(&mut shard, reader, 0), [seq(0, 1)]);
                assert_eq!(shard.wake.pending, Vec::new());
                write(&test, &mut shard, a, &[20]);
                assert_eq!(woken(&mut shard), []);
                shard.committed().await.expect("the commit ends");
                assert_eq!(woken(&mut shard), [reader]);
                assert_eq!(taken(&mut shard, reader, 0), [seq(1, 1)]);
            });
        }

        #[test]
        fn lists_no_index_for_a_live_frame_with_no_complete_reader() {
            run(49, |test| async move {
                let set = two_indexes();
                let mut shard = test.shard(AREA).await;
                let a = shard.open_writer(writer("a", 1, &set), NOW, MESH);
                write(&test, &mut shard, a, &[10]);
                assert_eq!(shard.wake.pending, Vec::new());
                let reader = shard.open_reader(Slot::new(0), COMPLETE, MESH);
                write(&test, &mut shard, a, &[20]);
                let place = shard.place(Slot::new(0));
                assert_eq!(shard.wake.pending, [(Slot::new(0), place)]);
                shard.committed().await.expect("the commit ends");
                assert_eq!(woken(&mut shard), [reader]);
                assert_eq!(taken(&mut shard, reader, 0), [seq(1, 1)]);
            });
        }

        #[test]
        fn unlists_an_index_at_the_commit_after_its_last_complete_reader_closes() {
            run(52, |test| async move {
                let set = two_indexes();
                let mut shard = test.shard(AREA).await;
                let a = shard.open_writer(writer("a", 1, &set), NOW, MESH);
                let reader = shard.open_reader(Slot::new(0), COMPLETE, MESH);
                write(&test, &mut shard, a, &[10]);
                shard.committed().await.expect("the commit ends");
                assert_eq!(woken(&mut shard), [reader]);
                write(&test, &mut shard, a, &[20]);
                shard.close_reader(reader, MESH);
                write(&test, &mut shard, a, &[30]);
                let place = shard.place(Slot::new(0));
                assert_eq!(woken(&mut shard), []);
                let listed = [(Slot::new(0), place)];
                assert_eq!(shard.wake.pending, listed, "no commit ended");
                shard.committed().await.expect("the commit ends");
                assert_eq!(woken(&mut shard), []);
                assert_eq!(shard.wake.pending, Vec::new());
            });
        }

        #[test]
        fn gives_a_complete_reader_a_frame_on_disk_with_no_commit_future() {
            run(39, |test| async move {
                let set = two_indexes();
                let mut shard = test.shard(AREA).await;
                let reader = shard.open_reader(Slot::new(0), COMPLETE, MESH);
                let a = shard.open_writer(writer("a", 1, &set), NOW, MESH);
                write(&test, &mut shard, a, &[10]);
                test.clock.sleep(Span::from_nanos(100_000_000)).await;
                assert_eq!(shard.stored(Slot::new(0), Path::Live), 1);
                assert_eq!(woken(&mut shard), [reader]);
                assert_eq!(taken(&mut shard, reader, 0), [seq(0, 1)]);
            });
        }

        #[test]
        fn names_no_complete_reader_that_took_its_frames() {
            run(40, |test| async move {
                let set = two_indexes();
                let mut shard = test.shard(AREA).await;
                let reader = shard.open_reader(Slot::new(0), COMPLETE, MESH);
                let a = shard.open_writer(writer("a", 1, &set), NOW, MESH);
                write(&test, &mut shard, a, &[10]);
                shard.committed().await.expect("the commit ends");
                assert_eq!(woken(&mut shard), [reader]);
                assert_eq!(taken(&mut shard, reader, 0), [seq(0, 1)]);
                assert_eq!(woken(&mut shard), []);
            });
        }

        #[test]
        fn names_each_reader_to_wake_once_in_order() {
            run(41, |test| async move {
                let set = two_indexes();
                let mut shard = test.shard(AREA).await;
                let first = shard.open_reader(Slot::new(0), Mode::Latest, MESH);
                let second = shard.open_reader(Slot::new(2), Mode::Latest, MESH);
                let a = shard.open_writer(writer("a", 1, &set), NOW, MESH);
                let later = frame(&test.pool, &set, &[(2, &[10])]);
                shard.write(a, LIVE, later, NOW, MESH).expect("written");
                write(&test, &mut shard, a, &[20]);
                assert_eq!(taken(&mut shard, first, 0), [seq(0, 1)]);
                write(&test, &mut shard, a, &[30]);
                assert_eq!(woken(&mut shard), [first, second]);
            });
        }

        #[test]
        fn panics_at_the_open_of_a_reader_of_an_index_it_does_not_carry() {
            let (mut sim, _handle) = start(29, |test| async move {
                let mut shard = test.shard(AREA).await;
                shard.open_reader(Slot::new(3), Mode::Latest, MESH);
            });
            assert_eq!(
                sim.run(),
                Err(sim::Error::Panicked {
                    thread: DIR.into(),
                    message: "the shard does not carry the index at Slot(3)".into(),
                    seed: 29,
                })
            );
        }
    }
}
