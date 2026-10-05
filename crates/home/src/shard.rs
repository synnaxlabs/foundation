//! The write path of one shard: the indexes it carries, their writers, and each frame
//! from split to one buffer append.

use std::rc::Rc;
use std::sync::Arc;
use std::{fmt, mem};

use block::Block;
use buffer::{Buffer, Entry};
use types::channel::Slot;
use types::frame::key_set::{self, KeySet};
use types::frame::{self, Draft, Label, Path};
use types::hash;
use types::time::{Interval, Monotonic, Stamp};

use crate::Refusal;
use crate::index::{Accepted, Index};
use crate::split::Split;
use crate::writer::{self, Writer};
use crate::{handoff, order, split, stored};

/// The write path of one shard. Each call is on the shard's thread.
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
}

/// An open writer.
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
    gate: control::Key,
}

/// The buffers of a write, kept from one write to the next.
#[derive(Debug, Default)]
struct Scratch {
    split: split::Scratch,
    /// The check of each present group, in group order.
    checks: Vec<(u32, Result<Accepted, Refusal>)>,
    /// The stored body of each accepted group, in group order.
    bodies: Vec<Body>,
    /// The handoff body of each group whose index has a handoff to record.
    handoffs: Vec<(u32, Option<Block>)>,
    /// Empty between appends. See [`reuse`].
    entries: Vec<Entry<'static>>,
    outcomes: Vec<Outcome>,
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
    /// A sync failed. The shard takes no more frames.
    Disk(env::files::Error),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Resend => write!(f, "the home does not take a resend frame yet"),
            Self::Full => write!(f, "the ring or the pool has no room for the frame"),
            Self::Disk(error) => write!(f, "a sync failed: {error}"),
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
            buffer,
            pool,
            limits,
            indexes: Vec::new(),
            places: hash::Map::default(),
            writers: hash::Map::default(),
            next: 0,
            scratch: Scratch::default(),
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
    /// no room waits for the next append on its index. A failed sync fails the next
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
                let gate = self.indexes[place].gate.open(control.clone(), lease, now);
                Claim { place, gate }
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
            self.indexes[claim.place].gate.close(claim.gate, now);
        }
        self.record(&session, mesh);
    }

    /// Applies `frame` to each index it holds, whole or not at all per index, in one
    /// append, at monotonic time `now` and mesh time `mesh`. Returns the outcome of
    /// each present group, in group order. An index's unrecorded handoff goes in
    /// before its frame.
    ///
    /// # Errors
    ///
    /// [`Error::Resend`] for a frame labeled resend. [`Error::Full`] for a backfill
    /// frame when the ring or the pool has no room; no seq moves. [`Error::Disk`]
    /// after a failed sync.
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
            let checked = match stamps {
                Ok(stamps) => index.check(claim.gate, path, stamps, now, mesh),
                Err(error) => Err(match index.gate.check(claim.gate, now) {
                    Ok(_) => Refusal::Codec(error),
                    Err(control) => Refusal::Control(control),
                }),
            };
            scratch.checks.push((group, checked));
        }
        let ready = frames(
            &self.pool,
            &mut split,
            &session.set,
            &mut scratch.checks,
            &mut scratch.bodies,
        )
        .and_then(|()| {
            let groups = scratch.checks.iter().map(|&(group, _)| group);
            handoffs(
                &self.pool,
                &self.indexes,
                session,
                groups,
                &mut scratch.handoffs,
            )
        });
        drop(split);
        let room = match ready {
            Ok(()) => append(&self.buffer, &mut scratch.entries, |batch| {
                let (handoffs, bodies) = (&scratch.handoffs, &scratch.bodies);
                entries(&self.indexes, session, path, mesh, handoffs, bodies, batch);
            }),
            Err(_) => Ok(false),
        };
        scratch.bodies.clear();
        let room = room.and_then(|room| match (room, path) {
            (false, Path::Backfill) => Err(Error::Full),
            _ => Ok(room),
        });
        match room {
            Ok(room) => Ok(scratch.finish(&mut self.indexes, session, room)),
            Err(error) => {
                scratch.checks.clear();
                scratch.handoffs.clear();
                Err(error)
            }
        }
    }

    /// Resolves when the next group commit ends.
    pub(crate) fn committed(&self) -> buffer::Commit<'_> {
        self.buffer.committed()
    }

    /// The first seq on `path` of the index at `slot` that is not on disk.
    pub(crate) fn stored(&self, slot: Slot, path: Path) -> u64 {
        self.buffer.durable(slot, path).seq
    }

    /// Appends the unrecorded handoff of each index of `session`, and marks each
    /// recorded when it went in.
    fn record(&mut self, session: &Session, mesh: Interval) {
        let groups = (0..session.claims.len()).map(|group| {
            u32::try_from(group).expect("invariant: a key set has u32 groups")
        });
        let pending = &mut self.scratch.handoffs;
        let made = handoffs(&self.pool, &self.indexes, session, groups, pending);
        if made.is_ok() && !pending.is_empty() {
            let room = append(&self.buffer, &mut self.scratch.entries, |batch| {
                entries(
                    &self.indexes,
                    session,
                    Path::Live,
                    mesh,
                    pending,
                    &[],
                    batch,
                );
            });
            if room == Ok(true) {
                for &(group, _) in pending.iter() {
                    self.indexes[session.claim(group).0.place].gate.recorded();
                }
            }
        }
        pending.clear();
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

impl Scratch {
    /// Spends the seq of each accepted group, marks each handoff recorded when the
    /// append found `room`, and returns the outcome of each present group.
    fn finish(
        &mut self,
        indexes: &mut [Index],
        session: &Session,
        room: bool,
    ) -> &[Outcome] {
        for (group, _) in self.handoffs.drain(..) {
            if room {
                indexes[session.claim(group).0.place].gate.recorded();
            }
        }
        self.outcomes.clear();
        for (group, checked) in self.checks.drain(..) {
            let (claim, entry) = session.claim(group);
            let slot = entry.slot;
            self.outcomes.push(match checked {
                Ok(accepted) => {
                    let range = range(&accepted);
                    indexes[claim.place].advance(accepted);
                    if room {
                        Outcome::Applied { slot, range }
                    } else {
                        Outcome::Lost { slot, range }
                    }
                }
                Err(refusal) => Outcome::Refused { slot, refusal },
            });
        }
        &self.outcomes
    }
}

/// Makes the index frame and the stored body of each accepted group of `checks`, in
/// group order, into `out`.
fn frames(
    pool: &block::Pool,
    split: &mut Split<'_>,
    set: &KeySet,
    checks: &mut [(u32, Result<Accepted, Refusal>)],
    out: &mut Vec<Body>,
) -> Result<(), block::Error> {
    for (group, checked) in checks {
        if let Ok(accepted) = checked {
            let draft = split.frame(pool, *group)?;
            let parts = stored::body(pool, accepted.freeze(draft, *group), set)?;
            out.push(Body {
                group: *group,
                range: range(accepted),
                last: accepted.last(),
                parts,
            });
        }
    }
    Ok(())
}

/// Makes the handoff body of the index of each of `groups` whose gate has a handoff to
/// record, into `out`.
fn handoffs(
    pool: &block::Pool,
    indexes: &[Index],
    session: &Session,
    groups: impl Iterator<Item = u32>,
    out: &mut Vec<(u32, Option<Block>)>,
) -> Result<(), block::Error> {
    for group in groups {
        let (claim, _) = session.claim(group);
        if let Some(handoff) = indexes[claim.place].gate.handoff() {
            out.push((group, handoff::body(pool, handoff)?));
        }
    }
    Ok(())
}

/// Makes the entries of one append on `path` into `out`: each handoff, on the live
/// path at its tail, then each body.
fn entries<'a>(
    indexes: &[Index],
    session: &Session,
    path: Path,
    mesh: Interval,
    handoffs: &'a [(u32, Option<Block>)],
    bodies: &'a [Body],
    out: &mut Vec<Entry<'a>>,
) {
    let handoffs = handoffs.iter().map(|(group, parts)| {
        let (claim, entry) = session.claim(*group);
        Entry {
            index: entry.key,
            slot: entry.slot,
            path: Path::Live,
            first: indexes[claim.place].order.tail(Path::Live).seq,
            len: 0,
            stored_at: mesh.latest,
            last: None,
            tag: handoff::TAG,
            parts: parts.as_slice(),
        }
    });
    let bodies = bodies.iter().map(|body| {
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
            parts: &body.parts,
        }
    });
    out.extend(handoffs.chain(bodies));
}

/// Appends the entries `fill` makes as one batch, in the vector kept in `spare`.
/// Returns whether the batch found room. An empty batch appends nothing.
///
/// # Errors
///
/// [`Error::Disk`] after a failed sync.
fn append<'a>(
    buffer: &Buffer,
    spare: &mut Vec<Entry<'static>>,
    fill: impl FnOnce(&mut Vec<Entry<'a>>),
) -> Result<bool, Error> {
    let mut batch = reuse(mem::take(spare));
    fill(&mut batch);
    let room = if batch.is_empty() {
        Ok(true)
    } else {
        room(buffer.append(&batch))
    };
    *spare = reuse(batch);
    room
}

/// Empties `entries` and gives back its allocation for entries of another lifetime,
/// so an append allocates no vector.
fn reuse<'b>(mut entries: Vec<Entry<'_>>) -> Vec<Entry<'b>> {
    entries.clear();
    // Same layout, so the collect runs in place and keeps the allocation.
    entries
        .into_iter()
        .map(|_| -> Entry<'b> { unreachable!("the entries are cleared") })
        .collect()
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
/// [`Error::Disk`] after a failed sync.
///
/// # Panics
///
/// If the append failed for another reason, which no append can.
fn room(appended: Result<(), buffer::Error>) -> Result<bool, Error> {
    match appended {
        Ok(()) => Ok(true),
        Err(buffer::Error::Full { .. } | buffer::Error::Pool(_)) => Ok(false),
        Err(buffer::Error::Files(error)) => Err(Error::Disk(error)),
        Err(error) => {
            panic!("invariant: an append fails only for room or a sync: {error}")
        }
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
    use types::channel::Slots;
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
        /// that carries the indexes of [`two_indexes`].
        async fn shard(&self, area: u64) -> Shard {
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
            let mut slots = Slots::new();
            for n in 0..4 {
                slots.assign(key(Slot::new(n)));
            }
            let buffer = Buffer::open(config, &mut slots).await.expect("opens");
            let mut shard = Shard::new(buffer, Rc::clone(&self.pool), LIMITS);
            shard.carry(Slot::new(0));
            shard.carry(Slot::new(2));
            shard
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

        /// Takes every block of the pool.
        fn fill(&self) -> Vec<Unique> {
            let mut blocks = Vec::new();
            for len in [1 << 16, BLOCK, 64, 1] {
                while let Ok(block) = self.pool.alloc(len) {
                    blocks.push(block);
                }
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
            let a = shard.open_writer(writer("a", 1, &set), NOW, MESH);
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
            let write = frame(&test.pool, &set, &[(0, &[20]), (1, &[2])]);
            let disk = Error::Disk(failed);
            assert_eq!(
                disk.to_string(),
                "a sync failed: sync of shard-0/ring failed with OS error 5"
            );
            assert_eq!(shard.write(a, LIVE, write, NOW, MESH), Err(disk));
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
    fn reuses_the_allocation_of_the_entries() {
        let mut entries: Vec<Entry<'_>> = Vec::with_capacity(8);
        let at = entries.as_ptr().addr();
        let parts: [Block; 0] = [];
        entries.push(Entry {
            index: key(Slot::new(0)),
            slot: Slot::new(0),
            path: Path::Live,
            first: 0,
            len: 0,
            stored_at: Stamp::from_nanos(1),
            last: None,
            tag: handoff::TAG,
            parts: &parts,
        });
        let entries: Vec<Entry<'static>> = reuse(entries);
        assert_eq!(
            (entries.as_ptr().addr(), entries.len(), entries.capacity()),
            (at, 0, 8)
        );
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
}
