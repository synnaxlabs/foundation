//! The indexes one shard carries: their writers, each frame from split to one buffer
//! append, and their readers.

use std::fmt;
use std::ops::Range;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use buffer::{Buffer, Entry};
use types::channel::Slot;
use types::frame::key_set::{self, KeySet};
use types::frame::{self, Draft, Frame, Label, Path};
use types::hash;
use types::time::{Monotonic, Stamp};

use crate::Refusal;
use crate::index::{Accepted, Index};
use crate::reader;
use crate::split::Split;
use crate::writer::{self, Writer};
use crate::{handoff, order, split, stored};

/// The indexes of one shard, with their writers and readers. It is not `Send`: each
/// call is on the shard's thread.
///
/// # Examples
///
/// ```
/// # use std::path::PathBuf;
/// # use std::rc::Rc;
/// #
/// # use block::{Heap, Pool};
/// # use buffer::{Buffer, Layout};
/// # use types::channel::{Key, Slots};
/// # use types::frame::key_set::{Group, Interner};
/// # use types::sample::{Scalar, Type};
/// #
/// # type Error = Box<dyn std::error::Error>;
/// #
/// # async fn example(
/// #     node: sim::node::Node,
/// #     tasks: env::tasks::Tasks,
/// # ) -> Result<(), Error> {
/// #     let (driver, clock) = clock::Clock::new(node.clock());
/// #     let wall = node.wall();
/// #     tasks.spawn(async move { driver.run(wall).await });
/// #     let (stamps, values) = (Key::from_u128(1), Key::from_u128(2));
/// #     let mut slots = Slots::new();
/// #     let mut interner = Interner::new();
/// #     let index = slots.assign(stamps);
/// #     slots.assign(values);
/// #     interner.slots().assign(stamps);
/// #     interner.slots().assign(values);
/// #     let set = interner.intern(&[Group {
/// #         index: stamps,
/// #         data: &[(values, Type::Scalar(Scalar::I64))],
/// #     }]);
/// #     let config = block::Config { budget: 1 << 21 };
/// #     let heap = Heap::new(config.reservation());
/// #     let config = buffer::Config {
/// #         files: node.files(),
/// #         dir: PathBuf::from("shard-0"),
/// #         pool: Rc::new(Pool::new(config, heap)),
/// #         clock: node.clock(),
/// #         tasks,
/// #         entropy: node.entropy(),
/// #         layout: Layout::new(1 << 18, 4087).expect("a ring"),
/// #         commit: Span::from_nanos(10_000_000),
/// #     };
/// #     let buffer = Buffer::open(config, &mut slots).await?;
/// #     while clock.now().mesh.is_none() {
/// #         node.clock().sleep(Span::from_nanos(1)).await;
/// #     }
/// use home::{Config, Outcome, Shard, order, writer};
/// use types::authority::Authority;
/// use types::frame::{Draft, Form, Label, Path, Range};
/// use types::time::{Span, Stamp};
///
/// let mut shard = Shard::new(Config {
///     shard: 0,
///     buffer,
///     clock,
///     limits: order::Limits {
///         earliest: Stamp::from_nanos(1),
///         ahead: Span::from_nanos(1_000_000_000),
///     },
/// });
/// shard.carry(index);
/// let reader = shard.open_complete(index, 1 << 20);
///
/// let mut frame = Draft::new(shard.pool(), &set, Form::Raw, &[(0, 8), (1, 8)])?;
/// for (entry, sample) in [(0, 10_i64), (1, 7)] {
///     let series = frame.series_mut(entry).expect("the series is present");
///     series.copy_from_slice(&sample.to_le_bytes());
/// }
/// frame.set_count(0, 1);
/// let writer = shard.open_writer(writer::Writer {
///     subject: "a".parse()?,
///     authority: Authority(1),
///     lease: None,
///     set,
/// })?;
///
/// let range = Range { seq: 0, count: 1 };
/// let written = shard.write(writer, Label::Path(Path::Live), frame)?;
/// assert_eq!(written, [Outcome::Applied { slot: index, range }]);
///
/// shard.committed().await?;
/// let mut woken = Vec::new();
/// shard.woken(&mut woken);
/// assert_eq!(woken, [reader.into()]);
/// let taken = shard.take(reader.into()).expect("a frame waits");
/// assert_eq!(taken.range(0), Some(range));
/// #     Ok(())
/// # }
/// #
/// # fn main() {
/// #     let mut sim = sim::Sim::new(sim::Config::default());
/// #     let node = sim.node(sim::node::Config::default());
/// #     let config = env::shards::Config {
/// #         name: "shard-0".into(),
/// #         core: None,
/// #     };
/// #     let handle = node
/// #         .shards()
/// #         .start(config, move |tasks| async move {
/// #             example(node, tasks).await.expect("the example ends");
/// #         })
/// #         .expect("the shard starts");
/// #     sim.run().expect("the run ends");
/// #     handle.join().expect("the shard ended");
/// # }
/// ```
#[derive(Debug)]
pub struct Shard {
    /// The shard's number on its node.
    number: u32,
    buffer: Buffer,
    clock: clock::Reader,
    limits: order::Limits,
    indexes: Vec<Index>,
    /// The place in `indexes` of each carried index.
    places: hash::Map<Slot, usize>,
    /// Each open writer, by its number on the shard.
    writers: hash::Map<u64, Session>,
    next: u64,
    scratch: Scratch,
    /// The readers of each index, by the same place as `indexes`.
    readers: reader::Set,
}

/// What a shard is built from.
#[derive(Debug)]
pub struct Config {
    /// The shard's number on its node. Each writer key the shard gives carries it,
    /// so it must differ for each shard of the node.
    pub shard: u32,
    /// The shard's buffer. Index frames, stored headers, and handoff bodies come
    /// from its pool.
    pub buffer: Buffer,
    /// The node's clocks. Control leases expire on its monotonic clock. Stamp
    /// checks, handoffs, and stored entries read its mesh time.
    pub clock: clock::Reader,
    /// The stamps each index accepts.
    pub limits: order::Limits,
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
    /// The check of each present group, in group order, with its index frame once
    /// frozen.
    checks: Vec<(u32, Result<Accepted, Refusal>, Option<Frame>)>,
    /// The stored entry of each accepted group, in group order. Empty between
    /// appends.
    entries: Vec<Entry>,
    outcomes: Vec<Outcome>,
}

/// What became of one group of a frame.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
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

/// Why a write failed. No seq moves for any of them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Error {
    /// The frame is labeled resend, which the home does not take yet.
    Resend,
    /// A backfill frame found no room in the ring or the pool. No seq moves. The pool
    /// has room again when commits end and readers take their frames. The ring has
    /// room again only when records leave it, which no write does. No call says when
    /// room returns: write the frame again on a timer.
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

/// Resolves when every frame written and every handoff appended before
/// [`Shard::committed`] is on disk, or with the error that ended the buffer first. It
/// does not borrow the shard, and it holds the shard's ring open until it drops.
#[derive(Debug)]
pub struct Commit(buffer::Commit);

impl Future for Commit {
    type Output = Result<(), env::files::Error>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        Pin::new(&mut self.0).poll(cx)
    }
}

impl Shard {
    /// A shard over `config.buffer` that carries no index yet.
    pub fn new(config: Config) -> Self {
        let Config {
            shard,
            buffer,
            clock,
            limits,
        } = config;
        Self {
            number: shard,
            buffer,
            clock,
            limits,
            indexes: Vec::new(),
            places: hash::Map::default(),
            writers: hash::Map::default(),
            next: 0,
            scratch: Scratch::default(),
            readers: reader::Set::default(),
        }
    }

    /// Carries the index at `slot`, with no writer in control. Each path continues
    /// from its tail in the buffer.
    ///
    /// # Panics
    ///
    /// If the shard carries `slot` already.
    pub fn carry(&mut self, slot: Slot) {
        let tail = |path| {
            let tail = self.buffer.tail(slot, path);
            order::Tail {
                stamp: tail.stamp,
                seq: tail.seq,
            }
        };
        let live = tail(Path::Live);
        let index = Index::new(self.limits, live, tail(Path::Backfill));
        let place = self.indexes.len();
        let carried = self.places.insert(slot, place);
        assert!(carried.is_none(), "the shard carries {slot:?} already");
        self.indexes.push(index);
        self.readers.carry(place, slot, live.seq);
    }

    /// The pool of the shard's buffer. Frames that a writer fills come from it.
    #[must_use]
    pub fn pool(&self) -> &block::Pool {
        self.buffer.pool()
    }

    /// Opens `writer` on each index of its key set, and appends a handoff for each
    /// index where it takes control. A handoff that finds no room waits for the next
    /// append on its index. A failed commit fails the next write.
    ///
    /// # Errors
    ///
    /// In this order: [`writer::Error::Unsynced`] before the node first has mesh
    /// time, [`writer::Error::Lease`] for a lease that is not longer than zero, and
    /// [`writer::Error::Type`] for the first series of the key set with a type the
    /// home does not write yet. None changes the shard.
    ///
    /// # Panics
    ///
    /// If an index of the key set is not carried, in a call that gives no error.
    pub fn open_writer(
        &mut self,
        writer: Writer,
    ) -> Result<writer::Key, writer::Error> {
        let Writer {
            subject,
            authority,
            lease,
            set,
        } = writer;
        let (now, mesh) = self.now().ok_or(writer::Error::Unsynced)?;
        let lease = lease.map(writer::lease).transpose()?;
        if let Some(&key_set::Entry {
            slot, data_type, ..
        }) = split::unwritten(&set)
        {
            return Err(writer::Error::Type { slot, data_type });
        }
        let control = control::Writer { subject, authority };
        let entries = set.entries();
        let mut claims = Vec::with_capacity(set.groups().len());
        for &entry in set.groups() {
            let place = self.place(entries[entry].slot);
            let key = self.indexes[place].gate.open(control.clone(), lease, now);
            claims.push(Claim { place, key });
        }
        let session = Session { set, claims };
        self.record_all(&session, mesh);
        let key = writer::Key {
            shard: self.number,
            number: self.next,
        };
        self.next += 1;
        self.writers.insert(key.number, session);
        Ok(key)
    }

    /// Closes the writer, and appends a handoff for each index it held, as
    /// [`open_writer`](Self::open_writer) does.
    ///
    /// # Panics
    ///
    /// If the writer is not open, or `key` is of another shard.
    pub fn close_writer(&mut self, key: writer::Key) {
        let number = key.on(self.number);
        let Some(session) = self.writers.remove(&number) else {
            panic!("writer {number} is not open");
        };
        let (now, mesh) = self.time();
        for claim in &session.claims {
            self.indexes[claim.place].gate.close(claim.key, now);
        }
        self.record_all(&session, mesh);
    }

    /// Applies `frame` to each index it holds, whole or not at all per index. The
    /// frame's bodies go in one append, after the unrecorded handoff of each of its
    /// indexes. Returns the outcome of each present group, in group order.
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
    /// If the writer is not open or `key` is of another shard, before any error. If
    /// the frame is not of the writer's key set, unless it is labeled resend: the
    /// shard does not read a resend frame.
    pub fn write(
        &mut self,
        key: writer::Key,
        label: Label,
        frame: Draft,
    ) -> Result<&[Outcome], Error> {
        let number = key.on(self.number);
        let Some(session) = self.writers.get(&number) else {
            panic!("writer {number} is not open");
        };
        let Label::Path(path) = label else {
            return Err(Error::Resend);
        };
        let (now, mesh) = self.time();
        let scratch = &mut self.scratch;
        let mut split = scratch.split.split(&session.set, frame);
        while let Some((group, stamps)) = split.next() {
            let (claim, _) = session.claim(group);
            let index = &mut self.indexes[claim.place];
            let checked = index.check(claim.key, path, stamps, now, mesh);
            scratch.checks.push((group, checked, None));
        }
        let groups = scratch.checks.iter().map(|&(group, ..)| group);
        let recorded = record(&self.buffer, &mut self.indexes, session, groups, mesh);
        let entries = &mut scratch.entries;
        let made = freeze(
            entries,
            self.buffer.pool(),
            &mut split,
            &session.set,
            &mut scratch.checks,
            mesh,
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
                &mut self.readers,
                &mut scratch.outcomes,
            )),
            Err(error) => {
                scratch.checks.clear();
                Err(error)
            }
        }
    }

    /// Resolves when every frame written and every handoff appended before the call
    /// is on disk: at once when none waits, else at the end of the group commit that
    /// holds the last of them. Commits run without this future, so a caller may drop
    /// it. Call [`woken`](Self::woken) after it resolves.
    /// Gives the error that ended the buffer when it ended before they were on disk.
    pub fn committed(&self) -> Commit {
        Commit(self.buffer.committed())
    }

    /// Opens an unnamed complete reader on the index at `slot`, with a credit of
    /// `limit_bytes`. From the index's live tail on, it gets each live frame with
    /// samples after the commit that holds it, while the bytes it has spent are
    /// below its credit: a frame spends its [`Frame::charge`]. The first such frame
    /// that finds the credit spent is a miss: the reader gets neither it nor a later
    /// frame, no grant changes that, and [`behind`](Self::behind) reports it. The home
    /// does not read a missed frame back from disk yet. Close the reader and open a new one. The
    /// new one starts at the live tail of its open, so the frames from the miss to
    /// there reach neither reader.
    ///
    /// # Panics
    ///
    /// If the shard does not carry `slot`.
    #[must_use = "the reader stays open until `close_reader` gets its key"]
    pub fn open_complete(
        &mut self,
        slot: Slot,
        limit_bytes: u64,
    ) -> reader::complete::Key {
        let place = self.place(slot);
        let live = self.indexes[place].live_tail();
        let session = self.readers.open_complete(place, live, limit_bytes);
        reader::complete::Key { slot, session }
    }

    /// Opens an unnamed latest reader on the index at `slot`. It gets the index's
    /// newest live frame, before its commit. Take from it at once:
    /// [`woken`](Self::woken) does not name it for a frame it can take at open.
    ///
    /// # Panics
    ///
    /// If the shard does not carry `slot`.
    #[must_use = "the reader stays open until `close_reader` gets its key"]
    pub fn open_latest(&mut self, slot: Slot) -> reader::Key {
        let session = self.readers.open_latest(self.place(slot)).into();
        reader::Key { slot, session }
    }

    /// Raises the credit of the complete reader `key` to `limit_bytes` since it
    /// opened. A limit that is not higher changes nothing, and so does a grant to a
    /// closed reader: a grant can arrive after its reader closes.
    ///
    /// # Panics
    ///
    /// If the shard never gave `key`.
    pub fn grant(&mut self, key: reader::complete::Key, limit_bytes: u64) {
        let place = self.place(key.slot);
        self.readers.grant(place, key.session, limit_bytes);
    }

    /// Takes the next frame of the reader `key`, or `None` when none waits or the
    /// reader is closed. A complete reader that misses a frame
    /// ([`open_complete`](Self::open_complete)) gets the frames before it, and then
    /// `None`.
    ///
    /// # Panics
    ///
    /// If the shard never gave `key`.
    pub fn take(&mut self, key: reader::Key) -> Option<Frame> {
        self.readers.take(self.place(key.slot), key.session)
    }

    /// Whether the complete reader `key` missed a live frame, so it gets no later
    /// one, as it had no credit for the frame. [`woken`](Self::woken) names it once
    /// when it misses one with no frame waiting. `false` for a closed reader.
    ///
    /// # Panics
    ///
    /// If the shard never gave `key`.
    #[must_use]
    pub fn behind(&self, key: reader::complete::Key) -> bool {
        self.readers.behind(self.place(key.slot), key.session)
    }

    /// Closes the reader `key`. Its waiting frames do not go out, and
    /// [`woken`](Self::woken) does not name it. A close of a closed reader changes
    /// nothing.
    ///
    /// # Panics
    ///
    /// If the shard never gave `key`.
    pub fn close_reader(&mut self, key: reader::Key) {
        self.readers.close(self.place(key.slot), key.session);
    }

    /// Replaces `keys` with the readers to wake since the last call, each once, in slot
    /// order and with the latest readers of an index first. Complete readers first get
    /// the live frames now on disk. A key is a hint: take from each until
    /// [`take`](Self::take) gives `None`. Call it after each write and each commit.
    /// When a commit ended since the last call, it reads each index with live frames
    /// queued for complete readers; else it reads none. Pass the same `keys` each
    /// time: no call allocates once a call has given as many keys.
    pub fn woken(&mut self, keys: &mut Vec<reader::Key>) {
        self.readers.woken(&self.buffer, keys);
    }

    /// A reading of the monotonic clock, and mesh time at it: the midpoint of the
    /// clock's interval, which never goes back. The latest edge can go back as the
    /// error shrinks, and is a century out while the error is unknown. `None` before
    /// the node first has mesh time.
    fn now(&self) -> Option<(Monotonic, Stamp)> {
        let clock::Time { monotonic, mesh } = self.clock.now();
        let mesh = mesh?;
        let midpoint = mesh.earliest.nanos().midpoint(mesh.latest.nanos());
        Some((monotonic, Stamp::from_nanos(midpoint)))
    }

    /// Both clocks now, as [`now`](Self::now) gives them.
    ///
    /// # Panics
    ///
    /// Before the node first has mesh time. No writer opens before it, and mesh time
    /// stays once known.
    fn time(&self) -> (Monotonic, Stamp) {
        let now = self.now();
        now.expect("invariant: a writer opened with mesh time, which stays")
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
    fn record_all(&mut self, session: &Session, mesh: Stamp) {
        let groups = (0..session.claims.len()).map(|group| {
            u32::try_from(group).expect("invariant: a key set has u32 groups")
        });
        // A handoff with no room waits, and a failed commit fails the next write.
        drop(record(
            &self.buffer,
            &mut self.indexes,
            session,
            groups,
            mesh,
        ));
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

/// Freezes the index frame from `split` of each accepted group of `checks` into its
/// check, and pushes its stored entry onto `entries`, in group order, at mesh time
/// `stored_at`.
///
/// # Errors
///
/// [`block::Error`] when `pool` has no block for an index frame or a header.
fn freeze(
    entries: &mut Vec<Entry>,
    pool: &block::Pool,
    split: &mut Split<'_>,
    set: &KeySet,
    checks: &mut [(u32, Result<Accepted, Refusal>, Option<Frame>)],
    stored_at: Stamp,
) -> Result<(), block::Error> {
    for (group, checked, frozen) in checks {
        if let Ok(accepted) = checked {
            let draft = split.frame(pool, *group)?;
            let frame = frozen.insert(accepted.freeze(draft, *group));
            let last = accepted.last();
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
    indexes: &mut [Index],
    session: &Session,
    groups: impl Iterator<Item = u32>,
    mesh: Stamp,
) -> Result<bool, Error> {
    let pool = buffer.pool();
    let mut all = true;
    for group in groups {
        let (claim, entry) = session.claim(group);
        let index = &mut indexes[claim.place];
        let Some((handoff, first)) = index.handoff() else {
            continue;
        };
        let appended = match handoff::entry(pool, handoff, entry, first, mesh) {
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
/// present group into `out`: applied when the append found `room`, else lost. Each
/// frame goes to `readers`.
fn spend<'a>(
    checks: &mut Vec<(u32, Result<Accepted, Refusal>, Option<Frame>)>,
    indexes: &mut [Index],
    session: &Session,
    room: bool,
    readers: &mut reader::Set,
    out: &'a mut Vec<Outcome>,
) -> &'a [Outcome] {
    out.clear();
    for (group, checked, frozen) in checks.drain(..) {
        let (claim, entry) = session.claim(group);
        let slot = entry.slot;
        let index = &mut indexes[claim.place];
        out.push(match checked {
            Ok(accepted) if room => {
                let seq = accepted.seq();
                let range = range(&seq);
                index.spend(accepted);
                let frame = frozen.expect("invariant: a stored frame was frozen");
                readers.applied(claim.place, frame, seq);
                Outcome::Applied { slot, range }
            }
            Ok(accepted) => {
                let range = range(&accepted.seq());
                index.spend(accepted);
                if let Some(frame) = frozen {
                    readers.lost(claim.place, frame);
                }
                Outcome::Lost { slot, range }
            }
            Err(refusal) => Outcome::Refused { slot, refusal },
        });
    }
    out
}

/// The seq range of an accepted group at `seq`.
fn range(seq: &Range<u64>) -> frame::Range {
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
    use std::rc::Rc;
    use std::task::Waker;

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
    const LEASE: Span = Span::from_nanos(1_000_000);
    /// More than half of `LEASE`: one wait keeps a lease, and two end it.
    const WAIT: Span = Span::from_nanos(600_000);
    /// Past the first commit interval, while its sync runs.
    const SYNC: Span = Span::from_nanos(10_001_000);
    const LIMITS: order::Limits = order::Limits {
        earliest: Stamp::from_nanos(1),
        ahead: Span::from_nanos(1_000_000_000),
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
        reader: clock::Reader,
        tasks: Tasks,
        entropy: Entropy,
    }

    impl Test {
        /// What a test on a shard of `node` gets, with the node's clock running.
        fn new(node: sim::node::Node, tasks: Tasks) -> Self {
            let config = block::Config { budget: POOL };
            let pool = Pool::new(config.clone(), Heap::new(config.reservation()));
            let (clock, reader) = clock::Clock::new(node.clock());
            let wall = node.wall();
            tasks.spawn(async move { clock.run(wall).await });
            Self {
                clock: node.clock(),
                reader,
                entropy: node.entropy(),
                node,
                pool: Rc::new(pool),
                tasks,
            }
        }

        /// A shard over the ring of the node, made with `area` bytes when it is new,
        /// with slots 0 to `slots` assigned and no index carried, once the node has
        /// mesh time.
        async fn open(&self, area: u64, slots: u32) -> Shard {
            let buffer = self.buffer(area, BODY_MAX, slots).await;
            self.over(buffer).await
        }

        /// A shard over `buffer`, once the node has mesh time.
        async fn over(&self, buffer: Buffer) -> Shard {
            self.numbered(0, buffer).await
        }

        /// A shard of the number `shard` over `buffer`, once the node has mesh time.
        async fn numbered(&self, shard: u32, buffer: Buffer) -> Shard {
            while self.reader.now().mesh.is_none() {
                self.clock.sleep(Span::from_nanos(1)).await;
            }
            Self::with(shard, buffer, self.reader.clone())
        }

        /// A shard over the ring of the node that never has mesh time, with no index
        /// carried.
        async fn unsynced(&self) -> Shard {
            let buffer = self.buffer(AREA, BODY_MAX, 4).await;
            // A clock that never runs never has mesh time.
            let (_, reader) = clock::Clock::new(self.clock.clone());
            Self::with(0, buffer, reader)
        }

        /// A shard of the number `shard` over `buffer`, with the clocks of `reader`.
        fn with(shard: u32, buffer: Buffer, reader: clock::Reader) -> Shard {
            Shard::new(Config {
                shard,
                buffer,
                clock: reader,
                limits: LIMITS,
            })
        }

        /// Mesh time now: the midpoint of the clock's interval.
        fn now(&self) -> Stamp {
            let now = self.reader.now().mesh.expect("the node has mesh time");
            Stamp::from_nanos(now.earliest.nanos().midpoint(now.latest.nanos()))
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

        /// A shard, a writer on it with a lease of `LEASE` that opened `WAIT` ago, and
        /// the writer's key set.
        async fn leased(&self) -> (Shard, writer::Key, Arc<KeySet>) {
            let set = two_indexes();
            let mut shard = self.shard(AREA).await;
            let leased = Writer {
                lease: Some(LEASE),
                ..writer("a", 1, &set)
            };
            let key = shard.open_writer(leased).expect("synced");
            self.clock.sleep(WAIT).await;
            (shard, key, set)
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
        let (sim, node) = one_node(seed);
        let config = env::shards::Config {
            name: DIR.into(),
            core: None,
        };
        let handle = node
            .shards()
            .start(config, move |tasks| main(Test::new(node, tasks)))
            .expect("the shard starts");
        (sim, handle)
    }

    fn one_node(seed: u64) -> (sim::Sim, sim::node::Node) {
        let mut sim = sim::Sim::new(sim::Config {
            seed,
            ..sim::Config::default()
        });
        let node = sim.node(sim::node::Config::default());
        (sim, node)
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

    /// The key set of one index, at a slot that [`Test::shard`] does not carry.
    fn not_carried() -> Arc<KeySet> {
        interner().intern(&[Group {
            index: key(Slot::new(3)),
            data: &[],
        }])
    }

    fn writer(subject: &str, authority: u8, set: &Arc<KeySet>) -> Writer {
        Writer {
            subject: subject.parse().expect("a valid name"),
            authority: Authority(authority),
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

    /// An encoded frame of `set` with each series of `series`, an entry and its bytes,
    /// and each count of `counts`, a group and its count.
    fn encoded_frame(
        pool: &Pool,
        set: &KeySet,
        series: &[(usize, &[u8])],
        counts: &[(u32, u32)],
    ) -> Draft {
        let lens: Vec<_> = series
            .iter()
            .map(|&(entry, bytes)| (entry, bytes.len()))
            .collect();
        let mut draft = Draft::new(pool, set, Form::Encoded, &lens).expect("a frame");
        for &(entry, bytes) in series {
            let out = draft.series_mut(entry).expect("the series is present");
            out.copy_from_slice(bytes);
        }
        for &(group, count) in counts {
            draft.set_count(group, count);
        }
        draft
    }

    /// `bytes` with the tag of its first vector made 9, which is no tag.
    fn untagged(mut bytes: Vec<u8>) -> Vec<u8> {
        bytes[0] = 9;
        bytes
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

    /// Asserts that `call` panics on a shard that does not carry the index at
    /// slot 3.
    fn check_not_carried(seed: u64, call: fn(&mut Shard)) {
        let (mut sim, _handle) = start(seed, move |test| async move {
            call(&mut test.shard(AREA).await);
        });
        assert_eq!(
            sim.run(),
            Err(sim::Error::Panicked {
                thread: DIR.into(),
                message: "the shard does not carry the index at Slot(3)".into(),
                seed,
            })
        );
    }

    const CREDIT: u64 = 1 << 20;

    fn woken(shard: &mut Shard) -> Vec<reader::Key> {
        let mut keys = Vec::new();
        shard.woken(&mut keys);
        keys
    }

    /// Opens a complete reader on the index at `slot`, with a credit of `CREDIT`.
    fn complete(shard: &mut Shard, slot: Slot) -> reader::Key {
        shard.open_complete(slot, CREDIT).into()
    }

    /// The seq of the index group `group` of each frame `reader` takes now.
    fn taken(shard: &mut Shard, reader: reader::Key, group: u32) -> Vec<Range> {
        iter::from_fn(|| shard.take(reader))
            .map(|frame| frame.range(group).expect("the index is present"))
            .collect()
    }

    fn seq(seq: u64, count: u32) -> Range {
        Range { seq, count }
    }

    /// The first seq on `path` of the index at `slot` that the ring does not hold
    /// on disk. It reads the buffer of the shard, because no call of `Shard` gives
    /// a stored seq. Use it only where no complete reader shows the seq: on the
    /// backfill path, for a lost frame, after a restart, and before a commit ends.
    fn stored(shard: &Shard, slot: Slot, path: Path) -> u64 {
        shard.buffer.durable(slot, path).seq
    }

    #[test]
    fn gives_each_index_gapless_seq_stored_after_the_commit() {
        run(1, |test| async move {
            let set = two_indexes();
            let mut shard = test.shard(AREA).await;
            let zero = complete(&mut shard, Slot::new(0));
            let two = complete(&mut shard, Slot::new(2));
            let a = shard.open_writer(writer("a", 1, &set)).expect("synced");
            let both = frame(
                &test.pool,
                &set,
                &[(0, &[10, 20]), (1, &[1, 2]), (2, &[15])],
            );
            assert_eq!(
                shard.write(a, LIVE, both),
                Ok(&[applied(0, 0, 2), applied(2, 0, 1)][..])
            );
            let one = frame(&test.pool, &set, &[(0, &[30]), (1, &[3])]);
            assert_eq!(shard.write(a, LIVE, one), Ok(&[applied(0, 2, 1)][..]));
            assert_eq!(stored(&shard, Slot::new(0), Path::Live), 0);
            assert_eq!(woken(&mut shard), []);
            shard.committed().await.expect("the commit ends");
            assert_eq!(woken(&mut shard), [zero, two]);
            assert_eq!(taken(&mut shard, zero, 0), [seq(0, 2), seq(2, 1)]);
            assert_eq!(taken(&mut shard, two, 1), [seq(0, 1)]);
            assert_eq!(stored(&shard, Slot::new(0), Path::Backfill), 0);
        });
    }

    #[test]
    fn continues_each_path_from_its_tail_in_the_buffer() {
        run(2, |test| async move {
            let set = two_indexes();
            let mut shard = test.shard(AREA).await;
            let a = shard.open_writer(writer("a", 1, &set)).expect("synced");
            let write = frame(&test.pool, &set, &[(0, &[10, 20]), (1, &[1, 2])]);
            shard.write(a, LIVE, write).expect("written");
            shard.committed().await.expect("the commit ends");
            drop(shard);
            let mut shard = test.shard(AREA).await;
            let a = shard.open_writer(writer("a", 1, &set)).expect("synced");
            let late = frame(&test.pool, &set, &[(0, &[15]), (1, &[3])]);
            let backwards = order::Error::Backwards {
                path: Path::Live,
                before: Stamp::from_nanos(20),
                stamp: Stamp::from_nanos(15),
            };
            assert_eq!(
                shard.write(a, LIVE, late),
                Ok(&[refused(0, Refusal::Order(backwards))][..])
            );
            let next = frame(&test.pool, &set, &[(0, &[30]), (1, &[3])]);
            assert_eq!(shard.write(a, LIVE, next), Ok(&[applied(0, 2, 1)][..]));
        });
    }

    #[test]
    fn refuses_a_writer_without_control_before_its_series_and_spends_no_seq() {
        run(3, |test| async move {
            let set = two_indexes();
            let mut shard = test.shard(AREA).await;
            let a = shard.open_writer(writer("a", 2, &set)).expect("synced");
            let b = shard.open_writer(writer("b", 1, &set)).expect("synced");
            let short =
                frame(&test.pool, &set, &[(0, &[10, 20]), (1, &[1]), (2, &[10])]);
            let waiting = Refusal::Waiting;
            assert_eq!(
                waiting.to_string(),
                "not in control: another writer holds the gate"
            );
            assert_eq!(
                shard.write(b, LIVE, short),
                Ok(&[refused(0, waiting.clone()), refused(2, waiting)][..])
            );
            let write = frame(&test.pool, &set, &[(2, &[10])]);
            assert_eq!(shard.write(a, LIVE, write), Ok(&[applied(2, 0, 1)][..]));
        });
    }

    #[test]
    fn refuses_a_backwards_index_and_applies_the_other() {
        run(4, |test| async move {
            let set = two_indexes();
            let mut shard = test.shard(AREA).await;
            let a = shard.open_writer(writer("a", 1, &set)).expect("synced");
            let write = frame(&test.pool, &set, &[(0, &[20]), (1, &[1]), (2, &[20])]);
            shard.write(a, LIVE, write).expect("written");
            let write = frame(&test.pool, &set, &[(0, &[10]), (1, &[2]), (2, &[30])]);
            let backwards = order::Error::Backwards {
                path: Path::Live,
                before: Stamp::from_nanos(20),
                stamp: Stamp::from_nanos(10),
            };
            assert_eq!(
                shard.write(a, LIVE, write),
                Ok(&[refused(0, Refusal::Order(backwards)), applied(2, 1, 1)][..])
            );
        });
    }

    #[test]
    fn refuses_a_backwards_stamp_in_a_later_vector_of_its_index() {
        run(46, |test| async move {
            let set = two_indexes();
            let mut shard = test.shard(AREA).await;
            let a = shard.open_writer(writer("a", 1, &set)).expect("synced");
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
                shard.write(a, LIVE, write),
                Ok(&[refused(0, Refusal::Order(backwards)), applied(2, 0, 1)][..])
            );
        });
    }

    #[test]
    fn refuses_an_encoded_index_whose_later_vector_is_not_valid() {
        run(47, |test| async move {
            let set = two_indexes();
            let mut shard = test.shard(AREA).await;
            let a = shard.open_writer(writer("a", 1, &set)).expect("synced");
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
            let refusal = Refusal::Codec {
                channel: key(Slot::new(0)),
                error: codec::Error::Tag { vector: 1, tag: 9 },
            };
            assert_eq!(
                shard.write(a, LIVE, write),
                Ok(&[refused(0, refusal), applied(2, 0, 1)][..])
            );
            let write = frame(&test.pool, &set, &[(0, &[11])]);
            assert_eq!(shard.write(a, LIVE, write), Ok(&[applied(0, 0, 1)][..]));
        });
    }

    #[test]
    fn refuses_an_encoded_index_that_is_not_valid_before_its_order() {
        run(48, |test| async move {
            let set = two_indexes();
            let mut shard = test.shard(AREA).await;
            let a = shard.open_writer(writer("a", 1, &set)).expect("synced");
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
            let refusal = Refusal::Codec {
                channel: key(Slot::new(0)),
                error: codec::Error::Tag { vector: 1, tag: 9 },
            };
            assert_eq!(
                shard.write(a, LIVE, write),
                Ok(&[refused(0, refusal), applied(2, 0, 1)][..])
            );
        });
    }

    #[test]
    fn refuses_an_encoded_index_cut_short_after_a_stamp_that_goes_back() {
        run(70, |test| async move {
            let set = two_indexes();
            let mut shard = test.shard(AREA).await;
            let a = shard.open_writer(writer("a", 1, &set)).expect("synced");
            let mut stamps: Vec<i64> = (10..2510).collect();
            stamps[5] = 1;
            let mut index = encoded(&stamps);
            index.truncate(encoded(&stamps[..1024]).len() + 1);
            let write = encoded_frame(&test.pool, &set, &[(0, &index)], &[(0, 2500)]);
            let refusal = Refusal::Codec {
                channel: key(Slot::new(0)),
                error: codec::Error::Truncated {
                    vector: 1,
                    needed: 2,
                    available: 1,
                },
            };
            assert_eq!(shard.write(a, LIVE, write), Ok(&[refused(0, refusal)][..]));
        });
    }

    #[test]
    fn refuses_an_encoded_index_with_bytes_after_its_last_vector() {
        run(71, |test| async move {
            let set = two_indexes();
            let mut shard = test.shard(AREA).await;
            let a = shard.open_writer(writer("a", 1, &set)).expect("synced");
            let mut index = encoded(&[10, 20]);
            index.push(0);
            let write = encoded_frame(&test.pool, &set, &[(0, &index)], &[(0, 2)]);
            let refusal = Refusal::Codec {
                channel: key(Slot::new(0)),
                error: codec::Error::Trailing { extra: 1 },
            };
            assert_eq!(shard.write(a, LIVE, write), Ok(&[refused(0, refusal)][..]));
            let write = frame(&test.pool, &set, &[(0, &[10, 20])]);
            assert_eq!(shard.write(a, LIVE, write), Ok(&[applied(0, 0, 2)][..]));
        });
    }

    #[test]
    fn refuses_a_group_with_the_error_of_its_index_before_a_data_series_after_it() {
        run(72, |test| async move {
            let set = two_indexes();
            let mut shard = test.shard(AREA).await;
            let a = shard.open_writer(writer("a", 1, &set)).expect("synced");
            let index = untagged(encoded(&[10, 20]));
            let data = untagged(encoded(&[1, 2]));
            let series = [(0, &index[..]), (1, &data[..])];
            let write = encoded_frame(&test.pool, &set, &series, &[(0, 2)]);
            let refusal = Refusal::Codec {
                channel: key(Slot::new(0)),
                error: codec::Error::Tag { vector: 0, tag: 9 },
            };
            assert_eq!(shard.write(a, LIVE, write), Ok(&[refused(0, refusal)][..]));
        });
    }

    #[test]
    fn refuses_a_group_with_the_error_of_a_data_series_before_its_index() {
        run(73, |test| async move {
            let mut shard = test.open(AREA, 2).await;
            shard.carry(Slot::new(1));
            let set = interner().intern(&[Group {
                index: key(Slot::new(1)),
                data: &[(key(Slot::new(0)), Type::Scalar(Scalar::I64))],
            }]);
            let a = shard.open_writer(writer("a", 1, &set)).expect("synced");
            let data = untagged(encoded(&[1, 2]));
            let index = untagged(encoded(&[10, 20]));
            let series = [(0, &data[..]), (1, &index[..])];
            let write = encoded_frame(&test.pool, &set, &series, &[(0, 2)]);
            let refusal = Refusal::Codec {
                channel: key(Slot::new(0)),
                error: codec::Error::Tag { vector: 0, tag: 9 },
            };
            assert_eq!(shard.write(a, LIVE, write), Ok(&[refused(1, refusal)][..]));
        });
    }

    #[test]
    fn refuses_a_series_that_does_not_fit_its_count_and_names_its_channel() {
        run(5, |test| async move {
            let set = two_indexes();
            let mut shard = test.shard(AREA).await;
            let a = shard.open_writer(writer("a", 1, &set)).expect("synced");
            let short =
                frame(&test.pool, &set, &[(0, &[10, 20]), (1, &[1]), (2, &[10])]);
            let refusal = Refusal::Codec {
                channel: key(Slot::new(1)),
                error: codec::Error::Length {
                    expected: 16,
                    actual: 8,
                },
            };
            assert_eq!(
                refusal.to_string(),
                "channel 01000000-0000-0000-0000-000000000001: the values hold 8 bytes, \
                 but the samples take 16"
            );
            assert_eq!(
                shard.write(a, LIVE, short),
                Ok(&[refused(0, refusal), applied(2, 0, 1)][..])
            );
            let write = frame(&test.pool, &set, &[(0, &[10]), (1, &[1])]);
            assert_eq!(shard.write(a, LIVE, write), Ok(&[applied(0, 0, 1)][..]));
        });
    }

    #[test]
    fn loses_live_frames_and_refuses_a_backfill_frame_when_the_ring_is_full() {
        run(6, |test| async move {
            let set = two_indexes();
            let mut shard = test.shard(1 << 16).await;
            let a = shard.open_writer(writer("a", 1, &set)).expect("synced");
            let mut outcomes = Vec::new();
            for n in 0..24_i64 {
                let stamps: Vec<i64> = (0..400).map(|k| 10 + n * 400 + k).collect();
                let values = scattered(400);
                let write = frame(&test.pool, &set, &[(0, &stamps), (1, &values)]);
                let written = shard.write(a, LIVE, write).expect("written");
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
            assert_eq!(shard.write(a, BACKFILL, write), Err(Error::Full));
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
            let a = shard.open_writer(writer("a", 1, &set)).expect("synced");
            let live = frame(&test.pool, &set, &[(0, &[10, 20]), (1, &[1, 2])]);
            let backfill = frame(&test.pool, &set, &[(0, &[1, 2]), (1, &[1, 2])]);
            let blocks = test.fill();
            assert_eq!(shard.write(a, LIVE, live), Ok(&[lost(0, 0, 2)][..]));
            assert_eq!(shard.write(a, BACKFILL, backfill), Err(Error::Full));
            drop(blocks);
            let backfill = frame(&test.pool, &set, &[(0, &[1, 2]), (1, &[1, 2])]);
            assert_eq!(
                shard.write(a, BACKFILL, backfill),
                Ok(&[applied(0, 0, 2)][..])
            );
            let live = frame(&test.pool, &set, &[(0, &[30]), (1, &[3])]);
            assert_eq!(shard.write(a, LIVE, live), Ok(&[applied(0, 2, 1)][..]));
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
            let a = shard.open_writer(writer("a", 1, &set)).expect("synced");
            assert_eq!(shard.write(a, LIVE, live), Ok(&[lost(0, 0, 1)][..]));
            assert_eq!(shard.write(a, BACKFILL, backfill), Err(Error::Full));
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
            let a = shard.open_writer(writer("a", 1, &set)).expect("synced");
            let room = [80, 56].map(|len| test.pool.alloc(len).expect("a block"));
            let blocks = test.fill();
            drop(room);
            assert_eq!(
                shard.write(a, LIVE, live),
                Ok(&[lost(0, 0, 1), lost(2, 0, 100)][..])
            );
            drop((twin, blocks));
            shard.committed().await.expect("the commit ends");
            assert_eq!(stored(&shard, Slot::new(0), Path::Live), 0);
        });
    }

    #[test]
    fn refuses_a_frame_too_large_for_one_write_and_spends_nothing() {
        run(38, |test| async move {
            let set = two_indexes();
            let mut shard = test.shard(AREA).await;
            let a = shard.open_writer(writer("a", 1, &set)).expect("synced");
            let stamps: Vec<i64> = (10..610).collect();
            let values = scattered(600);
            for (label, stamp) in [(LIVE, 700), (BACKFILL, 5)] {
                let large = frame(&test.pool, &set, &[(0, &stamps), (1, &values)]);
                assert_eq!(shard.write(a, label, large), Err(Error::Large));
                let small = frame(&test.pool, &set, &[(0, &[stamp]), (1, &[1])]);
                assert_eq!(shard.write(a, label, small), Ok(&[applied(0, 0, 1)][..]));
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
            let a = shard.open_writer(writer("a", 1, &set)).expect("synced");
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
                assert_eq!(shard.write(a, label, large), Err(Error::Large));
                let small = frame(&test.pool, &set, &[(0, &[stamp]), (1, &[1])]);
                assert_eq!(shard.write(a, label, small), Ok(&[applied(0, 0, 1)][..]));
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
            let a = shard
                .open_writer(writer("subject-a", 1, &set))
                .expect("synced");
            drop(blocks);
            assert_eq!(shard.write(a, LIVE, large), Err(Error::Large));
            shard.committed().await.expect("the commit ends");
            let handoff = handoff_to("subject-a");
            assert_eq!(find(&test.ring().await, &handoff).len(), 1);
            let small = frame(&test.pool, &set, &[(0, &[700]), (1, &[1])]);
            assert_eq!(shard.write(a, LIVE, small), Ok(&[applied(0, 0, 1)][..]));
            shard.committed().await.expect("the commit ends");
            assert_eq!(find(&test.ring().await, &handoff).len(), 1);
        });
    }

    #[test]
    fn loses_a_large_live_frame_whose_handoff_finds_no_room() {
        run(40, |test| async move {
            let set = two_indexes();
            let mut shard = test.shard(1 << 16).await;
            let a = shard.open_writer(writer("a", 1, &set)).expect("synced");
            let mut stamp = 10;
            loop {
                let write = frame(&test.pool, &set, &[(2, &[stamp])]);
                let written = shard.write(a, LIVE, write).expect("written");
                stamp += 1;
                if matches!(written, [Outcome::Lost { .. }]) {
                    break;
                }
                shard.committed().await.expect("the commit ends");
            }
            let stamps: Vec<i64> = (10..610).collect();
            let large =
                || frame(&test.pool, &set, &[(0, &stamps), (1, &scattered(600))]);
            assert_eq!(shard.write(a, LIVE, large()), Err(Error::Large));
            let b = shard.open_writer(writer("b", 2, &set)).expect("synced");
            assert!(shard.indexes[0].handoff().is_some(), "no room at the open");
            // The handoff is appended before the bodies, so the size is never checked.
            assert_eq!(shard.write(b, LIVE, large()), Ok(&[lost(0, 0, 600)][..]));
            let len = 240_000;
            let stamps: Vec<i64> = (1000..).take(len).collect();
            let values = scattered(len);
            let series: [(usize, &[i64]); 3] =
                [(0, &stamps), (1, &values), (2, &[stamp])];
            let huge = frame(&pool(4 * POOL), &set, &series);
            assert_eq!(
                shard.write(b, LIVE, huge),
                Ok(&[lost(0, 600, 240_000), lost(2, 16, 1)][..])
            );
        });
    }

    #[test]
    fn records_a_handoff_at_open_and_close() {
        run(8, |test| async move {
            let set = two_indexes();
            let mut shard = test.shard(AREA).await;
            let a = shard
                .open_writer(writer("subject-a", 1, &set))
                .expect("synced");
            shard.committed().await.expect("the commit ends");
            let handoffs = find(&test.ring().await, &handoff_to("subject-a"));
            assert_eq!(handoffs.len(), 2, "a handoff on each index");
            let b = shard
                .open_writer(writer("subject-b", 1, &set))
                .expect("synced");
            shard.close_writer(a);
            shard.committed().await.expect("the commit ends");
            let handoffs = find(&test.ring().await, &handoff_to("subject-b"));
            assert_eq!(handoffs.len(), 2, "b takes each index at the close of a");
            shard.close_writer(b);
        });
    }

    #[test]
    fn records_each_handoff_at_open_when_no_record_holds_them_together() {
        run(17, |test| async move {
            let (mut shard, set) = test.wide(WIDE).await;
            let long = "b".repeat(Name::MAX_BYTES);
            shard.open_writer(writer(&long, 1, &set)).expect("synced");
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
            let a = shard
                .open_writer(writer("subject-a", 4, &set))
                .expect("synced");
            let long = "c".repeat(200);
            shard.open_writer(writer(&long, 2, &zero)).expect("synced");
            shard
                .open_writer(writer("subject-x", 1, &two))
                .expect("synced");
            // A block for the handoff to x, and none for the handoff to c.
            let to_x = handoff_to("subject-x");
            let room = test.pool.alloc(to_x.len()).expect("a block");
            let blocks = test.fill();
            drop(room);
            shard.close_writer(a);
            drop(blocks);
            // The next input on index 2: y outranks x and takes control.
            shard
                .open_writer(writer("subject-y", 3, &two))
                .expect("synced");
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
            let lease = Span::from_nanos(10);
            let a = Writer {
                lease: Some(lease),
                ..writer("subject-a", 2, &set)
            };
            let a = shard.open_writer(a).expect("synced");
            shard
                .open_writer(writer("subject-b", 1, &set))
                .expect("synced");
            test.clock.sleep(Span::from_nanos(20)).await;
            let write = frame(&test.pool, &set, &[(2, &[10])]);
            let expired = refused(2, Refusal::Expired);
            assert_eq!(shard.write(a, LIVE, write), Ok(&[expired][..]));
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
    fn does_not_renew_the_lease_of_an_index_that_a_write_does_not_hold() {
        run(90, |test| async move {
            let (mut shard, a, set) = test.leased().await;
            let first = frame(&test.pool, &set, &[(0, &[10]), (1, &[1])]);
            assert_eq!(shard.write(a, LIVE, first), Ok(&[applied(0, 0, 1)][..]));
            test.clock.sleep(WAIT).await;
            let second = frame(&test.pool, &set, &[(0, &[20]), (1, &[2]), (2, &[20])]);
            assert_eq!(
                shard.write(a, LIVE, second),
                Ok(&[applied(0, 1, 1), refused(2, Refusal::Expired)][..])
            );
        });
    }

    #[test]
    fn does_not_renew_the_lease_of_an_index_before_the_one_that_a_write_holds() {
        run(93, |test| async move {
            let (mut shard, a, set) = test.leased().await;
            let first = frame(&test.pool, &set, &[(2, &[10])]);
            assert_eq!(shard.write(a, LIVE, first), Ok(&[applied(2, 0, 1)][..]));
            test.clock.sleep(WAIT).await;
            let second = frame(&test.pool, &set, &[(0, &[20]), (1, &[2]), (2, &[20])]);
            assert_eq!(
                shard.write(a, LIVE, second),
                Ok(&[refused(0, Refusal::Expired), applied(2, 1, 1)][..])
            );
        });
    }

    #[test]
    fn renews_the_lease_for_an_applied_group_with_no_samples() {
        run(94, |test| async move {
            let (mut shard, a, set) = test.leased().await;
            let empty = frame(&test.pool, &set, &[(2, &[])]);
            assert_eq!(shard.write(a, LIVE, empty), Ok(&[applied(2, 0, 0)][..]));
            test.clock.sleep(WAIT).await;
            let next = frame(&test.pool, &set, &[(2, &[20])]);
            assert_eq!(shard.write(a, LIVE, next), Ok(&[applied(2, 0, 1)][..]));
        });
    }

    #[test]
    fn renews_the_lease_for_a_lost_group() {
        run(91, |test| async move {
            let (mut shard, a, set) = test.leased().await;
            let live = frame(&test.pool, &set, &[(2, &[10])]);
            let blocks = test.fill();
            assert_eq!(shard.write(a, LIVE, live), Ok(&[lost(2, 0, 1)][..]));
            drop(blocks);
            test.clock.sleep(WAIT).await;
            let next = frame(&test.pool, &set, &[(2, &[20])]);
            assert_eq!(shard.write(a, LIVE, next), Ok(&[applied(2, 1, 1)][..]));
        });
    }

    #[test]
    fn does_not_renew_the_lease_for_a_backfill_frame_that_finds_no_room() {
        run(92, |test| async move {
            let (mut shard, a, set) = test.leased().await;
            let first = frame(&test.pool, &set, &[(2, &[1])]);
            let blocks = test.fill();
            assert_eq!(shard.write(a, BACKFILL, first), Err(Error::Full));
            drop(blocks);
            test.clock.sleep(WAIT).await;
            let again = frame(&test.pool, &set, &[(2, &[1])]);
            let expired = refused(2, Refusal::Expired);
            assert_eq!(shard.write(a, BACKFILL, again), Ok(&[expired][..]));
        });
    }

    #[test]
    fn does_not_renew_the_lease_for_a_frame_too_large_for_one_write() {
        run(95, |test| async move {
            let (mut shard, a, set) = test.leased().await;
            let stamps: Vec<i64> = (10..610).collect();
            let large = frame(&test.pool, &set, &[(0, &stamps), (1, &scattered(600))]);
            assert_eq!(shard.write(a, LIVE, large), Err(Error::Large));
            test.clock.sleep(WAIT).await;
            let next = frame(&test.pool, &set, &[(0, &[700]), (1, &[1])]);
            let expired = refused(0, Refusal::Expired);
            assert_eq!(shard.write(a, LIVE, next), Ok(&[expired][..]));
        });
    }

    #[test]
    fn records_each_handoff_at_close_when_no_record_holds_them_together() {
        run(18, |test| async move {
            let (mut shard, set) = test.wide(WIDE).await;
            let a = shard.open_writer(writer("a", 2, &set)).expect("synced");
            let long = "b".repeat(Name::MAX_BYTES);
            shard.open_writer(writer(&long, 1, &set)).expect("synced");
            shard.close_writer(a);
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
            let a = shard.open_writer(writer(&long, 1, &set)).expect("synced");
            drop(blocks);
            let series: Vec<(usize, &[i64])> = set
                .groups()
                .iter()
                .map(|&entry| (entry, &[10][..]))
                .collect();
            let write = frame(&test.pool, &set, &series);
            let each: Vec<_> = (0..WIDE).map(|slot| applied(slot, 0, 1)).collect();
            assert_eq!(shard.write(a, LIVE, write), Ok(&each[..]));
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
            let a = shard.open_writer(writer("a", 1, &set)).expect("synced");
            let mut seq = 0;
            loop {
                let stamp = 10 + i64::try_from(seq).expect("a short test");
                let write = frame(&test.pool, &set, &[(2, &[stamp])]);
                let written = shard.write(a, LIVE, write).expect("written");
                seq += 1;
                if matches!(written, [Outcome::Lost { .. }]) {
                    break;
                }
                shard.committed().await.expect("the commit ends");
            }
            let subject = "b".repeat(64);
            let b = shard
                .open_writer(writer(&subject, 2, &set))
                .expect("synced");
            let waiting = |shard: &Shard| {
                shard
                    .indexes
                    .iter()
                    .all(|index| index.gate.handoff().is_some())
            };
            assert!(waiting(&shard), "no room at the open");
            let write = frame(&test.pool, &set, &[(2, &[1_000])]);
            assert_eq!(shard.write(b, LIVE, write), Ok(&[lost(2, seq, 1)][..]));
            assert!(waiting(&shard), "no room for the frame");
        });
    }

    #[test]
    fn stamps_each_entry_with_the_midpoint_of_mesh_time() {
        run(14, |test| async move {
            let set = two_indexes();
            let mut shard = test.shard(AREA).await;
            let edges = test.reader.now().mesh.expect("the node has mesh time");
            let mesh = test.now();
            let a = shard.open_writer(writer("a", 1, &set)).expect("synced");
            let write = frame(&test.pool, &set, &[(0, &[10]), (1, &[1]), (2, &[10])]);
            shard.write(a, LIVE, write).expect("written");
            shard.committed().await.expect("the commit ends");
            let ring = test.ring().await;
            let count = |stamp: Stamp| find(&ring, &stamp.nanos().to_le_bytes()).len();
            assert_eq!(count(mesh), 4, "two handoffs and two bodies");
            assert_eq!(count(edges.earliest), 0);
            assert_eq!(count(edges.latest), 0);
        });
    }

    #[test]
    fn loses_a_frame_whose_index_has_a_handoff_with_no_block() {
        run(20, |test| async move {
            let set = two_indexes();
            let mut shard = test.shard(AREA).await;
            // Writer a leaves an open record, so the frame needs a block only for its
            // body.
            let _a = shard.open_writer(writer("a", 1, &set)).expect("synced");
            let live = frame(&test.pool, &set, &[(0, &[10]), (1, &[1])]);
            let again = frame(&test.pool, &set, &[(0, &[20]), (1, &[2])]);
            let room = test.pool.alloc(80).expect("a block");
            let blocks = test.fill();
            let subject = "b".repeat(200);
            let b = shard
                .open_writer(writer(&subject, 2, &set))
                .expect("synced");
            drop(room);
            assert_eq!(shard.write(b, LIVE, live), Ok(&[lost(0, 0, 1)][..]));
            drop(blocks);
            assert_eq!(shard.write(b, LIVE, again), Ok(&[applied(0, 1, 1)][..]));
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
            let _a = shard.open_writer(writer("a", 1, &set)).expect("synced");
            let live = frame(&test.pool, &set, &[(0, &[10]), (1, &[1])]);
            // The handoff and the frame's body each need the one free block.
            let room = test.pool.alloc(80).expect("a block");
            let blocks = test.fill();
            let subject = "b".repeat(70);
            let b = shard
                .open_writer(writer(&subject, 2, &set))
                .expect("synced");
            drop(room);
            assert_eq!(shard.write(b, LIVE, live), Ok(&[lost(0, 0, 1)][..]));
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
            let a = shard
                .open_writer(writer("subject-a", 1, &set))
                .expect("synced");
            drop(blocks);
            assert_eq!(shard.write(a, LIVE, first), Ok(&[applied(0, 0, 1)][..]));
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
            assert_eq!(shard.write(a, LIVE, again), Ok(&[applied(0, 1, 1)][..]));
            let second = frame(&test.pool, &set, &[(2, &[10])]);
            assert_eq!(shard.write(a, LIVE, second), Ok(&[applied(2, 0, 1)][..]));
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
            let a = shard.open_writer(writer("a", 2, &set)).expect("synced");
            let b = shard.open_writer(writer("b", 1, &set)).expect("synced");
            test.node.fail_file(FilePath::new(RING), Operation::Sync);
            let write = frame(&test.pool, &set, &[(0, &[10]), (1, &[1])]);
            assert_eq!(shard.write(a, LIVE, write), Ok(&[applied(0, 0, 1)][..]));
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
            assert_eq!(shard.write(a, LIVE, write), Err(disk.clone()));
            let refused = frame(&test.pool, &set, &[(2, &[20])]);
            assert_eq!(shard.write(b, LIVE, refused), Err(disk.clone()));
            let live = frame(&test.pool, &set, &[(0, &[30]), (1, &[3])]);
            let backfill = frame(&test.pool, &set, &[(0, &[1]), (1, &[1])]);
            let blocks = test.fill();
            assert_eq!(shard.write(a, LIVE, live), Err(disk.clone()));
            assert_eq!(shard.write(a, BACKFILL, backfill), Err(disk));
            drop(blocks);
        });
    }

    #[test]
    fn opens_no_writer_before_the_node_has_mesh_time() {
        run(60, |test| async move {
            let mut shard = test.unsynced().await;
            shard.carry(Slot::new(0));
            shard.carry(Slot::new(2));
            let a = shard.open_writer(writer("a", 1, &two_indexes()));
            assert_eq!(a, Err(writer::Error::Unsynced));
        });
    }

    #[test]
    fn refuses_a_stamp_past_mesh_time_and_its_limit() {
        run(61, |test| async move {
            let set = two_indexes();
            let mut shard = test.shard(AREA).await;
            let a = shard.open_writer(writer("a", 1, &set)).expect("synced");
            let latest = test.now().nanos() + LIMITS.ahead.nanos();
            let edge = frame(&test.pool, &set, &[(2, &[latest])]);
            assert_eq!(shard.write(a, LIVE, edge), Ok(&[applied(2, 0, 1)][..]));
            let past = frame(&test.pool, &set, &[(2, &[latest + 1])]);
            let ahead = order::Error::Ahead {
                stamp: Stamp::from_nanos(latest + 1),
                latest: Stamp::from_nanos(latest),
            };
            assert_eq!(
                shard.write(a, LIVE, past),
                Ok(&[refused(2, Refusal::Order(ahead))][..])
            );
        });
    }

    #[test]
    fn continues_each_path_after_a_power_cut_after_a_commit() {
        let (mut sim, node) = one_node(62);
        sim.run_on(&node, |node, tasks| async move {
            let test = Test::new(node, tasks);
            let set = two_indexes();
            let mut shard = test.shard(AREA).await;
            let a = shard.open_writer(writer("a", 1, &set)).expect("synced");
            let write = frame(&test.pool, &set, &[(0, &[10, 20]), (1, &[1, 2])]);
            shard.write(a, LIVE, write).expect("written");
            let backfill = frame(&test.pool, &set, &[(0, &[1]), (1, &[1])]);
            shard.write(a, BACKFILL, backfill).expect("written");
            shard.committed().await.expect("the commit ends");
        })
        .expect("the first run ends");
        sim.crash(&node, sim::Crash::Power);
        sim.run_on(&node, |node, tasks| async move {
            let test = Test::new(node, tasks);
            let set = two_indexes();
            let mut shard = test.shard(AREA).await;
            let stored = [Path::Live, Path::Backfill]
                .map(|path| stored(&shard, Slot::new(0), path));
            assert_eq!(stored, [2, 1]);
            let a = shard.open_writer(writer("a", 1, &set)).expect("synced");
            let before = frame(&test.pool, &set, &[(0, &[15]), (1, &[3])]);
            let backwards = order::Error::Backwards {
                path: Path::Live,
                before: Stamp::from_nanos(20),
                stamp: Stamp::from_nanos(15),
            };
            assert_eq!(
                shard.write(a, LIVE, before),
                Ok(&[refused(0, Refusal::Order(backwards))][..])
            );
            let live = frame(&test.pool, &set, &[(0, &[30]), (1, &[3])]);
            assert_eq!(shard.write(a, LIVE, live), Ok(&[applied(0, 2, 1)][..]));
            let backfill = frame(&test.pool, &set, &[(0, &[2]), (1, &[2])]);
            assert_eq!(
                shard.write(a, BACKFILL, backfill),
                Ok(&[applied(0, 1, 1)][..])
            );
        })
        .expect("the run after the cut ends");
    }

    #[test]
    fn continues_from_the_stored_tail_after_a_power_cut_before_a_commit() {
        let (mut sim, node) = one_node(63);
        sim.run_on(&node, |node, tasks| async move {
            let test = Test::new(node, tasks);
            let set = two_indexes();
            let mut shard = test.shard(AREA).await;
            let a = shard.open_writer(writer("a", 1, &set)).expect("synced");
            let write = frame(&test.pool, &set, &[(0, &[10, 20]), (1, &[1, 2])]);
            shard.write(a, LIVE, write).expect("written");
            shard.committed().await.expect("the commit ends");
            let write = frame(&test.pool, &set, &[(0, &[30]), (1, &[3])]);
            assert_eq!(shard.write(a, LIVE, write), Ok(&[applied(0, 2, 1)][..]));
        })
        .expect("the first run ends");
        sim.crash(&node, sim::Crash::Power);
        sim.run_on(&node, |node, tasks| async move {
            let test = Test::new(node, tasks);
            let set = two_indexes();
            let mut shard = test.shard(AREA).await;
            assert_eq!(stored(&shard, Slot::new(0), Path::Live), 2);
            let a = shard.open_writer(writer("a", 1, &set)).expect("synced");
            let write = frame(&test.pool, &set, &[(0, &[25]), (1, &[3])]);
            assert_eq!(shard.write(a, LIVE, write), Ok(&[applied(0, 2, 1)][..]));
        })
        .expect("the run after the cut ends");
    }

    #[test]
    fn refuses_a_resend_frame() {
        run(11, |test| async move {
            let set = two_indexes();
            let mut shard = test.shard(AREA).await;
            let a = shard.open_writer(writer("a", 1, &set)).expect("synced");
            let write = frame(&test.pool, &set, &[(2, &[10])]);
            assert_eq!(shard.write(a, Label::Resend, write), Err(Error::Resend));
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
            let mut shard = test.over(buffer).await;
            shard.carry(Slot::new(0));
            let set = interner().intern(&[Group {
                index: key(Slot::new(0)),
                data: &[],
            }]);
            let long = "b".repeat(Name::MAX_BYTES);
            shard.open_writer(writer(&long, 1, &set)).expect("synced");
            shard.committed().await.expect("the commit ends");
            let waiting = shard.indexes[0].handoff().is_some();
            let handoffs = find(&test.ring().await, &handoff_to(&long));
            assert_eq!((waiting, handoffs.len()), (false, 1));
        });
    }

    #[test]
    fn panics_at_the_open_of_a_writer_of_an_index_it_does_not_carry() {
        check_not_carried(12, |shard| {
            let opened = shard.open_writer(writer("a", 1, &not_carried()));
            opened.expect("synced");
        });
    }

    #[test]
    fn gives_unsynced_before_it_checks_that_an_index_is_carried() {
        run(84, |test| async move {
            let mut shard = test.unsynced().await;
            let a = shard.open_writer(writer("a", 1, &not_carried()));
            assert_eq!(a, Err(writer::Error::Unsynced));
        });
    }

    #[test]
    fn refuses_a_lease_of_zero_before_it_checks_that_an_index_is_carried() {
        run(85, |test| async move {
            let mut shard = test.shard(AREA).await;
            let zero = Writer {
                lease: Some(Span::ZERO),
                ..writer("a", 1, &not_carried())
            };
            let refused = writer::Error::Lease { span: Span::ZERO };
            assert_eq!(shard.open_writer(zero), Err(refused));
        });
    }

    #[test]
    fn tags_each_handoff_and_each_body() {
        run(21, |test| async move {
            let set = two_indexes();
            let mut shard = test.shard(AREA).await;
            let marked = test.now();
            let a = shard.open_writer(writer("a", 1, &set)).expect("synced");
            let write = frame(&test.pool, &set, &[(0, &[10]), (1, &[1]), (2, &[10])]);
            shard.write(a, LIVE, write).expect("written");
            shard.committed().await.expect("the commit ends");
            let zero = key(Slot::new(0)).as_u128();
            let two = key(Slot::new(2)).as_u128();
            assert_eq!(
                headers(&test.ring().await, marked),
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
            let marked = test.now();
            let a = shard.open_writer(writer("a", 1, &set)).expect("synced");
            let write = frame(&test.pool, &set, &[(0, &[1, 2]), (1, &[10, 20])]);
            assert_eq!(shard.write(a, LIVE, write), Ok(&[applied(2, 0, 2)][..]));
            shard.committed().await.expect("the commit ends");
            let two = key(Slot::new(2)).as_u128();
            assert_eq!(
                headers(&test.ring().await, marked),
                [(two, 0, 0, 0, 1), (two, 0, 0, 2, 0)]
            );
        });
    }

    #[test]
    fn records_a_waiting_handoff_on_the_live_path_before_a_backfill_frame() {
        run(22, |test| async move {
            let set = two_indexes();
            let mut shard = test.shard(AREA).await;
            let a = shard.open_writer(writer("a", 1, &set)).expect("synced");
            let live = frame(&test.pool, &set, &[(0, &[10, 20]), (1, &[1, 2])]);
            shard.write(a, LIVE, live).expect("written");
            let backfill = frame(&test.pool, &set, &[(0, &[1]), (1, &[1])]);
            let blocks = test.fill();
            test.clock.sleep(Span::from_nanos(1)).await;
            let b = shard.open_writer(writer("b", 2, &set)).expect("synced");
            drop(blocks);
            let marked = test.now();
            assert_eq!(
                shard.write(b, BACKFILL, backfill),
                Ok(&[applied(0, 0, 1)][..])
            );
            shard.committed().await.expect("the commit ends");
            let zero = key(Slot::new(0)).as_u128();
            assert_eq!(
                headers(&test.ring().await, marked),
                [(zero, 0, 2, 0, 1), (zero, 1, 0, 1, 0)]
            );
        });
    }

    #[test]
    fn stores_the_series_of_each_frame() {
        run(23, |test| async move {
            let set = two_indexes();
            let mut shard = test.shard(AREA).await;
            let marked = test.now();
            let a = shard.open_writer(writer("a", 1, &set)).expect("synced");
            let values: [i64; 2] = [0x0123_4567_89AB_CDEF, -5];
            let write = frame(&test.pool, &set, &[(0, &[10, 20]), (1, &values)]);
            shard.write(a, LIVE, write).expect("written");
            shard.committed().await.expect("the commit ends");
            let ring = test.ring().await;
            let data: Vec<_> = bodies(&ring, marked)
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

        fn latest(shard: &mut Shard, slot: Slot) -> reader::Key {
            shard.open_latest(slot)
        }

        fn close(shard: &mut Shard, reader: reader::Key) {
            shard.close_reader(reader);
        }

        /// Writes a live frame of the index at slot 0 with `stamps`.
        fn write(test: &Test, shard: &mut Shard, a: writer::Key, stamps: &[i64]) {
            let set = two_indexes();
            let values = vec![1; stamps.len()];
            let write = frame(&test.pool, &set, &[(0, stamps), (1, &values)]);
            let written = shard.write(a, LIVE, write).expect("written");
            assert!(
                matches!(written, [Outcome::Applied { .. } | Outcome::Lost { .. }]),
                "{written:?}"
            );
        }

        #[test]
        fn opens_and_closes_a_reader_of_each_mode_with_no_mesh_time() {
            run(97, |test| async move {
                let mut shard = test.unsynced().await;
                shard.carry(Slot::new(0));
                let session = shard.open_complete(Slot::new(0), 1);
                let readers = [session.into(), shard.open_latest(Slot::new(0))];
                assert_ne!(readers[0], readers[1]);
                shard.grant(session, CREDIT);
                for reader in readers {
                    assert_eq!(taken(&mut shard, reader, 0), []);
                    close(&mut shard, reader);
                }
                assert_eq!(woken(&mut shard), []);
            });
        }

        #[test]
        fn gives_frames_to_a_reader_of_each_mode_opened_with_no_mesh_time() {
            run(98, |test| async move {
                let buffer = test.buffer(AREA, BODY_MAX, 4).await;
                let (clock, mesh) = clock::Clock::new(test.clock.clone());
                let mut shard = Test::with(0, buffer, mesh.clone());
                shard.carry(Slot::new(0));
                shard.carry(Slot::new(2));
                let complete = complete(&mut shard, Slot::new(0));
                let latest = latest(&mut shard, Slot::new(0));
                let set = two_indexes();
                let unsynced = shard.open_writer(writer("a", 1, &set));
                assert_eq!(unsynced, Err(writer::Error::Unsynced));
                let wall = test.node.wall();
                test.tasks.spawn(async move { clock.run(wall).await });
                while mesh.now().mesh.is_none() {
                    test.clock.sleep(Span::from_nanos(1)).await;
                }
                let a = shard.open_writer(writer("a", 1, &set)).expect("synced");
                write(&test, &mut shard, a, &[10]);
                assert_eq!(woken(&mut shard), [latest]);
                assert_eq!(taken(&mut shard, latest, 0), [seq(0, 1)]);
                shard.committed().await.expect("the commit ends");
                assert_eq!(woken(&mut shard), [complete]);
                assert_eq!(taken(&mut shard, complete, 0), [seq(0, 1)]);
            });
        }

        #[test]
        fn takes_nothing_from_a_closed_reader_and_closes_it_again() {
            run(96, |test| async move {
                let set = two_indexes();
                let mut shard = test.shard(AREA).await;
                let readers = [
                    complete(&mut shard, Slot::new(0)),
                    latest(&mut shard, Slot::new(0)),
                ];
                let a = shard.open_writer(writer("a", 1, &set)).expect("synced");
                write(&test, &mut shard, a, &[10]);
                shard.committed().await.expect("the commit ends");
                assert_eq!(woken(&mut shard), [readers[1], readers[0]]);
                assert_eq!(taken(&mut shard, readers[1], 0), [seq(0, 1)]);
                write(&test, &mut shard, a, &[20]);
                for reader in readers {
                    close(&mut shard, reader);
                    assert_eq!(taken(&mut shard, reader, 0), []);
                    close(&mut shard, reader);
                }
                assert_eq!(woken(&mut shard), []);
            });
        }

        #[test]
        fn gives_a_latest_reader_the_newest_frame_before_its_commit() {
            run(33, |test| async move {
                let set = two_indexes();
                let mut shard = test.shard(AREA).await;
                let reader = latest(&mut shard, Slot::new(0));
                assert_eq!(woken(&mut shard), []);
                let a = shard.open_writer(writer("a", 1, &set)).expect("synced");
                write(&test, &mut shard, a, &[10, 20]);
                write(&test, &mut shard, a, &[30]);
                assert_eq!(stored(&shard, Slot::new(0), Path::Live), 0);
                assert_eq!(woken(&mut shard), [reader]);
                assert_eq!(taken(&mut shard, reader, 0), [seq(2, 1)]);
                let late = latest(&mut shard, Slot::new(0));
                assert_eq!(woken(&mut shard), []);
                assert_eq!(taken(&mut shard, late, 0), [seq(2, 1)]);
            });
        }

        /// A reader replaced by one that takes the newest frame at open is named
        /// once, so the keys keep their capacity.
        #[test]
        fn names_a_reader_that_took_at_open_once() {
            run(35, |test| async move {
                let set = two_indexes();
                let mut shard = test.shard(AREA).await;
                let mut readers: Vec<_> =
                    (0..4).map(|_| latest(&mut shard, Slot::new(0))).collect();
                let a = shard.open_writer(writer("a", 1, &set)).expect("synced");
                write(&test, &mut shard, a, &[10]);
                let mut keys = Vec::new();
                shard.woken(&mut keys);
                assert_eq!(keys.len(), 4);
                let capacity = keys.capacity();
                for &reader in &readers {
                    assert_eq!(taken(&mut shard, reader, 0).len(), 1);
                }
                close(&mut shard, readers[3]);
                readers[3] = latest(&mut shard, Slot::new(0));
                assert_eq!(taken(&mut shard, readers[3], 0).len(), 1);
                write(&test, &mut shard, a, &[20]);
                shard.woken(&mut keys);
                readers.sort_unstable();
                assert_eq!(keys, readers);
                assert_eq!(keys.capacity(), capacity);
            });
        }

        #[test]
        fn replaces_the_keys_it_gave_in_the_last_call() {
            run(36, |test| async move {
                let set = two_indexes();
                let mut shard = test.shard(AREA).await;
                let reader = complete(&mut shard, Slot::new(0));
                let a = shard.open_writer(writer("a", 1, &set)).expect("synced");
                write(&test, &mut shard, a, &[10]);
                shard.committed().await.expect("the commit ends");
                let mut keys = Vec::new();
                shard.woken(&mut keys);
                assert_eq!(keys, [reader]);
                shard.woken(&mut keys);
                assert_eq!(keys, []);
            });
        }

        #[test]
        fn gives_a_complete_reader_each_frame_after_the_commit_that_holds_it() {
            run(34, |test| async move {
                let set = two_indexes();
                let mut shard = test.shard(AREA).await;
                let reader = complete(&mut shard, Slot::new(0));
                let a = shard.open_writer(writer("a", 1, &set)).expect("synced");
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
                let latest = latest(&mut shard, Slot::new(0));
                let complete = complete(&mut shard, Slot::new(0));
                let a = shard.open_writer(writer("a", 1, &set)).expect("synced");
                write(&test, &mut shard, a, &[10]);
                test.clock.sleep(SYNC).await;
                assert_eq!(stored(&shard, Slot::new(0), Path::Live), 0, "a sync runs");
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
                let reader = complete(&mut shard, Slot::new(0));
                let a = shard.open_writer(writer("a", 1, &set)).expect("synced");
                write(&test, &mut shard, a, &[10]);
                let lost = frame(&test.pool, &set, &[(0, &[20, 30]), (1, &[2, 3])]);
                let blocks = test.fill();
                assert_eq!(shard.write(a, LIVE, lost), Ok(&[super::lost(0, 1, 2)][..]));
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
                let reader = latest(&mut shard, Slot::new(2));
                let a = shard.open_writer(writer("a", 1, &set)).expect("synced");
                let mut next = 0;
                loop {
                    let stamp = 10 + i64::try_from(next).expect("a short test");
                    let write = frame(&test.pool, &set, &[(2, &[stamp])]);
                    let written = shard.write(a, LIVE, write).expect("written");
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
                let session = shard.open_complete(Slot::new(0), 1);
                let reader = reader::Key::from(session);
                let a = shard.open_writer(writer("a", 1, &set)).expect("synced");
                write(&test, &mut shard, a, &[10]);
                write(&test, &mut shard, a, &[20]);
                assert!(!shard.behind(session));
                shard.committed().await.expect("the commit ends");
                assert_eq!(woken(&mut shard), [reader]);
                assert!(shard.behind(session));
                assert_eq!(taken(&mut shard, reader, 0), [seq(0, 1)]);
                shard.grant(session, CREDIT);
                write(&test, &mut shard, a, &[30]);
                shard.committed().await.expect("the commit ends");
                assert_eq!(woken(&mut shard), []);
                assert_eq!(taken(&mut shard, reader, 0), []);
            });
        }

        #[test]
        fn does_not_count_a_frame_of_no_samples_as_a_miss_of_a_complete_reader() {
            run(104, |test| async move {
                let set = two_indexes();
                let mut shard = test.shard(AREA).await;
                let session = shard.open_complete(Slot::new(2), 1);
                let reader = reader::Key::from(session);
                let a = shard.open_writer(writer("a", 1, &set)).expect("synced");
                let first = frame(&test.pool, &set, &[(2, &[10])]);
                assert_eq!(shard.write(a, LIVE, first), Ok(&[applied(2, 0, 1)][..]));
                shard.committed().await.expect("the commit ends");
                assert_eq!(woken(&mut shard), [reader]);
                assert_eq!(taken(&mut shard, reader, 1), [seq(0, 1)]);
                let empty = frame(&test.pool, &set, &[(2, &[])]);
                assert_eq!(shard.write(a, LIVE, empty), Ok(&[applied(2, 1, 0)][..]));
                shard.committed().await.expect("the commit ends");
                assert_eq!(woken(&mut shard), []);
                shard.grant(session, CREDIT);
                let later = frame(&test.pool, &set, &[(2, &[20])]);
                assert_eq!(shard.write(a, LIVE, later), Ok(&[applied(2, 1, 1)][..]));
                shard.committed().await.expect("the commit ends");
                assert_eq!(woken(&mut shard), [reader]);
                assert_eq!(taken(&mut shard, reader, 1), [seq(1, 1)]);
            });
        }

        #[test]
        fn raises_the_credit_of_a_complete_reader_with_a_grant() {
            run(25, |test| async move {
                let set = two_indexes();
                let mut shard = test.shard(AREA).await;
                let session = shard.open_complete(Slot::new(0), 1);
                let reader = reader::Key::from(session);
                let a = shard.open_writer(writer("a", 1, &set)).expect("synced");
                write(&test, &mut shard, a, &[10]);
                shard.committed().await.expect("the commit ends");
                assert_eq!(woken(&mut shard), [reader]);
                assert_eq!(taken(&mut shard, reader, 0), [seq(0, 1)]);
                shard.grant(session, CREDIT);
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
                let session = shard.open_complete(Slot::new(0), 1);
                close(&mut shard, session.into());
                shard.grant(session, CREDIT);
                let after = shard.open_complete(Slot::new(0), 1);
                assert_ne!(after, session);
            });
        }

        #[test]
        fn gives_a_complete_reader_that_opens_after_writes_only_later_frames() {
            run(26, |test| async move {
                let set = two_indexes();
                let mut shard = test.shard(AREA).await;
                let a = shard.open_writer(writer("a", 1, &set)).expect("synced");
                write(&test, &mut shard, a, &[10]);
                shard.committed().await.expect("the commit ends");
                write(&test, &mut shard, a, &[20, 30]);
                let reader = complete(&mut shard, Slot::new(0));
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
                let first = complete(&mut shard, Slot::new(0));
                let second = complete(&mut shard, Slot::new(0));
                let other = complete(&mut shard, Slot::new(2));
                let a = shard.open_writer(writer("a", 1, &set)).expect("synced");
                write(&test, &mut shard, a, &[10, 20]);
                let both = frame(&test.pool, &set, &[(0, &[30]), (1, &[3]), (2, &[5])]);
                shard.write(a, LIVE, both).expect("written");
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
                let latest = latest(&mut shard, Slot::new(0));
                let complete = complete(&mut shard, Slot::new(0));
                let a = shard.open_writer(writer("a", 1, &set)).expect("synced");
                write(&test, &mut shard, a, &[10]);
                close(&mut shard, latest);
                assert_eq!(woken(&mut shard), []);
                write(&test, &mut shard, a, &[20]);
                shard.committed().await.expect("the commit ends");
                close(&mut shard, complete);
                assert_eq!(woken(&mut shard), []);
            });
        }

        #[test]
        fn gives_no_reader_a_backfill_frame() {
            run(30, |test| async move {
                let set = two_indexes();
                let mut shard = test.shard(AREA).await;
                let latest = latest(&mut shard, Slot::new(0));
                let complete = complete(&mut shard, Slot::new(0));
                let a = shard.open_writer(writer("a", 1, &set)).expect("synced");
                let backfill = frame(&test.pool, &set, &[(0, &[1, 2]), (1, &[1, 2])]);
                assert_eq!(
                    shard.write(a, BACKFILL, backfill),
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
                let reader = complete(&mut shard, Slot::new(0));
                let a = shard.open_writer(writer("a", 1, &set)).expect("synced");
                write(&test, &mut shard, a, &[10]);
                let first = shard.committed();
                test.clock.sleep(SYNC).await;
                assert_eq!(stored(&shard, Slot::new(0), Path::Live), 0, "a sync runs");
                write(&test, &mut shard, a, &[20]);
                first.await.expect("the first commit ends");
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
                let reader = complete(&mut shard, Slot::new(0));
                let a = shard.open_writer(writer("a", 1, &set)).expect("synced");
                write(&test, &mut shard, a, &[10]);
                let first = shard.committed();
                test.clock.sleep(SYNC).await;
                write(&test, &mut shard, a, &[20]);
                first.await.expect("the first commit ends");
                let place = shard.place(Slot::new(0));
                assert_eq!(woken(&mut shard), [reader]);
                assert_eq!(taken(&mut shard, reader, 0), [seq(0, 1)]);
                assert_eq!(shard.readers.listed(), [place]);
                shard.committed().await.expect("the commit ends");
                assert_eq!(woken(&mut shard), [reader]);
                assert_eq!(shard.readers.listed(), []);
            });
        }

        #[test]
        fn releases_a_frame_at_the_first_woken_after_its_commit() {
            run(50, |test| async move {
                let set = two_indexes();
                let mut shard = test.shard(AREA).await;
                let reader = complete(&mut shard, Slot::new(0));
                let a = shard.open_writer(writer("a", 1, &set)).expect("synced");
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
                let reader = complete(&mut shard, Slot::new(0));
                let a = shard.open_writer(writer("a", 1, &set)).expect("synced");
                write(&test, &mut shard, a, &[10]);
                shard.committed().await.expect("the commit ends");
                assert_eq!(woken(&mut shard), [reader]);
                assert_eq!(taken(&mut shard, reader, 0), [seq(0, 1)]);
                assert_eq!(shard.readers.listed(), []);
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
                let a = shard.open_writer(writer("a", 1, &set)).expect("synced");
                write(&test, &mut shard, a, &[10]);
                assert_eq!(shard.readers.listed(), []);
                let reader = complete(&mut shard, Slot::new(0));
                write(&test, &mut shard, a, &[20]);
                let place = shard.place(Slot::new(0));
                assert_eq!(shard.readers.listed(), [place]);
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
                let a = shard.open_writer(writer("a", 1, &set)).expect("synced");
                let reader = complete(&mut shard, Slot::new(0));
                write(&test, &mut shard, a, &[10]);
                shard.committed().await.expect("the commit ends");
                assert_eq!(woken(&mut shard), [reader]);
                write(&test, &mut shard, a, &[20]);
                close(&mut shard, reader);
                write(&test, &mut shard, a, &[30]);
                let place = shard.place(Slot::new(0));
                assert_eq!(woken(&mut shard), []);
                assert_eq!(shard.readers.listed(), [place], "no commit ended");
                shard.committed().await.expect("the commit ends");
                assert_eq!(woken(&mut shard), []);
                assert_eq!(shard.readers.listed(), []);
            });
        }

        #[test]
        fn gives_a_complete_reader_a_frame_on_disk_with_no_commit_future() {
            run(39, |test| async move {
                let set = two_indexes();
                let mut shard = test.shard(AREA).await;
                let reader = complete(&mut shard, Slot::new(0));
                let a = shard.open_writer(writer("a", 1, &set)).expect("synced");
                write(&test, &mut shard, a, &[10]);
                test.clock.sleep(Span::from_nanos(100_000_000)).await;
                assert_eq!(woken(&mut shard), [reader]);
                assert_eq!(taken(&mut shard, reader, 0), [seq(0, 1)]);
            });
        }

        #[test]
        fn names_no_complete_reader_that_took_its_frames() {
            run(40, |test| async move {
                let set = two_indexes();
                let mut shard = test.shard(AREA).await;
                let reader = complete(&mut shard, Slot::new(0));
                let a = shard.open_writer(writer("a", 1, &set)).expect("synced");
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
                let first = latest(&mut shard, Slot::new(0));
                let second = latest(&mut shard, Slot::new(2));
                let a = shard.open_writer(writer("a", 1, &set)).expect("synced");
                let later = frame(&test.pool, &set, &[(2, &[10])]);
                shard.write(a, LIVE, later).expect("written");
                write(&test, &mut shard, a, &[20]);
                assert_eq!(taken(&mut shard, first, 0), [seq(0, 1)]);
                write(&test, &mut shard, a, &[30]);
                assert_eq!(woken(&mut shard), [first, second]);
            });
        }

        #[test]
        fn names_the_latest_readers_of_an_index_before_its_complete_readers() {
            run(53, |test| async move {
                let set = two_indexes();
                let mut shard = test.shard(AREA).await;
                let complete = complete(&mut shard, Slot::new(0));
                let latest = latest(&mut shard, Slot::new(0));
                let a = shard.open_writer(writer("a", 1, &set)).expect("synced");
                write(&test, &mut shard, a, &[10]);
                shard.committed().await.expect("the commit ends");
                assert_eq!(woken(&mut shard), [latest, complete]);
            });
        }

        #[test]
        fn panics_at_the_open_of_a_latest_reader_of_an_index_it_does_not_carry() {
            check_not_carried(29, |shard| {
                let _key = shard.open_latest(Slot::new(3));
            });
        }

        #[test]
        fn panics_at_the_open_of_a_complete_reader_of_an_index_it_does_not_carry() {
            check_not_carried(99, |shard| {
                let _key = shard.open_complete(Slot::new(3), CREDIT);
            });
        }

        #[test]
        fn panics_at_the_take_of_a_reader_of_an_index_it_does_not_carry() {
            check_not_carried(79, |shard| {
                let reader = latest(shard, Slot::new(2));
                let other = reader::Key {
                    slot: Slot::new(3),
                    ..reader
                };
                drop(shard.take(other));
            });
        }

        #[test]
        fn panics_at_the_close_of_a_reader_of_an_index_it_does_not_carry() {
            check_not_carried(80, |shard| {
                let reader = latest(shard, Slot::new(2));
                let other = reader::Key {
                    slot: Slot::new(3),
                    ..reader
                };
                shard.close_reader(other);
            });
        }

        #[test]
        fn panics_at_the_grant_to_a_reader_of_an_index_it_does_not_carry() {
            check_not_carried(86, |shard| {
                let reader = shard.open_complete(Slot::new(2), CREDIT);
                let other = reader::complete::Key {
                    slot: Slot::new(3),
                    ..reader
                };
                shard.grant(other, CREDIT + 1);
            });
        }
    }

    #[test]
    fn starts_a_lease_at_the_open_of_its_writer() {
        run(64, |test| async move {
            let set = two_indexes();
            let mut shard = test.shard(AREA).await;
            let lease = Span::from_nanos(10);
            let a = Writer {
                lease: Some(lease),
                ..writer("a", 1, &set)
            };
            let a = shard.open_writer(a).expect("synced");
            let write = frame(&test.pool, &set, &[(2, &[10])]);
            assert_eq!(shard.write(a, LIVE, write), Ok(&[applied(2, 0, 1)][..]));
        });
    }

    #[test]
    fn starts_the_lease_of_a_waiter_when_it_takes_control_at_a_close() {
        run(65, |test| async move {
            let set = two_indexes();
            let mut shard = test.shard(AREA).await;
            let a = shard.open_writer(writer("a", 2, &set)).expect("synced");
            let lease = Span::from_nanos(10);
            let b = Writer {
                lease: Some(lease),
                ..writer("b", 1, &set)
            };
            let b = shard.open_writer(b).expect("synced");
            shard.close_writer(a);
            let write = frame(&test.pool, &set, &[(2, &[10])]);
            assert_eq!(shard.write(b, LIVE, write), Ok(&[applied(2, 0, 1)][..]));
        });
    }

    #[test]
    fn stamps_a_handoff_at_close_with_mesh_time_at_the_close() {
        run(66, |test| async move {
            let set = two_indexes();
            let mut shard = test.shard(AREA).await;
            let a = shard.open_writer(writer("a", 2, &set)).expect("synced");
            shard.open_writer(writer("b", 1, &set)).expect("synced");
            test.clock.sleep(Span::from_nanos(1)).await;
            let marked = test.now();
            shard.close_writer(a);
            shard.committed().await.expect("the commit ends");
            let zero = key(Slot::new(0)).as_u128();
            let two = key(Slot::new(2)).as_u128();
            assert_eq!(
                headers(&test.ring().await, marked),
                [(zero, 0, 0, 0, 1), (two, 0, 0, 0, 1)]
            );
        });
    }

    #[test]
    fn takes_a_write_while_a_commit_future_waits() {
        run(67, |test| async move {
            let set = two_indexes();
            let mut shard = test.shard(AREA).await;
            let reader = complete(&mut shard, Slot::new(0));
            let a = shard.open_writer(writer("a", 1, &set)).expect("synced");
            let first = frame(&test.pool, &set, &[(0, &[10]), (1, &[1])]);
            shard.write(a, LIVE, first).expect("written");
            let commit = shard.committed();
            let second = frame(&test.pool, &set, &[(0, &[20]), (1, &[2])]);
            assert_eq!(shard.write(a, LIVE, second), Ok(&[applied(0, 1, 1)][..]));
            commit.await.expect("the commit ends");
            assert_eq!(woken(&mut shard), [reader]);
            assert_eq!(taken(&mut shard, reader, 0), [seq(0, 1), seq(1, 1)]);
        });
    }

    #[test]
    fn holds_a_commit_future_until_a_handoff_with_no_frame_is_on_disk() {
        run(105, |test| async move {
            let set = two_indexes();
            let mut shard = test.shard(AREA).await;
            let mut context = Context::from_waker(Waker::noop());
            let mut none = shard.committed();
            assert_eq!(Pin::new(&mut none).poll(&mut context), Poll::Ready(Ok(())));
            shard
                .open_writer(writer("subject-a", 1, &set))
                .expect("synced");
            let handoff = handoff_to("subject-a");
            let mut commit = shard.committed();
            assert_eq!(Pin::new(&mut commit).poll(&mut context), Poll::Pending);
            assert_eq!(find(&test.ring().await, &handoff).len(), 0);
            commit.await.expect("the commit ends");
            assert_eq!(find(&test.ring().await, &handoff).len(), 2);
        });
    }

    #[test]
    fn refuses_a_lease_of_zero_before_any_gate_changes() {
        run(71, |test| async move {
            let set = two_indexes();
            let mut shard = test.shard(AREA).await;
            let zero = Writer {
                lease: Some(Span::ZERO),
                ..writer("a", 2, &set)
            };
            let refused = shard.open_writer(zero).expect_err("a lease of zero");
            assert_eq!(refused, writer::Error::Lease { span: Span::ZERO });
            assert_eq!(
                refused.to_string(),
                "control lease must be longer than zero, got 0s"
            );
            let b = shard.open_writer(writer("b", 1, &set)).expect("synced");
            let write = frame(&test.pool, &set, &[(2, &[10])]);
            assert_eq!(shard.write(b, LIVE, write), Ok(&[applied(2, 0, 1)][..]));
        });
    }

    #[test]
    fn refuses_a_lease_of_zero_as_unsynced_before_the_node_has_mesh_time() {
        run(72, |test| async move {
            let mut shard = test.unsynced().await;
            shard.carry(Slot::new(0));
            shard.carry(Slot::new(2));
            let zero = Writer {
                lease: Some(Span::ZERO),
                ..writer("a", 1, &two_indexes())
            };
            assert_eq!(shard.open_writer(zero), Err(writer::Error::Unsynced));
        });
    }

    /// Runs `call` with shard 3 and a key that shard 2 gave for the number of its
    /// second open writer, and gives the panic of the run.
    fn with_a_key_of_another_shard(
        seed: u64,
        call: impl FnOnce(&Test, &mut Shard, writer::Key) + Send + 'static,
    ) -> Result<(), sim::Error> {
        let (mut sim, _handle) = start(seed, move |test| async move {
            let set = two_indexes();
            let buffer = test.buffer(AREA, BODY_MAX, 4).await;
            let mut shard = test.numbered(3, buffer).await;
            shard.carry(Slot::new(0));
            shard.carry(Slot::new(2));
            shard.open_writer(writer("a", 1, &set)).expect("synced");
            let b = shard.open_writer(writer("b", 1, &set)).expect("synced");
            let other = writer::Key { shard: 2, ..b };
            call(&test, &mut shard, other);
        });
        sim.run()
    }

    #[test]
    fn panics_on_a_write_with_a_writer_key_of_another_shard() {
        let ran = with_a_key_of_another_shard(73, |test, shard, other| {
            let set = two_indexes();
            let write = frame(&test.pool, &set, &[(2, &[10])]);
            drop(shard.write(other, LIVE, write));
        });
        assert_eq!(
            ran,
            Err(sim::Error::Panicked {
                thread: DIR.into(),
                message: "writer 1 is of shard 2, not shard 3".into(),
                seed: 73,
            })
        );
    }

    #[test]
    fn panics_on_a_close_with_a_writer_key_of_another_shard() {
        let ran = with_a_key_of_another_shard(74, |_, shard, other| {
            shard.close_writer(other);
        });
        assert_eq!(
            ran,
            Err(sim::Error::Panicked {
                thread: DIR.into(),
                message: "writer 1 is of shard 2, not shard 3".into(),
                seed: 74,
            })
        );
    }

    #[test]
    fn panics_on_a_resend_write_with_a_writer_key_of_another_shard() {
        let ran = with_a_key_of_another_shard(75, |test, shard, other| {
            let set = two_indexes();
            let write = frame(&test.pool, &set, &[(2, &[10])]);
            drop(shard.write(other, Label::Resend, write));
        });
        assert_eq!(
            ran,
            Err(sim::Error::Panicked {
                thread: DIR.into(),
                message: "writer 1 is of shard 2, not shard 3".into(),
                seed: 75,
            })
        );
    }

    #[test]
    fn gives_and_takes_keys_of_its_own_number() {
        run(76, |test| async move {
            let buffer = test.buffer(AREA, BODY_MAX, 4).await;
            let mut shard = test.numbered(1, buffer).await;
            shard.carry(Slot::new(0));
            shard.carry(Slot::new(2));
            let set = two_indexes();
            let a = shard.open_writer(writer("a", 1, &set)).expect("synced");
            assert_eq!(
                a,
                writer::Key {
                    shard: 1,
                    number: 0
                }
            );
            let write = frame(&test.pool, &set, &[(2, &[10])]);
            assert_eq!(shard.write(a, LIVE, write), Ok(&[applied(2, 0, 1)][..]));
            shard.close_writer(a);
        });
    }

    /// Runs `call` with a shard and the key of the second writer it opened, now
    /// closed, and gives the panic of the run.
    fn with_a_closed_writer(
        seed: u64,
        call: impl FnOnce(&Test, &mut Shard, writer::Key) + Send + 'static,
    ) -> Result<(), sim::Error> {
        let (mut sim, _handle) = start(seed, move |test| async move {
            let set = two_indexes();
            let mut shard = test.shard(AREA).await;
            shard.open_writer(writer("a", 1, &set)).expect("synced");
            let b = shard.open_writer(writer("b", 1, &set)).expect("synced");
            shard.close_writer(b);
            call(&test, &mut shard, b);
        });
        sim.run()
    }

    #[test]
    fn panics_on_a_write_with_a_closed_writer() {
        let ran = with_a_closed_writer(77, |test, shard, closed| {
            let set = two_indexes();
            let write = frame(&test.pool, &set, &[(2, &[10])]);
            drop(shard.write(closed, LIVE, write));
        });
        assert_eq!(
            ran,
            Err(sim::Error::Panicked {
                thread: DIR.into(),
                message: "writer 1 is not open".into(),
                seed: 77,
            })
        );
    }

    #[test]
    fn panics_on_a_resend_write_with_a_closed_writer() {
        let ran = with_a_closed_writer(81, |test, shard, closed| {
            let set = two_indexes();
            let write = frame(&test.pool, &set, &[(2, &[10])]);
            drop(shard.write(closed, Label::Resend, write));
        });
        assert_eq!(
            ran,
            Err(sim::Error::Panicked {
                thread: DIR.into(),
                message: "writer 1 is not open".into(),
                seed: 81,
            })
        );
    }

    /// Writes a frame of a second key set with `label`, expects [`Error::Resend`],
    /// and gives the run.
    fn write_of_another_key_set(seed: u64, label: Label) -> Result<(), sim::Error> {
        let (mut sim, _handle) = start(seed, move |test| async move {
            let mut interner = interner();
            let group = Group {
                index: key(Slot::new(2)),
                data: &[],
            };
            let set = interner.intern(&[group]);
            let data = [(key(Slot::new(1)), Type::Scalar(Scalar::I64))];
            let other = interner.intern(&[
                Group {
                    index: key(Slot::new(0)),
                    data: &data,
                },
                group,
            ]);
            let mut shard = test.shard(AREA).await;
            let a = shard.open_writer(writer("a", 1, &set)).expect("synced");
            let write = frame(&test.pool, &other, &[(2, &[10])]);
            assert_eq!(shard.write(a, label, write), Err(Error::Resend));
        });
        sim.run()
    }

    #[test]
    fn panics_on_a_write_of_a_frame_of_another_key_set() {
        assert_eq!(
            write_of_another_key_set(82, LIVE),
            Err(sim::Error::Panicked {
                thread: DIR.into(),
                message: "the frame is of key set 1, not of key set 0".into(),
                seed: 82,
            })
        );
    }

    /// A key set of the index at slot 2 with a `String` series.
    fn string_series() -> Arc<KeySet> {
        interner().intern(&[Group {
            index: key(Slot::new(2)),
            data: &[(key(Slot::new(3)), Type::String)],
        }])
    }

    #[test]
    fn refuses_a_key_set_with_a_series_of_a_type_the_home_does_not_write() {
        run(87, |test| async move {
            let mut shard = test.shard(AREA).await;
            let refused = shard.open_writer(writer("a", 2, &string_series()));
            assert_eq!(
                refused,
                Err(writer::Error::Type {
                    slot: Slot::new(3),
                    data_type: Type::String,
                })
            );
            let set = two_indexes();
            let b = shard.open_writer(writer("b", 1, &set)).expect("synced");
            let write = frame(&test.pool, &set, &[(2, &[10])]);
            assert_eq!(shard.write(b, LIVE, write), Ok(&[applied(2, 0, 1)][..]));
        });
    }

    #[test]
    fn refuses_the_type_of_a_series_before_the_panic_of_an_index_not_carried() {
        run(102, |test| async move {
            let mut shard = test.shard(AREA).await;
            let set = interner().intern(&[Group {
                index: key(Slot::new(3)),
                data: &[(key(Slot::new(1)), Type::Bytes)],
            }]);
            assert_eq!(
                shard.open_writer(writer("a", 1, &set)),
                Err(writer::Error::Type {
                    slot: Slot::new(1),
                    data_type: Type::Bytes,
                })
            );
        });
    }

    #[test]
    fn gives_the_pool_of_its_buffer() {
        run(103, |test| async move {
            let shard = test.shard(AREA).await;
            assert!(std::ptr::eq(shard.pool(), &raw const *test.pool));
        });
    }

    #[test]
    fn refuses_a_lease_of_zero_before_the_type_of_a_series() {
        run(88, |test| async move {
            let mut shard = test.shard(AREA).await;
            let zero = Writer {
                lease: Some(Span::ZERO),
                ..writer("a", 1, &string_series())
            };
            assert_eq!(
                shard.open_writer(zero),
                Err(writer::Error::Lease { span: Span::ZERO })
            );
        });
    }

    #[test]
    fn refuses_the_type_of_a_series_as_unsynced_before_the_node_has_mesh_time() {
        run(101, |test| async move {
            let mut shard = test.unsynced().await;
            shard.carry(Slot::new(2));
            let a = shard.open_writer(writer("a", 1, &string_series()));
            assert_eq!(a, Err(writer::Error::Unsynced));
        });
    }

    #[test]
    fn refuses_a_resend_frame_before_it_reads_the_frame() {
        assert_eq!(write_of_another_key_set(83, Label::Resend), Ok(()));
    }

    #[test]
    fn panics_on_a_second_close_of_a_writer() {
        let ran = with_a_closed_writer(78, |_, shard, closed| {
            shard.close_writer(closed);
        });
        assert_eq!(
            ran,
            Err(sim::Error::Panicked {
                thread: DIR.into(),
                message: "writer 1 is not open".into(),
                seed: 78,
            })
        );
    }

    #[test]
    fn leaves_each_gate_as_it_was_after_an_unsynced_open() {
        run(68, |test| async move {
            let buffer = test.buffer(AREA, BODY_MAX, 4).await;
            let (clock, mesh) = clock::Clock::new(test.clock.clone());
            let mut shard = Shard::new(Config {
                shard: 0,
                buffer,
                clock: mesh.clone(),
                limits: LIMITS,
            });
            shard.carry(Slot::new(0));
            shard.carry(Slot::new(2));
            let set = two_indexes();
            assert_eq!(
                shard.open_writer(writer("a", 1, &set)),
                Err(writer::Error::Unsynced)
            );
            let wall = test.node.wall();
            test.tasks.spawn(async move { clock.run(wall).await });
            while mesh.now().mesh.is_none() {
                test.clock.sleep(Span::from_nanos(1)).await;
            }
            let b = shard.open_writer(writer("b", 1, &set)).expect("synced");
            let write = frame(&test.pool, &set, &[(2, &[10])]);
            assert_eq!(shard.write(b, LIVE, write), Ok(&[applied(2, 0, 1)][..]));
        });
    }

    #[test]
    fn refuses_a_stamp_years_ahead_when_the_os_clock_has_no_bound() {
        let mut sim = sim::Sim::new(sim::Config {
            seed: 69,
            ..sim::Config::default()
        });
        // With no bound on the OS clock, mesh time is a century wide.
        let node = sim.node(sim::node::Config {
            wall_error: None,
            ..sim::node::Config::default()
        });
        sim.run_on(&node, |node, tasks| async move {
            let test = Test::new(node, tasks);
            let set = two_indexes();
            let mut shard = test.shard(AREA).await;
            let a = shard.open_writer(writer("a", 1, &set)).expect("synced");
            let now = test.now();
            let far: Stamp = "2036-01-01T00:00:00Z".parse().expect("a valid stamp");
            let ahead = order::Error::Ahead {
                stamp: far,
                latest: Stamp::from_nanos(now.nanos() + LIMITS.ahead.nanos()),
            };
            let bad = frame(&test.pool, &set, &[(2, &[far.nanos()])]);
            assert_eq!(
                shard.write(a, LIVE, bad),
                Ok(&[refused(2, Refusal::Order(ahead))][..])
            );
            let good = frame(&test.pool, &set, &[(2, &[now.nanos()])]);
            assert_eq!(shard.write(a, LIVE, good), Ok(&[applied(2, 0, 1)][..]));
        })
        .expect("the run ends");
    }
}
