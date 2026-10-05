//! The indexes one shard carries: their writers, each frame from split to one buffer
//! append, and their readers.

use std::cell::Cell;
use std::rc::Rc;
use std::sync::Arc;
use std::{fmt, mem};

use block::Block;
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
    buffer: Rc<Buffer>,
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
    /// Set when a [`Shard::committed`] future resolves, until [`Wake::settle`].
    committed: Rc<Cell<bool>>,
    /// The slot and place of each index with live frames for complete readers that
    /// may not be on disk, each once.
    listed: Vec<(Slot, usize)>,
    /// Each reader to wake, with a frame to take.
    keys: Vec<reader::Key>,
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
    batch: Batch,
    outcomes: Vec<Outcome>,
}

/// The stored bodies of one frame's append. Empty between appends.
#[derive(Debug, Default)]
struct Batch {
    /// The stored body of each accepted group, in group order.
    bodies: Vec<Body>,
}

/// The stored body of one accepted group.
#[derive(Debug)]
struct Body {
    group: u32,
    range: frame::Range,
    last: Option<Stamp>,
    parts: [Block; 2],
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
    /// A backfill frame found no room in the ring or the pool. Nothing is spent, and
    /// the writer writes the frame again later.
    Full,
    /// A commit failed. The shard takes no more frames.
    Disk(env::files::Error),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Resend => write!(f, "the home does not take a resend frame yet"),
            Self::Full => write!(f, "the ring or the pool has no room for the frame"),
            Self::Disk(error) => write!(f, "a commit failed: {error}"),
        }
    }
}

impl std::error::Error for Error {}

impl Shard {
    /// A shard over `buffer` that carries no index yet. Index frames, stored headers,
    /// and handoff bodies come from `pool`.
    pub(crate) fn new(
        buffer: Buffer,
        pool: Rc<block::Pool>,
        limits: order::Config,
    ) -> Self {
        Self {
            buffer: Rc::new(buffer),
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
        let claims = set
            .groups()
            .iter()
            .map(|&entry| {
                let entry = &entries[entry];
                let Some(&place) = self.places.get(&entry.slot) else {
                    panic!("the shard does not carry index {}", entry.key);
                };
                let key = self.indexes[place].gate.open(control.clone(), lease, now);
                Claim { place, key }
            })
            .collect();
        let session = Session { set, claims };
        self.record(&session, mesh);
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
        self.record(&session, mesh);
    }

    /// Applies `frame` to each index it holds, whole or not at all per index, at
    /// monotonic time `now` and mesh time `mesh`. The frame's bodies go in one append,
    /// after the unrecorded handoff of each of its indexes. Returns the outcome of
    /// each present group, in group order.
    ///
    /// # Errors
    ///
    /// [`Error::Resend`] for a frame labeled resend. [`Error::Full`] for a backfill
    /// frame when the ring or the pool has no room; no seq moves. [`Error::Disk`]
    /// after a failed commit.
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
        for (group, stamps) in split.groups() {
            let (claim, _) = session.claim(group);
            let index = &mut self.indexes[claim.place];
            let checked = index.check(claim.key, path, stamps, now, mesh);
            scratch.checks.push((group, checked));
        }
        let groups = scratch.checks.iter().map(|&(group, _)| group);
        let recorded = handoffs(
            &self.buffer,
            &self.pool,
            &mut self.indexes,
            session,
            groups,
            mesh,
        );
        let batch = &mut scratch.batch;
        let made =
            batch.bodies(&self.pool, &mut split, &session.set, &mut scratch.checks);
        drop(split);
        let ready = made.is_ok() && recorded == Ok(true);
        if !ready {
            batch.clear();
        }
        let appended = batch.append(&self.buffer, session, path, mesh);
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
                path,
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

    /// Resolves when every frame written before its first poll is on disk. It does not
    /// borrow the shard, so writes go on while a commit runs. After it resolves, the
    /// next [`woken`](Self::woken) or [`take`](Self::take) gives each complete reader
    /// the live frames now on disk, in time linear in the indexes with live frames
    /// written since the commit before.
    ///
    /// # Errors
    ///
    /// The error that ended the buffer.
    pub(crate) fn committed(
        &self,
    ) -> impl Future<Output = Result<(), buffer::Error>> + 'static {
        let buffer = Rc::clone(&self.buffer);
        let committed = Rc::clone(&self.wake.committed);
        async move {
            buffer.committed().await?;
            committed.set(true);
            Ok(())
        }
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
        let Some(&place) = self.places.get(&slot) else {
            panic!("the shard does not carry the index at {slot:?}");
        };
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
    /// limit that is not higher changes nothing.
    ///
    /// # Panics
    ///
    /// If the complete reader is not open.
    pub(crate) fn grant(&mut self, key: reader::Key, limit_bytes: u64) {
        self.index(key).readers.grant(key.session, limit_bytes);
    }

    /// Takes the reader's next frame, or `None` when none waits.
    ///
    /// # Panics
    ///
    /// If the reader is not open.
    pub(crate) fn take(&mut self, key: reader::Key) -> Option<Frame> {
        self.wake.settle(&self.buffer, &mut self.indexes);
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

    /// Takes the readers to wake, each with a frame to take, since the last call. A
    /// reader that took its frames and got more appears again.
    pub(crate) fn woken(&mut self) -> impl Iterator<Item = reader::Key> + '_ {
        self.wake.settle(&self.buffer, &mut self.indexes);
        self.wake.keys.drain(..)
    }

    fn index(&mut self, key: reader::Key) -> &mut Index {
        let Some(&place) = self.places.get(&key.slot) else {
            panic!("the shard does not carry the index at {:?}", key.slot);
        };
        &mut self.indexes[place]
    }

    /// Appends the unrecorded handoff of each index of `session`.
    fn record(&mut self, session: &Session, mesh: Interval) {
        let groups = (0..session.claims.len()).map(|group| {
            u32::try_from(group).expect("invariant: a key set has u32 groups")
        });
        // A handoff with no room waits, and a failed commit fails the next write.
        drop(handoffs(
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
        let keys = sessions
            .iter()
            .map(|&session| reader::Key { slot, session });
        self.keys.extend(keys);
    }

    /// Lists `index`, at `slot` and `place`, which took a live frame, once.
    fn list(&mut self, slot: Slot, place: usize, index: &mut Index) {
        if !mem::replace(&mut index.listed, true) {
            self.listed.push((slot, place));
        }
    }

    /// Gives complete readers the live frames on disk, once after each commit.
    fn settle(&mut self, buffer: &Buffer, indexes: &mut [Index]) {
        if !self.committed.replace(false) {
            return;
        }
        let mut listed = mem::take(&mut self.listed);
        listed.retain(|&(slot, place)| {
            let durable = buffer.durable(slot, Path::Live).seq;
            let index = &mut indexes[place];
            self.add(slot, index.readers.release(durable));
            index.listed = buffer.tail(slot, Path::Live).seq > durable;
            index.listed
        });
        self.listed = listed;
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

impl Batch {
    /// Adds the stored body of each accepted group of `checks`, in group order, with
    /// its index frame from `split`.
    fn bodies(
        &mut self,
        pool: &block::Pool,
        split: &mut Split<'_>,
        set: &KeySet,
        checks: &mut [(u32, Result<Accepted, Refusal>)],
    ) -> Result<(), block::Error> {
        for (group, checked) in checks {
            if let Ok(accepted) = checked {
                let draft = split.frame(pool, *group)?;
                let parts = stored::body(pool, accepted.freeze(draft, *group), set)?;
                self.bodies.push(Body {
                    group: *group,
                    range: range(accepted),
                    last: accepted.last(),
                    parts,
                });
            }
        }
        Ok(())
    }

    /// Appends the batch as one record on `path` at mesh time `mesh`, and empties
    /// it. Returns whether it found room. An empty batch appends nothing.
    ///
    /// # Errors
    ///
    /// [`Error::Disk`] after a failed commit, also for an empty batch.
    fn append(
        &mut self,
        buffer: &Buffer,
        session: &Session,
        path: Path,
        mesh: Interval,
    ) -> Result<bool, Error> {
        let bodies = self.bodies.drain(..).map(|body| {
            let (_, entry) = session.claim(body.group);
            Entry {
                index: entry.key,
                slot: entry.slot,
                path,
                first: body.range.seq,
                len: body.range.count,
                stored_at: mesh.latest,
                last: body.last,
                tag: stored::TAG,
                parts: body.parts.into(),
            }
        });
        room(buffer.append(bodies))
    }

    /// Empties the batch.
    fn clear(&mut self) {
        self.bodies.clear();
    }
}

/// Appends the unrecorded handoff of the index of each of `groups` of `session`, each
/// alone, on the live path at mesh time `mesh`, and marks it recorded. Stops at the
/// first that finds no room in the ring or the pool: it and the rest wait for the
/// next append on their index. Returns whether each found room.
///
/// # Errors
///
/// [`Error::Disk`] after a failed commit.
fn handoffs(
    buffer: &Buffer,
    pool: &block::Pool,
    indexes: &mut [Index],
    session: &Session,
    groups: impl Iterator<Item = u32>,
    mesh: Interval,
) -> Result<bool, Error> {
    for group in groups {
        let (claim, entry) = session.claim(group);
        let index = &mut indexes[claim.place];
        let Some((handoff, first)) = index.handoff() else {
            continue;
        };
        let Ok(parts) = handoff::body(pool, handoff) else {
            return Ok(false);
        };
        let appended = buffer.append([Entry {
            index: entry.key,
            slot: entry.slot,
            path: Path::Live,
            first,
            len: 0,
            stored_at: mesh.latest,
            last: None,
            tag: handoff::TAG,
            parts: parts.into(),
        }]);
        if !room(appended)? {
            return Ok(false);
        }
        index.gate.recorded();
    }
    Ok(true)
}

/// Spends the seq of each accepted group of `checks`, a frame on `path`, and makes
/// the outcome of each present group into `out`: applied when the append found
/// `room`, else lost. The readers to wake and each index with a stored live frame go
/// into `wake`.
fn spend<'a>(
    checks: &mut Vec<(u32, Result<Accepted, Refusal>)>,
    indexes: &mut [Index],
    session: &Session,
    path: Path,
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
                if path == Path::Live {
                    wake.list(slot, claim.place, index);
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
/// [`Error::Disk`] after a failed commit.
///
/// # Panics
///
/// If the append failed for another reason: a batch that no record holds, or a
/// failure that only an open has.
fn room(appended: Result<(), buffer::Error>) -> Result<bool, Error> {
    match appended {
        Ok(()) => Ok(true),
        Err(buffer::Error::Full { .. } | buffer::Error::Pool(_)) => Ok(false),
        Err(buffer::Error::Files(error)) => Err(Error::Disk(error)),
        Err(error) => panic!("an append failed for neither room nor a commit: {error}"),
    }
}

#[cfg(test)]
mod tests {
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
    use types::sample::{Scalar, Type};
    use types::time::Span;

    use super::*;
    use crate::common::{interner, key};

    const DIR: &str = "shard-0";
    const RING: &str = "shard-0/ring";
    const BLOCK: usize = 4096;
    /// A ring with room for each test but the full ring.
    const AREA: u64 = 1 << 18;
    /// A body that keeps a record in one block.
    const BODY_MAX: usize = 4087;
    const POOL: usize = 1 << 21;
    const COMMIT: Span = Span::from_nanos(10_000_000);
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
            let config = buffer::Config {
                files: self.node.files(),
                dir: PathBuf::from(DIR),
                pool: Rc::clone(&self.pool),
                clock: self.clock.clone(),
                tasks: self.tasks.clone(),
                entropy: self.entropy.clone(),
                layout: Layout::new(area, BODY_MAX).expect("a ring"),
                commit: COMMIT,
            };
            let mut assigned = Slots::new();
            for n in 0..slots {
                assigned.assign(key(Slot::new(n)));
            }
            let buffer = Buffer::open(config, &mut assigned).await.expect("opens");
            Shard::new(buffer, Rc::clone(&self.pool), LIMITS)
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
            let bytes = draft.series(entry).expect("the series is present");
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
                let values: Vec<i64> = (0..400)
                    .map(|k| {
                        (n * 400 + k)
                            .wrapping_mul(0x9E37_79B9_7F4A_7C15_u64.cast_signed())
                    })
                    .collect();
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
            let long = "b".repeat(255);
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
    fn records_each_handoff_at_close_when_no_record_holds_them_together() {
        run(18, |test| async move {
            let (mut shard, set) = test.wide(WIDE).await;
            let a = shard.open_writer(writer("a", 2, &set), NOW, MESH);
            let long = "b".repeat(255);
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
            let long = "b".repeat(255);
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
            assert_eq!(
                shard.committed().await,
                Err(buffer::Error::Files(failed.clone()))
            );
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
                message: "the shard does not carry index \
                          03000000-0000-0000-0000-000000000003"
                    .into(),
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
                    let Type::Scalar(scalar) = series.data_type else {
                        panic!("a scalar series");
                    };
                    let mut out = vec![0; 16];
                    codec::decode(scalar, 2, series.bytes, &mut out).expect("decodes");
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
        use std::task::{Context, Waker};
        use std::{iter, pin};

        use super::*;
        use crate::reader::Mode;

        const CREDIT: u64 = 1 << 20;
        const COMPLETE: Mode = Mode::Complete {
            limit_bytes: CREDIT,
        };

        fn woken(shard: &mut Shard) -> Vec<reader::Key> {
            shard.woken().collect()
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
                assert_eq!(shard.wake.listed, [(Slot::new(0), 0)]);
                assert_eq!(woken(&mut shard), []);
                assert_eq!(taken(&mut shard, reader, 0), []);
                shard.committed().await.expect("the commit ends");
                assert_eq!(woken(&mut shard), [reader]);
                assert_eq!(taken(&mut shard, reader, 0), [seq(0, 2), seq(2, 1)]);
                assert!(shard.wake.listed.is_empty(), "every frame is on disk");
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
                let mut commit = pin::pin!(shard.committed());
                let mut context = Context::from_waker(Waker::noop());
                assert!(commit.as_mut().poll(&mut context).is_pending());
                write(&test, &mut shard, a, &[20]);
                assert_eq!(woken(&mut shard), [latest]);
                assert_eq!(taken(&mut shard, latest, 0), [seq(1, 1)]);
                assert_eq!(taken(&mut shard, complete, 0), []);
                commit.await.expect("the commit ends");
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
                shard.committed().await.expect("the commit ends");
                assert!(
                    shard.wake.listed.is_empty(),
                    "a lost group waits for no commit"
                );
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
                assert_eq!(taken(&mut shard, reader, 0), [seq(0, 1)]);
                shard.grant(reader, CREDIT);
                write(&test, &mut shard, a, &[20]);
                write(&test, &mut shard, a, &[30]);
                shard.committed().await.expect("the commit ends");
                assert_eq!(taken(&mut shard, reader, 0), [seq(1, 1), seq(2, 1)]);
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
                let mut woken = woken(&mut shard);
                woken.sort_by_key(|key| (key.slot, key.session));
                assert_eq!(woken, [first, second, other]);
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
                assert!(
                    shard.wake.listed.is_empty(),
                    "no live frame waits for a commit"
                );
                shard.committed().await.expect("the commit ends");
                assert_eq!(woken(&mut shard), []);
                assert_eq!(taken(&mut shard, latest, 0), []);
                assert_eq!(taken(&mut shard, complete, 0), []);
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
